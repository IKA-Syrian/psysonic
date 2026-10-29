use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::gate::GateState;
use super::render::*;
use crate::format::{DitherMode, Formatter, OutputFormat};

// Counts allocations made on the current thread while `COUNTING` is set.
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

/// Shared fake clock.
#[derive(Clone, Default)]
struct FakeClock(Arc<AtomicU64>);

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Scripted sink: replays `script`, then stops. Before each `Ready` it advances the clock by the next step.
struct NullSink {
    script: Vec<WaitResult>,
    steps: Vec<u64>,
    pos: usize,
    clock: FakeClock,
    available: usize,
    written: Vec<u8>,
    writes: Vec<usize>,
}

impl NullSink {
    fn new(periods: usize, available: usize, clock: &FakeClock) -> Self {
        Self {
            script: vec![WaitResult::Ready; periods],
            steps: vec![10; periods],
            pos: 0,
            clock: clock.clone(),
            available,
            written: Vec::with_capacity(1 << 20),
            writes: Vec::with_capacity(1024),
        }
    }
}

impl RenderSink for NullSink {
    fn wait(&mut self) -> WaitResult {
        let Some(&r) = self.script.get(self.pos) else {
            return WaitResult::Stop;
        };
        let step = self.steps[self.pos];
        self.pos += 1;
        self.clock.0.fetch_add(step, Ordering::Relaxed);
        r
    }
    fn frames_available(&self) -> usize {
        self.available
    }
    fn write(&mut self, bytes: &[u8], frames: usize) -> Result<(), String> {
        self.written.extend_from_slice(bytes);
        self.writes.push(frames);
        Ok(())
    }
}

fn cfg(format: OutputFormat) -> RenderConfig {
    RenderConfig {
        channels: 2,
        rate_hz: 48_000,
        format,
        source_bits: 24,
        dither: DitherMode::Auto,
        lead_in_ms: 0,
        period_ms: 10,
        max_frames: 480,
        gate_max_age_ms: 500,
        seed: 7,
    }
}

const S24: OutputFormat = OutputFormat::S24Packed;
const S32: OutputFormat = OutputFormat::S32 { valid_bits: 24 };
const S16: OutputFormat = OutputFormat::S16;

fn gate_off() -> GateState {
    GateState::new()
}

fn gate_on(clock: &FakeClock) -> GateState {
    let g = GateState::new();
    g.publish(true, clock.now_ms());
    g
}

fn run_with<I: Iterator<Item = f32>>(
    c: RenderConfig,
    src: I,
    periods: usize,
    avail: usize,
    gate: &GateState,
    clock: &FakeClock,
) -> (Vec<u8>, RenderStats) {
    let mut sink = NullSink::new(periods, avail, clock);
    let mut rl = RenderLoop::new(c, src, clock.clone());
    rl.run(&mut sink, gate).unwrap();
    (sink.written, rl.stats())
}

#[test]
fn known_input_gives_expected_s24_and_s32_codes() {
    let input = [0.5f32, -0.5, 0.25, 0.0];
    let clock = FakeClock::default();
    let (b24, _) = run_with(cfg(S24), input.iter().copied(), 1, 2, &gate_off(), &clock);
    let codes = [4_194_304i32, -4_194_304, 2_097_152, 0];
    let mut want = Vec::new();
    for c in codes {
        want.extend_from_slice(&c.to_le_bytes()[..3]);
    }
    assert_eq!(b24, want);

    let (b32, _) = run_with(cfg(S32), input.iter().copied(), 1, 2, &gate_off(), &clock);
    let mut want = Vec::new();
    for c in codes {
        want.extend_from_slice(&(c << 8).to_le_bytes());
    }
    assert_eq!(b32, want);
}

