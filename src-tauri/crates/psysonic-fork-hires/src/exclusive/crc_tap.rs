//! Opt-in end-to-end bit-perfect check for the exclusive path (a "CRC tap").
//!
//! Enabled with `PSYSONIC_FORK_CRC_TAP=1` (read once). Two halves share a [`CrcBoard`]:
//!
//! - **Source side, [`CrcTapSource`]:** a pass-through wrapper (it changes no sample) placed at the end of a
//!   track's upstream chain, before the HQ stage. After skipping the first second (fade-in and settling) it
//!   records a window: a marker (the first 8 frames from the first non-silent frame on) and an FNV-1a 64 hash
//!   of `N` frames of integer codes `x * 2^23`. A window only counts when every sample is exactly on the 24-bit
//!   grid. The window is published to the board.
//! - **Sink side, [`CrcSink`]** (owned by the render loop): while the gate is on, it decodes the codes the
//!   formatter wrote, searches for the marker, hashes the following frames and compares the hash.
//!
//! Result states are in [`CrcResult`]; they are stored per track (`seq`). Nothing allocates on the render path:
//! the source preallocates its code buffer when it is built, the sink its receive buffer when it is built.
//! Disabled, the render loop pays one relaxed atomic load per period.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};

use crate::format::OutputFormat;

/// Frames in the marker.
pub const MARKER_FRAMES: usize = 8;
/// Most channels the tap handles; more is [`CrcResult::NotApplicable`].
pub const MAX_CHANNELS: usize = 8;
/// Default window length in frames (1 s at 48 kHz).
pub const DEFAULT_WINDOW_FRAMES: u32 = 48_000;
/// Results kept per track.
const RESULT_SLOTS: usize = 8;
const GRID_SCALE: f32 = 8_388_608.0; // 2^23

pub const NA_NOT_GRID24: &str = "source is not on a 24-bit grid";
pub const NA_NOT_GRID16: &str = "source is not on a 16-bit grid";
pub const NA_CHANNELS: &str = "channel count differs from the stream";
pub const NA_RATE: &str = "sample rate differs from the stream";
pub const NA_FORMAT: &str = "output format is not 16/24/32-bit integer";
pub const NA_TOO_MANY_CHANNELS: &str = "more than 8 channels";
pub const NA_SEEKED: &str = "seek during the window";
pub const NA_SUPERSEDED: &str = "next track started before the window completed";
pub const NA_WINDOW_TOO_BIG: &str = "window larger than the sink buffer";

/// The state of one track's check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcResult {
    Pending,
    Match {
        frames: u64,
    },
    /// First differing frame, counted from the start of the window. `0` also covers a marker that never
    /// showed up in the output while the gate was on.
    Mismatch {
        frame: u64,
    },
    NotApplicable(&'static str),
    GateDropped,
}

impl CrcResult {
    /// Human-readable form for status and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Pending => "pending".to_string(),
            Self::Match { frames } => format!("match ({frames} frames)"),
            Self::Mismatch { frame } => format!("mismatch at frame {frame}"),
            Self::NotApplicable(r) => format!("not applicable: {r}"),
            Self::GateDropped => "gate dropped".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Empty,
    /// Marker and window length published, recording continues.
    Marker,
    /// Hash and codes published.
    Complete,
    /// The source gave up; its result is already stored.
    Aborted,
}

/// Shared codes of a completed window (for locating a mismatch).
type SharedCodes = Arc<Mutex<Vec<i32>>>;

struct Slot {
    seq: u64,
    phase: Phase,
    channels: u16,
    rate_hz: u32,
    marker: [i32; MARKER_FRAMES * MAX_CHANNELS],
    marker_grid16: bool,
    n_frames: u64,
    hash: u64,
    grid16: bool,
    codes: Option<SharedCodes>,
}

#[derive(Clone)]
struct SlotView {
    seq: u64,
    channels: u16,
    rate_hz: u32,
    marker: [i32; MARKER_FRAMES * MAX_CHANNELS],
    marker_grid16: bool,
    n_frames: u64,
    hash: u64,
    grid16: bool,
    codes: Option<SharedCodes>,
}

