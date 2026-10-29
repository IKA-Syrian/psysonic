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
use std::sync::{LazyLock, Mutex, RwLock};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ForkHiResConfig {
    pub enabled: bool,
    pub policy: ForkPolicy,
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
    /// Always `"shared"` for now.
    pub backend: String,
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
/// `stream_sample_rate == 0` means the stream is closed.
pub fn plan_target_pure(
    cfg: &ForkHiResConfig,
    mix_rate_hz: u32,
    stream_sample_rate: u32,
    track_hz: u32,
    transition: Transition,
    blends: bool,
    upcoming: &[u32],
) -> Option<u32> {
    if !cfg.enabled {
        return None;
    }
    let caps = DeviceCaps::new(vec![mix_rate_hz])?;
    let plan = plan_output_rate(&PlanInput {
        backend: Backend::Shared { mix_rate_hz },
        policy: cfg.policy.into(),
        ceiling_hz: effective_ceiling_hz(cfg),
        blends_enabled: blends,
        current_stream_hz: (stream_sample_rate > 0).then_some(stream_sample_rate),
        track_hz,
        transition,
        upcoming_hz: upcoming,
        caps: &caps,
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
        refresh_mix_rate(dev.as_deref());
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
    plan_target_pure(
        &cfg,
        mix_rate(state),
        stream,
        track_hz,
        transition,
        blends,
        &queue_rates(),
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
    psysonic_fork_hires::hq_resample::primed_for_output(source, fmt.0, fmt.1)
}

/// A chained (gapless) track: HQ-resampled at the stream format, no priming (the queue is
/// already running).
pub fn chained_source<S: Source>(
    source: S,
    fmt: (ChannelCount, SampleRate),
) -> impl Source<Item = S::Item> {
    psysonic_fork_hires::hq_resample::for_output(source, fmt.0, fmt.1)
}

/// Whether the open stream is above 48 kHz while the fork engine is on. Restores the
/// pre-fill that the legacy hi-res path gave such streams.
pub fn high_rate_stream(state: &AudioEngine) -> bool {
    enabled() && state.stream_sample_rate.load(Ordering::Relaxed) > 48_000
}

/// Payload for a stream of `fmt` playing a track of `native_hz`.
pub fn format_payload(
    native_hz: u32,
    fmt: (ChannelCount, SampleRate),
    policy: ForkPolicy,
) -> ForkOutputFormat {
    let rate_hz = fmt.1.get();
    ForkOutputFormat {
        rate_hz,
        channels: fmt.0.get(),
        resampled_from_hz: (native_hz != rate_hz).then_some(native_hz),
        policy,
        backend: "shared".to_string(),
    }
}

/// Remember and emit `fork:audio-output-format`.
pub fn announce(app: &tauri::AppHandle, native_hz: u32, fmt: (ChannelCount, SampleRate)) {
    let payload = format_payload(native_hz, fmt, config().policy);
    *LAST_FORMAT.lock().unwrap_or_else(|e| e.into_inner()) = Some(payload.clone());
    let _ = app.emit("fork:audio-output-format", payload);
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
    ForkHiResStatus {
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
