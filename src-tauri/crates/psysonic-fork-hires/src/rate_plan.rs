//! Output-rate planner (design: `docs/fork-hires-engine-plan.md` §3.3).
//!
//! Types and helpers are implemented here; the policy core, [`plan_output_rate`], is left to the fork owner.

/// Which audio backend owns the output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Shared mode: the OS mixer runs at a fixed rate, so the stream must match it.
    Shared { mix_rate_hz: u32 },
    /// Exclusive mode: the stream may open at any rate the device supports.
    Exclusive,
}

/// How the stream rate follows the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Run at the ceiling while blends are on; never reopen for a transition.
    Seamless,
    /// Run at the max of the current and upcoming tracks; may Resync on a late higher-rate track.
    Adaptive,
}

/// How the next track starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// A hard start (stream closed, skip, or no blending): reopening is acceptable.
    Hard,
    /// A crossfade, gapless or AutoDJ blend into a running stream.
    Blend,
}

/// The sample rates a device supports, sorted ascending and deduplicated, never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCaps {
    rates_hz: Vec<u32>,
}

impl DeviceCaps {
    /// Sorts and dedups `rates_hz`. Returns `None` if it is empty or contains 0.
    pub fn new(mut rates_hz: Vec<u32>) -> Option<Self> {
        rates_hz.sort_unstable();
        rates_hz.dedup();
        if rates_hz.is_empty() || rates_hz[0] == 0 {
            return None;
        }
        Some(Self { rates_hz })
    }

    /// Supported rates, ascending.
    pub fn rates_hz(&self) -> &[u32] {
        &self.rates_hz
    }

    /// Highest supported rate.
    pub fn max_hz(&self) -> u32 {
        *self.rates_hz.last().expect("non-empty by construction")
    }

    /// Lowest supported rate.
    pub fn min_hz(&self) -> u32 {
        self.rates_hz[0]
    }

    pub fn supports(&self, hz: u32) -> bool {
        self.rates_hz.binary_search(&hz).is_ok()
    }

    /// The highest supported rate that is `<= ceiling_hz`; if none, the lowest supported rate.
    pub fn ceiling_hz(&self, ceiling_hz: u32) -> u32 {
        self.rates_hz
            .iter()
            .rev()
            .copied()
            .find(|&r| r <= ceiling_hz)
            .unwrap_or_else(|| self.min_hz())
    }

    /// The lowest supported rate that is `>= hz`; if none, the highest supported rate.
    pub fn at_or_above(&self, hz: u32) -> u32 {
        self.rates_hz
            .iter()
            .copied()
            .find(|&r| r >= hz)
            .unwrap_or_else(|| self.max_hz())
    }
}

/// Everything the planner looks at when a track is about to play.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    pub backend: Backend,
    pub policy: Policy,
    /// User ceiling in Hz (default 192_000).
    pub ceiling_hz: u32,
    /// `crossfade || gapless || AutoDJ`.
    pub blends_enabled: bool,
    /// Rate of the open stream; `None` = stream closed.
    pub current_stream_hz: Option<u32>,
    /// Decoded rate of the track about to play.
    pub track_hz: u32,
    pub transition: Transition,
    /// Decoded rates of the next queued tracks (Adaptive lookahead only).
    pub upcoming_hz: &'a [u32],
    pub caps: &'a DeviceCaps,
}

/// The planner's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Rate the output stream must run at for this track.
    pub stream_hz: u32,
    /// True iff the stream has to be (re)opened to get to `stream_hz`.
    pub reopen: bool,
}

/// Decides the output stream rate for the track about to play, and whether the stream must be reopened.
///
/// Let `dev_ceiling = input.caps.ceiling_hz(input.ceiling_hz)`, and let a transition be effectively
/// `Hard` when `input.current_stream_hz` is `None`, whatever `input.transition` says.
///
/// Rules (design: `docs/fork-hires-engine-plan.md` §3.3):
/// 1. **Shared backend:** `stream_hz` is always `mix_rate_hz`; `reopen` iff `current_stream_hz != Some(mix_rate_hz)`.
/// 2. **Stream closed** (`current_stream_hz == None`): treat any transition as `Hard`.
/// 3. **Native** (`blends_enabled == false`, any policy), on Hard: `caps.at_or_above(track_hz)`, capped by `dev_ceiling`.
/// 4. **Seamless + blends enabled**, on Hard: `dev_ceiling`.
/// 5. **Seamless**, on Blend with an open stream: never reopen; `stream_hz = current`.
/// 6. **Adaptive + blends enabled**, on Hard: `target = max(track_hz, max(upcoming_hz))`, and
///    `stream_hz = caps.at_or_above(target)` capped at `dev_ceiling`.
/// 7. **Adaptive**, on Blend: if `track_hz <= current`, no reopen. Otherwise Resync: reopen at the rule-6 rate.
/// 8. **Everywhere:** never above the device maximum. Never below `track_hz` unless `track_hz` is above the
///    ceiling or the device maximum.
/// 9. `reopen` is true iff the planned rate differs from the open stream (or the stream is closed).
///
/// Rules 1 to 9 are pinned by the `plan_*` tests in `rate_plan_tests.rs`.
pub fn plan_output_rate(input: &PlanInput) -> Plan {
    let stream_hz = match (input.backend, input.transition, input.current_stream_hz) {
        (Backend::Shared { mix_rate_hz }, _, _) => mix_rate_hz,
        // Blend into a running stream: Seamless never reopens; Adaptive only resyncs upward.
        (Backend::Exclusive, Transition::Blend, Some(cur)) => match input.policy {
            Policy::Seamless => cur,
            Policy::Adaptive if input.track_hz <= cur => cur,
            Policy::Adaptive => hard_start_hz(input),
        },
        // Hard start, or a closed stream (any transition is Hard then).
        (Backend::Exclusive, _, _) => hard_start_hz(input),
    };
    Plan {
        stream_hz,
        reopen: input.current_stream_hz != Some(stream_hz),
    }
}

/// Rate for a Hard start on an exclusive stream (rules 3, 4 and 6).
fn hard_start_hz(input: &PlanInput) -> u32 {
    let caps = input.caps;
    let dev_ceiling = caps.ceiling_hz(input.ceiling_hz);
    if !input.blends_enabled {
        return caps.at_or_above(input.track_hz).min(dev_ceiling);
    }
    match input.policy {
        Policy::Seamless => dev_ceiling,
        Policy::Adaptive => {
            let target = input
                .upcoming_hz
                .iter()
                .copied()
                .fold(input.track_hz, u32::max);
            caps.at_or_above(target).min(dev_ceiling)
        }
    }
}
