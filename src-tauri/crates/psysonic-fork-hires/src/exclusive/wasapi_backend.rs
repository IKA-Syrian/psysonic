//! Windows WASAPI exclusive-mode backend: a dedicated render thread owns the device (`AudioClient`,
//! `AudioRenderClient` and `Device` are `!Send`) and runs the cross-platform [`RenderLoop`] over a
//! [`WasapiSink`]. Callers only touch the `Send + Sync` handle, [`Backend`].

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use wasapi::{
    calculate_period_100ns, initialize_mta, AudioClient, AudioRenderClient, Device,
    DeviceEnumerator, Direction, Handle, SampleType, StreamMode, WasapiError, WaveFormat,
};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
    WaitForMultipleObjects,
};

use super::gate::GateState;
use super::lifecycle::{guarded, Lease, Registry, TimeoutTracker};
use super::negotiate::{negotiate, ExclusiveCaps};
use super::render::{
    RenderConfig, RenderLoop, RenderSink, RenderStats, SharedStats, SystemClock, WaitResult,
};
use super::{is_buffer_not_aligned, map_hresult, ExclusiveError, ExclusiveOpenConfig};
use crate::format::{DitherMode, OutputFormat};

const PROBE_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];
const PROBE_FORMATS: [OutputFormat; 4] = [
    OutputFormat::S32 { valid_bits: 24 },
    OutputFormat::S24Packed,
    OutputFormat::S16,
    OutputFormat::F32,
];
/// How long `open` waits for the render thread to report ready.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `drop` waits for the render thread to wind down.
const DROP_TIMEOUT: Duration = Duration::from_secs(2);
/// One wait for the device event; two in a row without it end the stream.
const WAIT_TIMEOUT_MS: u32 = 2_000;
const GATE_MAX_AGE_MS: u64 = 500;
/// How long `open` waits for another render thread in this process to release the same endpoint.
const REGISTRY_WAIT: Duration = Duration::from_millis(1_500);
/// Extra attempts after a `DeviceInUse`, and the pause before each.
const IN_USE_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(100);
/// Registry key for "the default render device" (not resolved to its endpoint id).
const DEFAULT_KEY: &str = "<default render device>";

fn registry() -> &'static Registry {
    static REGISTRY: Registry = Registry::new();
    &REGISTRY
}

