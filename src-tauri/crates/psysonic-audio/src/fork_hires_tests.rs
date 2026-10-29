use super::*;

fn on() -> ForkHiResConfig {
    ForkHiResConfig {
        enabled: true,
        ..Default::default()
    }
}

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("psysonic-fork-hires-{name}-{nanos}"))
}

#[test]
fn blends_enabled_truth_table() {
    for bits in 0..8u8 {
        let (c, g, a) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        assert_eq!(blends_enabled(c, g, a), bits != 0);
    }
}

#[test]
fn derive_transition_truth_table() {
    for bits in 0..32u8 {
        let b = |i: u8| bits & (1 << i) != 0;
        let (open, playing, xf, gapless, manual) = (b(0), b(1), b(2), b(3), b(4));
        let want = open && playing && (xf || (gapless && !manual));
        let got = derive_transition(open, playing, xf, gapless, manual);
        assert_eq!(got == Transition::Blend, want, "bits {bits:05b}");
    }
}

#[test]
fn plan_is_none_when_disabled() {
    let cfg = ForkHiResConfig::default();
    assert_eq!(
        plan_target_pure(
            &cfg,
            48_000,
            44_100,
            96_000,
            Transition::Hard,
            true,
            &[],
            None
        ),
        None
    );
}

#[test]
fn plan_follows_mix_rate_when_enabled() {
    let cfg = on();
    assert_eq!(
        plan_target_pure(
            &cfg,
            48_000,
            44_100,
            96_000,
            Transition::Blend,
            true,
            &[192_000],
            None
        ),
        Some(48_000)
    );
    assert_eq!(
        plan_target_pure(
            &cfg,
            48_000,
            48_000,
            44_100,
            Transition::Hard,
            false,
            &[],
            None
        ),
        Some(48_000)
    );
}

#[test]
fn zero_stream_rate_is_a_closed_stream() {
    let cfg = on();
    assert_eq!(
        plan_target_pure(&cfg, 44_100, 0, 96_000, Transition::Blend, true, &[], None),
        Some(44_100)
    );
}

#[test]
fn ceiling_defaults_to_192k() {
    let mut cfg = on();
    assert_eq!(effective_ceiling_hz(&cfg), 192_000);
    cfg.ceiling_hz = Some(96_000);
    assert_eq!(effective_ceiling_hz(&cfg), 96_000);
}

#[test]
fn defaults_match_the_spec() {
    let d = ForkHiResConfig::default();
    assert!(!d.enabled);
    assert_eq!(d.policy, ForkPolicy::Seamless);
    assert_eq!(
        (d.ceiling_hz, d.resync_silence_ms, d.chunk_frames),
        (None, 250, 1024)
    );
}

#[test]
fn config_serde_uses_camel_case() {
    let cfg = ForkHiResConfig {
        enabled: true,
        policy: ForkPolicy::Adaptive,
        backend: ForkBackend::Exclusive,
        ceiling_hz: Some(96_000),
        resync_silence_ms: 100,
        chunk_frames: 512,
    };
    let v = serde_json::to_value(&cfg).unwrap();
    assert_eq!(v["ceilingHz"], 96_000);
    assert_eq!(v["resyncSilenceMs"], 100);
    assert_eq!(v["chunkFrames"], 512);
    assert_eq!(v["policy"], "adaptive");
    assert_eq!(v["backend"], "exclusive");
    assert_eq!(serde_json::from_value::<ForkHiResConfig>(v).unwrap(), cfg);
    let none = serde_json::to_value(ForkHiResConfig::default()).unwrap();
    assert!(none["ceilingHz"].is_null());
}