struct Results {
    entries: [(u64, CrcResult); RESULT_SLOTS],
    /// Highest seq already handed out by [`CrcBoard::take_unreported`].
    reported: u64,
}

/// The meeting point of sources and sinks. Process-wide in production ([`board`]); tests make their own.
pub struct CrcBoard {
    enabled: AtomicBool,
    window_frames: AtomicU64,
    next_seq: AtomicU64,
    slot: Mutex<Slot>,
    results: Mutex<Results>,
}

impl CrcBoard {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            window_frames: AtomicU64::new(DEFAULT_WINDOW_FRAMES as u64),
            next_seq: AtomicU64::new(1),
            slot: Mutex::new(Slot {
                seq: 0,
                phase: Phase::Empty,
                channels: 0,
                rate_hz: 0,
                marker: [0; MARKER_FRAMES * MAX_CHANNELS],
                marker_grid16: false,
                n_frames: 0,
                hash: 0,
                grid16: false,
                codes: None,
            }),
            results: Mutex::new(Results {
                entries: [(0, CrcResult::Pending); RESULT_SLOTS],
                reported: 0,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Window length used by sources built from now on, and by sinks built from now on for their buffer.
    pub fn window_frames(&self) -> u64 {
        self.window_frames.load(Ordering::Relaxed)
    }

    pub fn set_window_frames(&self, frames: u64) {
        self.window_frames
            .store(frames.max(MARKER_FRAMES as u64), Ordering::Relaxed);
    }

    fn alloc_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Stores the result of track `seq` (overwrites that seq, else the oldest entry).
    pub fn set_result(&self, seq: u64, r: CrcResult) {
        let mut g = self.results.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.entries.iter_mut().find(|e| e.0 == seq) {
            e.1 = r;
            return;
        }
        let oldest = (0..RESULT_SLOTS)
            .min_by_key(|&i| g.entries[i].0)
            .unwrap_or(0);
        g.entries[oldest] = (seq, r);
    }

    pub fn result(&self, seq: u64) -> Option<CrcResult> {
        let g = self.results.lock().unwrap_or_else(|e| e.into_inner());
        g.entries
            .iter()
            .find(|e| e.0 == seq && seq != 0)
            .map(|e| e.1)
    }

    /// The result of the newest track that has one.
    pub fn latest(&self) -> Option<(u64, CrcResult)> {
        let g = self.results.lock().unwrap_or_else(|e| e.into_inner());
        g.entries
            .iter()
            .filter(|e| e.0 != 0)
            .max_by_key(|e| e.0)
            .copied()
    }

    /// The oldest final (non-pending) result not handed out before; for once-per-track logging.
    pub fn take_unreported(&self) -> Option<(u64, CrcResult)> {
        let mut g = self.results.lock().unwrap_or_else(|e| e.into_inner());
        let reported = g.reported;
        let next = g
            .entries
            .iter()
            .filter(|e| e.0 > reported && e.1 != CrcResult::Pending)
            .min_by_key(|e| e.0)
            .copied()?;
        g.reported = next.0;
        Some(next)
    }

    /// Cheap look at the slot without blocking; `None` when contended.
    fn peek(&self) -> Option<(u64, Phase)> {
        let g = self.slot.try_lock().ok()?;
        Some((g.seq, g.phase))
    }

    fn snapshot(&self) -> Option<SlotView> {
        let g = self.slot.try_lock().ok()?;
        Some(SlotView {
            seq: g.seq,
            channels: g.channels,
            rate_hz: g.rate_hz,
            marker: g.marker,
            marker_grid16: g.marker_grid16,
            n_frames: g.n_frames,
            hash: g.hash,
            grid16: g.grid16,
            codes: g.codes.clone(),
        })
    }
}

impl Default for CrcBoard {
    fn default() -> Self {
        Self::new(false)
    }
}

/// The process-wide board. Enabled by `PSYSONIC_FORK_CRC_TAP=1` (read once), or by [`set_enabled`].
pub fn board() -> &'static Arc<CrcBoard> {
    static BOARD: OnceLock<Arc<CrcBoard>> = OnceLock::new();
    BOARD.get_or_init(|| {
        let on = std::env::var("PSYSONIC_FORK_CRC_TAP").is_ok_and(|v| v == "1");
        Arc::new(CrcBoard::new(on))
    })
}

/// Is the tap on (process-wide board)?
pub fn enabled() -> bool {
    board().enabled()
}

/// Switches the process-wide tap; for tests and tools. Takes effect for sources and render loops built after.
pub fn set_enabled(on: bool) {
    board().set_enabled(on);
}

/// FNV-1a 64 over `code` as 4 little-endian bytes.
#[inline]
fn fnv_feed(mut h: u64, code: i32) -> u64 {
    for b in code.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

// ---- source side ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapState {
    /// Disabled at construction: pure pass-through.
    Off,
    Skipping,
    WaitNonZero,
    Recording,
    Done,
}

/// Window parameters of a [`CrcTapSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapConfig {
    /// Frames to skip before looking for the window; `None` = one second of the source's frames.
    pub skip_frames: Option<u64>,
    /// Window length in frames (at least [`MARKER_FRAMES`]).
    pub window_frames: u64,
}

