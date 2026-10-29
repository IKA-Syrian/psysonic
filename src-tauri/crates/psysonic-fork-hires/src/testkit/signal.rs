//! In-memory test signals.

use std::f64::consts::{FRAC_PI_4, TAU};

/// Mono sine starting at phase pi/4 (non-zero first sample). Integer-Hz tones are exactly periodic over whole seconds.
pub fn sine(freq_hz: u32, rate: u32, secs: f64, amplitude: f64) -> Vec<f32> {
    let count = (secs * rate as f64).round() as u64;
    (0..count)
        .map(|n| {
            let cycle = (freq_hz as u64 * n) % rate as u64;
            let phase = TAU * cycle as f64 / rate as f64 + FRAC_PI_4;
            (amplitude * phase.sin()) as f32
        })
        .collect()
}

/// Rounds onto a signed `bits`-deep integer grid and back, as a 16- or 24-bit file decodes.
pub fn quantize(samples: &[f32], bits: u32) -> Vec<f32> {
    assert!((1..=32).contains(&bits), "bits must be 1..=32");
    let scale = (1u64 << (bits - 1)) as f64;
    samples
        .iter()
        .map(|&s| ((s as f64 * scale).round().clamp(-scale, scale - 1.0) / scale) as f32)
        .collect()
}

pub fn interleave(channels: &[&[f32]]) -> Vec<f32> {
    let frames = channels.first().map_or(0, |c| c.len());
    assert!(channels.iter().all(|c| c.len() == frames));
    (0..frames)
        .flat_map(|i| channels.iter().map(move |c| c[i]))
        .collect()
}

pub fn deinterleave(samples: &[f32], channel: usize, count: usize) -> Vec<f32> {
    samples
        .iter()
        .skip(channel)
        .step_by(count)
        .copied()
        .collect()
}

/// First index where the samples differ bit for bit; a length difference counts as a mismatch at the shorter length.
pub fn first_mismatch(a: &[f32], b: &[f32]) -> Option<usize> {
    let common = a.len().min(b.len());
    (0..common)
        .find(|&i| a[i].to_bits() != b[i].to_bits())
        .or((a.len() != b.len()).then_some(common))
}
