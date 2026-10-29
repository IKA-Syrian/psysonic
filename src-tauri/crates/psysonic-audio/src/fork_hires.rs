//! Fork patch: glue for the fork's hi-res engine (`psysonic-fork-hires`, design in
//! `docs/fork-hires-engine-plan.md`). Every engine hook calls into this module, so
//! upstream files only carry one-line `// Fork patch:` calls and the logic, config and
//! state live here. It holds the config (persisted as `fork-hires.json` in the app data
//! folder), the Tauri commands, pure decision helpers, and the `audio_play` hooks:
//! [`plan_target`] (stream rate), [`stream_format`] / [`first_source`] (queue the first
//! track through the HQ resampler at the stream format) and [`announce`].
//!
//! Varispeed fallback: under the varispeed strategy `PlaybackRateSource` reports a scaled
//! sample rate (`base * speed`) that can change while playing, which would make the HQ
//! stage rebuild its FFT on the audio thread. While varispeed is active at a speed other
//! than 1.0, [`stream_format`] returns `None` and `audio_play` takes the upstream path for
//! that track. The preserve-pitch and speed-corrected strategies run their DSP off-thread
//! and report the base rate, so they go through the fork path. The check happens when the
//! track starts; toggling varispeed mid-track is not handled here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use psysonic_fork_hires::exclusive::gate::GateInputs;
use psysonic_fork_hires::hq_resample::PASSTHROUGH_ACTIVE;
use psysonic_fork_hires::rate_plan::{
    plan_output_rate, Backend, DeviceCaps, PlanInput, Policy, Transition,
};
use rodio::{ChannelCount, SampleRate, Source};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

use super::engine::AudioEngine;

/// Ceiling used when the config has none (Phase 3).
const DEFAULT_CEILING_HZ: u32 = 192_000;

/// How the stream rate follows the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ForkPolicy {
    Seamless,
    Adaptive,
}

impl From<ForkPolicy> for Policy {
    fn from(p: ForkPolicy) -> Self {
        match p {
            ForkPolicy::Seamless => Policy::Seamless,
            ForkPolicy::Adaptive => Policy::Adaptive,
        }
    }
}

/// Which output backend the fork engine asks for. `Exclusive` (WASAPI, Windows only) falls back to
/// shared mode when the device refuses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ForkBackend {
    #[default]
    Shared,
    Exclusive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkHiResConfig {
    pub enabled: bool,
    pub policy: ForkPolicy,
    /// Missing in files written before the exclusive backend existed: loads as `Shared`.
    #[serde(default)]
    pub backend: ForkBackend,
    /// `None` = 192 000 Hz.
    pub ceiling_hz: Option<u32>,
    pub resync_silence_ms: u32,
    pub chunk_frames: u32,
}

impl Default for ForkHiResConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: ForkPolicy::Seamless,
            backend: ForkBackend::Shared,
            ceiling_hz: None,
            resync_silence_ms: 250,
            chunk_frames: 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkHiResStatus {
    pub enabled: bool,
    pub policy: ForkPolicy,
    pub ceiling_hz: u32,
    /// Rate of the open stream; `None` when closed.
    pub stream_rate_hz: Option<u32>,
    /// The device's default (mixer) rate.
    pub mix_rate_hz: u32,
    /// What the last `audio_play` through the fork engine announced; `None` before the first.
    pub last_format: Option<ForkOutputFormat>,
    /// Backend of the open stream (`Shared` when closed). The configured one is in the config.
    pub backend: ForkBackend,
    /// Present while an exclusive stream is open.
    pub exclusive: Option<ForkExclusiveStatus>,
    /// True while the open exclusive stream is delivering the samples unaltered.
    pub bit_perfect: bool,
    /// Why the exclusive backend was last abandoned for shared mode, if it was.
    pub last_fallback_reason: Option<String>,
    /// The saved config (not the effective values above), so the UI can edit it without losing fields.
    pub config: ForkHiResConfig,
}

/// Live state of an open WASAPI exclusive stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkExclusiveStatus {
    /// Device sample format: `"S16"`, `"S24"`, `"S32"` or `"F32"`.
    pub format: String,
    pub valid_bits: u32,
    pub rate_hz: u32,
    /// Render counters (saturate at `u32::MAX`).
    pub periods: u32,
    pub late_wakes: u32,
    pub max_gap_us: u32,
    pub timeouts: u32,
    pub last_error: Option<String>,
    /// Result of the opt-in end-to-end bit-perfect check (`PSYSONIC_FORK_CRC_TAP=1`) for the newest track,
    /// e.g. `"match (48000 frames)"`; `None` when the check is off.
    pub bit_perfect_check: Option<String>,
}

