use super::*;

use std::num::{NonZeroU16, NonZeroU32};

fn cfg(ch: u16, hz: u32) -> OutputConfig {
    OutputConfig::new(NonZeroU16::new(ch).unwrap(), NonZeroU32::new(hz).unwrap())
}

#[test]
fn output_config_is_copy_with_rodio_accessor_types() {
    let c = cfg(2, 96_000);
    let copy = c; // Copy
    let ch: ChannelCount = c.channel_count();
    let hz: SampleRate = copy.sample_rate();
    assert_eq!((ch.get(), hz.get()), (2, 96_000));
}

#[test]
fn output_config_from_device_sink_config() {
    let d = rodio::stream::DeviceSinkConfig::default();
    let c = OutputConfig::from(&d);
    assert_eq!(c.channel_count(), d.channel_count());
    assert_eq!(c.sample_rate(), d.sample_rate());
}

fn excl_cfg() -> ForkHiResConfig {
    ForkHiResConfig {
        enabled: true,
        backend: ForkBackend::Exclusive,
        ..Default::default()
    }
}

fn never_called() -> GateSource {
    panic!("must not open an output")
}

#[test]
fn try_open_exclusive_returns_none_with_the_default_config() {
    // The global config defaults to disabled and no non-ignored test enables it.
    assert!(try_open_exclusive(None, 0).is_none());
    assert!(try_open_exclusive(Some("dev"), 192_000).is_none());
}

#[test]
fn open_returns_none_when_disabled_shared_or_sticky() {
    let fb = Mutex::new(FallbackState::default());
    let open = |cfg: &ForkHiResConfig, dev: Option<&str>| {
        open_exclusive_with(cfg, &fb, dev, 96_000, never_called, |_, _| {})
    };
    // Fork engine off.
    let off = ForkHiResConfig {
        enabled: false,
        ..excl_cfg()
    };
    assert!(open(&off, Some("dev")).is_none());
    // Backend shared (the default).
    let shared = ForkHiResConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(open(&shared, Some("dev")).is_none());
    // Device given up on.
    fb.lock().unwrap().set_sticky("dev", "busy".into());
    assert!(open(&excl_cfg(), Some("dev")).is_none());
    fb.lock().unwrap().set_sticky("", "busy".into());
    assert!(open(&excl_cfg(), None).is_none());
    // Nothing above recorded a new failure.
    assert_eq!(fb.lock().unwrap().sticky_reason("dev"), Some("busy"));
}

#[test]
fn should_try_exclusive_truth_table() {
    for bits in 0..8u8 {
        let (enabled, exclusive, sticky) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        let cfg = ForkHiResConfig {
            enabled,
            backend: if exclusive {
                ForkBackend::Exclusive
            } else {
                ForkBackend::Shared
            },
            ..Default::default()
        };
        assert_eq!(
            should_try_exclusive(&cfg, sticky),
            enabled && exclusive && !sticky,
            "bits {bits:03b}"
        );
    }
}

#[test]
fn sticky_fallback_set_and_clear() {
    let mut fb = FallbackState::default();
    assert!(!fb.is_sticky("a"));
    assert_eq!(fb.last_reason(), None);
    fb.set_sticky("a", "device in use".into());
    assert!(fb.is_sticky("a"));
    assert!(!fb.is_sticky("b"), "per device");
    assert_eq!(fb.sticky_reason("a"), Some("device in use"));
    assert_eq!(fb.last_reason().as_deref(), Some("device in use"));
    fb.clear();
    assert!(!fb.is_sticky("a"));
    assert_eq!(fb.last_reason(), None);
}

#[test]
fn second_runtime_failure_on_a_device_is_sticky() {
    let mut fb = FallbackState::default();
    assert_eq!(
        fb.record_runtime_failure("a", "gone".into()),
        FailureAction::Retry
    );
    assert!(!fb.is_sticky("a"));
    assert!(fb.last_reason().unwrap().contains("gone"));
    // Another device has its own count.
    assert_eq!(
        fb.record_runtime_failure("b", "x".into()),
        FailureAction::Retry
    );
    assert_eq!(
        fb.record_runtime_failure("a", "gone again".into()),
        FailureAction::FallBack
    );
    assert!(fb.is_sticky("a") && !fb.is_sticky("b"));
    assert!(fb.sticky_reason("a").unwrap().contains("gone again"));
    fb.clear();
    assert_eq!(
        fb.record_runtime_failure("a", "gone".into()),
        FailureAction::Retry,
        "clearing resets the count"
    );
}

#[test]
fn device_key_is_empty_for_the_default_device() {
    assert_eq!(device_key(None), "");
    assert_eq!(device_key(Some("wasapi:x")), "wasapi:x");
}

