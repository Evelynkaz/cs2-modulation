//! Picks the largest mip that fits a pixel budget without resampling
//! (`Renderer/Materials/MaterialLoader.cs:346-361`).

use crate::header::Header;
use crate::mip::mip_level_size;

/// The smallest mip level (`0` = full resolution) whose width and height
/// are both `<= max_side`, or the smallest mip available if none are.
/// Mirrors `MaterialLoader.cs`'s `while (... texWidth > max || texHeight >
/// max) { level++; texWidth >>= 1; texHeight >>= 1; }`, but steps through
/// `mip_level_size` (floor, minimum 1) instead of a raw `>>= 1`, so it can't
/// land on a `0` dimension along the way.
pub fn pick_mip_for_budget(header: &Header, max_side: u32) -> u32 {
    let mut level = 0u32;
    let last_level = u32::from(header.num_mip_levels).saturating_sub(1);
    while level < last_level {
        let width = mip_level_size(u32::from(header.width), level);
        let height = mip_level_size(u32::from(header.height), level);
        if width <= max_side && height <= max_side {
            break;
        }
        level += 1;
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::VTexFormat;
    use crate::header::VTexFlags;

    fn header(width: u16, height: u16, mips: u8) -> Header {
        Header {
            version: 1,
            flags: VTexFlags(0),
            width,
            height,
            depth: 1,
            format: VTexFormat::Bc7,
            num_mip_levels: mips,
            reflectivity: [1.0, 1.0, 1.0, 1.0],
            metadata: None,
            is_actually_compressed_mips: false,
            compressed_mip_sizes: None,
            has_sheet: false,
        }
    }

    #[test]
    fn picks_mip_0_when_already_within_budget() {
        let h = header(512, 512, 10);
        assert_eq!(pick_mip_for_budget(&h, 1024), 0);
    }

    #[test]
    fn picks_the_first_mip_that_fits() {
        // 4096 -> 2048 -> 1024 <= 1024: two halvings, level 2.
        let h = header(4096, 4096, 13);
        assert_eq!(pick_mip_for_budget(&h, 1024), 2);
    }

    #[test]
    fn falls_back_to_the_smallest_mip_if_none_fit() {
        let h = header(4096, 4096, 2); // only levels 0 (4096) and 1 (2048)
        assert_eq!(pick_mip_for_budget(&h, 64), 1);
    }

    #[test]
    fn single_mip_texture_always_picks_level_0() {
        let h = header(4096, 4096, 1);
        assert_eq!(pick_mip_for_budget(&h, 64), 0);
    }
}
