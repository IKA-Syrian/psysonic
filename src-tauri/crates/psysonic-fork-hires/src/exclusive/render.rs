//! The exclusive-mode render loop over a sink trait. The device (or a test double) implements [`RenderSink`];
//! this module owns everything else: pulling whole frames from the mixer source, lead-in silence, the gate
//! driven formatter settings and the timing counters.
//!
//! Silence is always written as exact zero bytes (lead-in, an ended source, a source that ends mid-period):
//! it never goes through the formatter, so dither can not put noise into it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::crc_tap::{self, CrcBoard, CrcSink};
use super::gate::GateState;
use crate::format::{should_dither, DitherMode, Formatter, OutputFormat};

/// The fixed attenuation rodio's `Player` applies to every sample (f32 multiply).
pub const MASTER_HEADROOM: f32 = 0.891_254;

/// A wake-up from [`RenderSink::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    /// The device wants data.
    Ready,
    /// Shut down.
    Stop,
    /// The wait timed out without the device asking for data.
    Timeout,
}

/// What the render loop writes to. WASAPI event-driven buffers implement this on Windows.
pub trait RenderSink {
    /// Blocks until the device wants data, a stop was requested, or a timeout elapsed.
    fn wait(&mut self) -> WaitResult;
    /// Frames the device can take right now.
    fn frames_available(&self) -> usize;
    /// Writes `frames` frames (`bytes.len() == frames * block_align`).
    fn write(&mut self, bytes: &[u8], frames: usize) -> Result<(), String>;
}

/// Monotonic clock, injected so tests are deterministic.
pub trait Clock {
    fn now_ms(&self) -> u64;
    /// Microseconds on the same timeline; defaults to `now_ms` scaled, which is exact for fake clocks.
    fn now_us(&self) -> u64 {
        self.now_ms().saturating_mul(1000)
    }
}

/// The process-wide monotonic clock ([`super::now_ms`]); all outputs and the control side share one timeline.
pub struct SystemClock;

impl SystemClock {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        super::now_ms()
    }
    fn now_us(&self) -> u64 {
        super::now_us()
    }
}

#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub channels: u16,
    pub rate_hz: u32,
    pub format: OutputFormat,
    /// Bit depth of the source material, for the dither decision.
    pub source_bits: u32,
    pub dither: DitherMode,
    /// Silence written before the source is first pulled.
    pub lead_in_ms: u32,
    /// Nominal period; a wake more than 1.5x this after the previous one counts as late.
    pub period_ms: u64,
    /// Largest number of frames written per period; sizes the scratch buffers.
    pub max_frames: usize,
    /// A gate heartbeat older than this counts as gate off.
    pub gate_max_age_ms: u64,
    pub seed: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderStats {
    /// Periods that wrote data.
    pub periods: u64,
    pub frames_written: u64,
    /// Wakes more than 1.5x the buffer duration (`frames / rate`, exact) after the previous wake.
    pub late_wakes: u64,
    /// Wakes more than 2x the buffer duration after the previous wake.
    pub gaps_over_2x: u64,
    /// Largest gap between two consecutive wakes, in microseconds.
    pub max_gap_us: u64,
    pub timeouts: u64,
    /// Periods rendered with the gate on (bit-perfect settings).
    pub gate_on_periods: u64,
}

/// Lock-free mirror of [`RenderStats`], published by the render thread and read from any thread.
#[derive(Debug, Default)]
pub struct SharedStats {
    periods: AtomicU64,
    frames_written: AtomicU64,
    late_wakes: AtomicU64,
    gaps_over_2x: AtomicU64,
    max_gap_us: AtomicU64,
    timeouts: AtomicU64,
    gate_on_periods: AtomicU64,
}

