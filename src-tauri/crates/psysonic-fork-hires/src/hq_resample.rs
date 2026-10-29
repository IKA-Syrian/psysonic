//! HQ resampler stage: one stateful rubato `FftFixedIn` per track, as a rodio `Source`.
//!
//! The fork engine resamples each track once, at the end of its chain, to the output stream's rate, so rodio's
//! own (linear, span-restarting) converter only ever sees audio that is already at the stream rate.
//!
//! - **Passthrough:** when the inner rate equals the target, samples are forwarded untouched (bit-exact, no rubato).
//! - **Timing:** the first `output_delay()` output frames are dropped and, when the inner source ends, the
//!   resampler is flushed with zero input until exactly `round(frames_in * to / from)` frames were emitted. No
//!   added latency, exact length.
//! - **Spans:** input is pulled frame by frame, independently of the inner span boundaries, so the output is
//!   bit-identical however the input is chunked into spans.
//! - **Seek:** forwarded to the inner source; on success the resampler is reset and the frame count restarts at
//!   the seek point, so the length of the remainder stays exact.
//! - **Allocations:** none in steady state; all buffers are allocated when a segment starts.
//!
//! Limitations:
//! - **Inner rate change mid-stream** (varispeed) is detected at chunk refills (and at frame starts in
//!   passthrough). The current resampler is flushed (its tail is emitted, exact length for that segment) and a new
//!   one is built. The change therefore takes effect at the next refill, up to `chunk_frames` input frames late.
//!   The segments are joined without crossfade; the output length is the sum of the segments' lengths.
//! - **Channel count** is fixed at construction. A change in the inner source is a `debug_assert` failure and is
//!   otherwise ignored (the construction-time count keeps being read).

use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

use rodio::source::{SeekError, UniformSourceIterator, Zero};
use rodio::{ChannelCount, SampleRate, Source};
use rubato::{FftFixedIn, Resampler};

pub const DEFAULT_CHUNK_FRAMES: usize = 1024;
/// FFT sub-chunks per input chunk; 1 gives the longest transform and the sharpest filter.
const SUB_CHUNKS: usize = 1;

struct Stage {
    rs: FftFixedIn<f32>,
    from: u32,
    delay: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Reading input and resampling.
    Streaming,
    /// Input is over (inner ended or its rate changed); feeding zeros until `limit` frames were emitted.
    Draining { inner_ended: bool },
}

pub struct HqResample<S: Source> {
    inner: S,
    target: u32,
    chunk_frames: usize,
    channels: NonZeroU16,
    /// `None` while passing through.
    stage: Option<Stage>,
    phase: Phase,
    out_len: usize,
    out_pos: usize,
    ch_cursor: usize,
    skip: usize,
    in_frames: u64,
    emitted: u64,
    limit: u64,
    /// Position inside the current frame while passing through.
    pt_phase: usize,
    done: bool,
}

impl<S: Source> HqResample<S> {
    pub fn new(inner: S, target_rate: NonZeroU32, chunk_frames: usize) -> Self {
        let channels = inner.channels();
        let mut this = Self {
            target: target_rate.get(),
            chunk_frames: chunk_frames.max(16),
            channels,
            stage: None,
            phase: Phase::Streaming,
            out_len: 0,
            out_pos: 0,
            ch_cursor: 0,
            skip: 0,
            in_frames: 0,
            emitted: 0,
            limit: u64::MAX,
            pt_phase: 0,
            done: false,
            inner,
        };
        let rate = this.inner.sample_rate().get();
        this.start_segment(rate);
        this
    }

    fn ch(&self) -> usize {
        self.channels.get() as usize
    }

    fn build_stage(&self, from: u32) -> Stage {
        let ch = self.ch();
        let rs = FftFixedIn::<f32>::new(
            from as usize,
            self.target as usize,
            self.chunk_frames,
            SUB_CHUNKS,
            ch,
        )
        .expect("rates are non-zero");
        let in_bufs = vec![vec![0.0; rs.input_frames_max()]; ch];
        let out_bufs = vec![vec![0.0; rs.output_frames_max()]; ch];
        Stage {
            delay: rs.output_delay(),
            rs,
            from,
            in_bufs,
            out_bufs,
        }
    }

    /// Starts a fresh segment (construction, seek, rate change) for input rate `rate`.
    fn start_segment(&mut self, rate: u32) {
        match &mut self.stage {
            Some(stage) if stage.from == rate => stage.rs.reset(),
            _ => self.stage = (rate != self.target).then(|| self.build_stage(rate)),
        }
        self.phase = Phase::Streaming;
        self.out_len = 0;
        self.out_pos = 0;
        self.ch_cursor = 0;
        self.skip = self.stage.as_ref().map_or(0, |s| s.delay);
        self.in_frames = 0;
        self.emitted = 0;
        self.limit = u64::MAX;
        self.pt_phase = 0;
        self.done = false;
    }