/// Payload of the `fork:audio-output-format` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkOutputFormat {
    pub rate_hz: u32,
    pub channels: u16,
    /// The track's native rate when it differs from the stream rate.
    pub resampled_from_hz: Option<u32>,
    pub policy: ForkPolicy,
    /// `"shared"` or `"exclusive"`: the backend of the open stream.
    pub backend: String,
    /// Why exclusive mode was abandoned for shared, if it was.
    pub fallback_reason: Option<String>,
    /// The bit-perfect gate right now (always false in shared mode).
    pub bit_perfect: bool,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static CONFIG: LazyLock<RwLock<ForkHiResConfig>> =
    LazyLock::new(|| RwLock::new(ForkHiResConfig::default()));
/// The output device's current mix rate, refreshed on device changes; 0 = not probed yet.
static MIX_RATE: AtomicU32 = AtomicU32::new(0);
static QUEUE_RATES: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static LAST_FORMAT: Mutex<Option<ForkOutputFormat>> = Mutex::new(None);

/// Fast path for hooks: is the fork engine on?
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn config() -> ForkHiResConfig {
    CONFIG.read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn apply_config(cfg: ForkHiResConfig) {
    // A new config gets a fresh chance at exclusive mode.
    crate::fork_output::clear_device_state();
    ENABLED.store(cfg.enabled, Ordering::Relaxed);
    *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = cfg;
}

/// Decoded rates of the upcoming queue, as last reported by the frontend.
pub fn queue_rates() -> Vec<u32> {
    QUEUE_RATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn effective_ceiling_hz(cfg: &ForkHiResConfig) -> u32 {
    cfg.ceiling_hz.unwrap_or(DEFAULT_CEILING_HZ)
}

/// A missing or invalid file gives the defaults.
pub fn load_config_from(path: &Path) -> ForkHiResConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config_to(path: &Path, cfg: &ForkHiResConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| e.to_string())
}

/// `<app data>/fork-hires.json`, next to `fork-settings.json`.
pub fn config_file(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("fork-hires.json"))
}

/// Whether any blend (crossfade, gapless, AutoDJ) can happen. The Rust engine has no
/// AutoDJ-enabled flag, so `autodj_suppress_autocrossfade` stands in for it: the frontend
/// sets it exactly while AutoDJ is on.
pub fn blends_enabled(crossfade: bool, gapless: bool, autodj_suppress: bool) -> bool {
    crossfade || gapless || autodj_suppress
}

/// `Blend` iff a stream is open and playing and the next start overlaps it.
pub fn derive_transition(
    stream_open: bool,
    sink_playing: bool,
    crossfade_local: bool,
    gapless: bool,
    manual: bool,
) -> Transition {
    if stream_open && sink_playing && (crossfade_local || (gapless && !manual)) {
        Transition::Blend
    } else {
        Transition::Hard
    }
}

/// Target stream rate for the track about to play; `None` when the fork engine is off.
/// `stream_sample_rate == 0` means the stream is closed. `exclusive_caps` is `Some` when the
/// exclusive backend should be planned for (its device caps); `None` plans for shared mode.
#[allow(clippy::too_many_arguments)]
pub fn plan_target_pure(
    cfg: &ForkHiResConfig,
    mix_rate_hz: u32,
    stream_sample_rate: u32,
    track_hz: u32,
    transition: Transition,
    blends: bool,
    upcoming: &[u32],
    exclusive_caps: Option<&DeviceCaps>,
) -> Option<u32> {
    if !cfg.enabled {
        return None;
    }
    let shared_caps = DeviceCaps::new(vec![mix_rate_hz])?;
    let (backend, caps) = match exclusive_caps {
        Some(c) => (Backend::Exclusive, c),
        None => (Backend::Shared { mix_rate_hz }, &shared_caps),
    };
    let plan = plan_output_rate(&PlanInput {
        backend,
        policy: cfg.policy.into(),
        ceiling_hz: effective_ceiling_hz(cfg),
        blends_enabled: blends,
        current_stream_hz: (stream_sample_rate > 0).then_some(stream_sample_rate),
        track_hz,
        transition,
        upcoming_hz: upcoming,
        caps,
    });
    Some(plan.stream_hz)
}

