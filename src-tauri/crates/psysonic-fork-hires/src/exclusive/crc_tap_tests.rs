use std::sync::Arc;
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::{ChannelCount, SampleRate, Source};

use super::*;
use crate::exclusive::gate::GateState;
use crate::exclusive::render::{
    Clock, RenderConfig, RenderLoop, RenderSink, SystemClock, WaitResult, MASTER_HEADROOM,
};
use crate::format::DitherMode;

const RATE: u32 = 48_000;
const SKIP: u64 = 100;
const WINDOW: u64 = 1_000;

fn board(window: u64) -> Arc<CrcBoard> {
    let b = Arc::new(CrcBoard::new(true));
    b.set_window_frames(window);
    b
}

fn cfg() -> TapConfig {
    TapConfig {
        skip_frames: Some(SKIP),
        window_frames: WINDOW,
    }
}

/// `frames` stereo frames of a quiet two-tone signal, every sample an exact 24-bit code (never zero).
/// `step` rounds the codes to a multiple of `step` (256 = on the 16-bit grid).
fn codes(frames: usize, step: i32) -> Vec<i32> {
    let mut v = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        for (c, f) in [1000.0f64, 1500.0].iter().enumerate() {
            let t = n as f64 / RATE as f64;
            let x = (2.0 * std::f64::consts::PI * f * t + 0.3 + c as f64).sin();
            let raw = (x * 8_000.0) as i32 + 9_000 * (c as i32 + 1);
            v.push(raw / step * step);
        }
    }
    v
}

fn to_f32(codes: &[i32]) -> Vec<f32> {
    codes.iter().map(|&k| k as f32 / 8_388_608.0).collect()
}

fn buffer(data: Vec<f32>, ch: u16) -> SamplesBuffer {
    SamplesBuffer::new(
        ChannelCount::new(ch).unwrap(),
        SampleRate::new(RATE).unwrap(),
        data,
    )
}

fn encode(codes: &[i32], fmt: OutputFormat) -> Vec<u8> {
    let mut out = Vec::new();
    for &k in codes {
        match fmt {
            OutputFormat::S24Packed => out.extend_from_slice(&k.to_le_bytes()[..3]),
            OutputFormat::S16 => out.extend_from_slice(&((k >> 8) as i16).to_le_bytes()),
            OutputFormat::S32 { .. } => out.extend_from_slice(&(k << 8).to_le_bytes()),
            OutputFormat::F32 => unreachable!(),
        }
    }
    out
}

/// Pulls the tap `period` frames at a time (as the render loop would), hands each period to the sink and lets
/// the caller alter the codes and the gate per period.
struct Run<'a> {
    fmt: OutputFormat,
    period: usize,
    gate: &'a dyn Fn(u64) -> bool,
    /// Called with the global frame index and the frame's codes.
    mutate: &'a dyn Fn(u64, &mut [i32]),
}

fn drive<S: Source>(tap: &mut CrcTapSource<S>, sink: &mut CrcSink, total_frames: usize, run: Run) {
    let ch = tap.channels().get() as usize;
    let mut frame = 0u64;
    while (frame as usize) < total_frames {
        let n = run.period.min(total_frames - frame as usize);
        let mut cs = Vec::with_capacity(n * ch);
        for _ in 0..n {
            let mut fr: Vec<i32> = (0..ch)
                .map(|_| (tap.next().unwrap_or(0.0) * 8_388_608.0).round() as i32)
                .collect();
            (run.mutate)(frame, &mut fr);
            cs.extend_from_slice(&fr);
            frame += 1;
        }
        let bytes = encode(&cs, run.fmt);
        sink.process(&bytes, n, (run.gate)(frame));
    }
}

fn plain<'a>(fmt: OutputFormat) -> Run<'a> {
    Run {
        fmt,
        period: 64,
        gate: &|_| true,
        mutate: &|_, _| {},
    }
}

const S24: OutputFormat = OutputFormat::S24Packed;

fn setup(
    b: &Arc<CrcBoard>,
    data: Vec<f32>,
    fmt: OutputFormat,
) -> (CrcTapSource<SamplesBuffer>, CrcSink) {
    let tap = CrcTapSource::with_board(buffer(data, 2), b.clone(), cfg());
    let sink = CrcSink::new(b.clone(), 2, RATE, fmt);
    (tap, sink)
}

#[test]
fn bit_exact_window_matches_for_every_period_size() {
    for period in [1usize, 7, 64, 480, 1500] {
        let b = board(WINDOW);
        let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), S24);
        let seq = tap.seq();
        drive(
            &mut tap,
            &mut sink,
            3000,
            Run {
                period,
                ..plain(S24)
            },
        );
        assert_eq!(
            b.result(seq),
            Some(CrcResult::Match { frames: WINDOW }),
            "period {period}"
        );
    }
}

