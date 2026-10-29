//! Quality, timing and behaviour of the HQ resampler stage, measured with the testkit.

use std::num::{NonZeroU16, NonZeroU32};
use std::time::{Duration, Instant};

use rodio::buffer::SamplesBuffer;
use rodio::mixer::mixer;
use rodio::mixer::Mixer;
use rodio::queue::queue;
use rodio::queue::SourcesQueueInput;
use rodio::{Player, Source};

use crate::hq_resample::{
    for_output, prime_queue, primed_for_output, priming_silence, HqResample, DEFAULT_CHUNK_FRAMES,
};
use crate::testkit::metrics::{analyze_tone, level_db};
use crate::testkit::signal::{first_mismatch, interleave, quantize, sine};
use crate::testkit::source::{Spans, Unbounded};

const UP: [(u32, u32); 9] = [
    (44_100, 48_000),
    (44_100, 88_200),
    (44_100, 96_000),
    (44_100, 176_400),
    (44_100, 192_000),
    (48_000, 96_000),
    (48_000, 192_000),
    (96_000, 192_000),
    (88_200, 176_400),
];
const DOWN: [(u32, u32); 5] = [
    (48_000, 44_100),
    (96_000, 48_000),
    (192_000, 48_000),
    (192_000, 44_100),
    (192_000, 96_000),
];

