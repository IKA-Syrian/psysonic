//! Is upstream's per-track chain an identity when every DSP stage is off? These tests build a source through
//! the real `decode::builders::build_source` with the arguments a normal (non-crossfade, non-gapless) `audio_play`
//! passes, from a synthetic 24-bit WAV, and compare it bit for bit with the raw decoder output. The second test
//! adds the fork's queue stage, the rodio `Player` and mixer, and the exclusive formatter, and expects the 24-bit
//! codes of the file back.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Duration;

use psysonic_fork_hires::exclusive::render::MASTER_HEADROOM;
use psysonic_fork_hires::format::{Formatter, OutputFormat};
use rodio::mixer::mixer;
use rodio::{ChannelCount, Player, SampleRate};

use crate::decode::{build_source, SizedDecoder};
use crate::fork_hires::first_source;
use crate::playback_rate::PlaybackRateAtomics;

const RATE: u32 = 48_000;
const SECS: usize = 2;

/// 2 s, 1 kHz at -6 dBFS (amplitude 0.5 of full scale), stereo with a phase offset on the right channel; the
/// 24-bit codes interleaved.
fn tone_codes() -> Vec<i32> {
    let mut v = Vec::with_capacity(RATE as usize * SECS * 2);
    for n in 0..RATE as usize * SECS {
        for phase in [0.0f64, 0.7] {
            let t = n as f64 / RATE as f64;
            let x = 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t + phase).sin();
            v.push((x * 8_388_608.0).round() as i32);
        }
    }
    v
}