#[test]
fn empty_and_ended_sources_give_silence() {
    let clock = FakeClock::default();
    let (b, _) = run_with(cfg(S24), std::iter::empty(), 3, 480, &gate_off(), &clock);
    assert_eq!(b.len(), 3 * 480 * 2 * 3);
    assert!(b.iter().all(|&x| x == 0));

    // Ends mid-period (and mid-frame: 7 samples): remaining bytes are exact zeros even with dither on.
    let mut c = cfg(S16);
    c.source_bits = 24;
    let src = std::iter::repeat_n(0.5f32, 7);
    let (b, _) = run_with(c, src, 2, 480, &gate_off(), &clock);
    assert_eq!(b.len(), 2 * 480 * 2 * 2);
    // 7 samples: three whole frames and one sample of the fourth; everything after that frame is exact zeros.
    assert!(b[4 * 2 * 2..].iter().all(|&x| x == 0));
    assert_ne!(&b[..2], &[0, 0]);
}

#[test]
fn only_whole_frames_are_pulled() {
    let pulls = Arc::new(AtomicU64::new(0));
    let p = pulls.clone();
    let src = std::iter::from_fn(move || {
        p.fetch_add(1, Ordering::Relaxed);
        Some(0.1)
    });
    let clock = FakeClock::default();
    let (b, _) = run_with(cfg(S24), src, 4, 3, &gate_off(), &clock);
    assert_eq!(pulls.load(Ordering::Relaxed), 4 * 3 * 2);
    assert_eq!(b.len(), 4 * 3 * 2 * 3);
}

#[test]
fn lead_in_is_exactly_n_ms_of_zeros() {
    let mut c = cfg(S24);
    c.lead_in_ms = 25; // 1200 frames: two full periods and 240 frames of the third
    let clock = FakeClock::default();
    let src = std::iter::repeat(0.5f32);
    let (b, _) = run_with(c, src, 4, 480, &gate_off(), &clock);
    let frame = 2 * 3;
    let lead_bytes = 1200 * frame;
    assert!(b[..lead_bytes].iter().all(|&x| x == 0));
    // The very first source sample lands at the first frame after the lead-in.
    assert_eq!(
        &b[lead_bytes..lead_bytes + 3],
        &4_194_304i32.to_le_bytes()[..3]
    );
    assert_eq!(b.len(), 4 * 480 * frame);
}

#[test]
fn lead_in_does_not_consume_the_source() {
    let mut c = cfg(S24);
    c.lead_in_ms = 10;
    let mut n = 0u32;
    let src = std::iter::from_fn(move || {
        n += 1;
        Some(n as f32 / 1_000_000.0)
    });
    let clock = FakeClock::default();
    let (b, _) = run_with(c, src, 2, 480, &gate_off(), &clock);
    let first = i32::from_le_bytes([b[480 * 6], b[480 * 6 + 1], b[480 * 6 + 2], 0]);
    assert_eq!(first, (1e-6f64 * 8_388_608.0).round() as i32); // sample #1
}

#[test]
fn gate_on_is_bit_exact_for_headroomed_24_bit_input() {
    let clock = FakeClock::default();
    let codes: Vec<i32> = (0..960).map(|i| (i - 480) * 17_393 + (i % 7)).collect();
    let src: Vec<f32> = codes
        .iter()
        .map(|&k| (k as f32 / 8_388_608.0) * MASTER_HEADROOM)
        .collect();
    for format in [S24, S32] {
        let (b, st) = run_with(
            cfg(format),
            src.clone().into_iter(),
            1,
            480,
            &gate_on(&clock),
            &clock,
        );
        assert_eq!(st.gate_on_periods, 1);
        let w = format.bytes_per_sample();
        for (i, &k) in codes.iter().enumerate() {
            let s = &b[i * w..(i + 1) * w];
            let got = if w == 3 {
                i32::from_le_bytes([s[0], s[1], s[2], if s[2] & 0x80 != 0 { 0xff } else { 0 }])
            } else {
                i32::from_le_bytes(s.try_into().unwrap()) >> 8
            };
            assert_eq!(got, k, "sample {i}");
        }
    }
}