#[test]
fn s32_and_s16_formats_match() {
    let s32 = OutputFormat::S32 { valid_bits: 24 };
    let b = board(WINDOW);
    let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), s32);
    drive(&mut tap, &mut sink, 3000, plain(s32));
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::Match { frames: WINDOW })
    );

    // A source on the 16-bit grid compares against `k >> 8`.
    let b = board(WINDOW);
    let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 256)), OutputFormat::S16);
    drive(&mut tap, &mut sink, 3000, plain(OutputFormat::S16));
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::Match { frames: WINDOW })
    );

    // A 24-bit-only source cannot be checked through 16 bits.
    let b = board(WINDOW);
    let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), OutputFormat::S16);
    drive(&mut tap, &mut sink, 3000, plain(OutputFormat::S16));
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::NotApplicable(NA_NOT_GRID16))
    );
}

#[test]
fn a_single_changed_code_is_a_mismatch_at_that_frame() {
    // The window starts at frame SKIP (the signal is never silent).
    for off in [10u64, 500, 999] {
        let b = board(WINDOW);
        let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), S24);
        let hit = SKIP + off;
        drive(
            &mut tap,
            &mut sink,
            3000,
            Run {
                mutate: &move |f, fr| {
                    if f == hit {
                        fr[1] ^= 1;
                    }
                },
                ..plain(S24)
            },
        );
        assert_eq!(
            b.result(tap.seq()),
            Some(CrcResult::Mismatch { frame: off }),
            "offset {off}"
        );
    }
}

#[test]
fn a_source_off_the_24_bit_grid_is_not_applicable() {
    let b = board(WINDOW);
    let mut data = to_f32(&codes(3000, 1));
    data[(SKIP as usize + 600) * 2] += 0.25 / 8_388_608.0; // a quarter of a code
    let (mut tap, mut sink) = setup(&b, data, S24);
    drive(&mut tap, &mut sink, 3000, plain(S24));
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::NotApplicable(NA_NOT_GRID24))
    );
    assert_eq!(NA_NOT_GRID24, "source is not on a 24-bit grid");
}

#[test]
fn gate_dropping_mid_window_invalidates_and_the_next_track_rearms() {
    let b = board(WINDOW);
    let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), S24);
    let first = tap.seq();
    drive(
        &mut tap,
        &mut sink,
        3000,
        Run {
            gate: &|f| !(400..450).contains(&f),
            ..plain(S24)
        },
    );
    assert_eq!(b.result(first), Some(CrcResult::GateDropped));

    // A new track on the same sink is checked on its own.
    let mut tap2 = CrcTapSource::with_board(buffer(to_f32(&codes(3000, 1)), 2), b.clone(), cfg());
    assert!(tap2.seq() > first);
    drive(&mut tap2, &mut sink, 3000, plain(S24));
    assert_eq!(
        b.result(tap2.seq()),
        Some(CrcResult::Match { frames: WINDOW })
    );
    assert_eq!(b.latest().map(|l| l.0), Some(tap2.seq()));
    // Both are reported once, in order.
    assert_eq!(b.take_unreported().map(|r| r.0), Some(first));
    assert_eq!(b.take_unreported().map(|r| r.0), Some(tap2.seq()));
    assert_eq!(b.take_unreported(), None);
}

#[test]
fn gate_off_when_the_window_appears_is_gate_dropped() {
    let b = board(WINDOW);
    let (mut tap, mut sink) = setup(&b, to_f32(&codes(3000, 1)), S24);
    drive(
        &mut tap,
        &mut sink,
        3000,
        Run {
            gate: &|_| false,
            ..plain(S24)
        },
    );
    assert_eq!(b.result(tap.seq()), Some(CrcResult::GateDropped));
}

#[test]
fn channel_and_rate_mismatches_are_not_applicable() {
    let b = board(WINDOW);
    let tap = CrcTapSource::with_board(buffer(to_f32(&codes(3000, 1)), 2), b.clone(), cfg());
    let mut sink = CrcSink::new(b.clone(), 2, 44_100, S24);
    let mut tap = tap;
    drive(&mut tap, &mut sink, 3000, plain(S24));
    assert_eq!(b.result(tap.seq()), Some(CrcResult::NotApplicable(NA_RATE)));

    let b = board(WINDOW);
    let mut tap = CrcTapSource::with_board(
        buffer(
            codes(3000, 1)
                .iter()
                .step_by(2)
                .map(|&k| k as f32 / 8_388_608.0)
                .collect(),
            1,
        ),
        b.clone(),
        cfg(),
    );
    let mut sink = CrcSink::new(b.clone(), 2, RATE, S24);
    // Mono source into a stereo sink: feed the sink stereo bytes of the same length.
    for _ in 0..30 {
        for _ in 0..64 {
            tap.next();
        }
        sink.process(&vec![0u8; 64 * 2 * 3], 64, true);
    }
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::NotApplicable(NA_CHANNELS))
    );
}

#[test]
fn disabled_publishes_nothing_and_changes_nothing() {
    let b = Arc::new(CrcBoard::new(false));
    let data = to_f32(&codes(3000, 1));
    let (mut tap, mut sink) = setup(&b, data.clone(), S24);
    assert_eq!(tap.seq(), 0);
    drive(&mut tap, &mut sink, 3000, plain(S24));
    assert_eq!(b.latest(), None);
    assert!(b
        .peek()
        .is_some_and(|(seq, ph)| seq == 0 && ph == Phase::Empty));
    // Turning it on later does not arm a sink that was built disabled.
    b.set_enabled(true);
    sink.process(&[0u8; 6], 2, true);
    assert_eq!(b.latest(), None);
}