fn ch(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn hz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

fn expected_len(frames: usize, from: u32, to: u32) -> usize {
    ((frames as u64 * to as u64 + from as u64 / 2) / from as u64) as usize
}

/// Mono input as one span.
pub(crate) fn hq_one_span(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let src = SamplesBuffer::new(ch(1), hz(from), input.to_vec());
    HqResample::new(src, hz(to), DEFAULT_CHUNK_FRAMES).collect()
}

/// Mono input in decoder-like packets.
pub(crate) fn hq_spans(input: &[f32], from: u32, to: u32, packet: usize) -> Vec<f32> {
    HqResample::new(
        Spans::new(input, from, packet),
        hz(to),
        DEFAULT_CHUNK_FRAMES,
    )
    .collect()
}

fn window(out: &[f32], to: u32) -> &[f32] {
    let start = to as usize / 2;
    &out[start..start + to as usize]
}

fn hf_freq(from: u32, to: u32) -> u32 {
    (0.42 * from.min(to) as f64) as u32
}

#[test]
fn quality_targets_all_pairs() {
    let mut failures = Vec::new();
    for &(from, to) in UP.iter().chain(DOWN.iter()) {
        let low_out = hq_one_span(&sine(1000, from, 2.0, 0.5), from, to);
        let low = analyze_tone(window(&low_out, to), to, 1000, 0.5);
        let f = hf_freq(from, to);
        let high_out = hq_one_span(&sine(f, from, 2.0, 0.5), from, to);
        let high = analyze_tone(window(&high_out, to), to, f, 0.5);
        let leak = (to < from).then(|| {
            let stop = (to / 2 + from / 2) / 2;
            level_db(
                window(&hq_one_span(&sine(stop, from, 2.0, 0.5), from, to), to),
                0.5,
            )
        });
        eprintln!(
            "{from:>6} -> {to:<6} 1k {:>7.1} dB | HF {f} {:>7.1} dB gain {:>6.3} dB | leak {leak:?}",
            low.residual_db, high.residual_db, high.gain_db
        );
        if low.residual_db > -120.0 {
            failures.push(format!("{from}->{to}: 1k residual {:.1}", low.residual_db));
        }
        if high.residual_db > -120.0 {
            failures.push(format!("{from}->{to}: HF residual {:.1}", high.residual_db));
        }
        if high.gain_db.abs() > 0.1 {
            failures.push(format!("{from}->{to}: HF gain {:.3}", high.gain_db));
        }
        if let Some(l) = leak {
            if l > -120.0 {
                failures.push(format!("{from}->{to}: stopband leak {l:.1}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn length_is_exact() {
    for &(from, to) in UP.iter().chain(DOWN.iter()) {
        for secs in [2.0, 3.7] {
            let input = sine(1000, from, secs, 0.5);
            let out = hq_one_span(&input, from, to);
            assert_eq!(
                out.len(),
                expected_len(input.len(), from, to),
                "{from}->{to} {secs}s"
            );
        }
    }
}

#[test]
fn short_and_empty_inputs() {
    for frames in [0usize, 1, 2, 5, 100, 1023, 1024, 1025] {
        let input = vec![0.25f32; frames];
        let out = hq_one_span(&input, 44_100, 192_000);
        assert_eq!(
            out.len(),
            expected_len(frames, 44_100, 192_000),
            "{frames} frames"
        );
        let out = hq_one_span(&input, 192_000, 44_100);
        assert_eq!(
            out.len(),
            expected_len(frames, 192_000, 44_100),
            "{frames} frames down"
        );
    }
}

#[test]
fn output_is_independent_of_span_boundaries() {
    for &(from, to) in &[
        (44_100, 192_000),
        (44_100, 48_000),
        (96_000, 192_000),
        (192_000, 44_100),
    ] {
        let input = sine(1000, from, 1.5, 0.5);
        let whole = hq_one_span(&input, from, to);
        for packet in [4096, 1152] {
            let spans = hq_spans(&input, from, to, packet);
            assert_eq!(
                first_mismatch(&whole, &spans),
                None,
                "{from}->{to} packet {packet}"
            );
        }
    }
}

#[test]
fn passthrough_is_bit_exact() {
    for rate in [44_100u32, 96_000, 192_000] {
        let left = quantize(&sine(1000, rate, 0.25, 0.5), 24);
        let right = quantize(&sine(997, rate, 0.25, 0.5), 24);
        let input = interleave(&[&left, &right]);
        let src = SamplesBuffer::new(ch(2), hz(rate), input.clone());
        let out: Vec<f32> = HqResample::new(src, hz(rate), DEFAULT_CHUNK_FRAMES).collect();
        assert_eq!(first_mismatch(&input, &out), None, "rate {rate}");
        let mono = quantize(&sine(1000, rate, 0.25, 0.5), 24);
        let out = hq_spans(&mono, rate, rate, 1152);
        assert_eq!(first_mismatch(&mono, &out), None, "spans rate {rate}");
    }
}

#[test]
fn source_metadata() {
    let src = SamplesBuffer::new(ch(2), hz(44_100), vec![0.0; 200]);
    let hq = HqResample::new(src, hz(192_000), DEFAULT_CHUNK_FRAMES);
    assert_eq!(hq.sample_rate().get(), 192_000);
    assert_eq!(hq.channels().get(), 2);
    assert_eq!(hq.current_span_len(), None);
    assert_eq!(hq.total_duration(), hq.total_duration());
}

fn stereo(rate: u32, secs: f64) -> Vec<f32> {
    let l = quantize(&sine(1000, rate, secs, 0.5), 24);
    let r = quantize(&sine(997, rate, secs, 0.5), 24);
    interleave(&[&l, &r])
}

fn hq_stereo(rate: u32, target: u32, secs: f64) -> Vec<f32> {
    let src = SamplesBuffer::new(ch(2), hz(rate), stereo(rate, secs));
    HqResample::new(src, hz(target), DEFAULT_CHUNK_FRAMES).collect()
}

#[test]
fn queue_and_mixer_chain_is_gapless_and_bit_exact() {
    const TARGET: u32 = 192_000;
    let a = hq_stereo(44_100, TARGET, 1.0);
    let b = hq_stereo(96_000, TARGET, 1.0);
    assert_eq!(a.len(), 2 * expected_len(44_100, 44_100, TARGET));
    assert_eq!(b.len(), 2 * expected_len(96_000, 96_000, TARGET));
    let (controller, output) = mixer(ch(2), hz(TARGET));
    let (input, queue_out) = queue(true);
    input.append(HqResample::new(
        SamplesBuffer::new(ch(2), hz(44_100), stereo(44_100, 1.0)),
        hz(TARGET),
        DEFAULT_CHUNK_FRAMES,
    ));
    input.append(HqResample::new(
        SamplesBuffer::new(ch(2), hz(96_000), stereo(96_000, 1.0)),
        hz(TARGET),
        DEFAULT_CHUNK_FRAMES,
    ));
    // The queue reports its next source's format only once something is queued, so it joins the mixer after the appends.
    controller.add(queue_out);
    let total = a.len() + b.len();
    let rendered: Vec<f32> = output.take(total + 20_000).collect();
    let start = rendered
        .iter()
        .position(|&s| s != 0.0)
        .expect("mixer emitted only silence");
    assert_eq!(start % 2, 0, "start {start} not frame-aligned");
    // The mixer sums into a +0.0 accumulator, so -0.0 comes out as +0.0.
    let expected: Vec<f32> = a.iter().chain(&b).map(|s| s + 0.0).collect();
    assert_eq!(
        first_mismatch(&expected, &rendered[start..start + total]),
        None,
        "boundary at sample {}",
        a.len()
    );
    assert!(
        rendered[start + total..].iter().all(|&s| s == 0.0),
        "audio continues past the exact total length"
    );
}

#[test]
fn seek_matches_unseeked_after_settle_region() {
    // The filter spans at most about two FFT blocks; the largest block is 4480 output frames (44.1 -> 192 kHz)
    // and the zero history of a fresh start only disturbs output inside that support.
    // Settle region: 2 * ceil(chunk * to / from) + chunk output frames, comfortably above it.
    for &(from, to) in &[
        (44_100u32, 192_000u32),
        (44_100, 48_000),
        (48_000, 44_100),
        (96_000, 192_000),
    ] {
        let input = sine(1000, from, 3.0, 0.5);
        let full = hq_one_span(&input, from, to);
        let mut seeked = HqResample::new(
            SamplesBuffer::new(ch(1), hz(from), input.clone()),
            hz(to),
            DEFAULT_CHUNK_FRAMES,
        );
        // Consume some output first so state is dirty before the seek.
        for _ in 0..5000 {
            seeked.next();
        }
        seeked.try_seek(Duration::from_secs(1)).unwrap();
        let tail: Vec<f32> = seeked.collect();
        let remaining = input.len() - from as usize;
        assert_eq!(
            tail.len(),
            expected_len(remaining, from, to),
            "{from}->{to} length after seek"
        );
        let offset = full.len() - tail.len();
        assert_eq!(offset, to as usize, "{from}->{to} alignment");
        let settle =
            2 * (DEFAULT_CHUNK_FRAMES * to as usize).div_ceil(from as usize) + DEFAULT_CHUNK_FRAMES;
        let max_diff = tail[settle..]
            .iter()
            .zip(&full[offset + settle..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        eprintln!("{from}->{to}: seek settle {settle} frames, max diff after {max_diff:e}");
        assert!(
            max_diff < 1e-5,
            "{from}->{to}: diff {max_diff:e} after settle {settle}"
        );
    }
}

/// Mono source whose reported rate switches after `switch_at` samples.
struct Switching {
    samples: Vec<f32>,
    pos: usize,
    switch_at: usize,
    first: u32,
    second: u32,
}

impl Iterator for Switching {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let s = self.samples.get(self.pos).copied();
        self.pos += 1;
        s
    }
}

impl Source for Switching {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::MIN
    }
    fn sample_rate(&self) -> NonZeroU32 {
        hz(if self.pos < self.switch_at {
            self.first
        } else {
            self.second
        })
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[test]
fn rate_change_mid_stream() {
    let to = 96_000u32;
    for (first, second) in [
        (44_100u32, 48_000u32),
        (48_000, 96_000),
        (96_000, 44_100),
        (44_100, 192_000),
    ] {
        // Switch at a multiple of the chunk so the segment sizes are known exactly.
        let switch_at = 20 * DEFAULT_CHUNK_FRAMES;
        let total = 60_000;
        let src = Switching {
            samples: sine(1000, first, 2.0, 0.5)[..total].to_vec(),
            pos: 0,
            switch_at,
            first,
            second,
        };
        let hq = HqResample::new(src, hz(to), DEFAULT_CHUNK_FRAMES);
        assert_eq!(hq.sample_rate().get(), to);
        let out: Vec<f32> = hq.collect();
        let expected =
            expected_len(switch_at, first, to) + expected_len(total - switch_at, second, to);
        let diff = out.len() as i64 - expected as i64;
        assert!(
            diff.abs() <= 1,
            "{first}->{second}: len {} vs {expected}",
            out.len()
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }
}

#[test]
fn six_channels() {
    let (from, to) = (44_100u32, 96_000u32);
    let freqs = [440u32, 1000, 2000, 3000, 5000, 7000];
    let chans: Vec<Vec<f32>> = freqs.iter().map(|&f| sine(f, from, 2.0, 0.5)).collect();
    let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
    let src = SamplesBuffer::new(ch(6), hz(from), interleave(&refs));
    let out: Vec<f32> = HqResample::new(src, hz(to), DEFAULT_CHUNK_FRAMES).collect();
    assert_eq!(out.len(), 6 * expected_len(2 * from as usize, from, to));
    for (c, &f) in freqs.iter().enumerate() {
        let mono: Vec<f32> = out.iter().skip(c).step_by(6).copied().collect();
        let report = analyze_tone(window(&mono, to), to, f, 0.5);
        assert!(report.residual_db < -120.0, "channel {c}: {report:?}");
    }
}

#[test]
fn eight_channels_stay_in_their_lane() {
    let (from, to) = (48_000u32, 192_000u32);
    let chans: Vec<Vec<f32>> = (0..8)
        .map(|c| {
            if c == 5 {
                sine(1000, from, 1.0, 0.5)
            } else {
                vec![0.0; from as usize]
            }
        })
        .collect();
    let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
    let src = SamplesBuffer::new(ch(8), hz(from), interleave(&refs));
    let out: Vec<f32> = HqResample::new(src, hz(to), DEFAULT_CHUNK_FRAMES).collect();
    assert_eq!(out.len(), 8 * to as usize);
    for c in (0..8).filter(|&c| c != 5) {
        assert!(
            out.iter().skip(c).step_by(8).all(|&s| s == 0.0),
            "channel {c} leaked"
        );
    }
}

#[test]
#[ignore = "perf: cargo test --release -p psysonic-fork-hires -- --ignored --nocapture hq_perf"]
fn hq_perf_realtime_factor() {
    for (from, to) in [(44_100u32, 192_000u32), (48_000, 192_000)] {
        let l = sine(1000, from, 30.0, 0.5);
        let r = sine(997, from, 30.0, 0.5);
        let input = interleave(&[&l, &r]);
        let src = SamplesBuffer::new(ch(2), hz(from), input);
        let start = Instant::now();
        let count = HqResample::new(src, hz(to), DEFAULT_CHUNK_FRAMES).count();
        let elapsed = start.elapsed().as_secs_f64();
        let factor = 30.0 / elapsed;
        eprintln!(
            "{from} -> {to} stereo: {count} samples in {elapsed:.3} s = {factor:.0}x realtime"
        );
        assert!(factor >= 50.0, "{from}->{to}: only {factor:.1}x");
    }
}

// ---- Player-first queueing (the engine's order) ----

const OUT: u32 = 192_000;

/// A test track: channel count, input rate, content.
#[derive(Clone)]
struct Track {
    channels: u16,
    rate: u32,
    data: Vec<f32>,
}

fn track(channels: u16, rate: u32, secs: f64, freq: u32) -> Track {
    let chans: Vec<Vec<f32>> = (0..channels)
        .map(|c| quantize(&sine(freq + 3 * c as u32, rate, secs, 0.5), 24))
        .collect();
    let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
    Track {
        channels,
        rate,
        data: interleave(&refs),
    }
}

fn queued(t: &Track, wrapped: bool) -> Box<dyn Source + Send> {
    let src = SamplesBuffer::new(ch(t.channels), hz(t.rate), t.data.clone());
    if wrapped {
        Box::new(for_output(src, ch(2), hz(OUT)))
    } else {
        Box::new(HqResample::new(src, hz(OUT), DEFAULT_CHUNK_FRAMES))
    }
}

/// The track rendered alone at the stream format (+0.0 normalised, as the mixer sums into +0.0).
fn alone(t: &Track) -> Vec<f32> {
    let src = SamplesBuffer::new(ch(t.channels), hz(t.rate), t.data.clone());
    for_output(src, ch(2), hz(OUT)).map(|s| s + 0.0).collect()
}

enum Sink {
    Player(Player),
    Queue(std::sync::Arc<SourcesQueueInput>),
}

impl Sink {
    /// `Player::connect_new` / `queue(true)` joins the mixer first, before any track is appended.
    fn connect(controller: &Mixer, use_player: bool) -> Self {
        if use_player {
            Sink::Player(Player::connect_new(controller))
        } else {
            let (input, output) = queue(true);
            controller.add(output);
            Sink::Queue(input)
        }
    }

    fn append(&self, source: Box<dyn Source + Send>) {
        match self {
            Sink::Player(p) => p.append(source),
            Sink::Queue(q) => q.append(source),
        }
    }

    fn prime(&self) {
        match self {
            Sink::Player(p) => p.append(priming_silence(ch(2), hz(OUT))),
            Sink::Queue(q) => prime_queue(q, ch(2), hz(OUT)),
        }
    }
}

fn first_nonzero(samples: &[f32], from: usize) -> usize {
    from + samples[from..]
        .iter()
        .position(|&s| s != 0.0)
        .expect("only silence")
}

/// Appends `early` before playback starts, pulls `lead` samples, appends `late`, and renders up to `total`
/// samples. Returns everything rendered.
fn render_player_first(
    use_player: bool,
    prime: bool,
    wrapped: bool,
    early: &[Track],
    lead: usize,
    late: &[Track],
    total: usize,
) -> Vec<f32> {
    let (controller, mut output) = mixer(ch(2), hz(OUT));
    let sink = Sink::connect(&controller, use_player);
    if prime {
        sink.prime();
    }
    for t in early {
        sink.append(queued(t, wrapped));
    }
    let mut got: Vec<f32> = output.by_ref().take(lead).collect();
    for t in late {
        sink.append(queued(t, wrapped));
    }
    got.extend(output.by_ref().take(total - lead));
    got
}

const EXTRA: usize = 40_000;

const CASES: [((u16, u32), (u16, u32)); 3] = [
    ((2, 44_100), (1, 96_000)),
    ((1, 44_100), (2, 96_000)),
    ((2, 44_100), (2, 96_000)),
];

/// Both tracks queued before playback: the join between them is sample-exact and channel counts are honoured.
#[test]
fn player_first_join_is_exact_with_for_output() {
    for use_player in [true, false] {
        for ((c1, r1), (c2, r2)) in CASES {
            let a = track(c1, r1, 1.0, 1000);
            let b = track(c2, r2, 1.0, 1500);
            let (ea, eb) = (alone(&a), alone(&b));
            let total = ea.len() + eb.len() + EXTRA;
            let got = render_player_first(use_player, true, true, &[a, b], 0, &[], total);
            let start = first_nonzero(&got, 0);
            assert_eq!(start % 2, 0);
            let expected: Vec<f32> = ea.iter().chain(&eb).copied().collect();
            assert_eq!(
                first_mismatch(&expected, &got[start..start + expected.len()]),
                None,
                "player={use_player} {c1}ch->{c2}ch join"
            );
            assert!(got[start + expected.len()..].iter().all(|&s| s == 0.0));
        }
    }
}

/// The second track is appended mid-playback of the first, and again after the first ended (idle silence).
#[test]
fn player_first_late_append_is_exact_with_for_output() {
    for use_player in [true, false] {
        for ((c1, r1), (c2, r2)) in CASES {
            let a = track(c1, r1, 1.0, 1000);
            let b = track(c2, r2, 2.0, 1500);
            let (ea, eb) = (alone(&a), alone(&b));
            // Mid-playback of a: gapless join.
            let total = ea.len() + eb.len() + EXTRA;
            let got = render_player_first(
                use_player,
                true,
                true,
                std::slice::from_ref(&a),
                50_000,
                std::slice::from_ref(&b),
                total,
            );
            let start = first_nonzero(&got, 0);
            let expected: Vec<f32> = ea.iter().chain(&eb).copied().collect();
            assert_eq!(
                first_mismatch(&expected, &got[start..start + expected.len()]),
                None,
                "player={use_player} mid-playback {c1}ch->{c2}ch"
            );
            // After idle: b appended once a is long over.
            let lead = ea.len() + 100_000;
            let total = lead + eb.len() + EXTRA;
            let got = render_player_first(use_player, true, true, &[a], lead, &[b], total);
            let start = first_nonzero(&got, 0);
            assert_eq!(
                first_mismatch(&ea, &got[start..start + ea.len()]),
                None,
                "idle: first track"
            );
            let start_b = first_nonzero(&got, start + ea.len());
            assert_eq!(start_b % 2, 0);
            let rendered_b = &got[start_b..start_b + eb.len()];
            assert_eq!(
                first_mismatch(&eb, rendered_b),
                None,
                "player={use_player} after idle {c1}ch->{c2}ch"
            );
            if c2 == 1 {
                // Up-mixed: L == R, at the right speed (length exact, tone at its frequency).
                assert_eq!(eb.len(), 2 * expected_len(2 * r2 as usize, r2, OUT));
                let left: Vec<f32> = rendered_b.iter().step_by(2).copied().collect();
                let right: Vec<f32> = rendered_b[1..].iter().step_by(2).copied().collect();
                assert_eq!(first_mismatch(&left, &right), None, "mono must be up-mixed");
                let report = analyze_tone(window(&left, OUT), OUT, 1500, 0.5);
                assert!(report.residual_db < -100.0, "{report:?}");
                assert!(report.gain_db.abs() < 0.1, "{report:?}");
            }
        }
    }
}

/// Without `for_output`, mono after stereo is read with the stale stereo count (pins the rodio 0.22.2 defect).
#[test]
fn player_first_without_for_output_misreads_channel_change() {
    let a = track(2, 44_100, 1.0, 1000);
    let b = track(1, 96_000, 1.0, 1500);
    let (ea, eb) = (alone(&a), alone(&b));
    let total = ea.len() + eb.len() + EXTRA;
    let got = render_player_first(true, true, false, &[a, b], 0, &[], total);
    let start = first_nonzero(&got, 0);
    let expected: Vec<f32> = ea.iter().chain(&eb).copied().collect();
    assert!(
        first_mismatch(&expected, &got[start..start + expected.len()]).is_some(),
        "if this passes, rodio fixed the stale channel count and for_output may be unnecessary"
    );
}

/// Renders one stereo track first in a freshly connected queue and returns `(shift, dropped_at_start, bad)`:
/// where the stream resumes being the track (output index minus track index), and how many samples around the
/// start are wrong. The stream is located by matching a clean middle window; everything from the resume point
/// on must then be bit-exact.
fn first_track_damage(use_player: bool, prime: bool) -> (i64, usize) {
    let a = track(2, 44_100, 1.0, 1000);
    let ea = alone(&a);
    let got = render_player_first(use_player, prime, true, &[a], 0, &[], ea.len() + 20_000);
    let shift = (0..16_384usize)
        .step_by(2)
        .find(|&s| got[s + 20_000..s + 24_000] == ea[20_000..24_000])
        .expect("track not found in the output") as i64;
    // Wrong samples on the track's own timeline, counted over the whole track.
    let bad = (0..ea.len())
        .filter(|&i| got[(i as i64 + shift) as usize].to_bits() != ea[i].to_bits())
        .count();
    (shift, bad)
}

/// Mechanism: `mixer.add(queue)` bootstraps its `UniformSourceIterator` while the queue's current source is the
/// initial mono 48 kHz `Empty`, with a 512 sample span (`threshold`). Whatever is queued first has its first 512
/// samples read as mono 48 kHz (then rate-converted x4 and up-mixed, so 512 samples become 4096 wrong ones).
/// Priming with 512 samples of silence at the stream format absorbs exactly that.
#[test]
fn first_track_after_connect_is_damaged_without_priming_and_exact_with_it() {
    for use_player in [true, false] {
        let (shift, bad) = first_track_damage(use_player, false);
        eprintln!(
            "player={use_player} unprimed: stream resumes at shift {shift}, {bad} wrong samples"
        );
        // The defect is confined to the first 512 queue samples (everything after is bit-exact).
        assert!(bad > 0 && bad <= 1024, "unprimed: {bad} wrong samples");
        let (shift, bad) = first_track_damage(use_player, true);
        assert_eq!(
            bad, 0,
            "player={use_player}: primed track must be bit-exact from its first sample"
        );
        assert_eq!(shift % 2, 0);
    }
}

// ---- PrimedSource: seek safety and the connect-to-append gap ----

/// How the first track is put into a new `Player`.
#[derive(Clone, Copy, Debug)]
enum Priming {
    /// A separate `priming_silence` source, then the track (the hazardous layout).
    Separate,
    /// One `PrimedSource` (`primed_for_output`).
    Part,
}

fn append_first(player: &Player, t: &Track, priming: Priming) {
    let src = SamplesBuffer::new(ch(t.channels), hz(t.rate), t.data.clone());
    match priming {
        Priming::Separate => {
            player.append(priming_silence(ch(2), hz(OUT)));
            player.append(for_output(src, ch(2), hz(OUT)));
        }
        Priming::Part => player.append(primed_for_output(src, ch(2), hz(OUT))),
    }
}

/// About 1 s in, at a quarter period of the 1 kHz test tone: seeking to a whole second would land on the same
/// phase as the start of the track and could not tell a swallowed seek from a honoured one.
const SEEK_POS: Duration = Duration::from_micros(1_000_250);

/// The track at the stream format, seeked to `pos` first (what a seek must produce).
fn alone_from(t: &Track, pos: Duration) -> Vec<f32> {
    let src = SamplesBuffer::new(ch(t.channels), hz(t.rate), t.data.clone());
    let mut s = for_output(src, ch(2), hz(OUT));
    s.try_seek(pos).expect("SamplesBuffer is seekable");
    s.map(|x| x + 0.0).collect()
}

/// Paused Player, first track appended, seek issued, then `play()`. Returns `(start, mismatch, len)`: where the
/// first non-silent output sample is, the first sample that differs from the expected seeked track (if any), and
/// the expected length.
fn paused_seek(priming: Priming, pos: Duration) -> (usize, Option<usize>, usize) {
    let t = track(2, 44_100, 2.0, 1000);
    let expected = alone_from(&t, pos);
    let (controller, mut output) = mixer(ch(2), hz(OUT));
    let player = Player::connect_new(&controller);
    player.pause();
    append_first(&player, &t, priming);
    // `Player::try_seek` blocks until a pulled source applies it, so issue it from another thread (as the app
    // does) and let the paused mixer pull silence until it is applied (rodio checks every 5 ms of queue samples). The track is not consumed meanwhile.
    let player = std::sync::Arc::new(player);
    let seeker = {
        let player = player.clone();
        std::thread::spawn(move || player.try_seek(pos).expect("seek on a paused player"))
    };
    std::thread::sleep(Duration::from_millis(100));
    let paused: Vec<f32> = output.by_ref().take(60_000).collect();
    assert!(paused.iter().all(|&s| s == 0.0));
    seeker.join().expect("seek thread");
    player.play();
    let got: Vec<f32> = output.by_ref().take(expected.len() + 20_000).collect();
    let start = first_nonzero(&got, 0);
    let end = (start + expected.len()).min(got.len());
    (
        start,
        first_mismatch(&expected, &got[start..end]),
        expected.len(),
    )
}

/// The hazard: with a separate priming source, a seek on a paused Player is consumed by the silence and the
/// track starts from 0, not from the seek position.
#[test]
fn separate_priming_swallows_a_seek_on_a_paused_player() {
    let (start, mismatch, len) = paused_seek(Priming::Separate, SEEK_POS);
    eprintln!("separate priming: first sound at {start}, first mismatch {mismatch:?} of {len}");
    assert!(
        mismatch.is_some(),
        "if this passes, rodio no longer lets priming swallow the seek and PrimedSource is optional"
    );
}

#[test]
fn primed_source_honours_a_seek_on_a_paused_player() {
    for pos in [SEEK_POS, Duration::ZERO] {
        let (start, mismatch, _) = paused_seek(Priming::Part, pos);
        assert_eq!(start % 2, 0);
        assert_eq!(mismatch, None, "seek to {pos:?}");
    }
}

/// Locates the track in `got` by a clean middle window and counts wrong samples over the whole track.
/// Returns `(shift, wrong)`, shift being the output index minus the track index.
fn locate_and_count(got: &[f32], ea: &[f32], max_shift: usize) -> (i64, usize) {
    let shift = (0..max_shift)
        .step_by(2)
        .find(|&s| got[s + 20_000..s + 24_000] == ea[20_000..24_000])
        .expect("track not found in the output") as i64;
    let wrong = (0..ea.len())
        .filter(|&i| got[(i as i64 + shift) as usize].to_bits() != ea[i].to_bits())
        .count();
    (shift, wrong)
}

/// R12 case: connect, append at once (before the mixer pulls anything); the first track is bit-exact.
#[test]
fn primed_source_first_track_is_exact_on_immediate_append() {
    let (shift, wrong) = connect_gap(0, Priming::Part);
    assert_eq!(shift % 2, 0);
    assert_eq!(
        wrong, 0,
        "first track must be bit-exact from its first sample"
    );
}

/// Connect, let the mixer pull `pre_pull_samples` (the engine takes 100 ms or more between connect and append),
/// then append the first track. Returns `(shift, wrong)` for the track.
fn connect_gap(pre_pull_samples: usize, priming: Priming) -> (i64, usize) {
    let a = track(2, 44_100, 1.0, 1000);
    let ea = alone(&a);
    let (controller, mut output) = mixer(ch(2), hz(OUT));
    let player = Player::connect_new(&controller);
    let mut got: Vec<f32> = output.by_ref().take(pre_pull_samples).collect();
    append_first(&player, &a, priming);
    got.extend(output.by_ref().take(ea.len() + 40_000));
    assert!(got[..pre_pull_samples].iter().all(|&s| s == 0.0));
    locate_and_count(&got, &ea, pre_pull_samples + 16_384)
}

#[test]
fn connect_to_append_gap_result() {
    let pre = 2 * OUT as usize / 5; // 200 ms of stereo samples
    for priming in [Priming::Part, Priming::Separate] {
        let (shift, wrong) = connect_gap(pre, priming);
        eprintln!("gap 200 ms {priming:?}: shift {shift} (pre-pull {pre}), {wrong} wrong samples");
        assert_eq!(wrong, 0, "{priming:?}");
    }
}

/// A gapless second track via plain `for_output` after a `PrimedSource` first track is sample-exact.
#[test]
fn gapless_second_track_after_primed_first_is_exact() {
    for ((c1, r1), (c2, r2)) in CASES {
        let a = track(c1, r1, 1.0, 1000);
        let b = track(c2, r2, 1.0, 1500);
        let (ea, eb) = (alone(&a), alone(&b));
        let (controller, mut output) = mixer(ch(2), hz(OUT));
        let player = Player::connect_new(&controller);
        append_first(&player, &a, Priming::Part);
        let src_b = SamplesBuffer::new(ch(c2), hz(r2), b.data.clone());
        player.append(for_output(src_b, ch(2), hz(OUT)));
        let got: Vec<f32> = output.by_ref().take(ea.len() + eb.len() + EXTRA).collect();
        let start = first_nonzero(&got, 0);
        let expected: Vec<f32> = ea.iter().chain(&eb).copied().collect();
        assert_eq!(
            first_mismatch(&expected, &got[start..start + expected.len()]),
            None,
            "{c1}ch->{c2}ch"
        );
        assert!(got[start + expected.len()..].iter().all(|&s| s == 0.0));
    }
}

// ---- Streaming-style source: unbounded spans ----

#[test]
fn unbounded_spans_give_the_same_output_as_spans() {
    for &(from, to) in &[
        (44_100, 192_000),
        (44_100, 48_000),
        (192_000, 44_100),
        (96_000, 96_000),
    ] {
        let input = sine(1000, from, 1.5, 0.5);
        let spans = hq_spans(&input, from, to, 1152);
        let unbounded: Vec<f32> =
            HqResample::new(Unbounded::new(&input, from), hz(to), DEFAULT_CHUNK_FRAMES).collect();
        assert_eq!(first_mismatch(&spans, &unbounded), None, "{from}->{to}");
        assert_eq!(unbounded.len(), expected_len(input.len(), from, to));
        let wrapped: Vec<f32> = for_output(Unbounded::new(&input, from), ch(1), hz(to)).collect();
        assert_eq!(
            first_mismatch(&spans, &wrapped),
            None,
            "{from}->{to} for_output"
        );
        assert_eq!(wrapped.len(), unbounded.len());
    }
    // Stereo through for_output, against a bounded-span source.
    let l = sine(1000, 44_100, 0.5, 0.5);
    let r = sine(1300, 44_100, 0.5, 0.5);
    let input = interleave(&[&l, &r]);
    let a: Vec<f32> =
        for_output(Unbounded::with_channels(&input, 44_100, 2), ch(2), hz(OUT)).collect();
    let b: Vec<f32> = for_output(
        SamplesBuffer::new(ch(2), hz(44_100), input.clone()),
        ch(2),
        hz(OUT),
    )
    .collect();
    assert_eq!(first_mismatch(&a, &b), None);
    assert_eq!(a.len(), 2 * expected_len(l.len(), 44_100, OUT));
}
