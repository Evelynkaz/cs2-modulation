//! BC6H block decode to linear-light float RGB, via the `bcdec_rs` crate
//! (`s6f3a2b_hdr.md` change item 1; see `NOTICE.md` for the dependency
//! choice). CS2's texture compiler only ever writes the unsigned variant:
//! `format::VTexFormat::Bc6H` has no separate signed/unsigned format id of
//! its own (`Resource/Enums/VTexFormat.cs`'s `BC6H = 19` is the only entry),
//! and the reference's own decoder table always selects the unsigned block
//! format for it (`Texture.cs:873`'s `TinyBCSharp.BlockFormat.BC6HUf32`),
//! so this decodes unsigned unconditionally, matching the reference.

use crate::error::TexError;

const BLOCK_PIXELS: u32 = 4;
const BLOCK_BYTES: usize = 16;

/// Decodes `data` into `out`, linear RGBA f32 row major, alpha always `1.0`
/// (BC6H carries no alpha channel), writing straight into the caller's
/// buffer rather than building and returning an intermediate one -- a
/// `width x height` BC6H mip's raw bytes can be 16x smaller than its
/// decoded float output (see `mip::hdr_element_count`'s doc comment), so a
/// second full-size allocation here on top of the caller's own output
/// buffer would double the peak memory right at the size this crate caps
/// (`s6f3a2b_hdr.md`'s "avoid the double peak" note). `out` must be exactly
/// `width * height * 4` floats (`debug_assert`ed); `data` must cover at
/// least `width` x `height` rounded up to 4x4 blocks
/// (`format::VTexFormat::Bc6H`'s `block_size()` of 16 bytes/block --
/// `mip::buffer_size_for` already sizes a mip's slice this way).
pub(crate) fn decode_image(
    data: &[u8],
    width: u32,
    height: u32,
    out: &mut [f32],
) -> Result<(), TexError> {
    debug_assert_eq!(out.len(), (width as usize) * (height as usize) * 4);

    let blocks_x = width.div_ceil(BLOCK_PIXELS);
    let blocks_y = height.div_ceil(BLOCK_PIXELS);
    let needed = (blocks_x as usize)
        .checked_mul(blocks_y as usize)
        .and_then(|b| b.checked_mul(BLOCK_BYTES))
        .ok_or(TexError::SizeOverflow)?;
    let data = data.get(..needed).ok_or_else(|| TexError::Truncated {
        detail: format!(
            "BC6H data is {} bytes, need {needed} for {width}x{height}",
            data.len()
        ),
    })?;

    let mut block = [0f32; 4 * 4 * 3];
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let block_index = (by * blocks_x + bx) as usize;
            let compressed = &data[block_index * BLOCK_BYTES..(block_index + 1) * BLOCK_BYTES];
            // Always unsigned -- see this module's doc comment.
            bcdec_rs::bc6h_float(compressed, &mut block, 4 * 3, false);

            for row in 0..BLOCK_PIXELS {
                let y = by * BLOCK_PIXELS + row;
                if y >= height {
                    break;
                }
                for col in 0..BLOCK_PIXELS {
                    let x = bx * BLOCK_PIXELS + col;
                    if x >= width {
                        continue;
                    }
                    let src = ((row * 4 + col) * 3) as usize;
                    let dst = ((y * width + x) * 4) as usize;
                    out[dst] = block[src];
                    out[dst + 1] = block[src + 1];
                    out[dst + 2] = block[src + 2];
                    out[dst + 3] = 1.0;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_block_decodes_to_black() {
        let data = [0u8; BLOCK_BYTES];
        let mut rgba = vec![0f32; 4 * 4 * 4];
        decode_image(&data, 4, 4, &mut rgba).unwrap();
        assert_eq!(rgba.len(), 4 * 4 * 4);
        for px in rgba.as_chunks::<4>().0 {
            assert_eq!(px, &[0.0, 0.0, 0.0, 1.0]);
        }
    }

    /// Cross-checks against `texture2ddecoder::decode_bc6_block_unsigned`
    /// (already a dependency of this crate, used for the LDR BCn formats):
    /// both are independent implementations of the same D3D11/Khronos BC6H
    /// spec, so tone-mapping our float output the same way that decoder
    /// clamps to `u8` (`(v.clamp(0,1) * 255.0) as u8`, matching its own
    /// `f32_to_u8`) should land on the same byte, for arbitrary block input
    /// (`s6f3a2b_hdr.md`'s "известные блоки ... из тестов выбранной
    /// библиотеки").
    #[test]
    fn matches_texture2ddecoder_clamped_output_on_arbitrary_blocks() {
        let mut state = 0x1234_5678u32;
        let mut next_u32 = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };

        for _ in 0..64 {
            let mut compressed = [0u8; BLOCK_BYTES];
            for chunk in compressed.chunks_mut(4) {
                chunk.copy_from_slice(&next_u32().to_le_bytes());
            }

            let mut ours = vec![0f32; 4 * 4 * 4];
            decode_image(&compressed, 4, 4, &mut ours).unwrap();
            let mut theirs = [0u32; 16];
            texture2ddecoder::decode_bc6_block_unsigned(&compressed, &mut theirs);

            for i in 0..16 {
                let [b, g, r, _a] = theirs[i].to_le_bytes();
                let ours_px = &ours[i * 4..i * 4 + 3];
                for (&ours_c, theirs_c) in ours_px.iter().zip([r, g, b]) {
                    let clamped = (ours_c.clamp(0.0, 1.0) * 255.0) as u8;
                    assert!(
                        (i16::from(clamped) - i16::from(theirs_c)).abs() <= 1,
                        "compressed={compressed:?} pixel {i}: ours(clamped)={clamped} theirs={theirs_c}"
                    );
                }
            }
        }
    }

    #[test]
    fn non_multiple_of_4_dimensions_do_not_panic() {
        // 6x6 rounds up to 2x2 blocks (8x8).
        let data = [0u8; BLOCK_BYTES * 4];
        let mut rgba = vec![0f32; 6 * 6 * 4];
        decode_image(&data, 6, 6, &mut rgba).unwrap();
        assert_eq!(rgba.len(), 6 * 6 * 4);
    }

    #[test]
    fn truncated_data_errors_not_panics() {
        let mut out = vec![0f32; 4 * 4 * 4];
        assert!(decode_image(&[0u8; 8], 4, 4, &mut out).is_err());
    }

    /// A malformed/adversarial BC6H block still decodes to *some* colour
    /// under the format's own bit-unpacking rules (there is no invalid bit
    /// pattern to reject at the block level -- see this module's doc
    /// comment on `bcdec_rs` being fuzz-tested against arbitrary input); the
    /// crate-level "hostile input is an error, not a panic" guarantee comes
    /// from the buffer-length check above, exercised here with the full
    /// range of block content.
    #[test]
    fn arbitrary_bit_patterns_never_panic() {
        let mut state = 0xdead_beefu32;
        let mut next_u32 = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for _ in 0..256 {
            let mut data = [0u8; BLOCK_BYTES * 4]; // 8x8 = 2x2 blocks
            for chunk in data.chunks_mut(4) {
                chunk.copy_from_slice(&next_u32().to_le_bytes());
            }
            let mut rgba = vec![0f32; 8 * 8 * 4];
            decode_image(&data, 8, 8, &mut rgba).unwrap();
            assert_eq!(rgba.len(), 8 * 8 * 4);
        }
    }
}