/// State shared between the render thread and its owner.
#[derive(Default)]
struct Flags {
    stop: AtomicBool,
    failed: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Flags {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Marks the stream failed and keeps the first reason. Only on failure paths, never in steady state.
    fn fail(&self, reason: impl Into<String>) {
        self.record_error(reason);
        self.failed.store(true, Ordering::Release);
    }

    /// Keeps the first reason only; later errors are usually consequences of it.
    fn record_error(&self, reason: impl Into<String>) {
        let mut g = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(reason.into());
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn check_cancelled(flags: &Flags) -> Result<(), ExclusiveError> {
    if flags.stopping() {
        Err(ExclusiveError::Other("open cancelled".into()))
    } else {
        Ok(())
    }
}

fn map_wasapi(e: WasapiError, context: &str) -> ExclusiveError {
    match e {
        WasapiError::Windows(w) => map_hresult(w.code().0, context),
        WasapiError::UnsupportedFormat => ExclusiveError::UnsupportedFormat,
        WasapiError::DeviceNotFound(_) => ExclusiveError::DeviceNotFound,
        other => ExclusiveError::Other(format!("{context}: {other}")),
    }
}

fn wave_format(f: OutputFormat, rate_hz: u32, channels: u16) -> WaveFormat {
    let (store, valid, ty) = match f {
        OutputFormat::S32 { valid_bits } => (32, valid_bits as usize, SampleType::Int),
        OutputFormat::S24Packed => (24, 24, SampleType::Int),
        OutputFormat::S16 => (16, 16, SampleType::Int),
        OutputFormat::F32 => (32, 32, SampleType::Float),
    };
    WaveFormat::new(store, valid, &ty, rate_hz as usize, channels as usize, None)
}

fn get_device(enumerator: &DeviceEnumerator, id: Option<&str>) -> Result<Device, ExclusiveError> {
    match id {
        Some(id) => enumerator
            .get_device(id)
            .map_err(|e| map_wasapi(e, "get_device")),
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| map_wasapi(e, "get_default_device")),
    }
}

fn mta() -> Result<(), ExclusiveError> {
    initialize_mta()
        .ok()
        .map_err(|e| map_hresult(e.code().0, "initialize_mta"))
}

/// Formats and rates `client` accepts in exclusive mode for `channels`.
fn probe_channels(client: &AudioClient, channels: u16) -> ExclusiveCaps {
    let mut by_format = Vec::new();
    for f in PROBE_FORMATS {
        let rates: Vec<u32> = PROBE_RATES
            .iter()
            .copied()
            .filter(|&r| {
                client
                    .is_supported_exclusive_with_quirks(&wave_format(f, r, channels))
                    .is_ok()
            })
            .collect();
        if !rates.is_empty() {
            by_format.push((f, rates));
        }
    }
    ExclusiveCaps {
        channels,
        by_format,
    }
}

fn probe_on_this_thread(endpoint_id: Option<&str>) -> Result<ExclusiveCaps, ExclusiveError> {
    mta()?;
    let enumerator = DeviceEnumerator::new().map_err(|e| map_wasapi(e, "DeviceEnumerator::new"))?;
    let device = get_device(&enumerator, endpoint_id)?;
    let client = device
        .get_iaudioclient()
        .map_err(|e| map_wasapi(e, "get_iaudioclient"))?;
    let stereo = probe_channels(&client, 2);
    if !stereo.by_format.is_empty() {
        return Ok(stereo);
    }
    let mix_channels = client
        .get_mixformat()
        .map(|m| m.get_nchannels())
        .unwrap_or(2);
    if mix_channels != 2 && mix_channels > 0 {
        let caps = probe_channels(&client, mix_channels);
        if !caps.by_format.is_empty() {
            return Ok(caps);
        }
    }
    Err(ExclusiveError::UnsupportedFormat)
}

/// Probes on a short-lived thread, so COM initialisation never leaks into the caller's thread.
pub(super) fn probe(endpoint_id: Option<&str>) -> Result<ExclusiveCaps, ExclusiveError> {
    let id = endpoint_id.map(str::to_owned);
    std::thread::Builder::new()
        .name("hires-wasapi-probe".into())
        .spawn(move || {
            guarded(|| probe_on_this_thread(id.as_deref()))
                .unwrap_or_else(|m| Err(ExclusiveError::Other(format!("probe panicked: {m}"))))
        })
        .map_err(|e| ExclusiveError::Other(format!("spawn probe thread: {e}")))?
        .join()
        .unwrap_or_else(|_| Err(ExclusiveError::Other("probe thread panicked".into())))
}

/// A manual-reset Win32 event used to wake the render thread for shutdown. Shared through an `Arc`, so the
/// handle outlives a render thread that is slow to wind down.
struct StopEvent(HANDLE);

// SAFETY: a Win32 event handle may be used (set, waited on, closed) from any thread.
unsafe impl Send for StopEvent {}
unsafe impl Sync for StopEvent {}

impl StopEvent {
    fn new() -> Result<Self, ExclusiveError> {
        // SAFETY: plain FFI call with no pointers.
        unsafe { CreateEventW(None, true, false, None) }
            .map(Self)
            .map_err(|e| map_hresult(e.code().0, "CreateEventW"))
    }

    fn signal(&self) {
        // SAFETY: the handle is valid for the lifetime of `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateEventW` and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// What the render thread hands back once the stream is running.
struct Ready {
    mixer: rodio::mixer::Mixer,
    format: OutputFormat,
}

type ReadyTx = SyncSender<Result<Ready, ExclusiveError>>;

struct WasapiSink {
    client: AudioClient,
    render: AudioRenderClient,
    event: Handle,
    stop: Arc<StopEvent>,
    flags: Arc<Flags>,
    /// The first timed-out wait is reported as a timeout, the second in a row ends the stream.
    timeouts: TimeoutTracker,
    /// Frames of lead-in silence still to be written before `ready` is reported.
    lead_frames_left: usize,
    ready: Option<(ReadyTx, Ready)>,
}

/// `WAIT_OBJECT_0 + 1`: the second handle (the stop event) was signalled.
const HANDLE_STOP: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);

impl RenderSink for WasapiSink {
    fn wait(&mut self) -> WaitResult {
        if self.flags.stopping() || self.flags.failed.load(Ordering::Acquire) {
            return WaitResult::Stop;
        }
        let handles = [self.event.as_handle(), self.stop.0];
        // SAFETY: both handles are valid for the duration of the call.
        let r = unsafe { WaitForMultipleObjects(&handles, false, WAIT_TIMEOUT_MS) };
        if r == WAIT_OBJECT_0 {
            self.timeouts.on_ready();
            WaitResult::Ready
        } else if r == HANDLE_STOP || self.flags.stopping() {
            WaitResult::Stop
        } else if r == WAIT_TIMEOUT {
            if self.timeouts.on_timeout() {
                self.flags
                    .fail("the device stopped asking for data (two 2 s waits timed out)");
                WaitResult::Stop
            } else {
                WaitResult::Timeout
            }
        } else {
            self.flags.fail("waiting for the device event failed");
            WaitResult::Stop
        }
    }

    fn frames_available(&self) -> usize {
        match self.client.get_available_space_in_frames() {
            Ok(n) => n as usize,
            Err(e) => {
                // DEVICE_INVALIDATED and friends: the next `wait` ends the loop.
                self.flags
                    .fail(format!("get_available_space_in_frames: {e}"));
                0
            }
        }
    }

    fn write(&mut self, bytes: &[u8], frames: usize) -> Result<(), String> {
        self.render
            .write_to_device(frames, bytes, None)
            .map_err(|e| e.to_string())?;
        if self.ready.is_some() {
            self.lead_frames_left = self.lead_frames_left.saturating_sub(frames);
            if self.lead_frames_left == 0 {
                if let Some((tx, ready)) = self.ready.take() {
                    let _ = tx.send(Ok(ready));
                }
            }
        }
        Ok(())
    }
}

/// MMCSS registration for the render thread, reverted on drop.
struct Mmcss(Option<HANDLE>);

impl Mmcss {
    fn enter() -> Self {
        let mut task_index = 0u32;
        // SAFETY: `task_index` outlives the call; failure only means no priority boost.
        let h = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
        Self(h.ok())
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: `h` came from `AvSetMmThreadCharacteristicsW` on this thread.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

/// Everything the render thread needs to run, built on that thread.
struct Running {
    sink: WasapiSink,
    render_loop: RenderLoop<rodio::mixer::MixerSource, SystemClock>,
}

/// Initialises the client, retrying once with the device's suggested size on `BUFFER_SIZE_NOT_ALIGNED`.
fn initialize(
    device: &Device,
    mut client: AudioClient,
    fmt: &WaveFormat,
    period_hns: i64,
) -> Result<AudioClient, ExclusiveError> {
    let mode = StreamMode::EventsExclusive { period_hns };
    match client.initialize_client(fmt, &Direction::Render, &mode) {
        Ok(()) => Ok(client),
        Err(WasapiError::Windows(e)) if is_buffer_not_aligned(e.code().0) => {
            let frames = client
                .get_buffer_size()
                .map_err(|e| map_wasapi(e, "get_buffer_size"))?;
            let aligned = calculate_period_100ns(frames as i64, fmt.get_samplespersec() as i64);
            let mut fresh = device
                .get_iaudioclient()
                .map_err(|e| map_wasapi(e, "get_iaudioclient"))?;
            let mode = StreamMode::EventsExclusive {
                period_hns: aligned,
            };
            fresh
                .initialize_client(fmt, &Direction::Render, &mode)
                .map_err(|e| map_wasapi(e, "initialize_client (aligned retry)"))?;
            Ok(fresh)
        }
        Err(e) => Err(map_wasapi(e, "initialize_client")),
    }
}

fn setup(
    cfg: &ExclusiveOpenConfig,
    stop: &Arc<StopEvent>,
    flags: &Arc<Flags>,
    stats: &Arc<SharedStats>,
    ready_tx: &ReadyTx,
) -> Result<Running, ExclusiveError> {
    let (Some(ch_nz), Some(rate_nz)) = (NonZero::new(cfg.channels), NonZero::new(cfg.rate_hz))
    else {
        return Err(ExclusiveError::Other("zero channels or sample rate".into()));
    };
    check_cancelled(flags)?;
    let enumerator = DeviceEnumerator::new().map_err(|e| map_wasapi(e, "DeviceEnumerator::new"))?;
    let device = get_device(&enumerator, cfg.endpoint_id.as_deref())?;
    let probe_client = device
        .get_iaudioclient()
        .map_err(|e| map_wasapi(e, "get_iaudioclient"))?;

    // Probe the requested rate in every format, then let `negotiate` choose.
    let mut by_format = Vec::new();
    let mut accepted: Vec<(OutputFormat, WaveFormat)> = Vec::new();
    let mut last_err = None;
    for f in PROBE_FORMATS {
        match probe_client.is_supported_exclusive_with_quirks(&wave_format(
            f,
            cfg.rate_hz,
            cfg.channels,
        )) {
            Ok(wf) => {
                by_format.push((f, vec![cfg.rate_hz]));
                accepted.push((f, wf));
            }
            Err(e) => last_err = Some(e),
        }
    }
    let caps = ExclusiveCaps {
        channels: cfg.channels,
        by_format,
    };
    let Some(format) = negotiate(&caps, cfg.rate_hz, cfg.source_bits) else {
        // Surface a more specific reason (in use, not allowed) when the probe itself failed that way.
        return Err(
            match last_err.map(|e| map_wasapi(e, "is_supported_exclusive")) {
                Some(
                    e @ (ExclusiveError::DeviceInUse
                    | ExclusiveError::ExclusiveNotAllowed
                    | ExclusiveError::DeviceInvalidated),
                ) => e,
                _ => ExclusiveError::UnsupportedFormat,
            },
        );
    };
    let Some(fmt) = accepted
        .into_iter()
        .find(|(f, _)| *f == format)
        .map(|(_, w)| w)
    else {
        return Err(ExclusiveError::UnsupportedFormat);
    };
    let block_align = fmt.get_blockalign() as usize;
    if block_align != cfg.channels as usize * format.bytes_per_sample() {
        return Err(ExclusiveError::Other(format!(
            "unexpected block align {block_align} for {format:?}"
        )));
    }

    let (_default_period, min_period) = probe_client
        .get_device_period()
        .map_err(|e| map_wasapi(e, "get_device_period"))?;
    let wanted = cfg.period_hns.max(min_period);
    let period = probe_client
        .calculate_aligned_period_near(wanted, Some(128), &fmt)
        .unwrap_or(wanted);
    // The probing client has served its purpose; exclusive Initialize needs a fresh one.
    drop(probe_client);
    check_cancelled(flags)?;
    let client = device
        .get_iaudioclient()
        .map_err(|e| map_wasapi(e, "get_iaudioclient"))?;
    let client = initialize(&device, client, &fmt, period)?;
    check_cancelled(flags)?;

    let buffer_frames = client
        .get_buffer_size()
        .map_err(|e| map_wasapi(e, "get_buffer_size"))? as usize;
    let event = client
        .set_get_eventhandle()
        .map_err(|e| map_wasapi(e, "set_get_eventhandle"))?;
    let render = client
        .get_audiorenderclient()
        .map_err(|e| map_wasapi(e, "get_audiorenderclient"))?;

    // Prefill one buffer of silence so the first event finds the device fed.
    let silence = vec![0u8; buffer_frames * block_align];
    render
        .write_to_device(buffer_frames, &silence, None)
        .map_err(|e| map_wasapi(e, "prefill"))?;

    let (mixer, source) = rodio::mixer::mixer(ch_nz, rate_nz);
    let render_cfg = RenderConfig {
        channels: cfg.channels,
        rate_hz: cfg.rate_hz,
        format,
        source_bits: cfg.source_bits,
        dither: DitherMode::Auto,
        lead_in_ms: cfg.lead_in_ms,
        period_ms: (buffer_frames as u64 * 1000 / cfg.rate_hz as u64).max(1),
        max_frames: buffer_frames,
        gate_max_age_ms: GATE_MAX_AGE_MS,
        seed: super::now_us() ^ 0x9E37_79B9_7F4A_7C15,
    };
    let lead_frames_left = (cfg.lead_in_ms as u64 * cfg.rate_hz as u64 / 1000) as usize;
    let render_loop =
        RenderLoop::new(render_cfg, source, SystemClock::new()).with_shared_stats(stats.clone());

    // A caller-side open timeout must not leave a started stream that nobody owns.
    check_cancelled(flags)?;
    client
        .start_stream()
        .map_err(|e| map_wasapi(e, "start_stream"))?;

    Ok(Running {
        sink: WasapiSink {
            client,
            render,
            event,
            stop: stop.clone(),
            flags: flags.clone(),
            timeouts: TimeoutTracker::default(),
            lead_frames_left,
            ready: Some((ready_tx.clone(), Ready { mixer, format })),
        },
        render_loop,
    })
}

/// The render thread's work, run under `catch_unwind`. All COM objects are dropped before it returns, or
/// while unwinding.
fn run_stream(
    cfg: &ExclusiveOpenConfig,
    gate: &GateState,
    stop: &Arc<StopEvent>,
    flags: &Arc<Flags>,
    stats: &Arc<SharedStats>,
    ready_tx: &ReadyTx,
) {
    let com = mta();
    let _mmcss = com.is_ok().then(Mmcss::enter);
    let running = com.and_then(|()| setup(cfg, stop, flags, stats, ready_tx));
    match running {
        Err(e) => {
            if !flags.stopping() {
                flags.record_error(e.to_string());
            }
            let _ = ready_tx.send(Err(e));
        }
        Ok(Running {
            mut sink,
            mut render_loop,
        }) => {
            let result = render_loop.run(&mut sink, gate);
            if let Err(msg) = &result {
                if !flags.stopping() {
                    flags.fail(format!("write failed: {msg}"));
                }
            }
            if let Some((tx, _)) = sink.ready.take() {
                // Ended before the lead-in finished.
                let reason = flags
                    .last_error()
                    .unwrap_or_else(|| "stream ended during start-up".into());
                let _ = tx.send(Err(ExclusiveError::Other(reason)));
            }
            let _ = sink.client.stop_stream();
            // `sink` (render client, event, client) and the loop drop here, releasing the device.
        }
    }
}

struct ThreadArgs {
    cfg: ExclusiveOpenConfig,
    gate: Arc<GateState>,
    stop: Arc<StopEvent>,
    flags: Arc<Flags>,
    stats: Arc<SharedStats>,
    ready_tx: ReadyTx,
    done_tx: SyncSender<()>,
    /// Holds the endpoint in the registry for as long as this thread lives, detached or not.
    lease: Lease,
}

fn render_thread(a: ThreadArgs) {
    let ThreadArgs {
        cfg,
        gate,
        stop,
        flags,
        stats,
        ready_tx,
        done_tx,
        lease,
    } = a;
    let outcome = guarded(|| run_stream(&cfg, &gate, &stop, &flags, &stats, &ready_tx));
    if let Err(msg) = outcome {
        let reason = format!("render thread panicked: {msg}");
        flags.fail(reason.clone());
        let _ = ready_tx.send(Err(ExclusiveError::Other(reason)));
    }
    // The device is released by now; free the endpoint, then tell the owner.
    drop(lease);
    let _ = done_tx.send(());
}

/// Stops the render thread with a bounded wait; also used to clean up a failed `open`.
struct ThreadCtl {
    stop: Arc<StopEvent>,
    flags: Arc<Flags>,
    /// Behind a `Mutex` only so the handle is `Sync`; it is touched solely from `shutdown`.
    done_rx: Mutex<Receiver<()>>,
    handle: Option<JoinHandle<()>>,
}

impl ThreadCtl {
    fn shutdown(&mut self) {
        self.flags.stop.store(true, Ordering::Release);
        self.stop.signal();
        if let Some(handle) = self.handle.take() {
            let rx = self.done_rx.get_mut().unwrap_or_else(|e| e.into_inner());
            if rx.recv_timeout(DROP_TIMEOUT).is_ok() {
                let _ = handle.join();
            }
            // Otherwise the thread is detached. It still owns its endpoint lease and an `Arc` of the stop
            // event, so a reopen of the same device waits for it and nothing dangles.
        }
    }
}

impl Drop for ThreadCtl {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) struct Backend {
    // Field order matters: the thread is stopped first (see `Drop for ThreadCtl`) when the struct drops.
    _ctl: ThreadCtl,
    mixer: rodio::mixer::Mixer,
    format: OutputFormat,
    rate_hz: u32,
    channels: u16,
    stats: Arc<SharedStats>,
    flags: Arc<Flags>,
}

impl Backend {
    /// Opens the endpoint. An endpoint still held by a render thread of this process is waited for; a
    /// `DeviceInUse` from the device itself is retried a few times, since a just-released exclusive stream
    /// can take a moment to free the device.
    pub(super) fn open(
        cfg: ExclusiveOpenConfig,
        gate: Arc<GateState>,
    ) -> Result<Self, ExclusiveError> {
        let key = cfg
            .endpoint_id
            .clone()
            .unwrap_or_else(|| DEFAULT_KEY.to_owned());
        let mut last = ExclusiveError::DeviceInUse;
        for attempt in 0..=IN_USE_RETRIES {
            let wait = if attempt == 0 {
                REGISTRY_WAIT
            } else {
                RETRY_DELAY
            };
            match registry().acquire(&key, wait) {
                None => last = ExclusiveError::DeviceInUse,
                Some(lease) => match Self::try_open(cfg.clone(), gate.clone(), lease) {
                    Ok(b) => return Ok(b),
                    Err(ExclusiveError::DeviceInUse) => {
                        last = ExclusiveError::DeviceInUse;
                        std::thread::sleep(RETRY_DELAY);
                    }
                    Err(e) => return Err(e),
                },
            }
        }
        Err(last)
    }

    fn try_open(
        cfg: ExclusiveOpenConfig,
        gate: Arc<GateState>,
        lease: Lease,
    ) -> Result<Self, ExclusiveError> {
        let stop = Arc::new(StopEvent::new()?);
        let flags = Arc::new(Flags::default());
        let stats = Arc::new(SharedStats::default());
        let (ready_tx, ready_rx) = sync_channel(2);
        let (done_tx, done_rx) = sync_channel(1);
        let (rate_hz, channels) = (cfg.rate_hz, cfg.channels);

        let args = ThreadArgs {
            cfg,
            gate,
            stop: stop.clone(),
            flags: flags.clone(),
            stats: stats.clone(),
            ready_tx,
            done_tx,
            lease,
        };
        // If the spawn fails the closure (and with it the lease) is dropped here.
        let handle = std::thread::Builder::new()
            .name("hires-wasapi-render".into())
            .spawn(move || render_thread(args))
            .map_err(|e| ExclusiveError::Other(format!("spawn render thread: {e}")))?;
        let ctl = ThreadCtl {
            stop,
            flags: flags.clone(),
            done_rx: Mutex::new(done_rx),
            handle: Some(handle),
        };

        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(Ready { mixer, format })) => Ok(Self {
                _ctl: ctl,
                mixer,
                format,
                rate_hz,
                channels,
                stats,
                flags,
            }),
            Ok(Err(e)) => Err(e), // `ctl` drops and winds the thread down
            Err(RecvTimeoutError::Timeout) => Err(ExclusiveError::Other(
                "timed out waiting for the render thread".into(),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(ExclusiveError::Other(format!(
                "render thread exited before reporting ready{}",
                flags
                    .last_error()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ))),
        }
    }

    pub(super) fn mixer(&self) -> &rodio::mixer::Mixer {
        &self.mixer
    }
    pub(super) fn format(&self) -> OutputFormat {
        self.format
    }
    pub(super) fn rate_hz(&self) -> u32 {
        self.rate_hz
    }
    pub(super) fn channels(&self) -> u16 {
        self.channels
    }
    pub(super) fn stats(&self) -> RenderStats {
        self.stats.snapshot()
    }
    pub(super) fn failed(&self) -> bool {
        self.flags.failed.load(Ordering::Acquire)
    }
    pub(super) fn last_error(&self) -> Option<String> {
        self.flags.last_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Backend>();
        assert_send_sync::<crate::exclusive::ExclusiveOutput>();
    }

    #[test]
    fn flags_keep_the_first_error() {
        let f = Flags::default();
        assert!(f.last_error().is_none() && !f.failed.load(Ordering::Relaxed));
        f.fail("first");
        f.fail("second");
        assert_eq!(f.last_error().as_deref(), Some("first"));
        assert!(f.failed.load(Ordering::Relaxed));
    }

    #[test]
    fn cancel_is_observed() {
        let f = Flags::default();
        assert!(check_cancelled(&f).is_ok());
        f.stop.store(true, Ordering::Release);
        assert!(check_cancelled(&f).is_err());
    }
}

#[cfg(test)]
mod hw_tests {
    //! Hardware tests: `cargo test -p psysonic-fork-hires -- --ignored --nocapture <name>`.
    //! They only ever open the virtual "Steam" speakers; exclusive mode takes a device away from other apps.
    use super::*;
    use crate::exclusive::{probe_exclusive_caps, ExclusiveOutput};
    use std::time::Instant;

    fn render_endpoints() -> Vec<(String, String)> {
        initialize_mta().ok().expect("mta");
        let en = DeviceEnumerator::new().expect("enumerator");
        let coll = en
            .get_device_collection(&Direction::Render)
            .expect("collection");
        coll.into_iter()
            .filter_map(|d| d.ok())
            .filter_map(|d| Some((d.get_id().ok()?, d.get_friendlyname().ok()?)))
            .collect()
    }

    /// The virtual Steam speakers (preferred over the other "Steam" endpoint, the streaming microphone's).
    fn steam_id() -> Option<String> {
        let all = render_endpoints();
        let pick = |needle: &str| all.iter().find(|(_, n)| n.contains(needle));
        pick("Steam Streaming Speakers")
            .or_else(|| pick("Steam"))
            .map(|(id, _)| id.clone())
    }

    fn cfg(id: &str, rate_hz: u32) -> ExclusiveOpenConfig {
        ExclusiveOpenConfig {
            endpoint_id: Some(id.to_owned()),
            rate_hz,
            channels: 2,
            source_bits: 16,
            period_hns: crate::exclusive::DEFAULT_PERIOD_HNS,
            lead_in_ms: 100,
        }
    }

    #[test]
    #[ignore = "hardware"]
    fn probe_lists_devices() {
        for (id, name) in render_endpoints() {
            println!("--- {name}\n    id: {id}");
            match probe_exclusive_caps(Some(&id)) {
                Ok(caps) => {
                    println!("    channels: {}", caps.channels);
                    for (f, rates) in &caps.by_format {
                        println!("    {f:?}: {rates:?}");
                    }
                }
                Err(e) => println!("    probe failed: {e} ({e:?})"),
            }
        }
    }

    #[test]
    #[ignore = "hardware"]
    fn open_close_on_steam_speakers() {
        let Some(id) = steam_id() else {
            println!("SKIP: no render endpoint with \"Steam\" in its name");
            return;
        };
        for rate in [48_000u32, 96_000] {
            let t0 = Instant::now();
            let out = ExclusiveOutput::open(cfg(&id, rate), Arc::new(GateState::new()))
                .unwrap_or_else(|e| panic!("open at {rate} Hz failed: {e} ({e:?})"));
            println!(
                "opened {rate} Hz in {:?}: format {:?}, {} ch",
                t0.elapsed(),
                out.format(),
                out.channels()
            );
            assert_eq!(out.rate_hz(), rate);
            std::thread::sleep(Duration::from_millis(300));
            let stats = out.stats();
            println!("stats after 300 ms: {stats:?}, failed={}", out.failed());
            println!(
                "gaps: max {} us, late (>1.5x) {}, over 2x {}, of {} periods; last_error={:?}",
                stats.max_gap_us,
                stats.late_wakes,
                stats.gaps_over_2x,
                stats.periods,
                out.last_error()
            );
            assert!(stats.periods > 0, "no periods rendered");
            assert!(!out.failed());
            let t1 = Instant::now();
            drop(out);
            let dt = t1.elapsed();
            println!("drop took {dt:?}");
            assert!(dt < Duration::from_secs(2), "drop too slow: {dt:?}");
        }
    }

    #[test]
    #[ignore = "hardware"]
    fn open_reports_in_use() {
        let Some(id) = steam_id() else {
            println!("SKIP: no render endpoint with \"Steam\" in its name");
            return;
        };
        let first = ExclusiveOutput::open(cfg(&id, 48_000), Arc::new(GateState::new()))
            .unwrap_or_else(|e| panic!("first open failed: {e} ({e:?})"));
        let second = ExclusiveOutput::open(cfg(&id, 48_000), Arc::new(GateState::new()));
        match &second {
            Ok(_) => println!("second open unexpectedly succeeded"),
            Err(e) => println!(
                "second open error: {e} ({e:?}), fall_back={}",
                e.should_fall_back()
            ),
        }
        assert!(second.is_err());
        drop(second);
        drop(first);
    }
}