/// Pass-through wrapper that records the check window; see the module docs. Never changes a sample.
pub struct CrcTapSource<S: Source> {
    inner: S,
    board: Arc<CrcBoard>,
    seq: u64,
    state: TapState,
    channels: usize,
    skip_target: u64,
    window: u64,
    frames_seen: u64,
    pos: usize,
    cur: [f32; MAX_CHANNELS],
    rec_frames: u64,
    hash: u64,
    grid16: bool,
    marker: [i32; MARKER_FRAMES * MAX_CHANNELS],
    marker_grid16: bool,
    codes: Vec<i32>,
    shared: Option<SharedCodes>,
}

impl<S: Source> CrcTapSource<S> {
    /// On the process-wide board with default parameters. A pure pass-through when the tap is off.
    pub fn new(inner: S) -> Self {
        let b = board().clone();
        let cfg = TapConfig {
            skip_frames: None,
            window_frames: b.window_frames(),
        };
        Self::with_board(inner, b, cfg)
    }

    /// On `board` with explicit parameters; armed iff the board is enabled now.
    pub fn with_board(inner: S, board: Arc<CrcBoard>, cfg: TapConfig) -> Self {
        let armed = board.enabled();
        let channels = inner.channels().get() as usize;
        let window = cfg.window_frames.max(MARKER_FRAMES as u64);
        let skip_target = cfg
            .skip_frames
            .unwrap_or_else(|| inner.sample_rate().get() as u64);
        let (state, seq, codes, shared) = if armed {
            (
                TapState::Skipping,
                board.alloc_seq(),
                Vec::with_capacity(window as usize * channels.min(MAX_CHANNELS)),
                Some(Arc::new(Mutex::new(Vec::new()))),
            )
        } else {
            (TapState::Off, 0, Vec::new(), None)
        };
        let too_many = armed && channels > MAX_CHANNELS;
        if too_many {
            board.set_result(seq, CrcResult::NotApplicable(NA_TOO_MANY_CHANNELS));
        }
        Self {
            inner,
            board,
            seq,
            state: if too_many { TapState::Done } else { state },
            channels,
            skip_target,
            window,
            frames_seen: 0,
            pos: 0,
            cur: [0.0; MAX_CHANNELS],
            rec_frames: 0,
            hash: FNV_OFFSET,
            grid16: true,
            marker: [0; MARKER_FRAMES * MAX_CHANNELS],
            marker_grid16: true,
            codes,
            shared,
        }
    }

