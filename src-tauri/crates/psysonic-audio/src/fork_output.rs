//! Fork: output-handle abstraction over the shared (cpal/rodio) stream and the WASAPI exclusive backend.
//!
//! `AudioEngine` stores an [`OutputHandle`] instead of a bare `Arc<rodio::MixerDeviceSink>`. It exposes the same
//! calls the engine makes on the sink (`mixer()`, `config().channel_count()`, `config().sample_rate()`), so
//! existing call sites compile unchanged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use psysonic_fork_hires::exclusive::gate::{bit_perfect, GateInputs};
use psysonic_fork_hires::exclusive::{
    now_ms, probe_exclusive_caps, ExclusiveCaps, ExclusiveOpenConfig, ExclusiveOutput, GateState,
    DEFAULT_PERIOD_HNS,
};
use psysonic_fork_hires::format::OutputFormat;
use psysonic_fork_hires::rate_plan::DeviceCaps;
use rodio::{ChannelCount, SampleRate};
use tauri::Manager;

use crate::fork_hires::{ForkBackend, ForkExclusiveStatus, ForkHiResConfig};

/// Channel count and sample rate of an open output. `Copy`, with the accessor names and types of
/// `rodio::stream::DeviceSinkConfig`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputConfig {
    channels: ChannelCount,
    rate: SampleRate,
}

impl OutputConfig {
    pub fn new(channels: ChannelCount, rate: SampleRate) -> Self {
        Self { channels, rate }
    }

    pub fn channel_count(&self) -> ChannelCount {
        self.channels
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.rate
    }
}

impl From<&rodio::stream::DeviceSinkConfig> for OutputConfig {
    fn from(c: &rodio::stream::DeviceSinkConfig) -> Self {
        Self::new(c.channel_count(), c.sample_rate())
    }
}

#[derive(Clone)]
enum Kind {
    Cpal(Arc<rodio::MixerDeviceSink>),
    /// `ExclusiveOutput` is `Send + Sync`. The mixer is a cheap clone kept alongside so `mixer()` can return a
    /// plain reference. The gate is shared with the render thread (published by the gate evaluator).
    Exclusive {
        out: Arc<ExclusiveOutput>,
        mixer: rodio::mixer::Mixer,
        gate: Arc<GateState>,
    },
}

/// An open audio output. Cheap to clone; the device is released when the last clone drops.
#[derive(Clone)]
pub struct OutputHandle {
    cfg: OutputConfig,
    kind: Kind,
}

/// How old a gate heartbeat may be for [`OutputHandle::bit_perfect`] to still count it.
const GATE_READ_MAX_AGE_MS: u64 = 100;

impl OutputHandle {
    pub fn mixer(&self) -> &rodio::mixer::Mixer {
        match &self.kind {
            Kind::Cpal(s) => s.mixer(),
            Kind::Exclusive { mixer, .. } => mixer,
        }
    }

    pub fn config(&self) -> &OutputConfig {
        &self.cfg
    }

    pub fn is_exclusive(&self) -> bool {
        matches!(self.kind, Kind::Exclusive { .. })
    }

    /// The bit-perfect gate right now; always false for a shared stream.
    pub fn bit_perfect(&self) -> bool {
        match &self.kind {
            Kind::Cpal(_) => false,
            Kind::Exclusive { gate, .. } => gate.read(now_ms(), GATE_READ_MAX_AGE_MS),
        }
    }

    /// Live format and counters of an exclusive stream.
    pub fn exclusive_status(&self) -> Option<ForkExclusiveStatus> {
        let Kind::Exclusive { out, .. } = &self.kind else {
            return None;
        };
        let fmt = out.format();
        let st = out.stats();
        let sat = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        Some(ForkExclusiveStatus {
            format: format_name(fmt).to_string(),
            valid_bits: u32::from(fmt.valid_bits()),
            rate_hz: out.rate_hz(),
            periods: sat(st.periods),
            late_wakes: sat(st.late_wakes),
            max_gap_us: sat(st.max_gap_us),
            timeouts: sat(st.timeouts),
            last_error: out.last_error(),
            bit_perfect_check: crate::fork_hires::bit_perfect_check_text(),
        })
    }