    fn enter_drain(&mut self, inner_ended: bool) {
        let from = self.stage.as_ref().map_or(1, |s| s.from) as u64;
        let to = self.target as u64;
        self.limit = (self.in_frames * to + from / 2) / from;
        self.phase = Phase::Draining { inner_ended };
    }

    /// Produces the next block of output frames into the stage's buffers; false when the source is finished.
    /// Returns true without output when the stream switched to passthrough.
    fn fill(&mut self) -> bool {
        self.out_len = 0;
        self.out_pos = 0;
        self.ch_cursor = 0;
        loop {
            if self.done {
                return false;
            }
            if self.phase == Phase::Streaming {
                let rate = self.inner.sample_rate().get();
                let from = self.stage.as_ref().map_or(self.target, |s| s.from);
                if rate != from {
                    if self.in_frames == 0 {
                        self.start_segment(rate);
                        if self.stage.is_none() {
                            return true;
                        }
                        continue;
                    }
                    self.enter_drain(false);
                    continue;
                }
            }
            if let Phase::Draining { inner_ended } = self.phase {
                if self.emitted >= self.limit {
                    if inner_ended {
                        self.done = true;
                        return false;
                    }
                    let rate = self.inner.sample_rate().get();
                    self.start_segment(rate);
                    if self.stage.is_none() {
                        return true;
                    }
                    continue;
                }
            }
            let channels = self.ch();
            let Some(stage) = self.stage.as_mut() else {
                return true;
            };
            let need = stage.rs.input_frames_next();
            if self.phase == Phase::Streaming {
                let mut got = 0;
                'frames: while got < need {
                    for c in 0..channels {
                        match self.inner.next() {
                            Some(s) => stage.in_bufs[c][got] = s,
                            None => break 'frames,
                        }
                    }
                    got += 1;
                }
                self.in_frames += got as u64;
                for buf in &mut stage.in_bufs {
                    buf[got..need].fill(0.0);
                }
                let (_, n) = stage
                    .rs
                    .process_into_buffer(&stage.in_bufs, &mut stage.out_bufs, None)
                    .expect("buffers are sized by the resampler");
                self.out_len = n;
                if got < need {
                    self.enter_drain(true);
                }
            } else {
                for buf in &mut stage.in_bufs {
                    buf[..need].fill(0.0);
                }
                let (_, n) = stage
                    .rs
                    .process_into_buffer(&stage.in_bufs, &mut stage.out_bufs, None)
                    .expect("buffers are sized by the resampler");
                self.out_len = n;
            }
            let trim = self.skip.min(self.out_len);
            self.skip -= trim;
            self.out_pos = trim;
            if self.out_pos < self.out_len {
                return true;
            }
        }
    }
}

impl<S: Source> Iterator for HqResample<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            let Some(stage) = self.stage.as_ref() else {
                if self.pt_phase == 0 {
                    let rate = self.inner.sample_rate().get();
                    if rate != self.target {
                        self.start_segment(rate);
                        continue;
                    }
                }
                let s = self.inner.next();
                if s.is_some() {
                    self.pt_phase = (self.pt_phase + 1) % self.ch();
                }
                return s;
            };
            if self.out_pos < self.out_len && self.emitted < self.limit {
                let s = stage.out_bufs[self.ch_cursor][self.out_pos];
                self.ch_cursor += 1;
                if self.ch_cursor == self.channels.get() as usize {
                    self.ch_cursor = 0;
                    self.out_pos += 1;
                    self.emitted += 1;
                }
                return Some(s);
            }
            if !self.fill() {
                return None;
            }
        }
    }
}

impl<S: Source> Source for HqResample<S> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZeroU16 {
        debug_assert_eq!(
            self.inner.channels(),
            self.channels,
            "channel count changes are not supported"
        );
        self.channels
    }

    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(self.target).expect("target rate is non-zero")
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        let rate = self.inner.sample_rate().get();
        self.start_segment(rate);
        Ok(())
    }
}

