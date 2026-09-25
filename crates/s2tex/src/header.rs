//! vtex_c header: version, flags, dimensions, format, mip count, and the
//! METADATA / COMPRESSED_MIP_SIZE / SHEET / CUBEMAP_RADIANCE_SH extra data
//! blocks (Texture.cs:420-539).
//!
//! Reads happen through one [`Cursor`] over the whole DATA block, jumping
//! back and forth exactly like the reference's single `BinaryReader`
//! stream, rather than slicing out per-entry sub-buffers: a real file's
//! `COMPRESSED_MIP_SIZE` entry declares a `size` that covers only its fixed
//! 12-byte header, with the variable-length mip size list living past that
//! declared size (still inside the DATA block, since it's the last entry) —
//! a sub-slice clipped to `size` would silently lose that list.

use crate::error::TexError;
use crate::format::VTexFormat;
use crate::reader::Cursor;

/// Bits of the header's `Flags` field we act on (`Resource/Enums/VTexFlags.cs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VTexFlags(pub u16);

impl VTexFlags {
    pub const CUBE_TEXTURE: u16 = 1 << 4;
    pub const VOLUME_TEXTURE: u16 = 1 << 5;

    pub fn is_cube(self) -> bool {
        self.0 & Self::CUBE_TEXTURE != 0
    }

    pub fn is_volume(self) -> bool {
        self.0 & Self::VOLUME_TEXTURE != 0
    }
}

/// The `METADATA` extra data entry: display rect and HDR range remap, always
/// padded to 128 bytes on disk (Texture.cs:467-497). Read and stored, not
/// otherwise used: `ActualWidth`/`ActualHeight` cropping and the range
/// remap are both outside this crate's contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct Metadata {
    pub display_rect_width: u16,
    pub display_rect_height: u16,
    pub motion_vectors_max_distance: i16,
    pub range_min: [f32; 4],
    pub range_max: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct Header {
    pub version: u16,
    pub flags: VTexFlags,
    pub width: u16,
    pub height: u16,
    pub depth: u16,
    pub format: VTexFormat,
    pub num_mip_levels: u8,
    /// The compiler's precomputed average-colour-ish reflectivity (RGBA, linear), read but never
    /// consumed before `s6f3a7_env_materials.md`'s colour-correction fix: `csgo_environment`'s
    /// per-layer `g_mTextureColorAdjust{N}`/`g_mTextureAdjust{N}` matrices use this as the
    /// contrast pivot (`RenderMaterial.cs:727-730`'s `colorTexture.Reflectivity`).
    pub reflectivity: [f32; 4],
    pub metadata: Option<Metadata>,
    /// `COMPRESSED_MIP_SIZE`'s `int1` flag: whether mips are *actually*
    /// LZ4-compressed, as opposed to the entry merely being present
    /// (Texture.cs:504-509, "TODO: Verify whether this int is the one that
    /// actually controls compression" — VRF itself is unsure, we follow it).
    pub is_actually_compressed_mips: bool,
    /// Per-mip on-disk size, indexed like [`Header::width`]'s mip levels
    /// (`0` = full resolution), *not* in the smallest-to-largest order the
    /// pixel bytes themselves are stored in (Texture.cs:500-518).
    pub compressed_mip_sizes: Option<Vec<u32>>,
    /// `SHEET` extra data was present. Read and stored as a presence flag
    /// only; sprite sheet animation data is out of scope here.
    pub has_sheet: bool,
}

/// Sanity limits so a hostile header can't force a huge allocation before
/// we've validated anything (`s6f3a2_tex.md` constraints: side <= 16384,
/// bounded mip/extra-data counts).
pub const MAX_SIDE: u32 = 16384;
const MAX_EXTRA_DATA_ENTRIES: u32 = 64;
const MAX_MIP_LEVELS: u32 = 32;

const EXTRA_DATA_SHEET: u32 = 2;
const EXTRA_DATA_METADATA: u32 = 3;
const EXTRA_DATA_COMPRESSED_MIP_SIZE: u32 = 4;