impl SharedStats {
    fn publish(&self, s: &RenderStats) {
        self.periods.store(s.periods, Ordering::Relaxed);
        self.frames_written
            .store(s.frames_written, Ordering::Relaxed);
        self.late_wakes.store(s.late_wakes, Ordering::Relaxed);
        self.gaps_over_2x.store(s.gaps_over_2x, Ordering::Relaxed);
        self.max_gap_us.store(s.max_gap_us, Ordering::Relaxed);
        self.timeouts.store(s.timeouts, Ordering::Relaxed);
        self.gate_on_periods
            .store(s.gate_on_periods, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RenderStats {
        RenderStats {
            periods: self.periods.load(Ordering::Relaxed),
            frames_written: self.frames_written.load(Ordering::Relaxed),
            late_wakes: self.late_wakes.load(Ordering::Relaxed),
            gaps_over_2x: self.gaps_over_2x.load(Ordering::Relaxed),
            max_gap_us: self.max_gap_us.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            gate_on_periods: self.gate_on_periods.load(Ordering::Relaxed),
        }
    }
}

pub struct RenderLoop<I: Iterator<Item = f32>, C: Clock> {
    cfg: RenderConfig,
    formatter: Formatter,
    source: I,
    clock: C,
    samples: Vec<f32>,
    bytes: Vec<u8>,
    lead_in_frames_left: usize,
    last_wake_us: Option<u64>,
    /// Exact duration of the previous period's buffer; 0 until one was written.
    last_expected_us: u64,
    applied_gate: Option<bool>,
    stats: RenderStats,
    shared: Option<Arc<SharedStats>>,
    /// Opt-in bit-perfect check (see [`crc_tap`]); one relaxed load per period when off.
    crc: CrcSink,
}

impl<I: Iterator<Item = f32>, C: Clock> RenderLoop<I, C> {
    /// Allocates the scratch buffers; the loop itself never allocates afterwards.
    pub fn new(cfg: RenderConfig, source: I, clock: C) -> Self {
        assert!(cfg.channels > 0 && cfg.rate_hz > 0 && cfg.max_frames > 0);
        let ch = cfg.channels as usize;
        let lead_in_frames_left = (cfg.lead_in_ms as u64 * cfg.rate_hz as u64 / 1000) as usize;
        Self {
            formatter: Formatter::new(cfg.format, cfg.seed),
            samples: vec![0.0; cfg.max_frames * ch],
            bytes: vec![0; cfg.max_frames * ch * cfg.format.bytes_per_sample()],
            source,
            clock,
            lead_in_frames_left,
            last_wake_us: None,
            last_expected_us: 0,
            applied_gate: None,
            stats: RenderStats::default(),
            shared: None,
            crc: CrcSink::new(
                crc_tap::board().clone(),
                cfg.channels,
                cfg.rate_hz,
                cfg.format,
            ),
            cfg,
        }
    }

    /// Uses `board` for the bit-perfect check instead of the process-wide one (tests).
    pub fn with_crc_board(mut self, board: Arc<CrcBoard>) -> Self {
        self.crc = CrcSink::new(board, self.cfg.channels, self.cfg.rate_hz, self.cfg.format);
        self
    }

    /// Mirrors the counters into `shared` after every period and timeout.
    pub fn with_shared_stats(mut self, shared: Arc<SharedStats>) -> Self {
        self.shared = Some(shared);
        self
    }

    fn publish_stats(&self) {
        if let Some(sh) = &self.shared {
            sh.publish(&self.stats);
        }
    }

    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    pub fn formatter(&self) -> &Formatter {
        &self.formatter
    }

    /// Gate on: limiter off, dither off, input gain undoing [`MASTER_HEADROOM`]. Gate off: limiter on, dither per
    /// [`should_dither`] (DSP assumed active), gain 1.0.
    fn apply_gate(&mut self, gate_on: bool) {
        if self.applied_gate == Some(gate_on) {
            return;
        }
        self.applied_gate = Some(gate_on);
        if gate_on {
            self.formatter.set_limiter(false);
            self.formatter.set_dither(false);
            self.formatter.set_input_gain(1.0 / MASTER_HEADROOM as f64);
        } else {
            self.formatter.set_limiter(true);
            self.formatter.set_dither(should_dither(
                self.cfg.dither,
                self.cfg.format.valid_bits() as u32,
                self.cfg.source_bits,
                true,
            ));
            self.formatter.set_input_gain(1.0);
        }
    }

    /// Runs until the sink reports [`WaitResult::Stop`] (`Ok`) or a write fails (`Err`).
    pub fn run<S: RenderSink>(&mut self, sink: &mut S, gate: &GateState) -> Result<(), String> {
        loop {
            match sink.wait() {
                WaitResult::Stop => return Ok(()),
                WaitResult::Timeout => {
                    self.stats.timeouts += 1;
                    self.publish_stats();
                    continue;
                }
                WaitResult::Ready => {}
            }
            let now = self.clock.now_ms();
            let now_us = self.clock.now_us();
            if let Some(last) = self.last_wake_us {
                let gap = now_us.saturating_sub(last);
                self.stats.max_gap_us = self.stats.max_gap_us.max(gap);
                let expected = self.last_expected_us;
                if expected > 0 {
                    // gap > 1.5 * expected and gap > 2 * expected, in integers
                    if gap * 2 > expected * 3 {
                        self.stats.late_wakes += 1;
                    }
                    if gap > expected * 2 {
                        self.stats.gaps_over_2x += 1;
                    }
                }
            }
            self.last_wake_us = Some(now_us);

            let frames = sink.frames_available().min(self.cfg.max_frames);
            if frames == 0 {
                continue;
            }
            self.last_expected_us = frames as u64 * 1_000_000 / self.cfg.rate_hz as u64;
            let gate_on = gate.read(now, self.cfg.gate_max_age_ms);
            self.apply_gate(gate_on);
            let n = self.fill(frames);
            self.crc.process(&self.bytes[..n], frames, gate_on);
            sink.write(&self.bytes[..n], frames)?;
            self.stats.periods += 1;
            self.stats.frames_written += frames as u64;
            self.stats.gate_on_periods += gate_on as u64;
            self.publish_stats();
        }
    }

    /// Renders `frames` frames into `self.bytes`; returns the byte count.
    fn fill(&mut self, frames: usize) -> usize {
        let ch = self.cfg.channels as usize;
        let width = self.cfg.format.bytes_per_sample();
        let lead = self.lead_in_frames_left.min(frames);
        self.lead_in_frames_left -= lead;

        // Pull whole frames after the lead-in. A frame with at least one real sample counts as valid.
        let mut valid_end = lead;
        for f in lead..frames {
            let mut any = false;
            for c in 0..ch {
                // Never latch on `None`: rodio's `MixerSource` returns `None` whenever no source is active
                // (before the first track, between tracks), and sources may be added at any later time.
                let s = match self.source.next() {
                    Some(v) => {
                        any = true;
                        v
                    }
                    None => 0.0,
                };
                self.samples[f * ch + c] = s;
            }
            if any {
                valid_end = f + 1;
            }
        }

        let total = frames * ch * width;
        let (lo, hi) = (lead * ch * width, valid_end * ch * width);
        self.bytes[..lo].fill(0);
        self.bytes[hi..total].fill(0);
        // Digital silence stays exactly zero: no dither, nothing through the formatter.
        let silent = self.samples[lead * ch..valid_end * ch]
            .iter()
            .all(|&x| x == 0.0);
        if silent {
            self.bytes[lo..hi].fill(0);
        } else {
            self.formatter.write_le(
                &self.samples[lead * ch..valid_end * ch],
                &mut self.bytes[lo..hi],
            );
        }
        total
    }
}
