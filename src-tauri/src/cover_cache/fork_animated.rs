//! Fork patch: animated covers. A song can carry an animated WebP as its
//! embedded cover (Apple/Tidal motion art); Navidrome hands it out unchanged.
//! The cache normally decodes every cover and writes still WebP tiers, which
//! keeps only the first frame, so tiers from `MIN_TIER` up store the server's
//! bytes as they are. The 128 px tier (dense lists, search) stays still.

use std::path::Path;

use super::disk::{tier_exists, tier_path, DERIVE_TIERS};

pub(super) const MIN_TIER: u32 = 256;

/// True for a WebP whose extended header (`VP8X`) has the animation flag set.
pub(super) fn is_animated_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 21
        && &bytes[0..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && &bytes[12..16] == b"VP8X"
        && bytes[20] & 0x02 != 0
}

/// Whether `tier` should keep the animated original instead of a still frame.
pub(super) fn keeps_animation(tier: u32) -> bool {
    tier >= MIN_TIER
}

/// Write the original animated bytes for `tier`.
pub(super) fn write_tier(dir: &Path, tier: u32, bytes: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(tier_path(dir, tier), bytes).map_err(|e| e.to_string())
}

/// Fill the derive tiers (up to 800) that keep animation and are not on disk
/// yet, so the background derive pass does not write still copies of them.
pub(super) fn fill_derive_tiers(dir: &Path, bytes: &[u8]) -> Result<(), String> {
    for &tier in DERIVE_TIERS.iter().filter(|t| keeps_animation(**t)) {
        if tier_exists(dir, tier).is_none() {
            write_tier(dir, tier, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp8x(flags: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(b"WEBPVP8X");
        b.extend_from_slice(&[10, 0, 0, 0, flags, 0, 0, 0]);
        b
    }

    #[test]
    fn detects_the_animation_flag_only() {
        assert!(is_animated_webp(&vp8x(0x02)));
        assert!(is_animated_webp(&vp8x(0x12)));
        assert!(!is_animated_webp(&vp8x(0x10)));
        assert!(!is_animated_webp(b"RIFF\0\0\0\0WEBPVP8 still image"));
        assert!(!is_animated_webp(b"\xff\xd8\xff\xe0 jpeg"));
    }

    #[test]
    fn small_list_tier_stays_still() {
        assert!(!keeps_animation(128));
        assert!(keeps_animation(256));
        assert!(keeps_animation(2000));
    }

    #[test]
    fn fills_only_missing_animated_derive_tiers() {
        let dir = crate::cover_cache::test_support::fresh_tmpdir("fork-animated");
        std::fs::write(tier_path(&dir, 512), b"existing").unwrap();
        fill_derive_tiers(&dir, b"anim").unwrap();
        assert!(tier_exists(&dir, 128).is_none());
        assert_eq!(std::fs::read(tier_path(&dir, 256)).unwrap(), b"anim");
        assert_eq!(std::fs::read(tier_path(&dir, 512)).unwrap(), b"existing");
        assert_eq!(std::fs::read(tier_path(&dir, 800)).unwrap(), b"anim");
    }
}