/// Parses the vtex header from a DATA block's bytes
/// (`resource.block_bytes(data_block)`; Texture.cs:420-539).
pub fn parse(bytes: &[u8]) -> Result<Header, TexError> {
    let mut c = Cursor::new(bytes);
    let version = c.u16()?;
    if version != 1 {
        return Err(TexError::UnsupportedVersion { actual: version });
    }
    let flags = VTexFlags(c.u16()?);
    let reflectivity = [c.f32()?, c.f32()?, c.f32()?, c.f32()?];
    let width = c.u16()?;
    let height = c.u16()?;
    let depth = c.u16()?;
    if width == 0 || height == 0 {
        return Err(TexError::InvalidDimensions { width, height });
    }
    if u32::from(width) > MAX_SIDE || u32::from(height) > MAX_SIDE || u32::from(depth) > MAX_SIDE {
        return Err(TexError::DimensionsTooLarge {
            width,
            height,
            depth,
            max: MAX_SIDE,
        });
    }
    let format = VTexFormat::from_raw(c.u8()?)?;
    let num_mip_levels = c.u8()?;
    if num_mip_levels == 0 {
        return Err(TexError::InvalidMipCount);
    }
    let _picmip0_res = c.u32()?;

    let mut header = Header {
        version,
        flags,
        width,
        height,
        depth,
        format,
        num_mip_levels,
        reflectivity,
        metadata: None,
        is_actually_compressed_mips: false,
        compressed_mip_sizes: None,
        has_sheet: false,
    };

    // extraDataOffset/extraDataCount are relative to extraDataOffset's own
    // field position, per the Source 2 offset convention (Texture.cs:446-451).
    let extra_data_offset_field_pos = c.pos();
    let extra_data_offset = c.u32()?;
    let extra_data_count = c.u32()?;
    if extra_data_count == 0 {
        return Ok(header);
    }
    if extra_data_count > MAX_EXTRA_DATA_ENTRIES {
        return Err(TexError::TooManyExtraData {
            count: extra_data_count,
            limit: MAX_EXTRA_DATA_ENTRIES,
        });
    }
    let table_pos = extra_data_offset_field_pos
        .checked_add(extra_data_offset as usize)
        .ok_or(TexError::Truncated {
            detail: "extra data table offset overflow".to_string(),
        })?;

    for i in 0..extra_data_count {
        let entry_pos = table_pos
            .checked_add((i as usize).checked_mul(12).ok_or(TexError::SizeOverflow)?)
            .ok_or(TexError::SizeOverflow)?;
        let mut ec = Cursor::new(bytes);
        ec.set_pos(entry_pos);
        let ty = ec.u32()?;
        // The per-entry offset, like extraDataOffset above, is relative to
        // its own field's position (Texture.cs:457-464).
        let offset_field_pos = ec.pos();
        let raw_offset = ec.u32()?;
        let _size = ec.u32()?; // informational only; see module doc comment
        let payload_pos = offset_field_pos
            .checked_add(raw_offset as usize)
            .ok_or(TexError::SizeOverflow)?;

        match ty {
            EXTRA_DATA_METADATA => header.metadata = Some(parse_metadata(bytes, payload_pos)?),
            EXTRA_DATA_COMPRESSED_MIP_SIZE => {
                let (is_compressed, sizes) = parse_compressed_mip_sizes(bytes, payload_pos)?;
                header.is_actually_compressed_mips = is_compressed;
                header.compressed_mip_sizes = Some(sizes);
            }
            EXTRA_DATA_SHEET => header.has_sheet = true,
            _ => {} // FALLBACK_BITS, CUBEMAP_RADIANCE_SH, anything unrecognised: not used here
        }
    }

    Ok(header)
}

fn parse_metadata(bytes: &[u8], payload_pos: usize) -> Result<Metadata, TexError> {
    let mut c = Cursor::new(bytes);
    c.set_pos(payload_pos);
    let _always_zero = c.u16()?;
    let display_rect_width = c.u16()?;
    let display_rect_height = c.u16()?;
    let motion_vectors_max_distance = c.i16()?;
    let range_min = [c.f32()?, c.f32()?, c.f32()?, c.f32()?];
    let range_max = [c.f32()?, c.f32()?, c.f32()?, c.f32()?];
    // The remaining 88 bytes of the 128-byte block are padding (Texture.cs:496).
    Ok(Metadata {
        display_rect_width,
        display_rect_height,
        motion_vectors_max_distance,
        range_min,
        range_max,
    })
}

