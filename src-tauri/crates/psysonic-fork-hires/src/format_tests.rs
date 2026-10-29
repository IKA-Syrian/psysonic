use crate::format::*;
use crate::testkit::signal::{quantize, sine};

fn write(f: &mut Formatter, input: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; input.len() * f.format().bytes_per_sample()];
    let n = f.write_le(input, &mut out);
    assert_eq!(n, out.len());
    out
}

fn i32s(bytes: &[u8]) -> Vec<i32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| i32::from_le_bytes(*b))
        .collect()
}

fn i16s(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

fn i24s(bytes: &[u8]) -> Vec<i32> {
    bytes
        .as_chunks::<3>()
        .0
        .iter()
        .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
        .collect()
}

/// The integer codes a `bits`-quantized f32 stands for.
fn codes(samples: &[f32], bits: u32) -> Vec<i64> {
    let scale = (1u64 << (bits - 1)) as f64;
    samples.iter().map(|&s| (s as f64 * scale) as i64).collect()
}

fn tone(bits: u32) -> Vec<f32> {
    quantize(&sine(1000, 48_000, 0.1, 0.7), bits)
}

#[test]
fn format_widths_and_bits() {
    assert_eq!(OutputFormat::F32.bytes_per_sample(), 4);
    assert_eq!(OutputFormat::S16.bytes_per_sample(), 2);
    assert_eq!(OutputFormat::S24Packed.bytes_per_sample(), 3);
    assert_eq!(OutputFormat::S32 { valid_bits: 24 }.bytes_per_sample(), 4);
    assert_eq!(OutputFormat::S16.valid_bits(), 16);
    assert_eq!(OutputFormat::S24Packed.valid_bits(), 24);
    assert_eq!(OutputFormat::S32 { valid_bits: 24 }.valid_bits(), 24);
    assert_eq!(OutputFormat::F32.valid_bits(), 32);
}

#[test]
fn roundtrip_24bit_through_s32_24_and_s24_packed() {
    let x = tone(24);
    let want = codes(&x, 24);
    let mut f = Formatter::new(OutputFormat::S32 { valid_bits: 24 }, 1);
    let got = i32s(&write(&mut f, &x));
    for (i, (&g, &w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, (w as i32) << 8, "S32/24 sample {i}: upper-bit placement");
        assert_eq!(g & 0xFF, 0, "S32/24 sample {i}: low byte must be zero");
    }
    let mut f = Formatter::new(OutputFormat::S24Packed, 1);
    let got = i24s(&write(&mut f, &x));
    assert_eq!(got.iter().map(|&v| v as i64).collect::<Vec<_>>(), want);
}

#[test]
fn roundtrip_16bit_through_s16_is_exact_without_dither() {
    let x = tone(16);
    let mut f = Formatter::new(OutputFormat::S16, 1);
    let got = i16s(&write(&mut f, &x));
    let want = codes(&x, 16);
    assert_eq!(got.iter().map(|&v| v as i64).collect::<Vec<_>>(), want);
}

#[test]
fn roundtrip_through_s32_32_maps_to_shifted_codes() {
    let mut f = Formatter::new(OutputFormat::S32 { valid_bits: 32 }, 1);
    let x24 = tone(24);
    let got = i32s(&write(&mut f, &x24));
    let want = codes(&x24, 24);
    assert!(
        got.iter().zip(&want).all(|(&g, &w)| g as i64 == w << 8),
        "24-bit as k<<8"
    );
    let x16 = tone(16);
    let got = i32s(&write(&mut f, &x16));
    let want = codes(&x16, 16);
    assert!(
        got.iter().zip(&want).all(|(&g, &w)| g as i64 == w << 16),
        "16-bit as k<<16"
    );
}

#[test]
fn roundtrip_16bit_through_s32_24_and_s24_packed() {
    let x = tone(16);
    let want = codes(&x, 16);
    let mut f = Formatter::new(OutputFormat::S32 { valid_bits: 24 }, 1);
    let got = i32s(&write(&mut f, &x));
    assert!(got.iter().zip(&want).all(|(&g, &w)| g as i64 == w << 16));
    let mut f = Formatter::new(OutputFormat::S24Packed, 1);
    let got = i24s(&write(&mut f, &x));
    assert!(got.iter().zip(&want).all(|(&g, &w)| g as i64 == w << 8));
}

#[test]
fn f32_is_bit_identical_below_threshold() {
    let x = tone(24);
    let mut f = Formatter::new(OutputFormat::F32, 1);
    f.set_dither(true);
    let out = write(&mut f, &x);
    for (i, (b, s)) in out.as_chunks::<4>().0.iter().zip(&x).enumerate() {
        assert_eq!(*b, s.to_le_bytes(), "sample {i}");
    }
    assert_eq!(f.limited_samples(), 0);
}

#[test]
fn extremes_map_to_min_and_max_code_with_limiter_off() {
    let input = [1.0f32, -1.0, 1.5, -1.5, 100.0, -100.0, 0.0];
    for (format, bits) in [
        (OutputFormat::S16, 16u32),
        (OutputFormat::S24Packed, 24),
        (OutputFormat::S32 { valid_bits: 24 }, 24),
        (OutputFormat::S32 { valid_bits: 32 }, 32),
    ] {
        let mut f = Formatter::new(format, 1);
        f.set_limiter(false);
        let out = write(&mut f, &input);
        let max = (1i64 << (bits - 1)) - 1;
        let min = -(1i64 << (bits - 1));
        let got: Vec<i64> = match format {
            OutputFormat::S16 => i16s(&out).iter().map(|&v| v as i64).collect(),
            OutputFormat::S24Packed => i24s(&out).iter().map(|&v| v as i64).collect(),
            OutputFormat::S32 { valid_bits } => i32s(&out)
                .iter()
                .map(|&v| (v >> (32 - valid_bits as u32)) as i64)
                .collect(),
            OutputFormat::F32 => unreachable!(),
        };
        assert_eq!(got, [max, min, max, min, max, min, 0], "{format:?}");
    }
}

#[test]
fn extremes_stay_within_full_scale_with_limiter_on() {
    for format in [
        OutputFormat::S16,
        OutputFormat::S24Packed,
        OutputFormat::S32 { valid_bits: 24 },
        OutputFormat::S32 { valid_bits: 32 },
    ] {
        let mut f = Formatter::new(format, 1);
        let out = write(
            &mut f,
            &[1.0, -1.0, 2.0, -2.0, 1e9, f32::INFINITY, f32::NEG_INFINITY],
        );
        let got: Vec<i64> = match format {
            OutputFormat::S16 => i16s(&out).iter().map(|&v| v as i64).collect(),
            OutputFormat::S24Packed => i24s(&out).iter().map(|&v| v as i64).collect(),
            _ => i32s(&out).iter().map(|&v| v as i64).collect(),
        };
        let bits = format.valid_bits() as u32;
        let (min, max) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
        let shift = if let OutputFormat::S32 { .. } = format {
            32 - bits
        } else {
            0
        };
        for g in got {
            let code = g >> shift;
            assert!((min..=max).contains(&code), "{format:?}: {g}");
        }
        assert_eq!(f.limited_samples(), 7, "{format:?}");
    }
}

#[test]
fn nan_writes_zero_code() {
    let mut f = Formatter::new(OutputFormat::S16, 1);
    assert_eq!(i16s(&write(&mut f, &[f32::NAN])), [0]);
}

#[test]
fn should_dither_truth_table() {
    use DitherMode::*;
    // (mode, target, source, dsp, expected)
    let cases = [
        (Auto, 16, 24, false, true),
        (Auto, 16, 16, false, false),
        (Auto, 16, 16, true, true),
        (Auto, 16, 8, false, false),
        (Auto, 16, 8, true, true),
        (Auto, 24, 32, true, false),
        (Auto, 24, 24, true, false),
        (Auto, 32, 32, true, false),
        (Auto, 23, 24, false, true),
        (Off, 16, 24, true, false),
        (Off, 16, 16, true, false),
    ];
    for (mode, target, source, dsp, want) in cases {
        assert_eq!(
            should_dither(mode, target, source, dsp),
            want,
            "{mode:?} target {target} source {source} dsp {dsp}"
        );
    }
}

/// Error in LSBs of 16-bit output against a constant 0.3 LSB offset input, with TPDF dither.
fn dither_errors(seed: u64, n: usize) -> Vec<f64> {
    let lsb = 1.0 / 32_768.0;
    let x = (1000.3 * lsb) as f32;
    let exact = x as f64 / lsb;
    let mut f = Formatter::new(OutputFormat::S16, seed);
    f.set_dither(true);
    let input = vec![x; n];
    i16s(&write(&mut f, &input))
        .iter()
        .map(|&v| v as f64 - exact)
        .collect()
}

#[test]
fn dither_error_is_zero_mean_with_tpdf_variance() {
    let e = dither_errors(42, 1_000_000);
    let n = e.len() as f64;
    let mean = e.iter().sum::<f64>() / n;
    let var = e.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    assert!(mean.abs() < 5e-3, "mean error {mean} LSB");
    let want = 1.0 / 12.0 + 1.0 / 6.0;
    assert!(
        (var - want).abs() < 0.01,
        "variance {var}, want {want} LSB^2"
    );
}

#[test]
fn dither_off_is_deterministic_and_seed_independent() {
    let x = vec![0.123_456_f32; 4096];
    let a = write(&mut Formatter::new(OutputFormat::S16, 1), &x);
    let b = write(&mut Formatter::new(OutputFormat::S16, 99), &x);
    assert_eq!(a, b);
    assert!(
        i16s(&a).windows(2).all(|w| w[0] == w[1]),
        "constant input, constant output"
    );
}

#[test]
fn dither_same_seed_same_output_different_seed_differs() {
    let x = vec![0.123_456_f32; 4096];
    let run = |seed| {
        let mut f = Formatter::new(OutputFormat::S16, seed);
        f.set_dither(true);
        write(&mut f, &x)
    };
    assert_eq!(run(7), run(7));
    assert_ne!(run(7), run(8));
    assert_ne!(
        run(7),
        write(&mut Formatter::new(OutputFormat::S16, 7), &x),
        "dither changes output"
    );
}

#[test]
fn limiter_is_bit_transparent_at_or_below_threshold() {
    let mut x: Vec<f32> = (-1000..=1000)
        .map(|i| i as f32 / 1000.0 * LIMITER_THRESHOLD)
        .collect();
    x.extend([LIMITER_THRESHOLD, -LIMITER_THRESHOLD, 0.0, -0.0]);
    let mut f = Formatter::new(OutputFormat::F32, 1);
    let out = write(&mut f, &x);
    for (i, (b, s)) in out.as_chunks::<4>().0.iter().zip(&x).enumerate() {
        assert_eq!(*b, s.to_le_bytes(), "sample {i} = {s}");
    }
    assert_eq!(f.limited_samples(), 0);
}

#[test]
fn limiter_counts_and_bounds_overs() {
    let mut f = Formatter::new(OutputFormat::F32, 1);
    let input = [0.5, 0.97, -0.98, 1.0, -1.5, 2.0, 0.9];
    let out = write(&mut f, &input);
    assert_eq!(f.limited_samples(), 5);
    for (b, s) in out.as_chunks::<4>().0.iter().zip(&input) {
        let y = f32::from_le_bytes(*b);
        assert!(y.abs() < 1.0, "{s} -> {y}");
        if s.abs() > LIMITER_THRESHOLD {
            assert!(
                y.abs() > LIMITER_THRESHOLD && y.abs() <= s.abs(),
                "{s} -> {y}"
            );
            assert_eq!(y.signum(), s.signum());
        }
    }
    // Counter accumulates across calls.
    write(&mut f, &[3.0]);
    assert_eq!(f.limited_samples(), 6);
}

#[test]
fn limiter_is_monotonic_across_the_knee() {
    let mut f = Formatter::new(OutputFormat::F32, 1);
    let input: Vec<f32> = (0..2000).map(|i| 0.9 + i as f32 * 0.001).collect();
    let out = write(&mut f, &input);
    let y: Vec<f32> = out
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    assert!(y.windows(2).all(|w| w[1] >= w[0]), "non-decreasing");
    assert!(y.iter().all(|v| v.abs() < 1.0));
}

#[test]
fn limiter_can_be_disabled_for_bit_perfect() {
    let mut f = Formatter::new(OutputFormat::F32, 1);
    f.set_limiter(false);
    let out = write(&mut f, &[1.5, -1.0]);
    assert_eq!(&out[..4], &1.5f32.to_le_bytes());
    assert_eq!(f.limited_samples(), 0);
}

#[test]
fn s24_packed_byte_order() {
    // +0.5 -> code 0x400000, -0.5 -> -0x400000 = 0xC00000, 1 LSB -> 0x000001, -1 LSB -> 0xFFFFFF.
    let lsb = 1.0 / 8_388_608.0;
    let mut f = Formatter::new(OutputFormat::S24Packed, 1);
    let out = write(&mut f, &[0.5, -0.5, lsb, -lsb]);
    assert_eq!(
        out,
        [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0xFF]
    );
}

#[test]
fn s32_24_in_32_places_bits_in_the_upper_three_bytes() {
    let mut f = Formatter::new(OutputFormat::S32 { valid_bits: 24 }, 1);
    let out = write(&mut f, &[0.5, 1.0 / 8_388_608.0]);
    assert_eq!(out, [0x00, 0x00, 0x00, 0x40, 0x00, 0x01, 0x00, 0x00]);
}

#[test]
#[should_panic(expected = "output buffer too small")]
fn write_asserts_output_length() {
    let mut f = Formatter::new(OutputFormat::S16, 1);
    f.write_le(&[0.0; 4], &mut [0u8; 7]);
}

#[test]
fn write_returns_bytes_written_and_leaves_tail_untouched() {
    let mut f = Formatter::new(OutputFormat::S24Packed, 1);
    let mut out = [0xAAu8; 12];
    assert_eq!(f.write_le(&[0.0, 0.0], &mut out), 6);
    assert_eq!(&out[6..], &[0xAA; 6]);
}