    /// Wraps an exclusive stream. `None` if its format is degenerate (zero rate or channels).
    pub(crate) fn from_exclusive(out: ExclusiveOutput, gate: Arc<GateState>) -> Option<Self> {
        let cfg = OutputConfig::new(
            ChannelCount::new(out.channels())?,
            SampleRate::new(out.rate_hz())?,
        );
        let mixer = out.mixer().clone();
        Some(Self {
            cfg,
            kind: Kind::Exclusive {
                out: Arc::new(out),
                mixer,
                gate,
            },
        })
    }

    fn exclusive_parts(&self) -> Option<(Weak<ExclusiveOutput>, Arc<GateState>)> {
        match &self.kind {
            Kind::Cpal(_) => None,
            Kind::Exclusive { out, gate, .. } => Some((Arc::downgrade(out), gate.clone())),
        }
    }

    #[cfg(test)]
    fn from_cpal_with_config(sink: Arc<rodio::MixerDeviceSink>, cfg: OutputConfig) -> Self {
        Self {
            cfg,
            kind: Kind::Cpal(sink),
        }
    }
}

impl From<Arc<rodio::MixerDeviceSink>> for OutputHandle {
    fn from(sink: Arc<rodio::MixerDeviceSink>) -> Self {
        Self {
            cfg: OutputConfig::from(sink.config()),
            kind: Kind::Cpal(sink),
        }
    }
}

fn format_name(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::F32 => "F32",
        OutputFormat::S16 => "S16",
        OutputFormat::S24Packed => "S24",
        OutputFormat::S32 { .. } => "S32",
    }
}

// ---- sticky fallbacks --------------------------------------------------------------------

/// A device that fails twice while playing is given up on.
const MAX_RUNTIME_FAILURES: u32 = 2;

/// What to do about a stream that failed while playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureAction {
    /// Try exclusive mode once more on the reopen.
    Retry,
    /// Stay in shared mode for this device.
    FallBack,
}

/// Per-device record of why exclusive mode was given up. Keys are device keys (`""` = system default).
#[derive(Default)]
pub(crate) struct FallbackState {
    sticky: HashMap<String, String>,
    runtime_failures: HashMap<String, u32>,
    last_reason: Option<String>,
}

