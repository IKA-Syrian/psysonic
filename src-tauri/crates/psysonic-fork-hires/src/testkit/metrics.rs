//! Tone metrics on a coherent one-second window: FFT bin k is k Hz, so an integer-Hz tone sits in exactly one bin.
//!
//! Powers are one-sided and weighted so they add up to the time-domain mean square: interior bins count twice,
//! DC and Nyquist once.

use rustfft::{num_complex::Complex, FftPlanner};

#[derive(Debug, Clone, Copy)]
pub struct ToneReport {
    /// Everything except the tone and DC (noise, distortion, aliases, images) relative to the tone power.
    pub residual_db: f64,
    /// Strongest single non-tone, non-DC bin relative to the tone.
    pub worst_spur_db: f64,
    /// Measured tone amplitude against the expected one.
    pub gain_db: f64,
    /// DC power relative to the tone power, as mean square over mean square.
    pub dc_db: f64,
}

const FLOOR: f64 = 1e-30;

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(FLOOR).log10()
}

fn assert_valid(window: &[f32], amplitude: f64) {
    assert!(
        window.iter().all(|s| s.is_finite()),
        "window has NaN or infinite samples"
    );
    assert!(amplitude > 0.0, "amplitude must be positive");
}

/// Weighted one-sided power per bin, 0..=N/2.
fn power_spectrum(window: &[f32]) -> Vec<f64> {
    let n = window.len();
    let mut buf: Vec<Complex<f64>> = window
        .iter()
        .map(|&s| Complex::new(s as f64, 0.0))
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let nyquist = n.is_multiple_of(2).then_some(n / 2);
    buf[..=n / 2]
        .iter()
        .enumerate()
        .map(|(bin, c)| {
            let weight = if bin == 0 || Some(bin) == nyquist {
                1.0
            } else {
                2.0
            };
            weight * c.norm_sqr()
        })
        .collect()
}

pub fn analyze_tone(window: &[f32], rate: u32, freq_hz: u32, amplitude: f64) -> ToneReport {
    assert_eq!(window.len(), rate as usize, "window must be one second");
    assert!(
        freq_hz >= 1 && freq_hz < rate / 2,
        "tone must sit in bins 1..N/2"
    );
    assert_valid(window, amplitude);
    let power = power_spectrum(window);
    let k = freq_hz as usize;
    let tone = power[k];
    assert!(tone > 0.0, "no energy in the tone bin (silence?)");
    let (mut residual, mut worst) = (0.0, 0.0f64);
    for (bin, &p) in power.iter().enumerate().skip(1) {
        if bin != k {
            residual += p;
            worst = worst.max(p);
        }
    }
    // A weighted interior bin is 2·|X|², and |X| = A·N/2, so the tone power is A²·N²/2.
    let measured = (2.0 * tone).sqrt() / window.len() as f64;
    ToneReport {
        residual_db: db(residual / tone),
        worst_spur_db: db(worst / tone),
        gain_db: db((measured / amplitude).powi(2)),
        dc_db: db(power[0] / tone),
    }
}

/// Mean-square power relative to a sine of peak `amplitude` (A^2/2), in dB.
pub fn level_db(window: &[f32], amplitude: f64) -> f64 {
    assert_valid(window, amplitude);
    let mean_square = window.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / window.len() as f64;
    db(mean_square / (amplitude * amplitude / 2.0))
}