/// The mix rate to plan with: the refreshed one when known, else the startup probe.
pub fn effective_mix_rate(stored: u32, startup_default: u32) -> u32 {
    if stored > 0 {
        stored
    } else {
        startup_default
    }
}

/// Mix rate of the device `device_name` (`None` = system default), as cpal reports it.
fn probe_mix_rate(device_name: Option<&str>) -> Option<u32> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let device = device_name
        .and_then(crate::dev_io::resolve_output_device)
        .or_else(|| {
            #[cfg(target_os = "linux")]
            if let Some(d) = crate::dev_io::resolve_output_device("pipewire")
                .or_else(|| crate::dev_io::resolve_output_device("pulse"))
            {
                return Some(d);
            }
            rodio::cpal::default_host().default_output_device()
        })?;
    device
        .default_output_config()
        .ok()
        .map(|c| c.sample_rate())
        .filter(|&r| r > 0)
}

/// Re-probe the device's mix rate and remember it. `None` when the fork engine is off
/// (nothing is stored) or the probe failed.
pub fn refresh_mix_rate(device_name: Option<&str>) -> Option<u32> {
    // The device (re)opens: forget what we learned about the old one (exclusive caps, sticky fallbacks).
    crate::fork_output::on_device_switch();
    refresh_mix_rate_keep_state(device_name)
}

/// [`refresh_mix_rate`] without touching the per-device exclusive state.
fn refresh_mix_rate_keep_state(device_name: Option<&str>) -> Option<u32> {
    if !enabled() {
        return None;
    }
    let rate = probe_mix_rate(device_name)?;
    MIX_RATE.store(rate, Ordering::Relaxed);
    Some(rate)
}

/// Mix rate for planning. Probes lazily the first time (start-up has no device selection yet).
fn mix_rate(state: &AudioEngine) -> u32 {
    if MIX_RATE.load(Ordering::Relaxed) == 0 {
        let dev = state
            .selected_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        refresh_mix_rate_keep_state(dev.as_deref());
    }
    effective_mix_rate(MIX_RATE.load(Ordering::Relaxed), state.device_default_rate)
}

/// True while varispeed would make the source report a scaled sample rate.
pub fn varispeed_active(rate: &crate::playback_rate::PlaybackRateAtomics) -> bool {
    crate::playback_rate::is_effect_active(rate)
        && rate.load_strategy() == crate::playback_rate::STRATEGY_VARISPEED
}

/// Stream rate for the track about to play (`audio_play` hook); `None` = fork engine off.
pub fn plan_target(
    state: &AudioEngine,
    track_hz: u32,
    crossfade_local: bool,
    gapless: bool,
    manual: bool,
) -> Option<u32> {
    let cfg = config();
    if !cfg.enabled {
        return None;
    }
    let stream = state.stream_sample_rate.load(Ordering::Relaxed);
    let blends = blends_enabled(
        state.crossfade_enabled.load(Ordering::Relaxed),
        state.gapless_enabled.load(Ordering::Relaxed),
        state.autodj_suppress_autocrossfade.load(Ordering::Relaxed),
    );
    let sink_playing = state
        .current
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sink
        .as_ref()
        .is_some_and(|s| !s.empty() && !s.is_paused());
    let transition = derive_transition(stream > 0, sink_playing, crossfade_local, gapless, manual);
    let device = state
        .selected_device
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let exclusive = crate::fork_output::planning_caps(&cfg, device.as_deref());
    plan_target_pure(
        &cfg,
        mix_rate(state),
        stream,
        track_hz,
        transition,
        blends,
        &queue_rates(),
        exclusive.as_ref(),
    )
}

/// Format of the open stream when the fork path should queue the track; `None` when the
/// engine is off, varispeed is active, or no stream is open (upstream path).
pub fn stream_format(state: &AudioEngine) -> Option<(ChannelCount, SampleRate)> {
    if !enabled() || varispeed_active(&state.playback_rate) {
        return None;
    }
    let guard = state
        .stream_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let config = *guard.as_ref()?.config();
    Some((config.channel_count(), config.sample_rate()))
}

/// The first track of a new `Player`: priming silence, then the HQ-resampled track.
pub fn first_source<S: Source>(
    source: S,
    fmt: (ChannelCount, SampleRate),
) -> impl Source<Item = S::Item> {
    psysonic_fork_hires::hq_resample::primed_for_output(crc_tapped(source), fmt.0, fmt.1)
}