    /// The track id this source publishes under (0 when the tap was off at construction).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn abort(&mut self, reason: &'static str) {
        self.board
            .set_result(self.seq, CrcResult::NotApplicable(reason));
        let mut g = self.board.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.seq <= self.seq {
            g.seq = self.seq;
            g.phase = Phase::Aborted;
            g.codes = None;
        }
        drop(g);
        self.state = TapState::Done;
    }

    fn publish_marker(&mut self) {
        self.board.set_result(self.seq, CrcResult::Pending);
        let mut g = self.board.slot.lock().unwrap_or_else(|e| e.into_inner());
        g.seq = self.seq;
        g.phase = Phase::Marker;
        g.channels = self.channels as u16;
        g.rate_hz = self.inner.sample_rate().get();
        g.marker = self.marker;
        g.marker_grid16 = self.marker_grid16;
        g.n_frames = self.window;
        g.codes = None;
    }

    fn publish_complete(&mut self) {
        if let Some(shared) = &self.shared {
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::swap(&mut *sh, &mut self.codes);
        }
        let mut g = self.board.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.seq == self.seq {
            g.phase = Phase::Complete;
            g.hash = self.hash;
            g.grid16 = self.grid16;
            g.codes = self.shared.clone();
        }
        drop(g);
        self.state = TapState::Done;
    }

    fn on_frame(&mut self) {
        match self.state {
            TapState::Skipping => {
                self.frames_seen += 1;
                if self.frames_seen >= self.skip_target {
                    self.state = TapState::WaitNonZero;
                }
            }
            TapState::WaitNonZero => {
                if self.cur[..self.channels].iter().all(|&x| x == 0.0) {
                    return;
                }
                if self.inner.channels().get() as usize != self.channels {
                    self.abort(NA_CHANNELS);
                    return;
                }
                self.state = TapState::Recording;
                self.record_frame();
            }
            TapState::Recording => self.record_frame(),
            TapState::Off | TapState::Done => {}
        }
    }

    fn record_frame(&mut self) {
        let ch = self.channels;
        if self.inner.channels().get() as usize != ch {
            self.abort(NA_CHANNELS);
            return;
        }
        let f = self.rec_frames as usize;
        for c in 0..ch {
            let v = self.cur[c] * GRID_SCALE;
            if !(v.is_finite() && v.fract() == 0.0 && (-GRID_SCALE..GRID_SCALE).contains(&v)) {
                self.abort(NA_NOT_GRID24);
                return;
            }
            let code = v as i32;
            let low_clear = code & 0xFF == 0;
            self.grid16 &= low_clear;
            self.hash = fnv_feed(self.hash, code);
            self.codes.push(code);
            if f < MARKER_FRAMES {
                self.marker[f * ch + c] = code;
                self.marker_grid16 &= low_clear;
            }
        }
        self.rec_frames += 1;
        if self.rec_frames == MARKER_FRAMES as u64 {
            self.publish_marker();
        }
        if self.rec_frames == self.window {
            self.publish_complete();
        }
    }
}

impl<S: Source> Iterator for CrcTapSource<S> {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        if matches!(self.state, TapState::Off | TapState::Done) {
            return Some(x);
        }
        if self.pos < MAX_CHANNELS {
            self.cur[self.pos] = x;
        }
        self.pos += 1;
        if self.pos >= self.channels {
            self.pos = 0;
            self.on_frame();
        }
        Some(x)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for CrcTapSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        match self.state {
            TapState::Recording => self.abort(NA_SEEKED),
            TapState::Skipping | TapState::WaitNonZero => self.state = TapState::Done,
            TapState::Off | TapState::Done => {}
        }
        self.pos = 0;
        Ok(())
    }
}

// ---- sink side --------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkState {
    Idle,
    Searching {
        left: u64,
    },
    Hashing,
    /// All frames hashed, waiting for the source's hash to be published.
    Awaiting,
}

/// The render-side half. Owns its buffers; [`process`](Self::process) never allocates.
pub struct CrcSink {
    board: Arc<CrcBoard>,
    channels: usize,
    rate_hz: u32,
    format: OutputFormat,
    supported: bool,
    /// Built while the board was enabled (buffers exist).
    armed: bool,
    state: SinkState,
    last_seq: u64,
    seq: u64,
    ring: Vec<i32>,
    ring_pos: usize,
    ring_filled: usize,
    marker: [i32; MARKER_FRAMES * MAX_CHANNELS],
    n_frames: u64,
    rec: u64,
    hash: u64,
    recv: Vec<i32>,
    cap_frames: u64,
}

