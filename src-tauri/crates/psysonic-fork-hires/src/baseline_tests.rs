//! What rodio's linear-interpolation converter and mixer do to audio today.

use std::num::{NonZeroU16, NonZeroU32};

use rodio::buffer::SamplesBuffer;
use rodio::mixer::mixer;
use rodio::source::UniformSourceIterator;

use crate::testkit::metrics::{analyze_tone, level_db};
use crate::testkit::signal::{first_mismatch, interleave, quantize, sine};
use crate::testkit::source::Spans;

const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

fn ch(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn hz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

fn stereo_24bit(rate: u32) -> Vec<f32> {
    let left = quantize(&sine(1000, rate, 0.25, 0.5), 24);
    let right = quantize(&sine(997, rate, 0.25, 0.5), 24);
    interleave(&[&left, &right])
}

#[test]
fn uniform_source_is_bit_exact_at_equal_rate() {
    for rate in RATES {
        let input = stereo_24bit(rate);
        let source = SamplesBuffer::new(ch(2), hz(rate), input.clone());
        let output: Vec<f32> = UniformSourceIterator::new(source, ch(2), hz(rate)).collect();
        assert_eq!(first_mismatch(&input, &output), None, "rate {rate}");
    }
}

#[test]
fn mixer_passes_one_source_through_bit_exact() {
    for rate in RATES {
        let input = stereo_24bit(rate);
        let (controller, source) = mixer(ch(2), hz(rate));
        controller.add(SamplesBuffer::new(ch(2), hz(rate), input.clone()));
        let output: Vec<f32> = source.take(input.len() + 4096).collect();
        let start = output
            .iter()
            .position(|&s| s != 0.0)
            .unwrap_or_else(|| panic!("rate {rate}: mixer emitted only silence"));
        assert_eq!(start % 2, 0, "rate {rate}: start {start} not frame-aligned");
        // The mixer sums into a +0.0 accumulator, so -0.0 comes out as +0.0; that is inaudible.
        let expected: Vec<f32> = input.iter().map(|s| s + 0.0).collect();
        assert_eq!(
            first_mismatch(&expected, &output[start..start + input.len()]),
            None,
            "rate {rate}"
        );
    }
}

fn rodio_one_span(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let source = SamplesBuffer::new(ch(1), hz(from), input.to_vec());
    UniformSourceIterator::new(source, ch(1), hz(to)).collect()
}

fn rodio_spans<const PACKET: usize>(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let source = Spans::new(input, from, PACKET);
    UniformSourceIterator::new(source, ch(1), hz(to)).collect()
}

fn ideal_linear_f64(input: &[f32], from: u32, to: u32) -> Vec<f64> {
    let len = (input.len() as f64 * to as f64 / from as f64).round() as usize;
    (0..len)
        .map(|i| {
            let pos = i as f64 * from as f64 / to as f64;
            let base = (pos.floor() as usize).min(input.len() - 1);
            let next = (base + 1).min(input.len() - 1);
            let frac = pos - base as f64;
            input[base] as f64 * (1.0 - frac) + input[next] as f64 * frac
        })
        .collect()
}

fn ideal_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    ideal_linear_f64(input, from, to)
        .into_iter()
        .map(|s| s as f32)
        .collect()
}

fn hq_spans_4096(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    crate::hq_resample_tests::hq_spans(input, from, to, 4096)
}

type Converter = fn(&[f32], u32, u32) -> Vec<f32>;

fn window(out: &[f32], to: u32) -> &[f32] {
    let start = to as usize / 2;
    &out[start..start + to as usize]
}

fn tone_out(convert: Converter, from: u32, to: u32, freq_hz: u32) -> Vec<f32> {
    convert(&sine(freq_hz, from, 2.0, 0.5), from, to)
}

#[test]
#[ignore = "baseline report: cargo test -p psysonic-fork-hires -- --ignored --nocapture"]
fn report_linear_resampler_quality() {
    const PAIRS: [(u32, u32); 10] = [
        (44_100, 48_000),
        (48_000, 44_100),
        (44_100, 96_000),
        (96_000, 44_100),
        (48_000, 96_000),
        (96_000, 48_000),
        (48_000, 192_000),
        (192_000, 48_000),
        (192_000, 44_100),
        (88_200, 176_400),
    ];
    const VARIANTS: [(&str, Converter); 6] = [
        ("rodio 1 span", rodio_one_span),
        ("rodio 4096 spans", rodio_spans::<4096>),
        ("rodio 1152 spans", rodio_spans::<1152>),
        ("ideal linear", ideal_linear),
        ("hq (rubato)", crate::hq_resample_tests::hq_one_span),
        ("hq 4096 spans", hq_spans_4096),
    ];
    eprintln!(
        "{:>15} | {:<16} | {:>9} | {:>9} | {:>9} | {:>9} | {:>8} | {:>13} | {:>7} | {:>9}",
        "from -> to",
        "converter",
        "1k resid",
        "1k spur",
        "HF resid",
        "HF spur",
        "HF gain",
        "stopband leak",
        "len err",
        "first dev"
    );
    for (from, to) in PAIRS {
        let reference = ideal_linear_f64(&sine(1000, from, 2.0, 0.5), from, to);
        let hf_freq = (0.42 * from.min(to) as f64) as u32;
        let stop_freq = (to / 2 + from / 2) / 2;
        for (name, convert) in VARIANTS {
            let low_out = tone_out(convert, from, to, 1000);
            let low = analyze_tone(window(&low_out, to), to, 1000, 0.5);
            let high_out = tone_out(convert, from, to, hf_freq);
            let high = analyze_tone(window(&high_out, to), to, hf_freq, 0.5);
            let leak = (to < from)
                .then(|| level_db(window(&tone_out(convert, from, to, stop_freq), to), 0.5));
            let len_err = low_out.len() as i64 - reference.len() as i64;
            let first_dev = low_out
                .iter()
                .zip(&reference)
                .position(|(&a, &b)| (a as f64 - b).abs() > 1e-4);
            if name == "ideal linear" {
                assert!(low.residual_db < -50.0, "{from}->{to}: {low:?}");
            }
            eprintln!(
                "{:>6} -> {:<6} | {:<16} | {:>6.1} dB | {:>6.1} dB | {:>6.1} dB | {:>6.1} dB | {:>5.1} dB | {:>13} | {:>7} | {:>9}",
                from,
                to,
                name,
                low.residual_db,
                low.worst_spur_db,
                high.residual_db,
                high.worst_spur_db,
                high.gain_db,
                leak.map_or("—".to_string(), |l| format!("{l:.1} dB")),
                len_err,
                first_dev.map_or("none".to_string(), |i| i.to_string()),
            );
        }
    }
}