/// A chained (gapless) track: HQ-resampled at the stream format, no priming (the queue is
/// already running).
pub fn chained_source<S: Source>(
    source: S,
    fmt: (ChannelCount, SampleRate),
) -> impl Source<Item = S::Item> {
    psysonic_fork_hires::hq_resample::for_output(crc_tapped(source), fmt.0, fmt.1)
}

/// The upstream chain's output wrapped in the bit-perfect check's tap, ahead of the HQ stage. A pure
/// pass-through (no sample changes) that does nothing unless `PSYSONIC_FORK_CRC_TAP=1`.
fn crc_tapped<S: Source>(source: S) -> psysonic_fork_hires::exclusive::crc_tap::CrcTapSource<S> {
    start_crc_logger();
    psysonic_fork_hires::exclusive::crc_tap::CrcTapSource::new(source)
}

/// With the check on, a background thread logs each track's final result once.
fn start_crc_logger() {
    use psysonic_fork_hires::exclusive::crc_tap;
    static STARTED: std::sync::Once = std::sync::Once::new();
    if !crc_tap::enabled() {
        return;
    }
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("fork-crc-log".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                while let Some((seq, r)) = crc_tap::board().take_unreported() {
                    crate::app_deprintln!(
                        "[fork-hires] bit-perfect check, track #{seq}: {}",
                        r.describe()
                    );
                }
            });
        if let Err(e) = spawned {
            crate::app_deprintln!("[fork-hires] bit-perfect check logger not started: {e}");
        }
    });
}

/// The newest track's check result for the status line; `None` when the check is off.
pub fn bit_perfect_check_text() -> Option<String> {
    use psysonic_fork_hires::exclusive::crc_tap;
    if !crc_tap::enabled() {
        return None;
    }
    Some(
        crc_tap::board()
            .latest()
            .map_or_else(|| "pending".to_string(), |(_, r)| r.describe()),
    )
}

/// Whether the open stream is above 48 kHz while the fork engine is on. Restores the
/// pre-fill that the legacy hi-res path gave such streams.
pub fn high_rate_stream(state: &AudioEngine) -> bool {
    enabled() && state.stream_sample_rate.load(Ordering::Relaxed) > 48_000
}

/// What the open output adds to the format payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputInfo {
    pub exclusive: bool,
    pub fallback_reason: Option<String>,
    pub bit_perfect: bool,
}

/// Payload for a stream of `fmt` playing a track of `native_hz`.
pub fn format_payload(
    native_hz: u32,
    fmt: (ChannelCount, SampleRate),
    policy: ForkPolicy,
    info: OutputInfo,
) -> ForkOutputFormat {
    let rate_hz = fmt.1.get();
    ForkOutputFormat {
        rate_hz,
        channels: fmt.0.get(),
        resampled_from_hz: (native_hz != rate_hz).then_some(native_hz),
        policy,
        backend: if info.exclusive {
            "exclusive"
        } else {
            "shared"
        }
        .to_string(),
        fallback_reason: info.fallback_reason,
        bit_perfect: info.exclusive && info.bit_perfect,
    }
}

fn output_info(app: &tauri::AppHandle) -> OutputInfo {
    let handle = app
        .try_state::<AudioEngine>()
        .and_then(|e| e.stream_handle.try_lock().ok().and_then(|g| g.clone()));
    OutputInfo {
        exclusive: handle.as_ref().is_some_and(|h| h.is_exclusive()),
        fallback_reason: crate::fork_output::last_fallback_reason(),
        bit_perfect: handle.as_ref().is_some_and(|h| h.bit_perfect()),
    }
}

/// Remember and emit `fork:audio-output-format`.
pub fn announce(app: &tauri::AppHandle, native_hz: u32, fmt: (ChannelCount, SampleRate)) {
    let payload = format_payload(native_hz, fmt, config().policy, output_info(app));
    *LAST_FORMAT.lock().unwrap_or_else(|e| e.into_inner()) = Some(payload.clone());
    let _ = app.emit("fork:audio-output-format", payload);
}

// ---- exclusive backend glue ---------------------------------------------------------------

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Remembered at start-up so the gate evaluator and the failure handler can reach the engine.
pub fn set_app_handle(app: tauri::AppHandle) {
    let _ = APP.set(app);
}

pub(crate) fn app_handle() -> Option<&'static tauri::AppHandle> {
    APP.get()
}

