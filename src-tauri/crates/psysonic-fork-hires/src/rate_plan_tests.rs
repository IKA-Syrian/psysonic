//! Helper tests (must pass) and `plan_*` rule tests (fail with `todo!` until `plan_output_rate` is written).

use crate::rate_plan::*;

const R441: u32 = 44_100;
const R48: u32 = 48_000;
const R882: u32 = 88_200;
const R96: u32 = 96_000;
const R1764: u32 = 176_400;
const R192: u32 = 192_000;
const R3528: u32 = 352_800;
const TRACKS: [u32; 7] = [R441, R48, R882, R96, R1764, R192, R3528];

fn caps(rates: &[u32]) -> DeviceCaps {
    DeviceCaps::new(rates.to_vec()).unwrap()
}

fn full() -> DeviceCaps {
    caps(&[R441, R48, R882, R96, R1764, R192])
}

fn cap_sets() -> Vec<(&'static str, DeviceCaps)> {
    vec![
        ("full 44.1-192", full()),
        ("up-to-96", caps(&[R441, R48, R882, R96])),
        ("48-only", caps(&[R48])),
    ]
}

fn open_streams(c: &DeviceCaps) -> Vec<Option<u32>> {
    let mut v = vec![None];
    v.extend(c.rates_hz().iter().map(|&r| Some(r)));
    v
}

// ---------- helper tests ----------

#[test]
fn caps_new_sorts_dedups_and_rejects_empty() {
    let c = DeviceCaps::new(vec![R192, R48, R48, R441]).unwrap();
    assert_eq!(c.rates_hz(), &[R441, R48, R192]);
    assert_eq!((c.min_hz(), c.max_hz()), (R441, R192));
    assert!(c.supports(R48) && !c.supports(R96));
    assert!(DeviceCaps::new(vec![]).is_none());
    assert!(DeviceCaps::new(vec![0, R48]).is_none());
}

#[test]
fn caps_ceiling_hz_picks_highest_not_above() {
    let c = caps(&[R441, R48, R96, R192]);
    assert_eq!(c.ceiling_hz(R192), R192);
    assert_eq!(c.ceiling_hz(R3528), R192);
    assert_eq!(c.ceiling_hz(R1764), R96);
    assert_eq!(c.ceiling_hz(R96), R96);
    assert_eq!(c.ceiling_hz(R882), R48);
    assert_eq!(c.ceiling_hz(R441), R441);
    assert_eq!(
        c.ceiling_hz(R441 - 1),
        R441,
        "none <= ceiling: lowest supported"
    );
    assert_eq!(c.ceiling_hz(0), R441);
}

#[test]
fn caps_at_or_above_picks_lowest_not_below() {
    let c = caps(&[R441, R48, R96, R192]);
    assert_eq!(c.at_or_above(0), R441);
    assert_eq!(c.at_or_above(R441), R441);
    assert_eq!(c.at_or_above(R441 + 1), R48);
    assert_eq!(c.at_or_above(R882), R96);
    assert_eq!(c.at_or_above(R192), R192);
    assert_eq!(c.at_or_above(R3528), R192, "none >= hz: highest supported");
}

#[test]
fn caps_single_rate_is_total() {
    let c = caps(&[R48]);
    for hz in TRACKS {
        assert_eq!(c.ceiling_hz(hz), R48);
        assert_eq!(c.at_or_above(hz), R48);
    }
}

// ---------- plan_* helpers ----------

fn input<'a>(
    caps: &'a DeviceCaps,
    policy: Policy,
    blends_enabled: bool,
    current: Option<u32>,
    track: u32,
    transition: Transition,
    upcoming: &'a [u32],
) -> PlanInput<'a> {
    PlanInput {
        backend: Backend::Exclusive,
        policy,
        ceiling_hz: R192,
        blends_enabled,
        current_stream_hz: current,
        track_hz: track,
        transition,
        upcoming_hz: upcoming,
        caps,
    }
}

fn keep(hz: u32) -> Plan {
    Plan {
        stream_hz: hz,
        reopen: false,
    }
}

fn expect_plan(i: &PlanInput, hz: u32, reopen: bool, what: &str) {
    let want = Plan {
        stream_hz: hz,
        reopen,
    };
    assert_eq!(plan_output_rate(i), want, "{what}: {i:?}");
}