#[test]
fn gate_on_passes_full_scale_without_limiting_and_gate_off_limits() {
    let clock = FakeClock::default();
    let y = 0.99f32 * MASTER_HEADROOM;
    let src = [y; 2];
    let (on, _) = run_with(
        cfg(S24),
        src.iter().copied(),
        1,
        1,
        &gate_on(&clock),
        &clock,
    );
    let want = (y as f64 / MASTER_HEADROOM as f64 * 8_388_608.0).round() as i32;
    assert_eq!(i32::from_le_bytes([on[0], on[1], on[2], 0]), want);
    let loud = [0.99f32; 2];
    let (off, _) = run_with(cfg(S24), loud.iter().copied(), 1, 1, &gate_off(), &clock);
    let got = i32::from_le_bytes([off[0], off[1], off[2], 0]);
    assert!(
        got < (0.99f64 * 8_388_608.0) as i32,
        "limiter must act: {got}"
    );
}

#[test]
fn stale_gate_counts_as_off() {
    let clock = FakeClock::default();
    let g = GateState::new();
    g.publish(true, 0);
    // Each wait advances the clock by 10 ms; after 500 ms the heartbeat is stale.
    let mut sink = NullSink::new(100, 1, &clock);
    let mut rl = RenderLoop::new(cfg(S24), std::iter::repeat(0.0f32), clock.clone());
    rl.run(&mut sink, &g).unwrap();
    // Wakes at 10..=500 ms are fresh (50), the rest stale.
    assert_eq!(rl.stats().gate_on_periods, 50);
    assert_eq!(rl.stats().periods, 100);
}

#[test]
fn gate_off_at_16_bit_dithers_deterministically() {
    let clock = FakeClock::default();
    let input: Vec<f32> = (0..960)
        .map(|i| ((i % 100) as f32 - 50.0) * 0.001)
        .collect();
    let (got, _) = run_with(
        cfg(S16),
        input.clone().into_iter(),
        1,
        480,
        &gate_off(),
        &clock,
    );

    let mut reference = Formatter::new(S16, 7);
    reference.set_dither(true);
    let mut want = vec![0u8; 960 * 2];
    reference.write_le(&input, &mut want);
    assert_eq!(got, want);

    let mut plain = Formatter::new(S16, 7);
    let mut undithered = vec![0u8; 960 * 2];
    plain.write_le(&input, &mut undithered);
    assert_ne!(got, undithered, "dither must change some codes");
}

#[test]
fn gate_on_at_16_bit_has_no_dither() {
    let clock = FakeClock::default();
    let mut c = cfg(S16);
    c.source_bits = 16;
    let input: Vec<f32> = (0..960).map(|i| (i as f32 - 480.0) / 32_768.0).collect();
    let y: Vec<f32> = input.iter().map(|&x| x * MASTER_HEADROOM).collect();
    let (got, _) = run_with(c, y.into_iter(), 1, 480, &gate_on(&clock), &clock);
    for i in 0..960 {
        let code = i16::from_le_bytes([got[i * 2], got[i * 2 + 1]]) as i32;
        assert_eq!(code, i as i32 - 480);
    }
}

#[test]
fn stop_is_prompt() {
    let clock = FakeClock::default();
    let pulls = Arc::new(AtomicU64::new(0));
    let p = pulls.clone();
    let src = std::iter::from_fn(move || {
        p.fetch_add(1, Ordering::Relaxed);
        Some(0.1f32)
    });
    let mut sink = NullSink::new(5, 480, &clock);
    sink.script[2] = WaitResult::Stop;
    let mut rl = RenderLoop::new(cfg(S24), src, clock.clone());
    assert!(rl.run(&mut sink, &gate_off()).is_ok());
    assert_eq!(rl.stats().periods, 2);
    assert_eq!(sink.writes, vec![480, 480]);
    assert_eq!(pulls.load(Ordering::Relaxed), 2 * 480 * 2);

    // Stop on the first wait: nothing pulled, nothing written.
    let mut sink = NullSink::new(1, 480, &clock);
    sink.script[0] = WaitResult::Stop;
    let before = pulls.load(Ordering::Relaxed);
    let p = pulls.clone();
    let src = std::iter::from_fn(move || {
        p.fetch_add(1, Ordering::Relaxed);
        Some(0.1f32)
    });
    let mut rl = RenderLoop::new(cfg(S24), src, clock.clone());
    rl.run(&mut sink, &gate_off()).unwrap();
    assert_eq!(pulls.load(Ordering::Relaxed), before);
    assert!(sink.writes.is_empty());
}

