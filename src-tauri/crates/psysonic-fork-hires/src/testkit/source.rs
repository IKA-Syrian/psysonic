//! Test sources.

use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

use rodio::Source;

/// Serves mono samples in fixed-size packets, the way `SizedDecoder` does. `current_span_len` is the whole
/// packet's length (rodio's contract: total span size, not what is left), and rodio reads it once per span.
pub struct Spans<'a> {
    samples: std::iter::Copied<std::slice::Iter<'a, f32>>,
    rate: NonZeroU32,
    packet: usize,
}

impl<'a> Spans<'a> {
    /// `rate` is the reported sample rate in Hz and `packet` the span length in samples. Panics if `rate` is 0.
    pub fn new(samples: &'a [f32], rate: u32, packet: usize) -> Self {
        Self {
            samples: samples.iter().copied(),
            rate: NonZeroU32::new(rate).expect("rate must be non-zero"),
            packet,
        }
    }
}

impl Iterator for Spans<'_> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.samples.next()
    }
}

impl Source for Spans<'_> {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.packet)
    }

    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::MIN
    }

    fn sample_rate(&self) -> NonZeroU32 {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Like [`Spans`], but reports `current_span_len() == None` (unbounded), as the real streaming decoder does.
pub struct Unbounded<'a> {
    samples: std::iter::Copied<std::slice::Iter<'a, f32>>,
    rate: NonZeroU32,
    channels: NonZeroU16,
}

impl<'a> Unbounded<'a> {
    /// Mono source at `rate` Hz. Panics if `rate` is 0.
    pub fn new(samples: &'a [f32], rate: u32) -> Self {
        Self::with_channels(samples, rate, 1)
    }

    /// Interleaved source with `channels` channels. Panics if `rate` or `channels` is 0.
    pub fn with_channels(samples: &'a [f32], rate: u32, channels: u16) -> Self {
        Self {
            samples: samples.iter().copied(),
            rate: NonZeroU32::new(rate).expect("rate must be non-zero"),
            channels: NonZeroU16::new(channels).expect("channels must be non-zero"),
        }
    }
}

impl Iterator for Unbounded<'_> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.samples.next()
    }
}

impl Source for Unbounded<'_> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZeroU16 {
        self.channels
    }

    fn sample_rate(&self) -> NonZeroU32 {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
