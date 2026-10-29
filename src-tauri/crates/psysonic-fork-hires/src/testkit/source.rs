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
