//! Thread-lifecycle helpers for the exclusive backend that need no hardware: the process-wide registry of
//! endpoints held by render threads, and panic containment.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Endpoint ids currently held by a live or detached render thread. A detached thread (one that did not
/// stop within the drop timeout) keeps its entry until it really ends, so a reopen of the same device waits
/// for it instead of racing into `DeviceInUse`.
pub(crate) struct Registry {
    held: Mutex<BTreeSet<String>>,
    released: Condvar,
}

/// Proof that `key` is held; releases it (and wakes waiters) on drop, including during a panic unwind.
pub(crate) struct Lease {
    registry: &'static Registry,
    key: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            held: Mutex::new(BTreeSet::new()),
            released: Condvar::new(),
        }
    }

    /// Waits up to `timeout` for `key` to be free, then takes it. `None` if it is still held.
    pub(crate) fn acquire(&'static self, key: &str, timeout: Duration) -> Option<Lease> {
        let guard = lock(&self.held);
        let (mut guard, _) = self
            .released
            .wait_timeout_while(guard, timeout, |held| held.contains(key))
            .unwrap_or_else(|e| e.into_inner());
        if guard.contains(key) {
            return None;
        }
        guard.insert(key.to_owned());
        Some(Lease {
            registry: self,
            key: key.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn is_held(&self, key: &str) -> bool {
        lock(&self.held).contains(key)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        lock(&self.registry.held).remove(&self.key);
        self.registry.released.notify_all();
    }
}

/// Policy for device-event waits that time out: the first is reported (and counted) as a timeout, a second one
/// in a row means the device is dead.
#[derive(Debug, Default)]
pub(crate) struct TimeoutTracker {
    consecutive: u32,
}

impl TimeoutTracker {
    pub(crate) fn on_ready(&mut self) {
        self.consecutive = 0;
    }

    /// Records a timed-out wait; true when it is fatal (the second in a row).
    pub(crate) fn on_timeout(&mut self) -> bool {
        self.consecutive += 1;
        self.consecutive >= 2
    }
}

/// Runs `f`, turning a panic into its message.
pub(crate) fn guarded<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with a non-string payload".to_owned()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    static R: Registry = Registry::new();

    #[test]
    fn second_acquire_times_out_until_released() {
        let lease = R.acquire("a", Duration::from_millis(10)).unwrap();
        assert!(R.is_held("a"));
        let t = Instant::now();
        assert!(R.acquire("a", Duration::from_millis(80)).is_none());
        assert!(t.elapsed() >= Duration::from_millis(70));
        // A different key is independent.
        drop(R.acquire("b", Duration::ZERO).unwrap());
        drop(lease);
        assert!(!R.is_held("a"));
        assert!(R.acquire("a", Duration::ZERO).is_some());
    }

    #[test]
    fn waiter_is_woken_by_release() {
        let lease = R.acquire("wake", Duration::ZERO).unwrap();
        let h = std::thread::spawn(|| {
            let t = Instant::now();
            let got = R.acquire("wake", Duration::from_secs(5)).is_some();
            (got, t.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(lease);
        let (got, waited) = h.join().unwrap();
        assert!(got);
        assert!(waited < Duration::from_secs(2), "woken late: {waited:?}");
    }

    #[test]
    fn lease_is_released_by_a_panic() {
        let lease = R.acquire("panic", Duration::ZERO).unwrap();
        let r = std::thread::spawn(move || {
            let _lease = lease;
            panic!("boom");
        })
        .join();
        assert!(r.is_err());
        assert!(!R.is_held("panic"));
    }

    #[test]
    fn second_consecutive_timeout_is_fatal() {
        let mut t = TimeoutTracker::default();
        assert!(!t.on_timeout());
        t.on_ready();
        assert!(!t.on_timeout());
        assert!(t.on_timeout());
    }

    #[test]
    fn guarded_reports_panic_messages() {
        assert_eq!(guarded(|| 5), Ok(5));
        assert_eq!(
            guarded(|| -> u8 { panic!("static msg") }),
            Err("static msg".into())
        );
        let n = 7;
        assert_eq!(
            guarded(|| -> u8 { panic!("formatted {n}") }),
            Err("formatted 7".into())
        );
    }

    #[test]
    fn guarded_panic_still_drops_the_lease() {
        let lease = R.acquire("guarded", Duration::ZERO).unwrap();
        let r = guarded(move || {
            let _lease = lease;
            panic!("render thread body");
        });
        assert!(r.is_err());
        assert!(!R.is_held("guarded"));
    }
}
