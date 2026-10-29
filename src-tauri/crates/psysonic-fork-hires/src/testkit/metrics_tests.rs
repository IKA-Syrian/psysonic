use super::metrics::*;
use crate::testkit::signal::sine;

#[test]
fn pure_sine_reads_at_float_precision() {
    let window = sine(1000, 48_000, 1.0, 0.5);
    let report = analyze_tone(&window, 48_000, 1000, 0.5);
    assert!(report.residual_db < -130.0, "{report:?}");
    assert!(report.gain_db.abs() < 0.01, "{report:?}");
}

#[test]
fn worst_spur_finds_a_weaker_second_tone() {
    let main = sine(1000, 48_000, 1.0, 0.5);
    let spur = sine(5000, 48_000, 1.0, 0.0005);
    let mix: Vec<f32> = main.iter().zip(&spur).map(|(a, b)| a + b).collect();
    let report = analyze_tone(&mix, 48_000, 1000, 0.5);
    assert!((report.worst_spur_db + 60.0).abs() < 0.5, "{report:?}");
}

#[test]
fn level_of_half_amplitude_sine_is_minus_six_db() {
    let window = sine(1000, 48_000, 1.0, 0.25);
    assert!((level_db(&window, 0.5) + 6.02).abs() < 0.01);
}

const RATE: u32 = 48_000;

fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

fn near(value: f64, expected: f64, tolerance: f64) {
    assert!(
        (value - expected).abs() <= tolerance,
        "{value} not within {tolerance} of {expected}"
    );
}

#[test]
fn spread_noise_reads_at_its_level() {
    let mix = add(&sine(1000, RATE, 1.0, 0.5), &sine(5000, RATE, 1.0, 0.05));
    let report = analyze_tone(&mix, RATE, 1000, 0.5);
    near(report.residual_db, -20.0, 0.1);
    near(report.worst_spur_db, -20.0, 0.1);
}

#[test]
fn nyquist_is_not_over_weighted() {
    let alternating: Vec<f32> = (0..RATE)
        .map(|n| if n % 2 == 0 { 0.05 } else { -0.05 })
        .collect();
    let mix = add(&sine(1000, RATE, 1.0, 0.5), &alternating);
    near(analyze_tone(&mix, RATE, 1000, 0.5).residual_db, -16.99, 0.1);
}

#[test]
fn neighbouring_bin_is_a_spur_not_the_tone() {
    let mix = add(&sine(1000, RATE, 1.0, 0.5), &sine(1001, RATE, 1.0, 0.05));
    let report = analyze_tone(&mix, RATE, 1000, 0.5);
    near(report.worst_spur_db, -20.0, 0.1);
    near(report.residual_db, -20.0, 0.1);
}

#[test]
fn gain_has_the_right_sign_and_scale() {
    let window = sine(1000, RATE, 1.0, 0.5);
    near(analyze_tone(&window, RATE, 1000, 1.0).gain_db, -6.02, 0.01);
    near(analyze_tone(&window, RATE, 1000, 0.25).gain_db, 6.02, 0.01);
}

#[test]
fn dc_offset_shows_up_in_dc_only() {
    let offset = vec![0.05f32; RATE as usize];
    let mix = add(&sine(1000, RATE, 1.0, 0.5), &offset);
    let report = analyze_tone(&mix, RATE, 1000, 0.5);
    // Mean square 0.05^2 against 0.5^2/2.
    near(report.dc_db, -16.99, 0.1);
    assert!(report.residual_db < -130.0, "{report:?}");
}

#[test]
#[should_panic(expected = "NaN")]
fn nan_sample_is_rejected() {
    let mut window = sine(1000, RATE, 1.0, 0.5);
    window[100] = f32::NAN;
    analyze_tone(&window, RATE, 1000, 0.5);
}

#[test]
#[should_panic(expected = "silence")]
fn silent_window_is_rejected() {
    analyze_tone(&vec![0.0; RATE as usize], RATE, 1000, 0.5);
}
