//! `PASSTHROUGH_ACTIVE` is process-global, so this lives in its own test binary with a single test.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::Ordering;

use psysonic_fork_hires::hq_resample::{HqResample, PASSTHROUGH_ACTIVE};
use rodio::buffer::SamplesBuffer;

fn buf(rate: u32) -> SamplesBuffer {
    SamplesBuffer::new(
        NonZeroU16::new(2).unwrap(),
        NonZeroU32::new(rate).unwrap(),
        vec![0.25f32; 4096],
    )
}

#[test]
fn flag_is_set_on_the_first_sample_of_a_segment() {
    let target = NonZeroU32::new(48_000).unwrap();
    PASSTHROUGH_ACTIVE.store(false, Ordering::SeqCst);

    // Built but not yet playing: the flag is untouched.
    let mut same = HqResample::new(buf(48_000), target, 1024);
    assert!(!PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));
    same.next();
    assert!(PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));

    // A chained resampling segment built while the first one plays does not change it until it is audible.
    let mut other = HqResample::new(buf(44_100), target, 1024);
    assert!(PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));
    for _ in 0..16 {
        same.next();
    }
    assert!(PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));
    other.next();
    assert!(!PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));

    // Later samples do not rewrite it.
    PASSTHROUGH_ACTIVE.store(true, Ordering::SeqCst);
    other.next();
    assert!(PASSTHROUGH_ACTIVE.load(Ordering::SeqCst));
}