#[test]
fn planning_caps_is_none_unless_exclusive_is_wanted() {
    let shared = ForkHiResConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(planning_caps(&shared, Some("dev")).is_none());
    assert!(planning_caps(&ForkHiResConfig::default(), None).is_none());
}

#[test]
fn gate_evaluator_publishes_and_stops_when_the_output_is_gone() {
    // No device: drive the evaluator with a dead Weak. It must exit without calling the source or on_failure.
    let gate = Arc::new(GateState::new());
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let tx2 = tx.clone();
    spawn_gate_evaluator(
        Weak::new(),
        gate.clone(),
        Box::new(move || {
            let _ = tx.send(());
            None
        }),
        move |_| {
            let _ = tx2.send(());
        },
    );
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    assert!(!gate.read(now_ms(), 1_000));
}

/// Hardware test. Opens ONLY the "Steam Streaming Speakers" endpoint, with a tone at about -62 dBFS.
/// Run: `cargo test -p psysonic-audio steam_exclusive -- --ignored --nocapture`.
#[cfg(windows)]
#[test]
#[ignore = "opens the Steam Streaming Speakers endpoint in exclusive mode"]
fn steam_exclusive_open_play_and_gate() {
    use crate::fork_hires::{apply_config, config, first_source, EngineGateValues};
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    use rodio::Player;

    // Find the endpoint by enumeration only (nothing is opened here).
    let host = rodio::cpal::default_host();
    let all: Vec<_> = host.output_devices().unwrap().collect();
    for d in &all {
        println!("endpoint: {:?}", d.description().map(|x| format!("{x:?}")));
    }
    let steam = all
        .into_iter()
        .find(|d| {
            d.description()
                .map(|x| format!("{x:?}").contains("Steam Streaming Speakers"))
                .unwrap_or(false)
        })
        .expect("Steam Streaming Speakers endpoint not found");
    let key = crate::dev_io::output_device_stable_key(&steam);
    println!("device key: {key}");
    drop(steam);

    apply_config(ForkHiResConfig {
        enabled: true,
        backend: ForkBackend::Exclusive,
        ..Default::default()
    });
    let caps = exclusive_caps(&key).expect("caps probe");
    println!("caps rates: {:?}", caps.rates());

    // The gate source: engine values the test controls.
    let values = Arc::new(Mutex::new(EngineGateValues {
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
    }));
    let v2 = values.clone();
    let fallbacks = Mutex::new(FallbackState::default());
    let failed = Arc::new(Mutex::new(None::<String>));
    let f2 = failed.clone();
    let handle = open_exclusive_with(
        &config(),
        &fallbacks,
        Some(&key),
        48_000,
        move || Box::new(move || Some(crate::fork_hires::gate_inputs_from(&v2.lock().unwrap()))),
        move |_, reason| *f2.lock().unwrap() = Some(reason),
    )
    .unwrap_or_else(|| {
        panic!(
            "exclusive open failed: {:?}",
            fallbacks.lock().unwrap().last_reason()
        )
    });
    assert!(handle.is_exclusive());
    let fmt = (
        handle.config().channel_count(),
        handle.config().sample_rate(),
    );
    println!("opened: {} Hz, {} ch", fmt.1.get(), fmt.0.get());
    println!("status at open: {:?}", handle.exclusive_status());

    // 1 s of a 1 kHz tone at 0.0008 (-62 dBFS), through the fork source chain.
    let rate = fmt.1.get() as usize;
    let ch = fmt.0.get() as usize;
    let mut data = Vec::with_capacity(rate * ch);
    for n in 0..rate {
        let x = 0.0008 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / rate as f32).sin();
        data.extend(std::iter::repeat_n(x, ch));
    }
    let player = Player::connect_new(handle.mixer());
    player.set_volume(crate::helpers::MASTER_HEADROOM);
    player.append(first_source(
        rodio::buffer::SamplesBuffer::new(fmt.0, fmt.1, data),
        fmt,
    ));

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        failed.lock().unwrap().is_none(),
        "{:?}",
        failed.lock().unwrap()
    );
    let st = handle.exclusive_status().expect("exclusive status");
    println!("status after 500 ms: {st:?}");
    assert!(st.periods > 0, "render thread must have written periods");
    assert_eq!(st.timeouts, 0);

    // Gate behaviour. Unity volume, EQ off, passthrough: bit-perfect.
    println!("gate (unity): {}", handle.bit_perfect());
    assert!(handle.bit_perfect(), "gate should be on at unity settings");
    // EQ on: off within a few ticks.
    values.lock().unwrap().eq_enabled = true;
    std::thread::sleep(Duration::from_millis(60));
    println!("gate (eq on): {}", handle.bit_perfect());
    assert!(!handle.bit_perfect());
    values.lock().unwrap().eq_enabled = false;
    // Volume off unity: off.
    values.lock().unwrap().base_volume = 0.8;
    std::thread::sleep(Duration::from_millis(60));
    println!("gate (volume 0.8): {}", handle.bit_perfect());
    assert!(!handle.bit_perfect());
    values.lock().unwrap().base_volume = 1.0;
    std::thread::sleep(Duration::from_millis(60));
    assert!(handle.bit_perfect(), "gate recovers");

    assert!(handle.exclusive_status().unwrap().last_error.is_none());
    drop(player);
    drop(handle);
    apply_config(ForkHiResConfig::default());
    println!("closed cleanly");
}