impl FallbackState {
    pub(crate) fn is_sticky(&self, key: &str) -> bool {
        self.sticky.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn sticky_reason(&self, key: &str) -> Option<&str> {
        self.sticky.get(key).map(String::as_str)
    }

    pub(crate) fn last_reason(&self) -> Option<String> {
        self.last_reason.clone()
    }

    /// Open-time failure: stay in shared mode for this device until the state is cleared.
    pub(crate) fn set_sticky(&mut self, key: &str, reason: String) {
        self.last_reason = Some(reason.clone());
        self.sticky.insert(key.to_string(), reason);
    }

    /// A stream failed while playing. The first failure earns one more try, the second makes it sticky.
    pub(crate) fn record_runtime_failure(&mut self, key: &str, reason: String) -> FailureAction {
        let n = self.runtime_failures.entry(key.to_string()).or_insert(0);
        *n += 1;
        if *n >= MAX_RUNTIME_FAILURES {
            self.set_sticky(key, format!("failed while playing: {reason}"));
            FailureAction::FallBack
        } else {
            self.last_reason = Some(format!("failed while playing: {reason}"));
            FailureAction::Retry
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

static FALLBACKS: LazyLock<Mutex<FallbackState>> = LazyLock::new(Mutex::default);
/// Probed exclusive capabilities per device key (`None` = the probe failed).
static CAPS: LazyLock<Mutex<HashMap<String, Option<ExclusiveCaps>>>> =
    LazyLock::new(Mutex::default);
/// True while a runtime-failure reopen is in flight, so its `refresh_mix_rate` call does not wipe the
/// failure bookkeeping.
static FAILURE_REOPEN: AtomicBool = AtomicBool::new(false);

fn device_key(device_name: Option<&str>) -> &str {
    device_name.unwrap_or("")
}

/// Forget the sticky fallbacks, failure counts and probed caps (config changed).
pub(crate) fn clear_device_state() {
    FALLBACKS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    CAPS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The output device is being switched or reopened (hook of `refresh_mix_rate`).
pub(crate) fn on_device_switch() {
    if !FAILURE_REOPEN.load(Ordering::Relaxed) {
        clear_device_state();
    }
}

/// Why exclusive mode was last abandoned for shared mode, if it was.
pub(crate) fn last_fallback_reason() -> Option<String> {
    FALLBACKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_reason()
}

fn is_sticky(key: &str) -> bool {
    FALLBACKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_sticky(key)
}

/// Whether the exclusive backend is wanted for a device: fork on, backend exclusive, not given up on.
pub(crate) fn should_try_exclusive(cfg: &ForkHiResConfig, sticky: bool) -> bool {
    cfg.enabled && cfg.backend == ForkBackend::Exclusive && !sticky
}

// ---- endpoint and caps -------------------------------------------------------------------

/// The WASAPI endpoint id of a device key (`None` = the default render endpoint). `None` when the device
/// cannot be found, or off Windows. The id is always explicit, so the endpoint registry never confuses the
/// default device with an explicit one.
fn resolve_endpoint_id(device_name: Option<&str>) -> Option<String> {
    #[cfg(windows)]
    {
        use rodio::cpal::traits::{DeviceTrait, HostTrait};
        let device = match device_name {
            Some(name) => crate::dev_io::resolve_output_device(name)?,
            None => rodio::cpal::default_host().default_output_device()?,
        };
        let id = device.id().ok()?.to_string();
        let id = id.strip_prefix("wasapi:").unwrap_or(&id);
        (!id.is_empty()).then(|| id.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = device_name;
        None
    }
}

/// Exclusive-mode capabilities of a device (`""` = default), probed on first use (about 40 ms) and cached
/// until the device or config changes. `None` when the probe failed.
pub(crate) fn exclusive_caps(key: &str) -> Option<ExclusiveCaps> {
    if let Some(hit) = CAPS.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return hit.clone();
    }
    let device_name = (!key.is_empty()).then_some(key);
    let caps = resolve_endpoint_id(device_name).and_then(|id| probe_exclusive_caps(Some(&id)).ok());
    CAPS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string(), caps.clone());
    caps
}

/// The planner's device caps when the exclusive backend should be planned for; `None` plans for shared mode.
pub(crate) fn planning_caps(
    cfg: &ForkHiResConfig,
    device_name: Option<&str>,
) -> Option<DeviceCaps> {
    let key = device_key(device_name);
    if !should_try_exclusive(cfg, is_sticky(key)) {
        return None;
    }
    exclusive_caps(key)?.device_caps()
}

/// Rate for the shared-mode open that follows a failed exclusive attempt: the device mix rate rather than a
/// rate only exclusive mode can do. Unchanged when exclusive is not in play.
pub(crate) fn shared_fallback_rate(device_name: Option<&str>, desired_rate: u32) -> u32 {
    let cfg = crate::fork_hires::config();
    let mix = crate::fork_hires::stored_mix_rate();
    if cfg.enabled
        && cfg.backend == ForkBackend::Exclusive
        && mix > 0
        && is_sticky(device_key(device_name))
    {
        mix
    } else {
        desired_rate
    }
}

// ---- gate evaluator ----------------------------------------------------------------------

/// Yields the current gate inputs; `None` (unreadable or contended) turns the gate off.
pub(crate) type GateSource = Box<dyn Fn() -> Option<GateInputs> + Send + 'static>;

/// How often the evaluator republishes the gate.
const GATE_TICK: Duration = Duration::from_millis(10);

/// Gate source backed by the app's `AudioEngine`.
fn engine_gate_source() -> GateSource {
    Box::new(|| {
        let app = crate::fork_hires::app_handle()?;
        let engine = app.try_state::<crate::engine::AudioEngine>()?;
        crate::fork_hires::engine_gate_inputs(&engine)
    })
}

/// Publishes the bit-perfect gate every tick until the output drops. On a runtime failure it publishes
/// false, calls `on_failure` once with the reason and ends.
fn spawn_gate_evaluator(
    out: Weak<ExclusiveOutput>,
    gate: Arc<GateState>,
    source: GateSource,
    on_failure: impl FnOnce(String) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("fork-gate".into())
        .spawn(move || loop {
            std::thread::sleep(GATE_TICK);
            let Some(output) = out.upgrade() else {
                return;
            };
            if output.failed() {
                let reason = output
                    .last_error()
                    .unwrap_or_else(|| "output stopped".to_string());
                drop(output);
                gate.publish(false, now_ms());
                on_failure(reason);
                return;
            }
            drop(output);
            let on = source().is_some_and(|i| bit_perfect(&i));
            gate.publish(on, now_ms());
        });
    if let Err(e) = spawned {
        crate::app_eprintln!("[psysonic] exclusive gate evaluator did not start: {e}");
    }
}

/// An exclusive stream died while playing: count it, and reopen once through the device-watcher path (which
/// replays the track on the rebuilt output). The second failure on a device makes it fall back to shared.
fn handle_runtime_failure(key: String, reason: String) {
    let action = FALLBACKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record_runtime_failure(&key, reason.clone());
    crate::app_eprintln!("[psysonic] exclusive output failed while playing ({reason}); {action:?}");
    let Some(app) = crate::fork_hires::app_handle().cloned() else {
        return;
    };
    let device = app
        .try_state::<crate::engine::AudioEngine>()
        .and_then(|e| e.selected_device.lock().ok().map(|d| d.clone()))
        .unwrap_or(None);
    FAILURE_REOPEN.store(true, Ordering::Relaxed);
    tauri::async_runtime::spawn(async move {
        let r = crate::device_watcher::reopen_output_stream_with_retry(
            &app,
            device,
            crate::device_watcher::ReopenNotify::DeviceChanged,
        )
        .await;
        FAILURE_REOPEN.store(false, Ordering::Relaxed);
        if let Err(e) = r {
            crate::app_eprintln!("[psysonic] reopen after exclusive failure failed: {e}");
        }
    });
}

// ---- open --------------------------------------------------------------------------------

/// Opens the WASAPI exclusive backend when enabled and possible. Returns `None` (shared mode) when the fork
/// engine is off, the backend is shared, the device was given up on, or the open failed (which makes the
/// device sticky-fallback).
pub(crate) fn try_open_exclusive(
    device_name: Option<&str>,
    desired_rate: u32,
) -> Option<OutputHandle> {
    open_exclusive_with(
        &crate::fork_hires::config(),
        &FALLBACKS,
        device_name,
        desired_rate,
        engine_gate_source,
        |key, reason| handle_runtime_failure(key.to_string(), reason),
    )
}

/// [`try_open_exclusive`] with its inputs injected: the config, the fallback state, where the gate inputs
/// come from and what to do on a runtime failure.
pub(crate) fn open_exclusive_with(
    cfg: &ForkHiResConfig,
    fallbacks: &Mutex<FallbackState>,
    device_name: Option<&str>,
    desired_rate: u32,
    source: impl FnOnce() -> GateSource,
    on_failure: impl FnOnce(&str, String) + Send + 'static,
) -> Option<OutputHandle> {
    let key = device_key(device_name);
    let sticky = fallbacks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_sticky(key);
    if !should_try_exclusive(cfg, sticky) || desired_rate == 0 {
        return None;
    }
    let fail = |reason: String| {
        crate::app_eprintln!(
            "[psysonic] exclusive output unavailable, using shared mode: {reason}"
        );
        fallbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_sticky(key, reason);
    };
    let Some(endpoint_id) = resolve_endpoint_id(device_name) else {
        // Not an exclusive failure: the device is missing (or this is not Windows); the shared path decides.
        return None;
    };
    // Snap to a rate the device accepts (the planner picks from the caps, other callers may not).
    let rate_hz = exclusive_caps(key)
        .and_then(|c| c.device_caps())
        .map_or(desired_rate, |c| c.at_or_above(desired_rate));
    let gate = Arc::new(GateState::new());
    let open = ExclusiveOutput::open(
        ExclusiveOpenConfig {
            endpoint_id: Some(endpoint_id),
            rate_hz,
            channels: 2,
            source_bits: 24,
            period_hns: DEFAULT_PERIOD_HNS,
            lead_in_ms: cfg.resync_silence_ms,
        },
        gate.clone(),
    );
    let out = match open {
        Ok(out) => out,
        Err(e) => {
            fail(e.to_string());
            return None;
        }
    };
    let Some(handle) = OutputHandle::from_exclusive(out, gate) else {
        fail("exclusive output reported an unusable format".to_string());
        return None;
    };
    if let Some((weak, gate)) = handle.exclusive_parts() {
        let key = key.to_string();
        spawn_gate_evaluator(weak, gate, source(), move |reason| on_failure(&key, reason));
    }
    Some(handle)
}

#[cfg(test)]
#[path = "fork_output_tests.rs"]
mod tests;