#[test]
fn lateness_and_timeouts_are_counted() {
    let clock = FakeClock::default();
    let mut sink = NullSink::new(6, 480, &clock);
    // Wake times: 10, 20, 35 (gap 15: not late), 56 (gap 21: late), then a timeout, then 66.
    sink.steps = vec![10, 10, 15, 21, 5, 5];
    sink.script[4] = WaitResult::Timeout;
    sink.script[5] = WaitResult::Ready;
    let mut rl = RenderLoop::new(cfg(S24), std::iter::repeat(0.0f32), clock.clone());
    rl.run(&mut sink, &gate_off()).unwrap();
    let st = rl.stats();
    assert_eq!(st.late_wakes, 1); // only the 35 -> 56 gap; timeouts do not count as wakes
    assert_eq!(st.timeouts, 1);
    assert_eq!(st.periods, 5);
}

#[test]
fn write_error_propagates() {
    struct Failing;
    impl RenderSink for Failing {
        fn wait(&mut self) -> WaitResult {
            WaitResult::Ready
        }
        fn frames_available(&self) -> usize {
            10
        }
        fn write(&mut self, _: &[u8], _: usize) -> Result<(), String> {
            Err("device lost".into())
        }
    }
    let clock = FakeClock::default();
    let mut rl = RenderLoop::new(cfg(S24), std::iter::repeat(0.0f32), clock);
    assert_eq!(rl.run(&mut Failing, &gate_off()), Err("device lost".into()));
}

#[test]
fn no_allocation_in_steady_state() {
    let clock = FakeClock::default();
    let g = GateState::new();
    g.publish(true, 0);
    let mut sink = NullSink::new(400, 480, &clock);
    sink.steps = vec![1; 400]; // gate goes stale mid-run, exercising both gate settings
    let mut x = 0.0f32;
    let src = std::iter::from_fn(move || {
        x = (x + 0.001) % 0.9;
        Some(x)
    });
    let mut c = cfg(S16);
    c.lead_in_ms = 30;
    c.gate_max_age_ms = 200;
    let mut rl = RenderLoop::new(c, src, clock.clone());

    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    rl.run(&mut sink, &g).unwrap();
    COUNTING.with(|c| c.set(false));
    let n = ALLOCS.with(Cell::get);
    println!("allocations over {} periods: {n}", rl.stats().periods);
    assert_eq!(rl.stats().periods, 400);
    assert!(rl.stats().gate_on_periods > 100 && rl.stats().gate_on_periods < 300);
    assert_eq!(n, 0);
}

// ---- empty-mixer / late-source behaviour (real rodio mixer) ----

/// Calls `hook(n)` before the `n`-th wake (0-based), then reports `Ready` for `periods` wakes.
struct HookSink<F: FnMut(usize)> {
    hook: F,
    periods: usize,
    pos: usize,
    frames: usize,
    periods_out: Vec<Vec<f32>>,
}

impl<F: FnMut(usize)> RenderSink for HookSink<F> {
    fn wait(&mut self) -> WaitResult {
        if self.pos == self.periods {
            return WaitResult::Stop;
        }
        (self.hook)(self.pos);
        self.pos += 1;
        WaitResult::Ready
    }
    fn frames_available(&self) -> usize {
        self.frames
    }
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn write(&mut self, bytes: &[u8], _frames: usize) -> Result<(), String> {
        self.periods_out.push(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        );
        Ok(())
    }
}

fn buffer(frames: usize, v: f32) -> rodio::buffer::SamplesBuffer {
    rodio::buffer::SamplesBuffer::new(
        std::num::NonZero::new(2).unwrap(),
        std::num::NonZero::new(48_000).unwrap(),
        vec![v; frames * 2],
    )
}