/// 24-bit PCM WAV of the interleaved codes.
fn wav24(codes: &[i32], rate: u32, ch: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(codes.len() * 3);
    for k in codes {
        data.extend_from_slice(&k.to_le_bytes()[..3]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * ch as u32 * 3).to_le_bytes());
    out.extend_from_slice(&(ch * 3).to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// What a normal `audio_play` (no crossfade, no gapless, EQ off, no speed change, native decode) hands to
/// `build_source`: a 5 ms micro-fade-in, `resample_target_hz = 0`, a stereo device, `hi_res` off.
fn built(data: Vec<u8>) -> crate::decode::BuiltSource {
    build_source(
        data,
        SECS as f64,
        Arc::new(std::array::from_fn::<_, 10, _>(|_| {
            AtomicU32::new(0f32.to_bits())
        })),
        Arc::new(AtomicBool::new(false)), // EQ off
        Arc::new(AtomicU32::new(0f32.to_bits())),
        PlaybackRateAtomics::new(), // speed 1.0
        Arc::new(AtomicBool::new(false)),
        Duration::from_millis(5),
        Arc::new(AtomicU64::new(0)),
        0,
        2,
        Some("wav"),
        false,
    )
    .expect("synthetic WAV decodes")
}

fn first_diff(a: &[f32], b: &[f32]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x.to_bits() != y.to_bits())
}

#[test]
fn upstream_chain_is_an_identity_with_dsp_off() {
    let _globals = crate::spectrum::tests::lock_globals();
    let codes = tone_codes();
    let file = wav24(&codes, RATE, 2);
    let want: Vec<f32> = codes.iter().map(|&k| k as f32 / 8_388_608.0).collect();

    // The raw decoder output: must itself be the file's 24-bit codes.
    let raw: Vec<f32> = SizedDecoder::new(file.clone(), Some("wav"), false)
        .expect("decoder")
        .collect();
    assert_eq!(raw.len(), want.len(), "decoder length");
    assert_eq!(
        first_diff(&raw, &want),
        None,
        "the decoder must return k / 2^23 exactly"
    );

    // The chain: decode > rate > EQ (off) > fade-in (5 ms) > fade-out (idle) > spectrum tap > counting.
    let chain: Vec<f32> = built(file).source.collect();
    assert_eq!(chain.len(), raw.len(), "chain length");
    let skip = RATE as usize * 2; // one second of stereo samples
    let d = first_diff(&chain[skip..], &raw[skip..]);
    assert_eq!(
        d.map(|i| (i + skip, chain[i + skip], raw[i + skip])),
        None,
        "samples after the first second differ from the raw decode"
    );
    // The 5 ms fade-in is the only expected difference, and only at the start (this also shows the
    // comparison is sensitive: the fade does change the first samples).
    let fade_samples = (RATE as usize * 5 / 1000) * 2;
    assert!(first_diff(&chain[..fade_samples], &raw[..fade_samples]).is_some());
    assert_eq!(
        first_diff(&chain[fade_samples..], &raw[fade_samples..]),
        None,
        "everything after the 5 ms fade-in is identical"
    );
}

#[test]
fn engine_path_to_the_formatter_returns_the_file_codes() {
    let _globals = crate::spectrum::tests::lock_globals();
    let codes = tone_codes();
    let file = wav24(&codes, RATE, 2);
    let src = built(file).source;
    let fmt = (
        ChannelCount::new(2).unwrap(),
        SampleRate::new(RATE).unwrap(),
    );
    let (controller, output) = mixer(fmt.0, fmt.1);
    let player = Player::connect_new(&controller);
    player.set_volume(MASTER_HEADROOM);
    player.append(first_source(src, fmt));
    let mut rendered = Vec::new();
    for s in output {
        rendered.push(s);
        if player.empty() || rendered.len() > 4 * RATE as usize * 2 {
            break;
        }
    }
    // 512 samples of queue lead-in plus 512 of priming silence.
    const PRIMING: usize = 1024;
    assert!(rendered[..PRIMING].iter().all(|&x| x == 0.0), "priming");

    // The exclusive render loop's formatter with the gate on.
    let mut f = Formatter::new(OutputFormat::S24Packed, 7);
    f.set_limiter(false);
    f.set_dither(false);
    f.set_input_gain(1.0 / MASTER_HEADROOM as f64);
    let body = &rendered[PRIMING..PRIMING + codes.len()];
    let mut bytes = vec![0u8; body.len() * 3];
    f.write_le(body, &mut bytes);
    let got: Vec<i32> = bytes
        .as_chunks::<3>()
        .0
        .iter()
        .map(|b| (((b[2] as i8) as i32) << 16) | ((b[1] as i32) << 8) | b[0] as i32)
        .collect();
    let skip = RATE as usize * 2;
    let bad = got[skip..]
        .iter()
        .zip(&codes[skip..])
        .position(|(g, c)| g != c)
        .map(|i| (i + skip, got[i + skip], codes[i + skip]));
    assert_eq!(
        bad, None,
        "first differing code after 1 s (index, got, want)"
    );
}

/// The whole engine path offline: chain, CRC tap, HQ stage (passthrough), `Player` at the master headroom, the
/// mixer and the exclusive render loop with the gate on. The tap's check must report a match.
#[test]
fn crc_tap_matches_through_the_chain_player_mixer_and_render_loop() {
    use psysonic_fork_hires::exclusive::crc_tap::{CrcBoard, CrcResult, CrcTapSource, TapConfig};
    use psysonic_fork_hires::exclusive::gate::GateState;
    use psysonic_fork_hires::exclusive::render::{
        Clock, RenderConfig, RenderLoop, RenderSink, SystemClock, WaitResult,
    };
    use psysonic_fork_hires::format::DitherMode;

    struct Null(usize);
    impl RenderSink for Null {
        fn wait(&mut self) -> WaitResult {
            if self.0 == 0 {
                return WaitResult::Stop;
            }
            self.0 -= 1;
            WaitResult::Ready
        }
        fn frames_available(&self) -> usize {
            480
        }
        fn write(&mut self, _: &[u8], _: usize) -> Result<(), String> {
            Ok(())
        }
    }

    let _globals = crate::spectrum::tests::lock_globals();
    let file = wav24(&tone_codes(), RATE, 2);
    let board = Arc::new(CrcBoard::new(true));
    board.set_window_frames(40_000);
    let tap = CrcTapSource::with_board(
        built(file).source,
        board.clone(),
        TapConfig {
            skip_frames: None,
            window_frames: 40_000,
        },
    );
    let seq = tap.seq();
    let fmt = (
        ChannelCount::new(2).unwrap(),
        SampleRate::new(RATE).unwrap(),
    );
    let (controller, output) = mixer(fmt.0, fmt.1);
    let player = Player::connect_new(&controller);
    player.set_volume(MASTER_HEADROOM);
    player.append(psysonic_fork_hires::hq_resample::primed_for_output(
        tap, fmt.0, fmt.1,
    ));
    let gate = GateState::new();
    gate.publish(true, SystemClock::new().now_ms());
    let cfg = RenderConfig {
        channels: 2,
        rate_hz: RATE,
        format: OutputFormat::S24Packed,
        source_bits: 24,
        dither: DitherMode::Auto,
        lead_in_ms: 0,
        period_ms: 10,
        max_frames: 480,
        gate_max_age_ms: 3_600_000,
        seed: 7,
    };
    let mut rl = RenderLoop::new(cfg, output, SystemClock::new()).with_crc_board(board.clone());
    rl.run(&mut Null(260), &gate).unwrap();
    assert_eq!(board.result(seq), Some(CrcResult::Match { frames: 40_000 }));
}
