//! WASAPI exclusive-mode output backend. The hardware-free core (format negotiation, period maths, the
//! bit-perfect gate and the render loop over a sink trait) is cross-platform; the Windows render thread that
//! owns the device is in `wasapi_backend`. This module is the facade: callers compile everywhere and get
//! [`ExclusiveError::Unsupported`] off Windows.

pub mod crc_tap;
pub mod gate;
mod lifecycle;
pub mod negotiate;
pub mod period;
pub mod render;

#[cfg(windows)]
mod wasapi_backend;

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::format::OutputFormat;
pub use gate::GateState;
pub use negotiate::ExclusiveCaps;
pub use period::DEFAULT_PERIOD_HNS;
pub use render::RenderStats;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Milliseconds on the process-wide monotonic clock shared by every output and the control side. Publish
/// gate heartbeats ([`GateState::publish`]) with this value.
pub fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// Microseconds on the same clock as [`now_ms`].
pub fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Why an exclusive-mode probe or open failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveError {
    /// Not built for Windows.
    Unsupported,
    DeviceNotFound,
    /// Another application holds the device (usually in exclusive mode).
    DeviceInUse,
    UnsupportedFormat,
    /// Exclusive mode is disabled for this device in Windows sound settings.
    ExclusiveNotAllowed,
    DeviceInvalidated,
    Other(String),
}

impl fmt::Display for ExclusiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("exclusive mode is not supported on this platform"),
            Self::DeviceNotFound => f.write_str("audio device not found"),
            Self::DeviceInUse => f.write_str("audio device is in use by another application"),
            Self::UnsupportedFormat => {
                f.write_str("audio device does not support the requested format")
            }
            Self::ExclusiveNotAllowed => {
                f.write_str("exclusive mode is disabled for this audio device")
            }
            Self::DeviceInvalidated => f.write_str("audio device was removed or reconfigured"),
            Self::Other(m) => write!(f, "exclusive mode error: {m}"),
        }
    }
}

impl std::error::Error for ExclusiveError {}

impl ExclusiveError {
    /// Open-time policy: true for every error, so a failed open always falls back to shared-mode output and
    /// the user still hears audio (device busy, format or exclusive mode refused, device gone, unexpected
    /// failure, or no exclusive support on this platform).
    ///
    /// This is about `open` only. A stream that fails while playing ([`ExclusiveOutput::failed`] turning
    /// true, see also [`ExclusiveOutput::last_error`]) is handled by the engine, which has to rebuild the
    /// output itself.
    pub fn should_fall_back(&self) -> bool {
        true
    }
}

// HRESULT values as raw i32 so the mapping is pure and testable on every platform.
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x8889_0019_u32 as i32;
const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;

/// True for `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_buffer_not_aligned(hresult: i32) -> bool {
    hresult == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
}

/// Maps an HRESULT from device lookup, format probing or `IAudioClient::Initialize` to an [`ExclusiveError`].
pub fn map_hresult(hresult: i32, context: &str) -> ExclusiveError {
    match hresult {
        AUDCLNT_E_DEVICE_IN_USE => ExclusiveError::DeviceInUse,
        AUDCLNT_E_UNSUPPORTED_FORMAT => ExclusiveError::UnsupportedFormat,
        AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => ExclusiveError::ExclusiveNotAllowed,
        AUDCLNT_E_DEVICE_INVALIDATED => ExclusiveError::DeviceInvalidated,
        E_NOTFOUND => ExclusiveError::DeviceNotFound,
        other => ExclusiveError::Other(format!("{context}: HRESULT {:#010x}", other as u32)),
    }
}

/// How to open an exclusive stream.
#[derive(Debug, Clone)]
pub struct ExclusiveOpenConfig {
    /// Endpoint id; `None` is the default render device.
    pub endpoint_id: Option<String>,
    pub rate_hz: u32,
    pub channels: u16,
    /// Bit depth of the source material, for format choice and the dither decision.
    pub source_bits: u32,
    /// Requested period in 100 ns units; aligned to what the device accepts.
    pub period_hns: i64,
    /// Silence written before the source is first pulled.
    pub lead_in_ms: u32,
}

/// Probes the exclusive-mode capabilities of an endpoint (`None` = default render device).
/// Tests stereo first, then the device's mix channel count if stereo yields nothing.
pub fn probe_exclusive_caps(endpoint_id: Option<&str>) -> Result<ExclusiveCaps, ExclusiveError> {
    #[cfg(windows)]
    {
        wasapi_backend::probe(endpoint_id)
    }
    #[cfg(not(windows))]
    {
        let _ = endpoint_id;
        Err(ExclusiveError::Unsupported)
    }
}