/// The one way the engine queues a track: the HQ stage wrapped in a `UniformSourceIterator` at the stream format.
///
/// rodio's queue keeps reporting the outgoing track's channel count until a span boundary, and the mixer's
/// converter only re-reads the format at span boundaries. A track with a different channel count (mono after
/// stereo) would then be played as if it had the old count, at the wrong speed and pitch. Wrapping each track
/// here makes every queued source report exactly `channels` / `rate` (up- or down-mixing the track's own
/// channels inside the wrapper, at the track's own span boundaries), so the stale metadata is always right.
///
/// For the first track appended to a new queue use [`primed_for_output`]; chained (gapless) tracks use this
/// function without priming.
pub fn for_output<S: Source>(
    source: S,
    channels: ChannelCount,
    rate: SampleRate,
) -> UniformSourceIterator<HqResample<S>> {
    UniformSourceIterator::new(
        HqResample::new(source, rate, DEFAULT_CHUNK_FRAMES),
        channels,
        rate,
    )
}

/// Silence at the stream format that covers the format guess rodio makes when a queue joins the mixer.
///
/// `mixer.add(queue)` (and so `Player::connect_new`) reads the queue's format while its current source is still
/// rodio's `Empty` (mono, 48 kHz) and reads the next 512 queue samples (`threshold`) as that format, whatever
/// is queued. Putting this silence first makes those samples harmless; afterwards the queue reports the
/// stream format, which is the real one. Append it before the mixer pulls its first sample.
pub fn priming_silence(channels: ChannelCount, rate: SampleRate) -> Zero {
    let ch = channels.get() as usize;
    Zero::new_samples(channels, rate, 512usize.div_ceil(ch) * ch)
}

/// Appends [`priming_silence`] to a queue that was just created (or `Player::connect_new`ed, via
/// `player.append(priming_silence(..))`).
pub fn prime_queue(
    input: &rodio::queue::SourcesQueueInput,
    channels: ChannelCount,
    rate: SampleRate,
) {
    input.append(priming_silence(channels, rate));
}

/// Number of priming samples for a stream format: [`priming_silence`]'s 512, rounded up to a whole frame.
fn priming_samples(channels: ChannelCount) -> usize {
    let ch = channels.get() as usize;
    512usize.div_ceil(ch) * ch
}

/// A source that emits the priming silence itself and then delegates to `S`, as one queue entry.
///
/// Why not a separate [`priming_silence`] source ahead of the track: rodio 0.22.2's `Player` applies a pending
/// `try_seek` in the `periodic_access` of whichever source is *current*. With a separate priming source in
/// front, a seek issued on a paused or just-created player (session restore, seek while paused) is consumed by
/// the silence, whose `try_seek` succeeds, and the track then starts from 0. Here the silence and the track are
/// one source, so the seek always reaches the track.
///
/// - Emits [`priming_silence`]'s 512 samples (rounded up to a whole frame) at the stream format, then `S`.
/// - `channels()` / `sample_rate()`: the stream format, which `S` (already [`for_output`]-wrapped) reports too.
/// - `current_span_len()`: the remaining priming length while priming (bounded, so the mixer re-reads the
///   format right after it), then `S`'s.
/// - `total_duration()`: `S`'s.
/// - `try_seek(pos)`: forwards to `S`; on success the remaining priming is dropped (the track starts at `pos`
///   at once). A failed seek leaves the priming in place.
///
/// Use [`primed_for_output`] for the first track appended to a new `Player`/queue only. Chained tracks use
/// plain [`for_output`], which is already sample-exact at the join.
pub struct PrimedSource<S: Source> {
    inner: S,
    remaining: usize,
    channels: ChannelCount,
    rate: SampleRate,
}

impl<S: Source> PrimedSource<S> {
    /// `channels` / `rate` must be the stream format, and `inner` must report exactly that format.
    pub fn new(inner: S, channels: ChannelCount, rate: SampleRate) -> Self {
        Self {
            inner,
            remaining: priming_samples(channels),
            channels,
            rate,
        }
    }

    /// Priming samples still to be emitted.
    pub fn priming_remaining(&self) -> usize {
        self.remaining
    }
}

impl<S: Source> Iterator for PrimedSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.remaining > 0 {
            self.remaining -= 1;
            return Some(0.0);
        }
        self.inner.next()
    }
}

impl<S: Source> Source for PrimedSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        if self.remaining > 0 {
            Some(self.remaining)
        } else {
            self.inner.current_span_len()
        }
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.remaining = 0;
        Ok(())
    }
}

/// [`for_output`] plus the priming silence in one source: the single call for the **first** track appended to a
/// new `Player` / queue. See [`PrimedSource`] for why the priming is part of the track and not a separate source.
pub fn primed_for_output<S: Source>(
    source: S,
    channels: ChannelCount,
    rate: SampleRate,
) -> PrimedSource<UniformSourceIterator<HqResample<S>>> {
    PrimedSource::new(for_output(source, channels, rate), channels, rate)
}