#[test]
fn tap_output_is_bit_identical_to_its_input() {
    // On-grid, off-grid, zeros, signed zero, NaN payloads, infinities, denormals, out-of-range.
    let mut data = to_f32(&codes(3000, 1));
    let odd = [
        0.0f32,
        -0.0,
        f32::NAN,
        f32::from_bits(0x7fc0_1234),
        f32::INFINITY,
        f32::NEG_INFINITY,
        1.0e-40,
        1.5,
        -1.5,
        0.123_456_79,
    ];
    for (i, v) in odd.iter().enumerate() {
        data[400 + i] = *v;
    }
    for (i, enabled) in [false, true].into_iter().enumerate() {
        let b = Arc::new(CrcBoard::new(enabled));
        let src = buffer(data.clone(), 2);
        let want_len = src.size_hint();
        let tap = CrcTapSource::with_board(src, b, cfg());
        assert_eq!(tap.channels().get(), 2, "{i}");
        assert_eq!(tap.sample_rate().get(), RATE);
        assert_eq!(tap.size_hint(), want_len);
        let got: Vec<u32> = tap.map(f32::to_bits).collect();
        let want: Vec<u32> = data.iter().map(|x| x.to_bits()).collect();
        assert_eq!(got, want, "enabled={enabled}");
    }
}

#[test]
fn seek_during_the_window_is_not_applicable() {
    let b = board(WINDOW);
    let mut tap = CrcTapSource::with_board(buffer(to_f32(&codes(3000, 1)), 2), b.clone(), cfg());
    for _ in 0..(SKIP as usize + 50) * 2 {
        tap.next();
    }
    tap.try_seek(Duration::from_millis(1)).unwrap();
    assert_eq!(
        b.result(tap.seq()),
        Some(CrcResult::NotApplicable(NA_SEEKED))
    );
}

#[test]
fn describe_strings() {
    assert_eq!(CrcResult::Pending.describe(), "pending");
    assert_eq!(
        CrcResult::Match { frames: 48_000 }.describe(),
        "match (48000 frames)"
    );
    assert_eq!(
        CrcResult::Mismatch { frame: 1234 }.describe(),
        "mismatch at frame 1234"
    );
    assert_eq!(
        CrcResult::NotApplicable(NA_NOT_GRID24).describe(),
        "not applicable: source is not on a 24-bit grid"
    );
    assert_eq!(CrcResult::GateDropped.describe(), "gate dropped");
}

#[test]
fn default_window_is_one_second_of_48k() {
    assert_eq!(CrcBoard::new(true).window_frames(), 48_000);
}

// ---- through the real render loop ------------------------------------------------------------------------

struct VecSink {
    periods: usize,
    frames: usize,
}

impl RenderSink for VecSink {
    fn wait(&mut self) -> WaitResult {
        if self.periods == 0 {
            return WaitResult::Stop;
        }
        self.periods -= 1;
        WaitResult::Ready
    }
    fn frames_available(&self) -> usize {
        self.frames
    }
    fn write(&mut self, _bytes: &[u8], _frames: usize) -> Result<(), String> {
        Ok(())
    }
}

fn render_cfg(format: OutputFormat) -> RenderConfig {
    RenderConfig {
        channels: 2,
        rate_hz: RATE,
        format,
        source_bits: 24,
        dither: DitherMode::Auto,
        lead_in_ms: 0,
        period_ms: 10,
        max_frames: 480,
        gate_max_age_ms: 3_600_000,
        seed: 7,
    }
}

/// Through the real formatter, with the Player's headroom multiplied in f32 as rodio does.
fn through_render_loop(format: OutputFormat, gate_on: bool) -> Option<CrcResult> {
    let b = board(WINDOW);
    let tap = CrcTapSource::with_board(buffer(to_f32(&codes(3000, 1)), 2), b.clone(), cfg());
    let seq = tap.seq();
    let src = tap.map(|x| x * MASTER_HEADROOM);
    let gate = GateState::new();
    gate.publish(gate_on, SystemClock::new().now_ms());
    let mut rl =
        RenderLoop::new(render_cfg(format), src, SystemClock::new()).with_crc_board(b.clone());
    let mut sink = VecSink {
        periods: 20,
        frames: 200,
    };
    rl.run(&mut sink, &gate).unwrap();
    b.result(seq)
}

#[test]
fn render_loop_formatter_path_matches() {
    assert_eq!(
        through_render_loop(S24, true),
        Some(CrcResult::Match { frames: WINDOW })
    );
    assert_eq!(
        through_render_loop(OutputFormat::S32 { valid_bits: 24 }, true),
        Some(CrcResult::Match { frames: WINDOW })
    );
}

#[test]
fn render_loop_with_the_gate_off_does_not_claim_a_match() {
    assert_eq!(
        through_render_loop(S24, false),
        Some(CrcResult::GateDropped)
    );
}