/// An open exclusive-mode stream. Add sources to [`mixer`](Self::mixer); dropping it stops the stream and
/// releases the device (bounded wait, never hangs the caller).
pub struct ExclusiveOutput {
    #[cfg(windows)]
    inner: wasapi_backend::Backend,
    #[cfg(not(windows))]
    never: std::convert::Infallible,
}

impl ExclusiveOutput {
    pub fn open(cfg: ExclusiveOpenConfig, gate: Arc<GateState>) -> Result<Self, ExclusiveError> {
        #[cfg(windows)]
        {
            wasapi_backend::Backend::open(cfg, gate).map(|inner| Self { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = (cfg, gate);
            Err(ExclusiveError::Unsupported)
        }
    }

    pub fn mixer(&self) -> &rodio::mixer::Mixer {
        #[cfg(windows)]
        {
            self.inner.mixer()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    pub fn format(&self) -> OutputFormat {
        #[cfg(windows)]
        {
            self.inner.format()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    pub fn rate_hz(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.rate_hz()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    pub fn channels(&self) -> u16 {
        #[cfg(windows)]
        {
            self.inner.channels()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    /// Snapshot of the render counters.
    pub fn stats(&self) -> RenderStats {
        #[cfg(windows)]
        {
            self.inner.stats()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    /// True once the render thread ended on a device error or a stalled device.
    pub fn failed(&self) -> bool {
        #[cfg(windows)]
        {
            self.inner.failed()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    /// Why the stream failed or could not start, if it did (the first error recorded).
    pub fn last_error(&self) -> Option<String> {
        #[cfg(windows)]
        {
            self.inner.last_error()
        }
        #[cfg(not(windows))]
        match self.never {}
    }

    /// [`now_ms`], kept as a method for callers that only hold the output.
    pub fn now_ms(&self) -> u64 {
        now_ms()
    }
}

#[cfg(test)]
mod headroom_tests;
#[cfg(test)]
mod render_tests;

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn hresult_mapping() {
        assert_eq!(
            map_hresult(AUDCLNT_E_DEVICE_IN_USE, "x"),
            ExclusiveError::DeviceInUse
        );
        assert_eq!(
            map_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT, "x"),
            ExclusiveError::UnsupportedFormat
        );
        assert_eq!(
            map_hresult(AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED, "x"),
            ExclusiveError::ExclusiveNotAllowed
        );
        assert_eq!(
            map_hresult(AUDCLNT_E_DEVICE_INVALIDATED, "x"),
            ExclusiveError::DeviceInvalidated
        );
        assert_eq!(map_hresult(E_NOTFOUND, "x"), ExclusiveError::DeviceNotFound);
        assert!(matches!(map_hresult(-1, "ctx"), ExclusiveError::Other(m) if m.contains("ctx")));
        assert!(is_buffer_not_aligned(AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED));
        assert!(!is_buffer_not_aligned(AUDCLNT_E_DEVICE_IN_USE));
    }

    #[test]
    fn fall_back_policy() {
        for e in [
            ExclusiveError::Unsupported,
            ExclusiveError::DeviceNotFound,
            ExclusiveError::DeviceInUse,
            ExclusiveError::UnsupportedFormat,
            ExclusiveError::ExclusiveNotAllowed,
            ExclusiveError::DeviceInvalidated,
            ExclusiveError::Other("x".into()),
        ] {
            assert!(e.should_fall_back(), "{e}");
        }
        assert!(!ExclusiveError::DeviceInUse.to_string().is_empty());
    }

    #[test]
    fn clock_is_monotonic_and_shared() {
        let (a, b) = (now_us(), now_us());
        assert!(b >= a);
        assert!(now_ms() <= now_us() / 1000 + 1);
    }

    #[test]
    fn output_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExclusiveOutput>();
    }

    #[cfg(windows)]
    #[test]
    fn constants_match_windows_crate() {
        use windows::Win32::Media::Audio as a;
        assert_eq!(
            AUDCLNT_E_DEVICE_INVALIDATED,
            a::AUDCLNT_E_DEVICE_INVALIDATED.0
        );
        assert_eq!(
            AUDCLNT_E_UNSUPPORTED_FORMAT,
            a::AUDCLNT_E_UNSUPPORTED_FORMAT.0
        );
        assert_eq!(AUDCLNT_E_DEVICE_IN_USE, a::AUDCLNT_E_DEVICE_IN_USE.0);
        assert_eq!(
            AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
            a::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED.0
        );
        assert_eq!(
            AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
            a::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED.0
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_off_windows() {
        assert_eq!(probe_exclusive_caps(None), Err(ExclusiveError::Unsupported));
    }
}
