use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rodio::source::Zero;
use rodio::{ChannelCount, Player, SampleRate};

use super::{fade_out_total_samples, swap_in_new_sink, SinkSwapInputs};
use crate::stream_idle::tests::minimal_engine;

#[test]
fn fade_total_is_secs_times_rate_times_channels() {
    assert_eq!(fade_out_total_samples(4.0, 192_000, 2), 1_536_000);
    assert_eq!(fade_out_total_samples(4.0, 48_000, 6), 1_152_000);
}

/// Crossfade 192 kHz stereo (A) -> 48 kHz stereo (B): by the time the swap runs
/// `audio_play` has already stored B's rate in `current_sample_rate`. A's fade
/// must still be armed at A's rate (R6: it used B's, so A faded 4x too fast).
#[tokio::test]
async fn outgoing_fade_uses_outgoing_rate_not_incoming() {
    let engine = minimal_engine();

    let (old_player, _old_out) = Player::new();
    old_player.append(Zero::new(
        ChannelCount::new(2).unwrap(),
        SampleRate::new(192_000).unwrap(),
    ));
    let old_trigger = Arc::new(AtomicBool::new(false));
    let old_samples = Arc::new(AtomicU64::new(0));
    {
        let mut cur = engine.current.lock().unwrap();
        cur.sink = Some(Arc::new(old_player));
        cur.fadeout_trigger = Some(old_trigger.clone());
        cur.fadeout_samples = Some(old_samples.clone());
    }
    // State already holds the INCOMING track's format.
    engine.current_sample_rate.store(48_000, Ordering::Relaxed);
    engine.current_channels.store(2, Ordering::Relaxed);

    let (new_player, _new_out) = Player::new();
    swap_in_new_sink(
        &engine,
        SinkSwapInputs {
            generation: 1,
            sink: Arc::new(new_player),
            duration_secs: 10.0,
            volume: 1.0,
            gain_linear: 1.0,
            fadeout_trigger: Arc::new(AtomicBool::new(false)),
            fadeout_samples: Arc::new(AtomicU64::new(0)),
            crossfade_enabled: true,
            actual_fade_secs: 4.0,
            outgoing_fade_secs: 4.0,
            outgoing_rate: 192_000,
            outgoing_channels: 2,
            start_paused: false,
            streaming_seek: None,
        },
    );

    assert!(old_trigger.load(Ordering::SeqCst));
    assert_eq!(old_samples.load(Ordering::SeqCst), 4 * 192_000 * 2);
}