fn parse_compressed_mip_sizes(
    bytes: &[u8],
    payload_pos: usize,
) -> Result<(bool, Vec<u32>), TexError> {
    let mut c = Cursor::new(bytes);
    c.set_pos(payload_pos);
    let flag = c.u32()?;
    if flag != 0 && flag != 1 {
        return Err(TexError::InvalidExtraData {
            detail: format!("COMPRESSED_MIP_SIZE flag {flag} is neither 0 nor 1"),
        });
    }
    let mips_offset_field_pos = c.pos();
    let mips_offset = c.u32()?;
    let mips = c.u32()?;
    if mips > MAX_MIP_LEVELS {
        return Err(TexError::TooManyMips {
            count: mips,
            limit: MAX_MIP_LEVELS,
        });
    }
    let list_pos = mips_offset_field_pos
        .checked_add(mips_offset as usize)
        .ok_or(TexError::SizeOverflow)?;
    let mut lc = Cursor::new(bytes);
    lc.set_pos(list_pos);
    let mut sizes = Vec::with_capacity(mips as usize);
    for _ in 0..mips {
        // Stored as a signed int32 on disk (Texture.cs:517), but a negative
        // size is nonsensical; reinterpreting the bits as u32 rather than
        // rejecting keeps this in line with "hostile input is an error
        // later, not a parse failure here" -- an implausible value simply
        // won't be smaller than the uncompressed size, so it's ignored by
        // `mip::extract`'s compressed-vs-not comparison.
        sizes.push(lc.u32()?);
    }
    Ok((flag == 1, sizes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// Builds a minimal valid header: version=1, flags=0, reflectivity=0,
    /// width/height/depth, format, num_mip_levels, picmip0res=0, no extra
    /// data.
    fn minimal_header(width: u16, height: u16, depth: u16, format: u8, mips: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes()); // version
        b.extend_from_slice(&0u16.to_le_bytes()); // flags
        b.extend_from_slice(&[0u8; 16]); // reflectivity
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&depth.to_le_bytes());
        b.push(format);
        b.push(mips);
        b.extend_from_slice(&0u32.to_le_bytes()); // picmip0res
        b.extend_from_slice(&0u32.to_le_bytes()); // extraDataOffset (unused, count=0)
        b.extend_from_slice(&0u32.to_le_bytes()); // extraDataCount
        b
    }

    #[test]
    fn parses_minimal_header() {
        let bytes = minimal_header(512, 256, 1, 1, 9);
        let h = parse(&bytes).unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.width, 512);
        assert_eq!(h.height, 256);
        assert_eq!(h.format, VTexFormat::Dxt1);
        assert_eq!(h.num_mip_levels, 9);
        assert!(h.compressed_mip_sizes.is_none());
        assert!(!h.flags.is_cube());
        assert!(!h.flags.is_volume());
    }

    #[test]
    fn rejects_non_1_version() {
        let mut bytes = minimal_header(4, 4, 1, 1, 1);
        bytes[0..2].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(
            parse(&bytes),
            Err(TexError::UnsupportedVersion { actual: 2 })
        ));
    }

    #[test]
    fn rejects_zero_dimensions() {
        let bytes = minimal_header(0, 4, 1, 1, 1);
        assert!(matches!(
            parse(&bytes),
            Err(TexError::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn rejects_oversized_dimensions() {
        let bytes = minimal_header(u16::MAX, 4, 1, 1, 1);
        assert!(matches!(
            parse(&bytes),
            Err(TexError::DimensionsTooLarge { .. })
        ));
    }

    #[test]
    fn cube_and_volume_flags_decode() {
        let mut bytes = minimal_header(4, 4, 1, 1, 1);
        bytes[2..4]
            .copy_from_slice(&(VTexFlags::CUBE_TEXTURE | VTexFlags::VOLUME_TEXTURE).to_le_bytes());
        let h = parse(&bytes).unwrap();
        assert!(h.flags.is_cube());
        assert!(h.flags.is_volume());
    }

    #[test]
    fn truncated_header_errors_not_panics() {
        for len in 0..32 {
            let bytes = vec![0u8; len];
            let _ = parse(&bytes);
        }
    }

    /// Extra data table + METADATA + COMPRESSED_MIP_SIZE laid out exactly
    /// like the real `base_mid_ver1_normal_psd_fdce5b8c.vtex_c` sample this
    /// crate's real-archive test reads (offsets relative to their own
    /// field, `COMPRESSED_MIP_SIZE`'s size list spilling past its own
    /// declared `size`).
    #[test]
    fn parses_extra_data_like_a_real_file() {
        let mut b = minimal_header(1024, 1024, 1, 20, 9);
        // Patch extraDataOffset (currently 0) / extraDataCount (currently 0).
        let offset_field_pos = b.len() - 8;
        b[offset_field_pos..offset_field_pos + 4].copy_from_slice(&u32_le(8));
        b[offset_field_pos + 4..offset_field_pos + 8].copy_from_slice(&u32_le(1));
        // One entry: type=COMPRESSED_MIP_SIZE, offset relative to its own
        // field (at entry_pos+4) so payload starts right after this
        // 12-byte entry (entry_pos+12, i.e. 8 past the offset field itself);
        // size deliberately understates the real payload.
        b.extend_from_slice(&u32_le(EXTRA_DATA_COMPRESSED_MIP_SIZE));
        b.extend_from_slice(&u32_le(8));
        b.extend_from_slice(&u32_le(12));
        // Payload: flag=1, mipsOffset relative to its own field (past the
        // mips count field, to the sizes list right after it) = 8, mips=2,
        // then the two sizes.
        b.extend_from_slice(&u32_le(1));
        b.extend_from_slice(&u32_le(8));
        b.extend_from_slice(&u32_le(2));
        b.extend_from_slice(&u32_le(111));
        b.extend_from_slice(&u32_le(222));

        let h = parse(&b).unwrap();
        assert!(h.is_actually_compressed_mips);
        assert_eq!(h.compressed_mip_sizes.unwrap(), vec![111, 222]);
    }
}