impl CrcSink {
    pub fn new(board: Arc<CrcBoard>, channels: u16, rate_hz: u32, format: OutputFormat) -> Self {
        let armed = board.enabled();
        let ch = channels as usize;
        let supported = ch > 0
            && ch <= MAX_CHANNELS
            && match format {
                OutputFormat::F32 => false,
                OutputFormat::S16 | OutputFormat::S24Packed => true,
                OutputFormat::S32 { valid_bits } => valid_bits >= 24,
            };
        let cap_frames = if armed { board.window_frames() } else { 0 };
        Self {
            channels: ch,
            rate_hz,
            format,
            supported,
            armed,
            state: SinkState::Idle,
            last_seq: 0,
            seq: 0,
            ring: vec![0; MARKER_FRAMES * ch.max(1)],
            ring_pos: 0,
            ring_filled: 0,
            marker: [0; MARKER_FRAMES * MAX_CHANNELS],
            n_frames: 0,
            rec: 0,
            hash: FNV_OFFSET,
            recv: Vec::with_capacity(if armed && supported {
                cap_frames as usize * ch
            } else {
                0
            }),
            cap_frames,
            board,
        }
    }

    /// Looks at one period. `bytes` holds `frames` frames as the formatter wrote them; `gate_on` is the gate the
    /// period was rendered with.
    pub fn process(&mut self, bytes: &[u8], frames: usize, gate_on: bool) {
        if !self.board.enabled.load(Ordering::Relaxed) {
            self.state = SinkState::Idle;
            return;
        }
        if !self.armed {
            return;
        }
        self.poll(gate_on);
        if matches!(
            self.state,
            SinkState::Searching { .. } | SinkState::Hashing | SinkState::Awaiting
        ) && !gate_on
        {
            self.finish(CrcResult::GateDropped);
        }
        if !self.supported {
            return;
        }
        let ch = self.channels;
        let width = self.format.bytes_per_sample();
        let mut cur = [0i32; MAX_CHANNELS];
        for f in 0..frames {
            for (c, slot) in cur.iter_mut().enumerate().take(ch) {
                let at = (f * ch + c) * width;
                *slot = self.decode(&bytes[at..at + width]);
            }
            self.push_ring(&cur[..ch]);
            match self.state {
                SinkState::Searching { left } => {
                    if self.ring_matches_marker() {
                        self.begin_hashing();
                    } else if left <= 1 {
                        self.finish(CrcResult::Mismatch { frame: 0 });
                    } else {
                        self.state = SinkState::Searching { left: left - 1 };
                    }
                }
                SinkState::Hashing => {
                    for &code in &cur[..ch] {
                        self.hash = fnv_feed(self.hash, code);
                        self.recv.push(code);
                    }
                    self.rec += 1;
                    if self.rec == self.n_frames {
                        self.state = SinkState::Awaiting;
                    }
                }
                SinkState::Idle | SinkState::Awaiting => {}
            }
        }
        if self.state == SinkState::Awaiting {
            self.try_complete();
        }
    }

    #[inline]
    fn decode(&self, b: &[u8]) -> i32 {
        match self.format {
            OutputFormat::S24Packed => {
                (((b[2] as i8) as i32) << 16) | ((b[1] as i32) << 8) | b[0] as i32
            }
            OutputFormat::S16 => (i16::from_le_bytes([b[0], b[1]]) as i32) << 8,
            OutputFormat::S32 { .. } => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8,
            OutputFormat::F32 => 0,
        }
    }

    fn push_ring(&mut self, frame: &[i32]) {
        let ch = self.channels;
        let at = self.ring_pos * ch;
        self.ring[at..at + ch].copy_from_slice(frame);
        self.ring_pos = (self.ring_pos + 1) % MARKER_FRAMES;
        self.ring_filled = (self.ring_filled + 1).min(MARKER_FRAMES);
    }

