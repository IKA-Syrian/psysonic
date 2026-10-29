//! Period maths in WASAPI's 100 ns units (`REFERENCE_TIME`).

/// 10 ms.
pub const DEFAULT_PERIOD_HNS: i64 = 100_000;
const HNS_PER_SEC: i128 = 10_000_000;

/// Frames in `period_hns` at `rate_hz`, rounded to the nearest frame (halves up).
pub fn frames_for_period(rate_hz: u32, period_hns: i64) -> u32 {
    let n = rate_hz as i128 * period_hns.max(0) as i128;
    ((n + HNS_PER_SEC / 2) / HNS_PER_SEC).min(u32::MAX as i128) as u32
}

/// Duration of `frames` at `rate_hz` in 100 ns units, rounded to the nearest unit (halves up).
/// Returns 0 for a zero rate.
pub fn period_for_frames(rate_hz: u32, frames: u32) -> i64 {
    if rate_hz == 0 {
        return 0;
    }
    ((frames as i128 * HNS_PER_SEC + rate_hz as i128 / 2) / rate_hz as i128) as i64
}

/// The smallest frame count `>= frames` whose size in bytes (`count * block_align_bytes`) is a multiple of
/// `align_bytes`. Always rounds up, never down; an already aligned count is returned unchanged. If either byte
/// argument is 0 there is nothing to align and `frames` is returned.
pub fn align_frames(frames: u32, block_align_bytes: u32, align_bytes: u32) -> u32 {
    if block_align_bytes == 0 || align_bytes == 0 {
        return frames;
    }
    let (mut a, mut b) = (align_bytes, block_align_bytes);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    // Frame counts that are multiples of `step` have byte sizes that are multiples of `align_bytes`.
    let step = align_bytes / a;
    frames.div_ceil(step) * step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_for_ten_ms() {
        assert_eq!(frames_for_period(48_000, DEFAULT_PERIOD_HNS), 480);
        assert_eq!(frames_for_period(96_000, DEFAULT_PERIOD_HNS), 960);
        assert_eq!(frames_for_period(192_000, DEFAULT_PERIOD_HNS), 1920);
        assert_eq!(frames_for_period(44_100, DEFAULT_PERIOD_HNS), 441);
        assert_eq!(frames_for_period(88_200, DEFAULT_PERIOD_HNS), 882);
        assert_eq!(frames_for_period(44_100, 0), 0);
    }

    #[test]
    fn period_for_frames_values() {
        assert_eq!(period_for_frames(48_000, 480), 100_000);
        assert_eq!(period_for_frames(44_100, 441), 100_000);
        // 448 / 44100 s = 101587.30... hns
        assert_eq!(period_for_frames(44_100, 448), 101_587);
        assert_eq!(period_for_frames(0, 10), 0);
    }

    #[test]
    fn roundtrip_is_close() {
        for rate in [44_100, 48_000, 88_200, 96_000, 176_400, 192_000] {
            let f = frames_for_period(rate, DEFAULT_PERIOD_HNS);
            let back = period_for_frames(rate, f);
            assert!((back - DEFAULT_PERIOD_HNS).abs() <= 10_000_000 / rate as i64 + 1);
        }
    }

    #[test]
    fn align_s24_stereo_44k1() {
        // 6-byte frames, 128-byte alignment: multiples of 64 frames (384 bytes = 3 * 128).
        let f = align_frames(441, 6, 128);
        assert_eq!(f, 448);
        assert_eq!(f * 6 % 128, 0);
        assert_eq!(align_frames(448, 6, 128), 448);
        assert_eq!(align_frames(449, 6, 128), 512);
    }

    #[test]
    fn align_other_block_sizes() {
        // 8-byte frames (S32 stereo / F32 stereo): multiples of 16.
        assert_eq!(align_frames(480, 8, 128), 480);
        assert_eq!(align_frames(441, 8, 128), 448);
        // 4-byte frames (S16 stereo): multiples of 32.
        assert_eq!(align_frames(441, 4, 128), 448);
        assert_eq!(align_frames(0, 4, 128), 0);
        // 3-byte frames (S24 mono): multiples of 128.
        assert_eq!(align_frames(441, 3, 128), 512);
        // 7-byte frames: coprime with 128, so multiples of 128.
        assert_eq!(align_frames(1, 7, 128), 128);
        assert_eq!(align_frames(5, 6, 0), 5);
    }

    #[test]
    fn align_is_minimal() {
        for block in [2u32, 3, 4, 6, 8, 12, 16] {
            for frames in 0..600u32 {
                let a = align_frames(frames, block, 128);
                assert!(a >= frames);
                assert_eq!(a * block % 128, 0);
                if a > frames {
                    assert!((frames..a).all(|f| f * block % 128 != 0));
                }
            }
        }
    }
}