#[test]
fn missing_or_invalid_file_loads_defaults() {
    let missing = tmp("missing");
    assert_eq!(load_config_from(&missing), ForkHiResConfig::default());
    let bad = tmp("bad");
    std::fs::write(&bad, "{ not json").unwrap();
    assert_eq!(load_config_from(&bad), ForkHiResConfig::default());
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn save_then_load_round_trips() {
    let dir = tmp("rt");
    let file = dir.join("nested").join("fork-hires.json");
    let cfg = ForkHiResConfig {
        enabled: true,
        ceiling_hz: Some(176_400),
        ..Default::default()
    };
    save_config_to(&file, &cfg).unwrap();
    assert_eq!(load_config_from(&file), cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- audio_play hooks -------------------------------------------------------------------

use crate::playback_rate::{
    PlaybackRateAtomics, STRATEGY_PRESERVE_PITCH, STRATEGY_SPEED_CORRECTED, STRATEGY_VARISPEED,
};
use crate::stream_idle::tests::minimal_engine;

fn rate_state(enabled: bool, strategy: u32, speed: f32) -> PlaybackRateAtomics {
    let a = PlaybackRateAtomics::new();
    a.enabled.store(enabled, Ordering::Relaxed);
    a.strategy.store(strategy, Ordering::Relaxed);
    a.speed.store(speed.to_bits(), Ordering::Relaxed);
    a
}

#[test]
fn stream_format_and_plan_target_are_none_when_disabled() {
    // The global flag defaults to off and no test in this crate turns it on.
    assert!(!enabled());
    let engine = minimal_engine();
    assert_eq!(stream_format(&engine), None);
    assert_eq!(plan_target(&engine, 96_000, true, true, false), None);
}

#[test]
fn varispeed_predicate() {
    // Only an active varispeed effect changes the reported rate.
    assert!(varispeed_active(&rate_state(
        true,
        STRATEGY_VARISPEED,
        1.25
    )));
    assert!(!varispeed_active(&rate_state(
        true,
        STRATEGY_VARISPEED,
        1.0
    )));
    assert!(!varispeed_active(&rate_state(
        false,
        STRATEGY_VARISPEED,
        1.25
    )));
    assert!(!varispeed_active(&rate_state(
        true,
        STRATEGY_PRESERVE_PITCH,
        1.25
    )));
    assert!(!varispeed_active(&rate_state(
        true,
        STRATEGY_SPEED_CORRECTED,
        1.25
    )));
    assert!(!varispeed_active(&PlaybackRateAtomics::new()));
}

#[test]
fn format_payload_marks_resampled_from_native() {
    let fmt = (
        ChannelCount::new(2).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    let p = format_payload(44_100, fmt, ForkPolicy::Seamless, OutputInfo::default());
    assert_eq!(
        (p.rate_hz, p.channels, p.resampled_from_hz),
        (48_000, 2, Some(44_100))
    );
    assert_eq!(
        format_payload(48_000, fmt, ForkPolicy::Adaptive, OutputInfo::default()).resampled_from_hz,
        None
    );
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["rateHz"], 48_000);
    assert_eq!(v["resampledFromHz"], 44_100);
    assert_eq!(v["policy"], "seamless");
    assert_eq!(v["backend"], "shared");
    assert_eq!(v["bitPerfect"], false);
    assert!(v["fallbackReason"].is_null());
}

#[test]
fn format_payload_reports_the_exclusive_backend_and_gate() {
    let fmt = (
        ChannelCount::new(2).unwrap(),
        SampleRate::new(96_000).unwrap(),
    );
    let info = |exclusive, bit_perfect| OutputInfo {
        exclusive,
        fallback_reason: Some("busy".into()),
        bit_perfect,
    };
    let p = format_payload(96_000, fmt, ForkPolicy::Seamless, info(true, true));
    assert_eq!((p.backend.as_str(), p.bit_perfect), ("exclusive", true));
    assert_eq!(p.fallback_reason.as_deref(), Some("busy"));
    // A shared stream is never bit-perfect, whatever the flag says.
    let p = format_payload(96_000, fmt, ForkPolicy::Seamless, info(false, true));
    assert_eq!((p.backend.as_str(), p.bit_perfect), ("shared", false));
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["fallbackReason"], "busy");
}

#[test]
fn old_config_json_without_backend_loads_as_shared() {
    let old = r#"{"enabled":true,"policy":"adaptive","ceilingHz":96000,"resyncSilenceMs":100,"chunkFrames":512}"#;
    let cfg: ForkHiResConfig = serde_json::from_str(old).unwrap();
    assert_eq!(cfg.backend, ForkBackend::Shared);
    assert!(cfg.enabled);
    // And through the file loader.
    let dir = tmp("old");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fork-hires.json");
    std::fs::write(&file, old).unwrap();
    assert_eq!(load_config_from(&file), cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- exclusive planning ---------------------------------------------------------------------

/// The Steam Streaming Speakers endpoint: 44.1 to 192 kHz in S24/S16.
fn steam_caps() -> DeviceCaps {
    DeviceCaps::new(vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000]).unwrap()
}

fn exclusive_cfg(policy: ForkPolicy) -> ForkHiResConfig {
    ForkHiResConfig {
        enabled: true,
        policy,
        backend: ForkBackend::Exclusive,
        ..Default::default()
    }
}

#[test]
fn exclusive_seamless_runs_at_the_ceiling() {
    let caps = steam_caps();
    let cfg = exclusive_cfg(ForkPolicy::Seamless);
    let plan = |c: &ForkHiResConfig| {
        plan_target_pure(
            c,
            48_000,
            0,
            44_100,
            Transition::Hard,
            true,
            &[],
            Some(&caps),
        )
    };
    assert_eq!(plan(&cfg), Some(192_000));
    // A lower user ceiling caps it.
    let capped = ForkHiResConfig {
        ceiling_hz: Some(96_000),
        ..cfg
    };
    assert_eq!(plan(&capped), Some(96_000));
}

#[test]
fn exclusive_native_follows_the_track_rate() {
    let caps = steam_caps();
    let cfg = exclusive_cfg(ForkPolicy::Seamless);
    // No blends: the lowest supported rate at or above the track rate.
    let plan = |track| {
        plan_target_pure(
            &cfg,
            48_000,
            0,
            track,
            Transition::Hard,
            false,
            &[],
            Some(&caps),
        )
    };
    assert_eq!(plan(44_100), Some(44_100));
    assert_eq!(plan(96_000), Some(96_000));
    assert_eq!(plan(32_000), Some(44_100));
    assert_eq!(plan(384_000), Some(192_000));
}

#[test]
fn without_exclusive_caps_the_plan_stays_on_the_mix_rate() {
    let cfg = exclusive_cfg(ForkPolicy::Seamless);
    assert_eq!(
        plan_target_pure(&cfg, 48_000, 0, 96_000, Transition::Hard, false, &[], None),
        Some(48_000)
    );
}

// ---- gate inputs --------------------------------------------------------------------------

fn unity() -> EngineGateValues {
    EngineGateValues {
        base_volume: 1.0,
        player_volume: Some(crate::helpers::MASTER_HEADROOM),
        replay_gain_linear: 1.0,
        normalization_engine: 0,
        eq_enabled: false,
        speed_effect_active: false,
        fading_out: false,
        preview: false,
        radio: false,
        passthrough: true,
    }
}

#[test]
fn gate_inputs_all_unity_is_bit_perfect() {
    use psysonic_fork_hires::exclusive::gate::bit_perfect;
    let i = gate_inputs_from(&unity());
    assert!(bit_perfect(&i));
    assert!(i.volume_is_unity && i.normalization_off && i.eq_off);
    assert!(i.speed_is_unity && i.no_blend_or_extra_players && i.rate_matches);
}

#[test]
fn gate_inputs_each_engine_value_breaks_exactly_its_input() {
    type Case = (
        &'static str,
        fn(&mut EngineGateValues),
        fn(&GateInputs) -> bool,
    );
    let cases: [Case; 12] = [
        (
            "base volume",
            |v| v.base_volume = 0.8,
            |i| i.volume_is_unity,
        ),
        (
            "player volume",
            |v| v.player_volume = Some(0.5),
            |i| i.volume_is_unity,
        ),
        (
            "player gain above headroom",
            |v| v.player_volume = Some(1.0),
            |i| i.volume_is_unity,
        ),
        (
            "no player",
            |v| v.player_volume = None,
            |i| i.volume_is_unity,
        ),
        (
            "replay gain",
            |v| v.replay_gain_linear = 0.9,
            |i| i.normalization_off,
        ),
        (
            "normalization engine",
            |v| v.normalization_engine = 1,
            |i| i.normalization_off,
        ),
        ("eq", |v| v.eq_enabled = true, |i| i.eq_off),
        (
            "speed",
            |v| v.speed_effect_active = true,
            |i| i.speed_is_unity,
        ),
        (
            "fading out",
            |v| v.fading_out = true,
            |i| i.no_blend_or_extra_players,
        ),
        (
            "preview",
            |v| v.preview = true,
            |i| i.no_blend_or_extra_players,
        ),
        ("radio", |v| v.radio = true, |i| i.no_blend_or_extra_players),
        ("resampling", |v| v.passthrough = false, |i| i.rate_matches),
    ];
    for (name, change, input) in cases {
        let mut v = unity();
        change(&mut v);
        let i = gate_inputs_from(&v);
        assert!(!input(&i), "{name}: its own input must be off");
        assert!(
            !psysonic_fork_hires::exclusive::gate::bit_perfect(&i),
            "{name}: the gate must be off"
        );
    }
}

#[test]
fn engine_gate_inputs_reads_a_minimal_engine() {
    let engine = minimal_engine();
    let i = engine_gate_inputs(&engine).expect("uncontended");
    // No player yet, so the volume is not unity; the rest is at rest.
    assert!(!i.volume_is_unity);
    assert!(i.normalization_off && i.eq_off && i.speed_is_unity && i.no_blend_or_extra_players);
    // A contended lock reads as unknown.
    let _held = engine.current.lock().unwrap();
    assert_eq!(engine_gate_inputs(&engine), None);
}

#[test]
fn effective_mix_rate_prefers_the_refreshed_value() {
    assert_eq!(effective_mix_rate(0, 48_000), 48_000);
    assert_eq!(effective_mix_rate(96_000, 48_000), 96_000);
    assert_eq!(effective_mix_rate(44_100, 44_100), 44_100);
}

#[test]
fn refresh_mix_rate_is_none_and_stores_nothing_when_disabled() {
    assert!(!enabled());
    assert_eq!(refresh_mix_rate(None), None);
    assert_eq!(refresh_mix_rate(Some("no such device")), None);
    assert_eq!(MIX_RATE.load(Ordering::Relaxed), 0);
}

#[test]
fn chained_source_has_no_priming_and_first_source_adds_512_samples() {
    use rodio::buffer::SamplesBuffer;
    let fmt = (
        ChannelCount::new(2).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    let data: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let mk = || SamplesBuffer::new(fmt.0, fmt.1, data.clone());
    let chained: Vec<f32> = chained_source(mk(), fmt).collect();
    let first: Vec<f32> = first_source(mk(), fmt).collect();
    assert_eq!(chained.len(), data.len());
    assert_eq!(first.len(), data.len() + 512);
    assert!(first[..512].iter().all(|&x| x == 0.0));
    assert_eq!(&first[512..], &chained[..]);
    // Same-rate passthrough is bit exact.
    assert_eq!(chained, data);
}

mod offline {
    //! The real decode path (`build_source`, native rate) into `first_source` at a 48 kHz stereo
    //! stream, played out through a rodio `Player` and mixer.

    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use psysonic_fork_hires::testkit::metrics::analyze_tone;
    use psysonic_fork_hires::testkit::signal::deinterleave;
    use rodio::mixer::mixer;
    use rodio::{ChannelCount, Player, SampleRate};

    use super::{chained_source, first_source, PlaybackRateAtomics};
    use crate::decode::build_source;

    const STREAM_HZ: u32 = 48_000;
    /// Silence before the track: the 512 samples rodio's queue emits before its first source is polled, plus
    /// the 512 stereo samples of `priming_silence` inside `first_source`.
    const PRIMING: usize = 1024;

    const LAME_SINE_MP3: &[u8] = include_bytes!("../fixtures/lame_sine_22050.mp3");

    /// Rendered stream samples (priming included), counter samples, native rate, native channels.
    fn play(data: Vec<u8>, hint: &str) -> (Vec<f32>, u64, u32, u16) {
        let _globals = crate::spectrum::tests::lock_globals();
        let counter = Arc::new(AtomicU64::new(0));
        let built = build_source(
            data,
            0.0,
            Arc::new(std::array::from_fn::<_, 10, _>(|_| {
                AtomicU32::new(0f32.to_bits())
            })),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0f32.to_bits())),
            PlaybackRateAtomics::new(),
            Arc::new(AtomicBool::new(false)),
            Duration::ZERO,
            counter.clone(),
            0, // native decode, as with the legacy hi-res path off
            0,
            Some(hint),
            false,
        )
        .expect("fixture decodes");
        let (native_hz, native_ch) = (built.output_rate, built.output_channels);
        let fmt = (
            ChannelCount::new(2).unwrap(),
            SampleRate::new(STREAM_HZ).unwrap(),
        );
        let (controller, output) = mixer(fmt.0, fmt.1);
        let player = Player::connect_new(&controller);
        player.append(first_source(built.source, fmt));
        let mut rendered = Vec::new();
        for s in output {
            rendered.push(s);
            if player.empty() || rendered.len() > 20 * STREAM_HZ as usize {
                break;
            }
        }
        (
            rendered,
            counter.load(Ordering::Relaxed),
            native_hz,
            native_ch,
        )
    }

    #[test]
    fn mp3_fixture_length_and_position_use_native_rate() {
        let (out, counted, native_hz, native_ch) = play(LAME_SINE_MP3.to_vec(), "mp3");
        assert_ne!(native_hz, STREAM_HZ, "fixture must need resampling");
        // README: 22050 frames of the signal remain after the Xing/LAME trim.
        let secs = 22_050.0 / native_hz as f64;
        // The counter ticks at the native format, so counter / (rate * channels) is the duration.
        let pos = counted as f64 / (native_hz as f64 * native_ch as f64);
        assert!((pos - secs).abs() < 0.001, "position {pos} vs {secs}");
        // HqResample yields round(frames * to / from) frames per track; the mixer may add or drop
        // a sample or two where the queue ends, so allow +-4 frames (0.08 ms).
        let want_frames = (secs * STREAM_HZ as f64).round() as i64;
        let got_frames = ((out.len() - PRIMING) / 2) as i64;
        eprintln!(
            "mp3: native {native_hz} Hz x{native_ch}, counted {counted}, out {} samples, frames {got_frames} vs {want_frames}",
            out.len()
        );
        assert!(
            (got_frames - want_frames).abs() <= 4,
            "frames {got_frames} vs {want_frames}"
        );
    }

    /// 16-bit PCM WAV: `secs` seconds at `rate`, one sine per channel (`freqs[ch]`), amplitude 0.5.
    fn wav(rate: u32, secs: u32, freqs: &[f64]) -> Vec<u8> {
        let ch = freqs.len();
        let n = rate as usize * secs as usize;
        let mut data = Vec::with_capacity(n * ch * 2);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            for f in freqs {
                let v = (2.0 * std::f64::consts::PI * f * t).sin() * 0.5;
                data.extend_from_slice(&((v * 32767.0).round() as i16).to_le_bytes());
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(ch as u16).to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * ch as u32 * 2).to_le_bytes());
        out.extend_from_slice(&(ch as u16 * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// 3 s stereo at 44.1 kHz: 1000 Hz left, 997 Hz right.
    fn wav_44k() -> Vec<u8> {
        wav(44_100, 3, &[1000.0, 997.0])
    }

    /// Plays `a` through `first_source`, then chains `b` through `chained_source`, onto one `Player` on
    /// a 48 kHz stereo mixer. Returns the rendered samples and each track's (counter, native rate, channels).
    fn play_chain(a: Vec<u8>, b: Vec<u8>) -> (Vec<f32>, [(u64, u32, u16); 2]) {
        let _globals = crate::spectrum::tests::lock_globals();
        let decode = |data: Vec<u8>| {
            let counter = Arc::new(AtomicU64::new(0));
            let built = build_source(
                data,
                0.0,
                Arc::new(std::array::from_fn::<_, 10, _>(|_| {
                    AtomicU32::new(0f32.to_bits())
                })),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicU32::new(0f32.to_bits())),
                PlaybackRateAtomics::new(),
                Arc::new(AtomicBool::new(false)),
                Duration::ZERO,
                counter.clone(),
                0,
                0,
                Some("wav"),
                false,
            )
            .expect("wav decodes");
            (built, counter)
        };
        let (ba, ca) = decode(a);
        let (bb, cb) = decode(b);
        let info = [
            (ba.output_rate, ba.output_channels),
            (bb.output_rate, bb.output_channels),
        ];
        let fmt = (
            ChannelCount::new(2).unwrap(),
            SampleRate::new(STREAM_HZ).unwrap(),
        );
        let (controller, output) = mixer(fmt.0, fmt.1);
        let player = Player::connect_new(&controller);
        player.append(first_source(ba.source, fmt));
        // The gapless chain appends the successor while the first track is still playing.
        player.append(chained_source(bb.source, fmt));
        let mut rendered = Vec::new();
        for s in output {
            rendered.push(s);
            if player.empty() || rendered.len() > 20 * STREAM_HZ as usize {
                break;
            }
        }
        (
            rendered,
            [
                (ca.load(Ordering::Relaxed), info[0].0, info[0].1),
                (cb.load(Ordering::Relaxed), info[1].0, info[1].1),
            ],
        )
    }

    /// First stereo frame at or after `from` where the left channel stops repeating with the 48-sample
    /// period of the 1000 Hz tone that precedes the boundary.
    fn boundary_frame(out: &[f32], from: usize) -> usize {
        (from..out.len() / 2)
            .find(|&n| (out[n * 2] - out[(n - 48) * 2]).abs() > 0.05)
            .expect("tone change found")
    }

    #[test]
    fn gapless_chain_across_rates_is_seamless() {
        // Track 1: 3 s, 44.1 kHz stereo, 1000 Hz. Track 2: 2 s, 32 kHz stereo, 2200 Hz on both channels.
        let (out, info) = play_chain(wav_44k(), wav(32_000, 2, &[2200.0, 2200.0]));
        assert_eq!(info[0], (3 * 44_100 * 2, 44_100, 2));
        assert_eq!(info[1], (2 * 32_000 * 2, 32_000, 2));
        let (f1, f2) = (3 * STREAM_HZ as i64, 2 * STREAM_HZ as i64);
        let got = ((out.len() - PRIMING) / 2) as i64;
        let boundary = boundary_frame(&out, PRIMING / 2 + 4800) as i64;
        let want_boundary = (PRIMING / 2) as i64 + f1;
        eprintln!(
            "chain: out {} samples, frames {got} vs {}, boundary {boundary} vs {want_boundary}",
            out.len(),
            f1 + f2
        );
        // Each track is exactly round(frames * 48000 / rate) frames; the mixer may add or drop a sample
        // or two where the queue ends (as in the single-track test): +-4 frames.
        assert!((got - (f1 + f2)).abs() <= 4, "total {got} vs {}", f1 + f2);
        // Track 2 starts where track 1 ends. The first output sample of a 2200 Hz sine is 0, so allow
        // two frames for the detector.
        assert!(
            (boundary - want_boundary).abs() <= 2,
            "boundary {boundary} vs {want_boundary}"
        );
        // Track 2 is the right tone at the right speed: 2200 Hz, one second, clear of the settle.
        let t2 = &out[(want_boundary as usize + 4800) * 2..];
        let one_sec = &t2[..STREAM_HZ as usize * 2];
        for ch in 0..2 {
            let r = analyze_tone(&deinterleave(one_sec, ch, 2), STREAM_HZ, 2200, 0.5);
            assert!(r.residual_db < -90.0, "ch{ch} residual {}", r.residual_db);
        }
    }

    #[test]
    fn gapless_chain_mono_then_stereo_comes_out_stereo_at_the_right_speed() {
        // Track 1: 2 s mono 44.1 kHz 1000 Hz. Track 2: 2 s stereo 32 kHz 2200 Hz.
        let (out, info) = play_chain(wav(44_100, 2, &[1000.0]), wav(32_000, 2, &[2200.0, 2200.0]));
        assert_eq!(info[0], (2 * 44_100, 44_100, 1));
        assert_eq!(info[1], (2 * 32_000 * 2, 32_000, 2));
        let (f1, f2) = (2 * STREAM_HZ as i64, 2 * STREAM_HZ as i64);
        let got = ((out.len() - PRIMING) / 2) as i64;
        let boundary = boundary_frame(&out, PRIMING / 2 + 4800) as i64;
        let want_boundary = (PRIMING / 2) as i64 + f1;
        eprintln!(
            "mono->stereo: out {} samples, frames {got} vs {}, boundary {boundary} vs {want_boundary}",
            out.len(),
            f1 + f2
        );
        assert!((got - (f1 + f2)).abs() <= 4, "total {got} vs {}", f1 + f2);
        assert!(
            (boundary - want_boundary).abs() <= 2,
            "boundary {boundary} vs {want_boundary}"
        );
        // The mono track is duplicated to both channels, at 1000 Hz (so the right speed).
        let body = &out[(PRIMING / 2 + 4800) * 2..];
        let one_sec = &body[..STREAM_HZ as usize * 2];
        let (l, r) = (deinterleave(one_sec, 0, 2), deinterleave(one_sec, 1, 2));
        assert_eq!(l, r, "mono must be centred");
        let res = analyze_tone(&l, STREAM_HZ, 1000, 0.5);
        assert!(res.residual_db < -90.0, "residual {}", res.residual_db);
    }

    #[test]
    fn lossless_tone_is_clean_after_the_fork_path() {
        let (out, counted, native_hz, native_ch) = play(wav_44k(), "wav");
        assert_eq!((native_hz, native_ch), (44_100, 2));
        let pos = counted as f64 / (native_hz as f64 * native_ch as f64);
        assert!((pos - 3.0).abs() < 0.001, "position {pos}");
        let want_frames = 3 * STREAM_HZ as i64;
        let got_frames = ((out.len() - PRIMING) / 2) as i64;
        eprintln!(
            "wav: out {} samples, frames {got_frames} vs {want_frames}",
            out.len()
        );
        assert!(
            (got_frames - want_frames).abs() <= 4,
            "{got_frames} vs {want_frames}"
        );
        // One coherent second, 0.5 s after the start (clear of the filter settle), per channel.
        let body = &out[PRIMING..];
        let skip = STREAM_HZ as usize / 2;
        let frames = &body[skip * 2..(skip + STREAM_HZ as usize) * 2];
        for (ch, f) in [(0usize, 1000u32), (1, 997)] {
            let r = analyze_tone(&deinterleave(frames, ch, 2), STREAM_HZ, f, 0.5);
            eprintln!("wav ch{ch}: {r:?}");
            // 16-bit source: quantisation floor about -98 dB; the HQ stage sits well below that.
            assert!(r.residual_db < -90.0, "ch{ch} residual {}", r.residual_db);
            assert!(r.gain_db.abs() < 0.01, "ch{ch} gain {}", r.gain_db);
        }
    }
}