    /// The last 8 frames, oldest first, equal the marker.
    fn ring_matches_marker(&self) -> bool {
        if self.ring_filled < MARKER_FRAMES {
            return false;
        }
        let ch = self.channels;
        (0..MARKER_FRAMES).all(|i| {
            let slot = (self.ring_pos + i) % MARKER_FRAMES;
            self.ring[slot * ch..(slot + 1) * ch] == self.marker[i * ch..(i + 1) * ch]
        })
    }

    fn begin_hashing(&mut self) {
        let ch = self.channels;
        self.hash = FNV_OFFSET;
        self.recv.clear();
        for &code in &self.marker[..MARKER_FRAMES * ch] {
            self.hash = fnv_feed(self.hash, code);
            self.recv.push(code);
        }
        self.rec = MARKER_FRAMES as u64;
        self.state = if self.rec == self.n_frames {
            SinkState::Awaiting
        } else {
            SinkState::Hashing
        };
    }

    fn finish(&mut self, r: CrcResult) {
        self.board.set_result(self.seq, r);
        self.state = SinkState::Idle;
    }

    fn poll(&mut self, gate_on: bool) {
        let Some((seq, phase)) = self.board.peek() else {
            return;
        };
        if self.state != SinkState::Idle && seq > self.seq {
            self.finish(CrcResult::NotApplicable(NA_SUPERSEDED));
        } else if self.state != SinkState::Idle && seq == self.seq && phase == Phase::Aborted {
            self.state = SinkState::Idle;
        }
        if self.state != SinkState::Idle || seq <= self.last_seq || phase == Phase::Empty {
            return;
        }
        self.last_seq = seq;
        if phase == Phase::Aborted {
            return;
        }
        let Some(v) = self.board.snapshot() else {
            self.last_seq = seq - 1;
            return;
        };
        self.seq = v.seq;
        let ch = self.channels;
        let verdict = if v.channels as usize != ch {
            Some(CrcResult::NotApplicable(NA_CHANNELS))
        } else if v.rate_hz != self.rate_hz {
            Some(CrcResult::NotApplicable(NA_RATE))
        } else if !self.supported {
            Some(CrcResult::NotApplicable(NA_FORMAT))
        } else if self.format == OutputFormat::S16 && !v.marker_grid16 {
            Some(CrcResult::NotApplicable(NA_NOT_GRID16))
        } else if v.n_frames > self.cap_frames {
            Some(CrcResult::NotApplicable(NA_WINDOW_TOO_BIG))
        } else if !gate_on {
            Some(CrcResult::GateDropped)
        } else {
            None
        };
        if let Some(r) = verdict {
            self.board.set_result(self.seq, r);
            return;
        }
        self.marker = v.marker;
        self.n_frames = v.n_frames;
        self.state = SinkState::Searching {
            left: self.rate_hz as u64,
        };
    }

    fn try_complete(&mut self) {
        let Some((seq, phase)) = self.board.peek() else {
            return;
        };
        if seq > self.seq {
            self.finish(CrcResult::NotApplicable(NA_SUPERSEDED));
            return;
        }
        if seq != self.seq || phase != Phase::Complete {
            if phase == Phase::Aborted && seq == self.seq {
                self.state = SinkState::Idle;
            }
            return;
        }
        let Some(v) = self.board.snapshot() else {
            return;
        };
        let r = if self.format == OutputFormat::S16 && !v.grid16 {
            CrcResult::NotApplicable(NA_NOT_GRID16)
        } else if v.hash == self.hash {
            CrcResult::Match {
                frames: self.n_frames,
            }
        } else {
            CrcResult::Mismatch {
                frame: self.locate(v.codes.as_ref()),
            }
        };
        self.finish(r);
    }

    /// First differing frame between what was received and the source's codes.
    fn locate(&self, codes: Option<&SharedCodes>) -> u64 {
        let Some(codes) = codes else {
            return 0;
        };
        let g = codes.lock().unwrap_or_else(|e| e.into_inner());
        let ch = self.channels.max(1);
        self.recv
            .iter()
            .zip(g.iter())
            .position(|(a, b)| a != b)
            .map_or(0, |i| (i / ch) as u64)
    }
}

#[cfg(test)]
#[path = "crc_tap_tests.rs"]
mod tests;