/// Runs the loop over a real rodio mixer for `periods` 4-frame periods; `hook(n, &mixer)` runs before wake `n`.
fn run_mixer(periods: usize, mut hook: impl FnMut(usize, &rodio::mixer::Mixer)) -> Vec<Vec<f32>> {
    let (mixer, source) = rodio::mixer::mixer(
        std::num::NonZero::new(2).unwrap(),
        std::num::NonZero::new(48_000).unwrap(),
    );
    let mut sink = HookSink {
        hook: |n| hook(n, &mixer),
        periods,
        pos: 0,
        frames: 4,
        periods_out: Vec::new(),
    };
    let mut rl = RenderLoop::new(cfg(OutputFormat::F32), source, FakeClock::default());
    rl.run(&mut sink, &gate_off()).unwrap();
    sink.periods_out
}

fn is_silent(p: &[f32]) -> bool {
    p.iter().all(|&x| x == 0.0)
}

#[test]
fn source_added_after_empty_periods_is_heard() {
    // Regression: an empty mixer yields `None`, which used to latch the loop into permanent silence.
    let out = run_mixer(10, |n, m| {
        if n == 3 {
            m.add(buffer(8, 0.5));
        }
    });
    assert_eq!(out.len(), 10);
    assert!(
        out[..3].iter().all(|p| is_silent(p)),
        "empty mixer must be silent"
    );
    assert!(out[3].iter().all(|&x| x > 0.4), "{:?}", out[3]);
    assert!(out[4].iter().all(|&x| x > 0.4), "{:?}", out[4]);
    assert!(out[5..].iter().all(|p| is_silent(p)));
}

#[test]
fn second_source_after_the_first_ended_is_heard() {
    let out = run_mixer(12, |n, m| match n {
        1 => m.add(buffer(4, 0.25)),
        6 => m.add(buffer(4, -0.25)),
        _ => {}
    });
    assert!(is_silent(&out[0]));
    assert!(out[1].iter().all(|&x| x > 0.2));
    assert!(
        out[2..6].iter().all(|p| is_silent(p)),
        "gap between tracks is silence"
    );
    assert!(out[6].iter().all(|&x| x < -0.2), "{:?}", out[6]);
    assert!(out[7..].iter().all(|p| is_silent(p)));
}

// ---- digital silence is never dithered ----

#[test]
fn silent_input_stays_bit_zero_even_with_dither() {
    let clock = FakeClock::default();
    // S16 from a 24-bit source, gate off: dither is on for non-silent input.
    let (got, _) = run_with(
        cfg(S16),
        std::iter::repeat(0.0f32),
        3,
        480,
        &gate_off(),
        &clock,
    );
    assert!(!got.is_empty() && got.iter().all(|&b| b == 0));
    // The same dither does touch a signal that is merely very quiet.
    let (quiet, _) = run_with(
        cfg(S16),
        std::iter::repeat(1e-6f32),
        1,
        480,
        &gate_off(),
        &clock,
    );
    assert!(quiet.iter().any(|&b| b != 0));
}

// ---- gap statistics in microseconds ----

/// A clock with an independent µs timeline.
#[derive(Clone, Default)]
struct UsClock(Arc<AtomicU64>);

impl Clock for UsClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed) / 1000
    }
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[test]
fn gap_stats_use_exact_buffer_duration() {
    // 448 frames at 48 kHz = 9333 us. Gaps: 9333 (on time), 13000 (<1.5x), 14100 (late), 19000 (>2x).
    let clock = UsClock::default();
    let gaps = [9_333u64, 13_000, 14_100, 19_000];
    let mut i = 0usize;
    let c = clock.clone();
    let mut sink = HookSink {
        hook: move |n| {
            if n > 0 {
                c.0.fetch_add(gaps[i], Ordering::Relaxed);
                i += 1;
            }
        },
        periods: 5,
        pos: 0,
        frames: 448,
        periods_out: Vec::new(),
    };
    let mut c = cfg(OutputFormat::F32);
    c.max_frames = 448;
    let mut rl = RenderLoop::new(c, std::iter::repeat(0.0f32), clock);
    rl.run(&mut sink, &gate_off()).unwrap();
    let st = rl.stats();
    assert_eq!(st.late_wakes, 2); // 14100 and 19000
    assert_eq!(st.gaps_over_2x, 1); // 19000 > 18666
    assert_eq!(st.max_gap_us, 19_000);
}
