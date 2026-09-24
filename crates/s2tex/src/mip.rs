//! Mip level dimensions, per-mip buffer sizes, and extracting one mip's
//! bytes from the smallest-to-largest on-disk layout, including the
//! optional per-mip LZ4 compression (Texture.cs:944-1077).

use crate::error::TexError;
use crate::format::VTexFormat;
use crate::header::Header;

/// Sanity cap on a single mip's decompressed buffer, matching the spirit of
/// `s2fmt::compress`'s own `MAX_DECOMPRESSED_SIZE`: real mips are at most a
/// few tens of MB, this just keeps a corrupt header from demanding a
/// multi-gigabyte allocation.
pub(crate) const MAX_BUFFER_SIZE: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
pub struct MipSizes {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

/// `max(size >> level, 1)` (Texture.cs:1285-1289).
pub fn mip_level_size(size: u32, level: u32) -> u32 {
    (size.checked_shr(level).unwrap_or(0)).max(1)
}

/// Width/height/depth of a given mip level (Texture.cs:951-963). `depth`
/// only shrinks with the level for a volume texture; a cubemap's depth is
/// multiplied by 6 (first face only is decoded — see `decode::decode_mip`).
pub fn sizes_for_level(header: &Header, level: u32) -> MipSizes {
    let width = mip_level_size(u32::from(header.width), level);
    let height = mip_level_size(u32::from(header.height), level);
    let mut depth = if header.flags.is_volume() {
        mip_level_size(u32::from(header.depth), level)
    } else {
        u32::from(header.depth)
    };
    if header.flags.is_cube() {
        depth = depth.saturating_mul(6);
    }
    MipSizes {
        width,
        height,
        depth,
    }
}

fn round_up_to_block(mut size: u32) -> u32 {
    let misalign = size % 4;
    if misalign > 0 {
        size += 4 - misalign;
    }
    if size < 4 && size > 0 {
        size = 4;
    }
    size
}

/// Total byte size of one mip level's data, across all depth slices
/// (Texture.cs:965-1006).
pub fn buffer_size_for(format: VTexFormat, sizes: MipSizes) -> Result<usize, TexError> {
    let block_size = u64::from(format.block_size());
    let (w, h, d) = if format.is_block_compressed() {
        let w = round_up_to_block(sizes.width);
        let h = round_up_to_block(sizes.height);
        let d = if sizes.depth < 4 && sizes.depth > 1 {
            4
        } else {
            sizes.depth
        };
        (w, h, d)
    } else {
        (sizes.width, sizes.height, sizes.depth)
    };

    let pixel_or_block_count = if format.is_block_compressed() {
        (u64::from(w) * u64::from(h)) >> 4
    } else {
        u64::from(w) * u64::from(h)
    };
    let total = pixel_or_block_count
        .checked_mul(u64::from(d))
        .and_then(|v| v.checked_mul(block_size))
        .ok_or(TexError::SizeOverflow)?;
    if total > MAX_BUFFER_SIZE {
        return Err(TexError::SizeTooLarge {
            requested: total,
            limit: MAX_BUFFER_SIZE,
        });
    }
    Ok(total as usize)
}

/// Total `f32` element count (`width * height * 4` channels `* depth`) an
/// [`crate::HdrImage`]'s `rgba` buffer needs for `sizes`, checked (as the
/// equivalent byte count, `elements * 4`) against the same
/// [`MAX_BUFFER_SIZE`] cap `buffer_size_for` applies to a mip's raw bytes.
/// Computed from the header/mip dimensions alone, before any bytes are read
/// or a float buffer allocated: a raw BC6H mip is 16x smaller than its
/// decoded float output (16 bytes cover a 4x4 block of 16 pixels, each of
/// which decodes to 16 bytes of float RGBA -- 1 byte/pixel compressed vs 16
/// bytes/pixel decoded), so `buffer_size_for`'s 1 GiB cap on the
/// *compressed* bytes lets a decoded size 16x that cap through; this is a
/// separate check on the *decoded* size (`s6f3a2b_hdr.md`'s crafted-header
/// cap).
pub fn hdr_element_count(sizes: MipSizes) -> Result<usize, TexError> {
    let pixels = u64::from(sizes.width)
        .checked_mul(u64::from(sizes.height))
        .ok_or(TexError::SizeOverflow)?;
    let elements = pixels
        .checked_mul(4)
        .and_then(|v| v.checked_mul(u64::from(sizes.depth)))
        .ok_or(TexError::SizeOverflow)?;
    let bytes = elements.checked_mul(4).ok_or(TexError::SizeOverflow)?;
    if bytes > MAX_BUFFER_SIZE {
        return Err(TexError::SizeTooLarge {
            requested: bytes,
            limit: MAX_BUFFER_SIZE,
        });
    }
    Ok(elements as usize)
}

/// One extracted mip level: its dimensions and decompressed bytes (still in
/// the source pixel format, not yet RGBA8).
pub struct MipData {
    pub sizes: MipSizes,
    pub bytes: Vec<u8>,
}

/// Extracts and (if needed) LZ4-decompresses `target_level`'s bytes.
///
/// `data` is the whole resource's bytes (not just the DATA block): the
/// pixel payload for a vtex_c lives immediately after the DATA block's
/// declared span, uncovered by the resource's own block table (see
/// `lib.rs`'s module doc comment), so `mip_data_offset` is
/// `data_block.offset + data_block.size` into that full buffer.
///
/// Mips are stored smallest-first (Texture.cs:1008-1033's `SkipMipmaps`
/// walks from `NumMipLevels-1` down to the desired level), so this walks
/// the same order, skipping every level it isn't asked for.
pub fn extract(
    header: &Header,
    data: &[u8],
    mip_data_offset: usize,
    target_level: u32,
) -> Result<MipData, TexError> {
    let num_levels = u32::from(header.num_mip_levels);
    if target_level >= num_levels {
        return Err(TexError::InvalidMipLevel {
            level: target_level,
            num_mip_levels: header.num_mip_levels,
        });
    }

    let mut cursor = mip_data_offset;
    for level in (0..num_levels).rev() {
        let sizes = sizes_for_level(header, level);
        let uncompressed_size = buffer_size_for(header.format, sizes)?;

        let stored_size = match (
            header.is_actually_compressed_mips,
            header
                .compressed_mip_sizes
                .as_ref()
                .and_then(|s| s.get(level as usize)),
        ) {
            (true, Some(&compressed)) if (compressed as usize) < uncompressed_size => {
                compressed as usize
            }
            _ => uncompressed_size,
        };

        let end = cursor
            .checked_add(stored_size)
            .ok_or(TexError::SizeOverflow)?;
        let chunk = data.get(cursor..end).ok_or_else(|| TexError::Truncated {
            detail: format!(
                "mip level {level}: need {stored_size} bytes at {cursor}, buffer has {}",
                data.len()
            ),
        })?;

        if level == target_level {
            let bytes = if stored_size < uncompressed_size {
                s2fmt::compress::lz4_block(chunk, uncompressed_size)?
            } else {
                chunk.to_vec()
            };
            return Ok(MipData { sizes, bytes });
        }

        cursor = end;
    }

    unreachable!("target_level was checked against num_levels above")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{Header, VTexFlags};

    fn header(
        width: u16,
        height: u16,
        depth: u16,
        format: VTexFormat,
        mips: u8,
        flags: u16,
    ) -> Header {
        Header {
            version: 1,
            flags: VTexFlags(flags),
            width,
            height,
            depth,
            format,
            num_mip_levels: mips,
            metadata: None,
            is_actually_compressed_mips: false,
            compressed_mip_sizes: None,
            has_sheet: false,
        }
    }

    #[test]
    fn mip_level_size_halves_and_floors_at_one() {
        assert_eq!(mip_level_size(1024, 0), 1024);
        assert_eq!(mip_level_size(1024, 10), 1);
        assert_eq!(mip_level_size(3, 1), 1);
    }

    #[test]
    fn block_compressed_buffer_size_rounds_up_to_4x4() {
        let h = header(6, 6, 1, VTexFormat::Dxt1, 1, 0);
        let sizes = sizes_for_level(&h, 0);
        // 6x6 rounds up to 8x8 = 4 blocks of 8 bytes (DXT1) = 32 bytes.
        assert_eq!(buffer_size_for(h.format, sizes).unwrap(), 32);
    }

    #[test]
    fn non_block_buffer_size_is_plain_pixel_count() {
        let h = header(4, 3, 1, VTexFormat::Rgba8888, 1, 0);
        let sizes = sizes_for_level(&h, 0);
        assert_eq!(buffer_size_for(h.format, sizes).unwrap(), 4 * 3 * 4);
    }

    #[test]
    fn hdr_element_count_rejects_a_16384_squared_by_4_volume() {
        // s6f3a2b_hdr.md's crafted case: BC6H 16384x16384, depth 4 -- 17 GiB
        // of decoded floats, 16x over the 1 GiB cap even before the depth
        // multiplier.
        let sizes = MipSizes {
            width: 16384,
            height: 16384,
            depth: 4,
        };
        assert!(matches!(
            hdr_element_count(sizes),
            Err(TexError::SizeTooLarge { .. })
        ));
    }

    #[test]
    fn hdr_element_count_allows_exactly_one_gib() {
        // 8192x8192x1 floats = 8192*8192*4*4 bytes = exactly 1<<30: the
        // check must use `>`, not `>=`, so a real 8192^2 irradiance texture
        // still decodes.
        let sizes = MipSizes {
            width: 8192,
            height: 8192,
            depth: 1,
        };
        assert_eq!(hdr_element_count(sizes).unwrap(), 8192 * 8192 * 4);
    }

    #[test]
    fn cube_flag_multiplies_depth_by_6() {
        let h = header(4, 4, 1, VTexFormat::Rgba8888, 1, VTexFlags::CUBE_TEXTURE);
        let sizes = sizes_for_level(&h, 0);
        assert_eq!(sizes.depth, 6);
    }

    #[test]
    fn extracts_smallest_to_largest_order() {
        // Two uncompressed RGBA8888 mips: level 1 = 2x2 (16 bytes), level 0
        // = 4x4 (64 bytes), stored smallest-first.
        let h = header(4, 4, 1, VTexFormat::Rgba8888, 2, 0);
        let mut data = vec![0xAAu8; 16]; // level 1 payload
        data.extend(vec![0xBBu8; 64]); // level 0 payload

        let mip1 = extract(&h, &data, 0, 1).unwrap();
        assert_eq!((mip1.sizes.width, mip1.sizes.height), (2, 2));
        assert_eq!(mip1.bytes, vec![0xAAu8; 16]);

        let mip0 = extract(&h, &data, 0, 0).unwrap();
        assert_eq!((mip0.sizes.width, mip0.sizes.height), (4, 4));
        assert_eq!(mip0.bytes, vec![0xBBu8; 64]);
    }

    #[test]
    fn lz4_compressed_mip_decompresses() {
        let h_base = header(4, 4, 1, VTexFormat::Rgba8888, 1, 0);
        let uncompressed = vec![0x42u8; 64];
        let compressed = lz4_flex::block::compress(&uncompressed);
        assert!(compressed.len() < uncompressed.len());
        let mut h = h_base;
        h.is_actually_compressed_mips = true;
        h.compressed_mip_sizes = Some(vec![compressed.len() as u32]);

        let mip0 = extract(&h, &compressed, 0, 0).unwrap();
        assert_eq!(mip0.bytes, uncompressed);
    }

    #[test]
    fn lz4_compressed_mip_in_a_two_level_chain_decompresses() {
        // Regression for the cursor advancing by `stored_size` (the actual
        // on-disk, possibly-compressed length) rather than
        // `uncompressed_size`: with only one mip level, `cursor = end` is
        // never even reached (the loop returns on its first iteration), so
        // that line needs a chain of at least two levels to exercise.
        let mip1_uncompressed = vec![0x11u8; 64]; // level 1: 4x4 RGBA8888
        let mip1_compressed = lz4_flex::block::compress(&mip1_uncompressed);
        assert!(mip1_compressed.len() < mip1_uncompressed.len());
        let mip0_bytes = vec![0xBBu8; 256]; // level 0: 8x8 RGBA8888, stored plain

        let mut data = mip1_compressed.clone();
        data.extend_from_slice(&mip0_bytes);

        let mut h = header(8, 8, 1, VTexFormat::Rgba8888, 2, 0);
        h.is_actually_compressed_mips = true;
        h.compressed_mip_sizes = Some(vec![256, mip1_compressed.len() as u32]);

        let mip0 = extract(&h, &data, 0, 0).unwrap();
        assert_eq!(mip0.bytes, mip0_bytes);

        let mip1 = extract(&h, &data, 0, 1).unwrap();
        assert_eq!(mip1.bytes, mip1_uncompressed);
    }

    #[test]
    fn invalid_mip_level_errors() {
        let h = header(4, 4, 1, VTexFormat::Rgba8888, 1, 0);
        assert!(matches!(
            extract(&h, &[0u8; 64], 0, 5),
            Err(TexError::InvalidMipLevel { .. })
        ));
    }

    #[test]
    fn truncated_data_errors_not_panics() {
        let h = header(4, 4, 1, VTexFormat::Rgba8888, 1, 0);
        assert!(extract(&h, &[0u8; 4], 0, 0).is_err());
    }
}