/// The last device-rate probe (0 = none yet).
pub(crate) fn stored_mix_rate() -> u32 {
    MIX_RATE.load(Ordering::Relaxed)
}

/// Engine values the bit-perfect gate depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineGateValues {
    pub base_volume: f32,
    /// Volume of the current `Player`; `None` when there is none (counts as not unity).
    pub player_volume: Option<f32>,
    pub replay_gain_linear: f32,
    pub normalization_engine: u32,
    pub eq_enabled: bool,
    pub speed_effect_active: bool,
    pub fading_out: bool,
    pub preview: bool,
    pub radio: bool,
    pub passthrough: bool,
}

/// Maps engine values to the gate inputs. Volume is unity only when the user volume is exactly 1.0 and the
/// `Player` sits at exactly the master headroom (no gain applied on top).
pub fn gate_inputs_from(v: &EngineGateValues) -> GateInputs {
    GateInputs {
        volume_is_unity: v.base_volume == 1.0
            && v.player_volume == Some(crate::helpers::MASTER_HEADROOM),
        normalization_off: v.replay_gain_linear == 1.0 && v.normalization_engine == 0,
        eq_off: !v.eq_enabled,
        speed_is_unity: !v.speed_effect_active,
        no_blend_or_extra_players: !v.fading_out && !v.preview && !v.radio,
        rate_matches: v.passthrough,
    }
}

/// Reads the engine without blocking: any contended lock yields `None` (gate off).
pub fn engine_gate_inputs(engine: &AudioEngine) -> Option<GateInputs> {
    let (base_volume, replay_gain_linear, player_volume) = {
        let cur = engine.current.try_lock().ok()?;
        (
            cur.base_volume,
            cur.replay_gain_linear,
            cur.sink.as_ref().map(|s| s.volume()),
        )
    };
    let fading_out = engine.fading_out_sink.try_lock().ok()?.is_some();
    let preview = engine.preview_sink.try_lock().ok()?.is_some();
    let radio = engine.radio_state.try_lock().ok()?.is_some();
    Some(gate_inputs_from(&EngineGateValues {
        base_volume,
        player_volume,
        replay_gain_linear,
        normalization_engine: engine.normalization_engine.load(Ordering::Relaxed),
        eq_enabled: engine.eq_enabled.load(Ordering::Relaxed),
        speed_effect_active: crate::playback_rate::is_effect_active(&engine.playback_rate),
        fading_out,
        preview,
        radio,
        passthrough: PASSTHROUGH_ACTIVE.load(Ordering::Relaxed),
    }))
}

/// Apply and persist the config.
#[tauri::command]
#[specta::specta]
pub fn fork_hires_set_config(app: tauri::AppHandle, config: ForkHiResConfig) -> Result<(), String> {
    apply_config(config.clone());
    let file = config_file(&app).ok_or("no app data folder")?;
    save_config_to(&file, &config)
}

/// Decoded rates of the upcoming queue (store only).
#[tauri::command]
#[specta::specta]
pub fn fork_hires_set_queue_rates(rates: Vec<u32>) {
    *QUEUE_RATES.lock().unwrap_or_else(|e| e.into_inner()) = rates;
}

#[tauri::command]
#[specta::specta]
pub fn fork_hires_status(state: State<'_, AudioEngine>) -> ForkHiResStatus {
    let cfg = config();
    let stream = state.stream_sample_rate.load(Ordering::Relaxed);
    let handle = state
        .stream_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let exclusive = handle.as_ref().and_then(|h| h.exclusive_status());
    ForkHiResStatus {
        config: cfg.clone(),
        backend: if exclusive.is_some() {
            ForkBackend::Exclusive
        } else {
            ForkBackend::Shared
        },
        bit_perfect: handle.as_ref().is_some_and(|h| h.bit_perfect()),
        exclusive,
        last_fallback_reason: crate::fork_output::last_fallback_reason(),
        enabled: cfg.enabled,
        policy: cfg.policy,
        ceiling_hz: effective_ceiling_hz(&cfg),
        stream_rate_hz: (stream > 0).then_some(stream),
        mix_rate_hz: effective_mix_rate(
            MIX_RATE.load(Ordering::Relaxed),
            state.device_default_rate,
        ),
        last_format: LAST_FORMAT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    }
}

#[cfg(test)]
#[path = "fork_hires_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fork_chain_identity_tests.rs"]
mod chain_identity_tests;
