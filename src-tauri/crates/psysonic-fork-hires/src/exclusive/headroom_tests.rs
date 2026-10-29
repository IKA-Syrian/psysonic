//! Proof that undoing rodio's fixed headroom (`x * 0.891_254_f32`) in the formatter with gain `1 / h` returns
//! every 24-bit code (and every 16-bit grid code) bit-exactly.

use super::render::MASTER_HEADROOM;
use crate::format::{Formatter, OutputFormat};

const FULL: f32 = 8_388_608.0; // 2^23

#[derive(Default, Debug)]
struct Report {
    checked: u64,
    s32_fail: Vec<(i32, i32)>,
    s24_fail: Vec<(i32, i32)>,
    s16_checked: u64,
    s16_fail: Vec<(i32, i32)>,
    s32_fail_count: u64,
    s24_fail_count: u64,
    s16_fail_count: u64,
}

fn formatter(format: OutputFormat) -> Formatter {
    let mut f = Formatter::new(format, 1);
    f.set_limiter(false);
    f.set_dither(false);
    f.set_input_gain(1.0 / MASTER_HEADROOM as f64);
    f
}

/// What rodio's `Amplify` does to a decoded 24-bit code.
fn headroomed(k: i32) -> f32 {
    (k as f32 / FULL) * MASTER_HEADROOM
}

fn decode24(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], if b[2] & 0x80 != 0 { 0xff } else { 0 }])
}

fn push(list: &mut Vec<(i32, i32)>, count: &mut u64, k: i32, got: i32) {
    *count += 1;
    if list.len() < 32 {
        list.push((k, got));
    }
}

fn check_24(codes: impl Iterator<Item = i32>, r: &mut Report) {
    let mut s32 = formatter(OutputFormat::S32 { valid_bits: 24 });
    let mut s24 = formatter(OutputFormat::S24Packed);
    const CHUNK: usize = 1 << 16;
    let mut ks = Vec::with_capacity(CHUNK);
    let mut ys = Vec::with_capacity(CHUNK);
    let mut out32 = vec![0u8; CHUNK * 4];
    let mut out24 = vec![0u8; CHUNK * 3];
    let mut codes = codes.peekable();
    while codes.peek().is_some() {
        ks.clear();
        ys.clear();
        ks.extend(codes.by_ref().take(CHUNK));
        ys.extend(ks.iter().map(|&k| headroomed(k)));
        s32.write_le(&ys, &mut out32);
        s24.write_le(&ys, &mut out24);
        for (i, &k) in ks.iter().enumerate() {
            let got32 = i32::from_le_bytes(out32[i * 4..i * 4 + 4].try_into().unwrap()) >> 8;
            if got32 != k {
                push(&mut r.s32_fail, &mut r.s32_fail_count, k, got32);
            }
            let got24 = decode24(&out24[i * 3..i * 3 + 3]);
            if got24 != k {
                push(&mut r.s24_fail, &mut r.s24_fail_count, k, got24);
            }
        }
        r.checked += ks.len() as u64;
    }
}

fn check_16(r: &mut Report) {
    let mut s16 = formatter(OutputFormat::S16);
    let js: Vec<i32> = (-32_768..=32_767).collect();
    let ys: Vec<f32> = js.iter().map(|&j| headroomed(j << 8)).collect();
    let mut out = vec![0u8; ys.len() * 2];
    s16.write_le(&ys, &mut out);
    for (i, &j) in js.iter().enumerate() {
        let got = i16::from_le_bytes([out[i * 2], out[i * 2 + 1]]) as i32;
        if got != j {
            push(&mut r.s16_fail, &mut r.s16_fail_count, j, got);
        }
    }
    r.s16_checked = js.len() as u64;
}

fn report(name: &str, r: &Report) {
    println!(
        "{name}: 24-bit codes checked {}; S32 failures {}; S24 failures {}; S16 grid codes checked {}, failures {}",
        r.checked, r.s32_fail_count, r.s24_fail_count, r.s16_checked, r.s16_fail_count
    );
    println!("first S32 failures (code, got): {:?}", r.s32_fail);
    println!("first S24 failures (code, got): {:?}", r.s24_fail);
    println!("first S16 failures (code, got): {:?}", r.s16_fail);
}

fn assert_clean(r: &Report) {
    assert_eq!(r.s32_fail_count, 0, "S32 failures, first: {:?}", r.s32_fail);
    assert_eq!(r.s24_fail_count, 0, "S24 failures, first: {:?}", r.s24_fail);
    assert_eq!(r.s16_fail_count, 0, "S16 failures, first: {:?}", r.s16_fail);
}

fn edges() -> Vec<i32> {
    let (min, max) = (-(1 << 23), (1 << 23) - 1);
    let mut v = vec![min, min + 1, min + 2, -1, 0, 1, 2, max - 1, max];
    for p in 1..23 {
        let b = 1i32 << p;
        v.extend([b - 1, b, b + 1, -b - 1, -b, -b + 1]);
    }
    v
}

#[test]
fn headroom_cancel_is_exact_dense() {
    let mut r = Report::default();
    let dense = (-(1i32 << 23)..(1 << 23)).step_by(97);
    check_24(dense.chain(edges()), &mut r);
    check_16(&mut r);
    report("dense", &r);
    assert!(r.checked > 170_000);
    assert_clean(&r);
}

#[test]
#[ignore = "exhaustive: all 16.7M codes, run with --release"]
fn headroom_cancel_is_exact_exhaustive() {
    let mut r = Report::default();
    check_24(-(1i32 << 23)..(1 << 23), &mut r);
    check_16(&mut r);
    report("exhaustive", &r);
    assert_eq!(r.checked, 1 << 24);
    assert_clean(&r);
}
