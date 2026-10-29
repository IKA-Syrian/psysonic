//! The bit-perfect gate: true only while nothing alters the samples between the decoder and the device.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Everything that must hold for the output to be bit-perfect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateInputs {
    pub volume_is_unity: bool,
    pub normalization_off: bool,
    pub eq_off: bool,
    pub speed_is_unity: bool,
    pub no_blend_or_extra_players: bool,
    /// The track's rate equals the stream rate (no resampling).
    pub rate_matches: bool,
}

/// True when every input holds.
pub fn bit_perfect(i: &GateInputs) -> bool {
    i.volume_is_unity
        && i.normalization_off
        && i.eq_off
        && i.speed_is_unity
        && i.no_blend_or_extra_players
        && i.rate_matches
}

/// How far ahead of the reader's clock a heartbeat may be before it counts as stale (publisher/reader skew).
pub const FUTURE_SLACK_MS: u64 = 50;

/// The gate value published by the control side and read lock-free by the render thread.
///
/// The value only counts while it is fresh: a heartbeat timestamp (monotonic ms) is stored with every publish,
/// and a read that finds it older than `max_age_ms` returns false, so a stalled publisher can never leave the
/// output claiming bit-perfect. Initially false.
#[derive(Debug, Default)]
pub struct GateState {
    value: AtomicBool,
    heartbeat_ms: AtomicU64,
}

impl GateState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Back to the initial state (false, never published). The engine should give each output a fresh
    /// `GateState`, or reset it when an output is reopened.
    pub fn reset(&self) {
        self.value.store(false, Ordering::Relaxed);
        self.heartbeat_ms.store(0, Ordering::Release);
    }

    pub fn publish(&self, value: bool, now_ms: u64) {
        self.value.store(value, Ordering::Relaxed);
        self.heartbeat_ms.store(now_ms, Ordering::Release);
    }

    /// The published value, or false if it is stale (more than `max_age_ms` old), was never published, or
    /// carries a heartbeat from the future (more than [`FUTURE_SLACK_MS`] ahead of `now_ms`, e.g. left over
    /// from a different clock).
    pub fn read(&self, now_ms: u64, max_age_ms: u64) -> bool {
        let beat = self.heartbeat_ms.load(Ordering::Acquire);
        self.value.load(Ordering::Relaxed)
            && beat <= now_ms.saturating_add(FUTURE_SLACK_MS)
            && now_ms.saturating_sub(beat) <= max_age_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_true() -> GateInputs {
        GateInputs {
            volume_is_unity: true,
            normalization_off: true,
            eq_off: true,
            speed_is_unity: true,
            no_blend_or_extra_players: true,
            rate_matches: true,
        }
    }

    #[test]
    fn all_true_is_bit_perfect() {
        assert!(bit_perfect(&all_true()));
    }

    #[test]
    fn each_input_alone_breaks_it() {
        let breakers: [fn(&mut GateInputs); 6] = [
            |i| i.volume_is_unity = false,
            |i| i.normalization_off = false,
            |i| i.eq_off = false,
            |i| i.speed_is_unity = false,
            |i| i.no_blend_or_extra_players = false,
            |i| i.rate_matches = false,
        ];
        for (n, b) in breakers.iter().enumerate() {
            let mut i = all_true();
            b(&mut i);
            assert!(!bit_perfect(&i), "input {n}");
        }
    }

    #[test]
    fn initial_state_is_false() {
        let g = GateState::new();
        assert!(!g.read(0, 1_000));
        assert!(!g.read(10_000, 1_000));
    }

    #[test]
    fn fresh_true_reads_true_and_false_reads_false() {
        let g = GateState::new();
        g.publish(true, 1_000);
        assert!(g.read(1_000, 500));
        assert!(g.read(1_500, 500));
        g.publish(false, 1_100);
        assert!(!g.read(1_100, 500));
    }

    #[test]
    fn stale_reads_false() {
        let g = GateState::new();
        g.publish(true, 1_000);
        assert!(!g.read(1_501, 500));
        // A new heartbeat revives it.
        g.publish(true, 1_600);
        assert!(g.read(1_700, 500));
    }

    #[test]
    fn heartbeat_from_the_future_is_stale() {
        // A beat left over from another clock (e.g. a previous output's timeline) must not read as fresh.
        let g = GateState::new();
        g.publish(true, 600_000);
        assert!(!g.read(1_000, 500));
        assert!(!g.read(599_000, 500));
        // Small publisher/reader skew is tolerated.
        assert!(g.read(600_000 - FUTURE_SLACK_MS, 500));
        assert!(g.read(600_100, 500));
    }

    #[test]
    fn reset_clears_value_and_heartbeat() {
        let g = GateState::new();
        g.publish(true, 1_000);
        assert!(g.read(1_000, 500));
        g.reset();
        assert!(!g.read(1_000, 500));
        g.publish(true, 1_100);
        assert!(g.read(1_100, 500));
    }
}