/// Hardware test of the bit-perfect check. ONLY the "Steam Streaming Speakers" endpoint, tone at -60 dBFS on the
/// 24-bit grid. Run: `cargo test -p psysonic-audio steam_exclusive_crc -- --ignored --nocapture`.
#[cfg(windows)]
#[test]
#[ignore = "opens the Steam Streaming Speakers endpoint in exclusive mode"]
fn steam_exclusive_crc_tap() {
    use crate::fork_hires::{apply_config, config, first_source, EngineGateValues};
    use psysonic_fork_hires::exclusive::crc_tap;
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    use rodio::Player;

    crc_tap::set_enabled(true);
    let host = rodio::cpal::default_host();
    let steam = host
        .output_devices()
        .unwrap()
        .find(|d| {
            d.description()
                .map(|x| format!("{x:?}").contains("Steam Streaming Speakers"))
                .unwrap_or(false)
        })
        .expect("Steam Streaming Speakers endpoint not found");
    let key = crate::dev_io::output_device_stable_key(&steam);
    drop(steam);
    apply_config(ForkHiResConfig {
        enabled: true,
        backend: ForkBackend::Exclusive,
        ..Default::default()
    });
    let values = Arc::new(Mutex::new(EngineGateValues {
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
    }));
    let v2 = values.clone();
    let fallbacks = Mutex::new(FallbackState::default());
    let handle = open_exclusive_with(
        &config(),
        &fallbacks,
        Some(&key),
        48_000,
        move || Box::new(move || Some(crate::fork_hires::gate_inputs_from(&v2.lock().unwrap()))),
        move |_, _| {},
    )
    .unwrap_or_else(|| panic!("open failed: {:?}", fallbacks.lock().unwrap().last_reason()));
    let fmt = (
        handle.config().channel_count(),
        handle.config().sample_rate(),
    );
    println!("opened: {} Hz, {} ch", fmt.1.get(), fmt.0.get());
    assert_eq!(fmt.1.get(), 48_000, "the check needs passthrough at 48 kHz");

    // 3 s of 1 kHz at -60 dBFS (amplitude 8388 codes) on the 24-bit grid.
    let rate = fmt.1.get() as usize;
    let ch = fmt.0.get() as usize;
    let mut data = Vec::with_capacity(3 * rate * ch);
    for n in 0..3 * rate {
        let t = n as f64 / rate as f64;
        let code = (8_388.608 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()).round() + 1.0;
        data.extend(std::iter::repeat_n(code as f32 / 8_388_608.0, ch));
    }
    let player = Player::connect_new(handle.mixer());
    player.set_volume(crate::helpers::MASTER_HEADROOM);
    player.append(first_source(
        rodio::buffer::SamplesBuffer::new(fmt.0, fmt.1, data),
        fmt,
    ));
    std::thread::sleep(Duration::from_millis(3_800));
    let st = handle.exclusive_status().expect("exclusive status");
    println!("status: {st:?}");
    println!("check: {:?}", crc_tap::board().latest());
    assert!(handle.bit_perfect());
    assert!(matches!(
        crc_tap::board().latest(),
        Some((_, crc_tap::CrcResult::Match { frames: 48_000 }))
    ));
    drop(player);
    drop(handle);
    apply_config(ForkHiResConfig::default());
    crc_tap::set_enabled(false);
}

#[test]
fn cpal_handle_copies_config_and_is_not_exclusive() {
    // A MixerDeviceSink cannot be built without an audio device (private fields, opens a cpal stream), so this
    // runs only where a default device exists; elsewhere the config/dispatch logic is covered above.
    let Ok(sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
        eprintln!("no audio device; skipping");
        return;
    };
    let expect = OutputConfig::from(sink.config());
    let h: OutputHandle = Arc::new(sink).into();
    assert_eq!(*h.config(), expect);
    assert!(!h.is_exclusive());
    let _ = h.mixer();
    let again = h.clone();
    assert_eq!(*again.config(), expect);

    let forced = OutputHandle::from_cpal_with_config(
        match &h.kind {
            Kind::Cpal(s) => s.clone(),
            Kind::Exclusive { .. } => unreachable!(),
        },
        cfg(2, 44_100),
    );
    assert_eq!(forced.config().sample_rate().get(), 44_100);
    assert!(!forced.is_exclusive());
}