/// What rules 3, 4 and 6 give for a Hard start.
fn hard_expectation(i: &PlanInput) -> u32 {
    let cap = i.caps.ceiling_hz(i.ceiling_hz);
    if !i.blends_enabled {
        return i.caps.at_or_above(i.track_hz).min(cap);
    }
    match i.policy {
        Policy::Seamless => cap,
        Policy::Adaptive => {
            let target = i.upcoming_hz.iter().copied().fold(i.track_hz, u32::max);
            i.caps.at_or_above(target).min(cap)
        }
    }
}

// ---------- plan_* rule tests ----------

#[test]
fn plan_table_matches_rules() {
    let upcomings: [&[u32]; 3] = [&[], &[R48], &[R192, R441]];
    for (cap_name, c) in cap_sets() {
        for policy in [Policy::Seamless, Policy::Adaptive] {
            for transition in [Transition::Hard, Transition::Blend] {
                for blends in [false, true] {
                    // A Blend transition only exists while blends are enabled.
                    if transition == Transition::Blend && !blends {
                        continue;
                    }
                    for track in TRACKS {
                        for current in open_streams(&c) {
                            for up in upcomings {
                                let i = input(&c, policy, blends, current, track, transition, up);
                                let ctx = format!("[{cap_name}] {i:?}");
                                let p = plan_output_rate(&i);
                                match current {
                                    Some(cur) if transition == Transition::Blend => match policy {
                                        Policy::Seamless => assert_eq!(
                                            p,
                                            keep(cur),
                                            "rule 5: Seamless blend keeps the stream, {ctx}"
                                        ),
                                        Policy::Adaptive if track <= cur => assert_eq!(
                                            p,
                                            keep(cur),
                                            "rule 7: Adaptive blend, track <= current, {ctx}"
                                        ),
                                        Policy::Adaptive => {
                                            let mut hard = i;
                                            hard.transition = Transition::Hard;
                                            assert_eq!(
                                                p.stream_hz,
                                                hard_expectation(&hard),
                                                "rule 7: Adaptive Resync uses the rule-6 rate, {ctx}"
                                            );
                                        }
                                    },
                                    _ => assert_eq!(
                                        p.stream_hz,
                                        hard_expectation(&i),
                                        "rules 2/3/4/6: hard start rate, {ctx}"
                                    ),
                                }
                                assert_eq!(
                                    p.reopen,
                                    current != Some(p.stream_hz),
                                    "rule 9: reopen iff rate differs from open stream, {ctx}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn plan_never_exceeds_device_or_undershoots_track_on_hard_starts() {
    for (cap_name, c) in cap_sets() {
        let dev_ceiling = c.ceiling_hz(R192);
        for policy in [Policy::Seamless, Policy::Adaptive] {
            for blends in [false, true] {
                for track in TRACKS {
                    for up in [&[][..], &[R3528][..], &[R441][..]] {
                        for cur in open_streams(&c) {
                            let i = input(&c, policy, blends, cur, track, Transition::Hard, up);
                            let p = plan_output_rate(&i);
                            let ctx = format!("[{cap_name}] {i:?} -> {p:?}");
                            assert!(
                                p.stream_hz <= c.max_hz(),
                                "rule 8: above device maximum, {ctx}"
                            );
                            assert!(c.supports(p.stream_hz), "rule 8: unsupported rate, {ctx}");
                            if track <= dev_ceiling {
                                assert!(
                                    p.stream_hz >= track,
                                    "rule 8: below the track rate without need, {ctx}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn plan_never_exceeds_device_on_adaptive_blends() {
    for (cap_name, c) in cap_sets() {
        for track in TRACKS {
            for cur in c.rates_hz().iter().map(|&r| Some(r)) {
                let i = input(
                    &c,
                    Policy::Adaptive,
                    true,
                    cur,
                    track,
                    Transition::Blend,
                    &[R3528],
                );
                let p = plan_output_rate(&i);
                let ctx = format!("[{cap_name}] {i:?} -> {p:?}");
                assert!(p.stream_hz <= c.max_hz(), "rule 8: above device max, {ctx}");
                if track <= c.ceiling_hz(R192) {
                    assert!(p.stream_hz >= track, "rule 8: below track rate, {ctx}");
                }
            }
        }
    }
}

#[test]
fn plan_shared_always_mix_rate() {
    for (cap_name, c) in cap_sets() {
        for mix in [R441, R48, R96] {
            for policy in [Policy::Seamless, Policy::Adaptive] {
                for transition in [Transition::Hard, Transition::Blend] {
                    for blends in [false, true] {
                        for track in TRACKS {
                            for cur in [None, Some(mix), Some(R192), Some(R441)] {
                                let mut i =
                                    input(&c, policy, blends, cur, track, transition, &[R192]);
                                i.backend = Backend::Shared { mix_rate_hz: mix };
                                let want = Plan {
                                    stream_hz: mix,
                                    reopen: cur != Some(mix),
                                };
                                assert_eq!(
                                    plan_output_rate(&i),
                                    want,
                                    "rule 1: [{cap_name}] {i:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn plan_reopen_iff_rate_differs() {
    for (cap_name, c) in cap_sets() {
        for policy in [Policy::Seamless, Policy::Adaptive] {
            for transition in [Transition::Hard, Transition::Blend] {
                for blends in [false, true] {
                    for track in TRACKS {
                        for cur in [None, Some(R48), Some(R96), Some(R192)] {
                            for backend in
                                [Backend::Exclusive, Backend::Shared { mix_rate_hz: R48 }]
                            {
                                let mut i =
                                    input(&c, policy, blends, cur, track, transition, &[R96]);
                                i.backend = backend;
                                let p = plan_output_rate(&i);
                                assert_eq!(
                                    p.reopen,
                                    cur != Some(p.stream_hz),
                                    "rule 9: [{cap_name}] {i:?} -> {p:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Fixed orders plus deterministic pseudo-random ones (xorshift) for the order properties.
fn sequences() -> Vec<Vec<u32>> {
    let mut seqs: Vec<Vec<u32>> = vec![
        TRACKS.to_vec(),
        TRACKS.iter().rev().copied().collect(),
        vec![R48, R48, R48, R192, R441, R3528],
        vec![R441, R192, R48, R1764, R96],
    ];
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..40 {
        let mut seq = Vec::new();
        for _ in 0..8 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            seq.push(TRACKS[(s % TRACKS.len() as u64) as usize]);
        }
        seqs.push(seq);
    }
    seqs
}

#[test]
fn plan_seamless_never_reopens_on_blend() {
    for (cap_name, c) in cap_sets() {
        for seq in sequences() {
            // Hard start, then blends. The queue window changes mid-way, with a late high-rate track.
            let first = input(
                &c,
                Policy::Seamless,
                true,
                None,
                seq[0],
                Transition::Hard,
                &[],
            );
            let start = plan_output_rate(&first);
            assert!(
                start.reopen,
                "rule 9: the first start opens the stream, [{cap_name}] {seq:?}"
            );
            for (n, &track) in seq.iter().enumerate().skip(1) {
                let late: &[u32] = if n >= seq.len() / 2 {
                    &[R3528, R192]
                } else {
                    &[]
                };
                let i = input(
                    &c,
                    Policy::Seamless,
                    true,
                    Some(start.stream_hz),
                    track,
                    Transition::Blend,
                    late,
                );
                assert_eq!(
                    plan_output_rate(&i),
                    keep(start.stream_hz),
                    "rule 5: Seamless reopened or changed rate on Blend at step {n}, [{cap_name}] {seq:?}"
                );
            }
        }
    }
}

#[test]
fn plan_homogeneous_queue_stays_native() {
    let c = full();
    for rate in [R441, R48, R882, R96, R1764, R192] {
        let queue = [rate; 4];
        // Adaptive with blends: hard start, then blends; the lookahead sees only the same rate.
        let start = input(
            &c,
            Policy::Adaptive,
            true,
            None,
            rate,
            Transition::Hard,
            &queue,
        );
        assert_eq!(
            plan_output_rate(&start).stream_hz,
            rate,
            "Adaptive start stays native at {rate}"
        );
        for _ in 0..queue.len() {
            let i = input(
                &c,
                Policy::Adaptive,
                true,
                Some(rate),
                rate,
                Transition::Blend,
                &queue,
            );
            assert_eq!(
                plan_output_rate(&i),
                keep(rate),
                "Adaptive homogeneous blend at {rate}"
            );
        }
        // Native (no blends, any policy): every start is Hard at the track's own rate.
        for policy in [Policy::Seamless, Policy::Adaptive] {
            let first = input(&c, policy, false, None, rate, Transition::Hard, &[]);
            assert_eq!(
                plan_output_rate(&first).stream_hz,
                rate,
                "Native start at {rate} ({policy:?})"
            );
            let again = input(&c, policy, false, Some(rate), rate, Transition::Hard, &[]);
            assert_eq!(
                plan_output_rate(&again),
                keep(rate),
                "Native homogeneous hard start at {rate} ({policy:?})"
            );
        }
    }
}

// ---------- named section 3.3 examples (exclusive, ceiling 192 kHz, full caps) ----------

#[test]
fn plan_example_192_to_48_crossfade() {
    let c = full();
    let a = input(
        &c,
        Policy::Seamless,
        true,
        None,
        R192,
        Transition::Hard,
        &[R48],
    );
    expect_plan(&a, R192, true, "192 first start");
    let b = input(
        &c,
        Policy::Seamless,
        true,
        Some(R192),
        R48,
        Transition::Blend,
        &[],
    );
    expect_plan(&b, R192, false, "48 is upsampled into the 192 stream");
}

#[test]
fn plan_example_48_to_192_late_addition() {
    let c = full();
    let a = input(&c, Policy::Seamless, true, None, R48, Transition::Hard, &[]);
    expect_plan(&a, R192, true, "48 starts at the ceiling");
    let b = input(
        &c,
        Policy::Seamless,
        true,
        Some(R192),
        R192,
        Transition::Blend,
        &[],
    );
    expect_plan(&b, R192, false, "192 added late passes through");
}

#[test]
fn plan_example_44_1_to_96_gapless() {
    let c = full();
    let a = input(
        &c,
        Policy::Seamless,
        true,
        None,
        R441,
        Transition::Hard,
        &[R96],
    );
    expect_plan(&a, R192, true, "44.1 start");
    let b = input(
        &c,
        Policy::Seamless,
        true,
        Some(R192),
        R96,
        Transition::Blend,
        &[],
    );
    expect_plan(&b, R192, false, "96 gapless boundary");
}

#[test]
fn plan_example_48_to_352_8_above_ceiling() {
    let c = full();
    let b = input(
        &c,
        Policy::Seamless,
        true,
        Some(R192),
        R3528,
        Transition::Blend,
        &[],
    );
    expect_plan(&b, R192, false, "352.8 is converted down to the ceiling");
    let a = input(
        &c,
        Policy::Seamless,
        true,
        None,
        R3528,
        Transition::Hard,
        &[],
    );
    expect_plan(&a, R192, true, "352.8 hard start is capped at the ceiling");
}

#[test]
fn plan_example_transitions_off_skip_to_88_2() {
    let c = full();
    for policy in [Policy::Seamless, Policy::Adaptive] {
        let i = input(&c, policy, false, Some(R192), R882, Transition::Hard, &[]);
        expect_plan(&i, R882, true, "Native follows the track");
    }
}

#[test]
fn plan_example_native_on_48_only_device() {
    let c = caps(&[R48]);
    let i = input(
        &c,
        Policy::Seamless,
        false,
        None,
        R441,
        Transition::Hard,
        &[],
    );
    expect_plan(&i, R48, true, "44.1 on a 48-only device opens at 48");
}

#[test]
fn plan_example_adaptive_late_higher_rate_resyncs() {
    let c = full();
    let a = input(
        &c,
        Policy::Adaptive,
        true,
        None,
        R48,
        Transition::Hard,
        &[R48],
    );
    expect_plan(&a, R48, true, "Adaptive starts native");
    let b = input(
        &c,
        Policy::Adaptive,
        true,
        Some(R48),
        R192,
        Transition::Blend,
        &[],
    );
    expect_plan(&b, R192, true, "late 192 forces a Resync reopen");
    let d = input(
        &c,
        Policy::Adaptive,
        true,
        Some(R192),
        R48,
        Transition::Blend,
        &[],
    );
    expect_plan(&d, R192, false, "lower track keeps the stream");
}

#[test]
fn plan_example_adaptive_lookahead_picks_max() {
    let c = full();
    let i = input(
        &c,
        Policy::Adaptive,
        true,
        None,
        R441,
        Transition::Hard,
        &[R48, R96],
    );
    expect_plan(&i, R96, true, "target = max(track, upcoming)");
}

#[test]
fn plan_example_shared_mode_uses_mix_rate() {
    let c = full();
    let mut i = input(
        &c,
        Policy::Seamless,
        true,
        Some(R48),
        R192,
        Transition::Blend,
        &[],
    );
    i.backend = Backend::Shared { mix_rate_hz: R48 };
    expect_plan(&i, R48, false, "shared stays at the mix rate");
    i.current_stream_hz = None;
    expect_plan(&i, R48, true, "shared opens at the mix rate");
}
