//! Picks the sample format for an exclusive-mode stream from what the device accepts.

use crate::format::OutputFormat;
use crate::rate_plan::DeviceCaps;

/// The formats and rates a device accepts in exclusive mode, for one channel count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveCaps {
    pub channels: u16,
    /// Each format with the rates (Hz) it was accepted at.
    pub by_format: Vec<(OutputFormat, Vec<u32>)>,
}

impl ExclusiveCaps {
    /// Sorted, deduplicated union of all rates.
    pub fn rates(&self) -> Vec<u32> {
        let mut all: Vec<u32> = self
            .by_format
            .iter()
            .flat_map(|(_, r)| r.iter().copied())
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// The union of rates as planner input; `None` when the device accepts nothing.
    pub fn device_caps(&self) -> Option<DeviceCaps> {
        DeviceCaps::new(self.rates())
    }

    /// Formats accepted at `rate_hz`.
    fn formats_at(&self, rate_hz: u32) -> impl Iterator<Item = OutputFormat> + '_ {
        self.by_format
            .iter()
            .filter(move |(_, r)| r.contains(&rate_hz))
            .map(|(f, _)| *f)
    }
}

/// Lower is better: S32 (24-in-32) > S24 packed > S16 > F32.
fn priority(f: OutputFormat) -> u8 {
    match f {
        OutputFormat::S32 { .. } => 0,
        OutputFormat::S24Packed => 1,
        OutputFormat::S16 => 2,
        OutputFormat::F32 => 3,
    }
}

/// The best format available at `rate_hz` for a source of `source_bits` bits.
///
/// Among formats accepted at that rate, those at least as deep as the source are preferred, ranked
/// S32 > S24Packed > S16 > F32. If none is deep enough, the deepest available wins (ties by the same ranking).
/// `None` if the device accepts nothing at that rate.
pub fn negotiate(caps: &ExclusiveCaps, rate_hz: u32, source_bits: u32) -> Option<OutputFormat> {
    let deep_enough = caps
        .formats_at(rate_hz)
        .filter(|f| f.valid_bits() as u32 >= source_bits)
        .min_by_key(|f| (priority(*f), std::cmp::Reverse(f.valid_bits())));
    deep_enough.or_else(|| {
        caps.formats_at(rate_hz)
            .min_by_key(|f| (std::cmp::Reverse(f.valid_bits()), priority(*f)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S32: OutputFormat = OutputFormat::S32 { valid_bits: 24 };
    const S24: OutputFormat = OutputFormat::S24Packed;
    const S16: OutputFormat = OutputFormat::S16;
    const F32: OutputFormat = OutputFormat::F32;

    fn caps(by_format: Vec<(OutputFormat, Vec<u32>)>) -> ExclusiveCaps {
        ExclusiveCaps {
            channels: 2,
            by_format,
        }
    }

    /// Probed: "Arctis Nova Pro", exclusive mode, 48 kHz only.
    fn arctis() -> ExclusiveCaps {
        caps(vec![(S24, vec![48_000]), (S16, vec![48_000])])
    }

    /// Probed: "Steam speakers", exclusive mode.
    fn steam() -> ExclusiveCaps {
        let r = vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        caps(vec![(S24, r.clone()), (S16, r)])
    }

    #[test]
    fn priority_order() {
        let c = caps(vec![
            (F32, vec![48_000]),
            (S16, vec![48_000]),
            (S24, vec![48_000]),
            (S32, vec![48_000]),
        ]);
        assert_eq!(negotiate(&c, 48_000, 16), Some(S32));
        let c = caps(vec![
            (F32, vec![48_000]),
            (S16, vec![48_000]),
            (S24, vec![48_000]),
        ]);
        assert_eq!(negotiate(&c, 48_000, 16), Some(S24));
        let c = caps(vec![(F32, vec![48_000]), (S16, vec![48_000])]);
        assert_eq!(negotiate(&c, 48_000, 16), Some(S16));
        let c = caps(vec![(F32, vec![48_000])]);
        assert_eq!(negotiate(&c, 48_000, 16), Some(F32));
    }

    #[test]
    fn depth_floor() {
        // Only S16 at this rate: a 16-bit source uses it, and so does a 24-bit one (deepest available).
        let c = caps(vec![(S24, vec![96_000]), (S16, vec![44_100])]);
        assert_eq!(negotiate(&c, 44_100, 16), Some(S16));
        assert_eq!(negotiate(&c, 44_100, 24), Some(S16));
        // Both available: a 24-bit source takes S24; a 16-bit one also takes the higher-priority S24.
        assert_eq!(negotiate(&arctis(), 48_000, 24), Some(S24));
        assert_eq!(negotiate(&arctis(), 48_000, 16), Some(S24));
    }

    #[test]
    fn deepest_when_nothing_is_deep_enough() {
        let c = caps(vec![
            (S16, vec![48_000]),
            (OutputFormat::S32 { valid_bits: 20 }, vec![48_000]),
        ]);
        assert_eq!(
            negotiate(&c, 48_000, 24),
            Some(OutputFormat::S32 { valid_bits: 20 })
        );
    }

    #[test]
    fn missing_rate_is_none() {
        assert_eq!(negotiate(&arctis(), 44_100, 16), None);
        assert_eq!(negotiate(&arctis(), 96_000, 24), None);
        assert_eq!(negotiate(&caps(vec![]), 48_000, 16), None);
    }

    #[test]
    fn arctis_fixture() {
        let c = arctis();
        assert_eq!(c.rates(), vec![48_000]);
        assert_eq!(c.device_caps().unwrap().rates_hz(), &[48_000]);
        assert_eq!(negotiate(&c, 48_000, 24), Some(S24));
    }

    #[test]
    fn steam_fixture() {
        let c = steam();
        assert_eq!(c.rates().len(), 6);
        assert!(c.device_caps().unwrap().supports(176_400));
        for r in c.rates() {
            assert_eq!(negotiate(&c, r, 24), Some(S24));
        }
        assert_eq!(negotiate(&c, 32_000, 16), None);
    }

    #[test]
    fn rates_is_sorted_union() {
        let c = caps(vec![
            (S16, vec![96_000, 44_100]),
            (S24, vec![44_100, 48_000]),
        ]);
        assert_eq!(c.rates(), vec![44_100, 48_000, 96_000]);
        assert!(caps(vec![]).device_caps().is_none());
    }
}
