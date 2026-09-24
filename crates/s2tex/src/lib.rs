//! Decodes a Source 2 `vtex_c` texture resource into an RGBA8 image and
//! re-encodes it as JPEG/PNG for a browser (see `s6f3a2_tex.md`).
//!
//! A vtex_c's pixel payload is a Source 2 quirk worth calling out up front:
//! the resource's `DATA` block declares a `Size` in the block table that
//! covers only the fixed header + extra data (Texture.cs:420-539) -- the
//! actual mip bytes are appended immediately after that declared span, in
//! bytes the block table never accounts for (`Texture.cs`'s own
//! `DataOffset => Offset + Size` reads straight past the block from the
//! same underlying file stream). `s2fmt::resource::Resource` doesn't
//! expose a way to read past a block's own declared range, and per this
//! crate's spec `s2fmt` isn't touched to add one; instead, every entry
//! point here takes the resource's raw bytes (`bytes`) alongside the
//! parsed [`s2fmt::resource::Resource`] (or parses them together in
//! [`decode_bytes`]) and reads the mip payload straight out of `bytes` at
//! `data_block.offset + data_block.size`.

mod bc6h;
mod budget;
mod decode;
pub mod encode;
mod error;
pub mod format;
mod half_float;
mod hdr_decode;
pub mod header;
mod mip;
mod reader;
mod redi;
pub mod rgbe;
pub mod tonemap;
mod transform;

pub use encode::{
    Encoded, EncodedFormat, alpha_is_significant, encode as encode_image,
    encode_gray as encode_image_gray,
};
pub use error::TexError;
pub use format::VTexFormat;
pub use header::Header;

use s2fmt::resource::{FourCC, Resource};

/// A decoded mip, ready to hand to [`encode_image`].
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, `[R, G, B, A]` per pixel.
    pub rgba: Vec<u8>,
    pub format: VTexFormat,
    /// Which mip level was decoded (`0` = full resolution); always `0` for
    /// the single-image raw JPEG/PNG formats.
    pub mip_level: u32,
    /// The source texture is a cubemap, array or volume texture, and only
    /// the first face/layer/slice was decoded (change item 3's "первый
    /// слой/грань, с пометкой в результате").
    pub truncated_to_first_layer: bool,
}

impl DecodedImage {
    /// Encodes this image the way the browser wants it: PNG for masks
    /// (`format::VTexFormat::is_mask`) or significant alpha, JPEG at
    /// `jpeg_quality` otherwise (`encode::encode`'s `lossless` flag). Kept
    /// here, rather than left to each caller, so the format's own mask
    /// predicate can't be forgotten downstream (the map exporter this feeds
    /// included).
    pub fn encode(&self, jpeg_quality: u8) -> Result<Encoded, TexError> {
        encode_image(
            &self.rgba,
            self.width,
            self.height,
            jpeg_quality,
            self.format.is_mask(),
        )
    }
}

/// Decodes `resource` (already parsed from `bytes`) to an RGBA8 image no
/// larger than `max_side` pixels on its longest side, picking the largest
/// mip that already fits rather than resampling (`budget::pick_mip_for_budget`).
///
/// `bytes` must be the same buffer `resource` was parsed from -- see this
/// module's doc comment for why the parsed `Resource` alone isn't enough.
pub fn decode(resource: &Resource, bytes: &[u8], max_side: u32) -> Result<DecodedImage, TexError> {
    let data_block = resource
        .block(FourCC::DATA)
        .ok_or(TexError::MissingDataBlock)?;
    let header_bytes = resource.block_bytes(data_block);
    let header = header::parse(header_bytes)?;

    let mip_data_offset = data_block
        .offset
        .checked_add(data_block.size)
        .ok_or(TexError::SizeOverflow)?;

    if header.format.is_raw_image() {
        let payload = bytes
            .get(mip_data_offset..)
            .ok_or_else(|| TexError::Truncated {
                detail: "no pixel payload after the DATA block".to_string(),
            })?;
        let (width, height, rgba) = decode::decode_raw_image(header.format, payload)?;
        return Ok(DecodedImage {
            width,
            height,
            rgba,
            format: header.format,
            mip_level: 0,
            truncated_to_first_layer: false,
        });
    }

    let target_level = budget::pick_mip_for_budget(&header, max_side);
    let mip = mip::extract(&header, bytes, mip_data_offset, target_level)?;

    // Cube/array/volume mips store every face/layer/slice back to back;
    // only the first is decoded (change item 3).
    let one_layer = mip::MipSizes {
        width: mip.sizes.width,
        height: mip.sizes.height,
        depth: 1,
    };
    let layer_len = mip::buffer_size_for(header.format, one_layer)?;
    let layer_bytes = mip
        .bytes
        .get(..layer_len)
        .ok_or_else(|| TexError::Truncated {
            detail: "mip data shorter than a single face/layer".to_string(),
        })?;

    let mut rgba = decode::decode_mip(
        header.format,
        mip.sizes.width,
        mip.sizes.height,
        layer_bytes,
    )?;

    let codec = redi::resolve_codec(resource, header.format, header.flags.is_cube());
    transform::apply(&mut rgba, codec);

    Ok(DecodedImage {
        width: mip.sizes.width,
        height: mip.sizes.height,
        rgba,
        format: header.format,
        mip_level: target_level,
        truncated_to_first_layer: mip.sizes.depth > 1,
    })
}

/// [`decode`], parsing `bytes` into a [`Resource`] first.
pub fn decode_bytes(bytes: &[u8], max_side: u32) -> Result<DecodedImage, TexError> {
    let resource = Resource::parse(bytes.to_vec())?;
    decode(&resource, bytes, max_side)
}

/// A decoded HDR mip, linear-light float RGBA (`s6f3a2b_hdr.md` change
/// item 3) -- [`decode`]'s counterpart for BC6H and the raw float formats,
/// which clamp/error in the LDR path instead (`decode::decode_mip`).
#[derive(Debug, Clone)]
pub struct HdrImage {
    pub width: u32,
    pub height: u32,
    /// `layers` slices of `width * height * 4` floats each, back to back,
    /// linear-light `[R, G, B, A]` per pixel: every face/slice the mip
    /// actually stores (`mip::sizes_for_level`'s `depth`), unlike
    /// [`decode`]'s LDR path, which always truncates to the first. For a
    /// cubemap that's the 6 faces in `Texture.cs`'s `CubemapFace` order
    /// (+X, -X, +Y, -Y, +Z, -Z; change item 3's "для кубов нужны ВСЕ 6
    /// граней"); for a volume texture (e.g. a light probe volume atlas)
    /// it's every depth slice at this mip level, so a caller can sample it
    /// per vertex on the CPU without a second, layer-truncated decode.
    pub rgba: Vec<f32>,
    pub format: VTexFormat,
    pub mip_level: u32,
    pub layers: u32,
    pub is_cube: bool,
}

impl HdrImage {
    fn layer_slice(&self, layer: u32) -> Result<&[f32], TexError> {
        if layer >= self.layers {
            return Err(TexError::InvalidHdrLayer {
                layer,
                layers: self.layers,
            });
        }
        let per_layer = (self.width as usize) * (self.height as usize) * 4;
        let start = per_layer * layer as usize;
        Ok(&self.rgba[start..start + per_layer])
    }

    /// Radiance `.hdr` (RGBE) bytes for `layer` (`0`-based), loadable by
    /// three.js's `RGBELoader` (`s6f3a2b_hdr.md` change item 4, `rgbe`).
    pub fn encode_radiance(&self, layer: u32) -> Result<Vec<u8>, TexError> {
        let slice = self.layer_slice(layer)?;
        Ok(rgbe::encode(self.width, self.height, slice))
    }

    /// A tone-mapped RGBA8 preview of `layer`, encoded the same way
    /// [`DecodedImage::encode`] encodes an LDR image (`s6f3a2b_hdr.md`
    /// change item 4, `tonemap`).
    pub fn tonemap_preview(&self, layer: u32, jpeg_quality: u8) -> Result<Encoded, TexError> {
        let slice = self.layer_slice(layer)?;
        let ldr = tonemap::preview_rgba8(self.width, self.height, slice);
        encode_image(&ldr, self.width, self.height, jpeg_quality, false)
    }
}

/// Decodes `resource` to an [`HdrImage`] no larger than `max_side` pixels on
/// its longest side (same mip budget as [`decode`]). Only the float HDR
/// formats and BC6H are supported (`hdr_decode::decode_hdr_slice`); embedded
/// JPEG/PNG/WebP formats can't be HDR and are rejected immediately.
///
/// Decodes every face/slice of the target mip level into one buffer, so its
/// size is capped at 1 GiB (`mip::hdr_element_count`, checked before any mip
/// bytes are even read): a large probe-volume atlas can exceed that on its
/// own (de_fachwerk's is 340x336x1800, de_cache's 300x292x1608, de_boulder's
/// 244x240x1224, all bigger than de_mirage's 164x152x384) and must be read
/// one slice at a time with [`decode_raw_mip`]/[`decode_raw_mip_bytes`] plus
/// [`decode_hdr_slice`] instead.
pub fn decode_hdr(resource: &Resource, bytes: &[u8], max_side: u32) -> Result<HdrImage, TexError> {
    let data_block = resource
        .block(FourCC::DATA)
        .ok_or(TexError::MissingDataBlock)?;
    let header_bytes = resource.block_bytes(data_block);
    let header = header::parse(header_bytes)?;

    if header.format.is_raw_image() {
        return Err(TexError::UnsupportedFormat {
            format: header.format,
        });
    }

    let mip_data_offset = data_block
        .offset
        .checked_add(data_block.size)
        .ok_or(TexError::SizeOverflow)?;

    let target_level = budget::pick_mip_for_budget(&header, max_side);

    // Computed from the header alone, before any mip bytes are read,
    // decompressed or allocated for: a raw BC6H mip can be 16x smaller than
    // its decoded float output, so `mip::extract`'s own byte-size cap can't
    // be relied on to catch an oversized decoded result
    // (`s6f3a2b_hdr.md`'s crafted-header cap).
    let sizes = mip::sizes_for_level(&header, target_level);
    let total_elements = mip::hdr_element_count(sizes)?;

    let mip = mip::extract(&header, bytes, mip_data_offset, target_level)?;

    // Every face/slice this mip stores is decoded (unlike the LDR path's
    // first-layer-only `decode`): 6 for a cubemap, the full depth for a
    // volume texture, both already folded into `sizes_for_level`'s `depth`.
    let layers = mip.sizes.depth;
    let per_layer_sizes = mip::MipSizes {
        width: mip.sizes.width,
        height: mip.sizes.height,
        depth: 1,
    };
    let per_layer_len = mip::buffer_size_for(header.format, per_layer_sizes)?;
    let per_layer_elements = (mip.sizes.width as usize) * (mip.sizes.height as usize) * 4;

    // One allocation, decoded into directly below (no intermediate
    // per-layer buffer) -- `s6f3a2b_hdr.md`'s "avoid the double peak" note.
    let mut rgba = vec![0f32; total_elements];
    for layer in 0..layers {
        let start = per_layer_len * (layer as usize);
        let end = start
            .checked_add(per_layer_len)
            .ok_or(TexError::SizeOverflow)?;
        let layer_bytes = mip
            .bytes
            .get(start..end)
            .ok_or_else(|| TexError::Truncated {
                detail: format!("mip data shorter than layer {layer} of {layers}"),
            })?;

        let out_start = per_layer_elements * (layer as usize);
        let out_end = out_start + per_layer_elements;
        hdr_decode::decode_hdr_slice(
            header.format,
            mip.sizes.width,
            mip.sizes.height,
            layer_bytes,
            &mut rgba[out_start..out_end],
        )?;
    }

    Ok(HdrImage {
        width: mip.sizes.width,
        height: mip.sizes.height,
        rgba,
        format: header.format,
        mip_level: target_level,
        layers,
        is_cube: header.flags.is_cube(),
    })
}

/// [`decode_hdr`], parsing `bytes` into a [`Resource`] first.
pub fn decode_hdr_bytes(bytes: &[u8], max_side: u32) -> Result<HdrImage, TexError> {
    let resource = Resource::parse(bytes.to_vec())?;
    decode_hdr(&resource, bytes, max_side)
}

/// One mip level's raw, still-compressed-format bytes (LZ4-decompressed if
/// the mip was stored compressed, but not decoded to pixels): every
/// face/slice back to back, in on-disk order. For a block format (BC6H
/// included) this is exactly the block stream a WebGL
/// `compressedTex{Sub}Image*D` call wants, so a browser can upload it
/// directly instead of round-tripping through a CPU-side float decode
/// (`s6f3a2b_hdr.md`'s note on lightmaps mostly shipping as raw BC6H
/// blocks).
#[derive(Debug, Clone)]
pub struct RawMip {
    pub width: u32,
    pub height: u32,
    /// Faces/slices this mip stores (6x for a cubemap, the mip-adjusted
    /// depth for a volume texture, 1 otherwise) -- see
    /// `mip::sizes_for_level`.
    pub depth: u32,
    pub format: VTexFormat,
    /// The mip level this was extracted at (`decode_raw_mip`'s `level`
    /// argument, echoed back here for [`decode_hdr_slice`]'s `HdrImage`
    /// result). `0` is the largest, full-resolution mip
    /// (`mip::mip_level_size`'s `size >> level`); higher levels are
    /// smaller, same convention as [`DecodedImage::mip_level`] and
    /// [`HdrImage::mip_level`].
    pub mip_level: u32,
    pub is_cube: bool,
    pub bytes: Vec<u8>,
}

/// Extracts `level`'s raw bytes (`mip::extract`) without decoding pixels.
/// `level` `0` is the largest, full-resolution mip -- see
/// [`RawMip::mip_level`].
pub fn decode_raw_mip(resource: &Resource, bytes: &[u8], level: u32) -> Result<RawMip, TexError> {
    let data_block = resource
        .block(FourCC::DATA)
        .ok_or(TexError::MissingDataBlock)?;
    let header_bytes = resource.block_bytes(data_block);
    let header = header::parse(header_bytes)?;

    let mip_data_offset = data_block
        .offset
        .checked_add(data_block.size)
        .ok_or(TexError::SizeOverflow)?;

    let mip = mip::extract(&header, bytes, mip_data_offset, level)?;
    Ok(RawMip {
        width: mip.sizes.width,
        height: mip.sizes.height,
        depth: mip.sizes.depth,
        format: header.format,
        mip_level: level,
        is_cube: header.flags.is_cube(),
        bytes: mip.bytes,
    })
}

/// [`decode_raw_mip`], parsing `bytes` into a [`Resource`] first.
pub fn decode_raw_mip_bytes(bytes: &[u8], level: u32) -> Result<RawMip, TexError> {
    let resource = Resource::parse(bytes.to_vec())?;
    decode_raw_mip(&resource, bytes, level)
}

/// Decodes `raw`'s `z`-th face/slice to a single-layer [`HdrImage`], without
/// the whole-mip float buffer [`decode_hdr`] would need for every
/// face/slice combined -- the per-slice API large probe-volume atlases need
/// (`decode_hdr`'s doc comment, `s6f3a2b_hdr.md`'s "map exporter will read
/// large probe-volume atlases SLICE BY SLICE, never whole" design
/// decision). Same 1 GiB sanity cap as [`decode_hdr`] (`mip::hdr_element_count`),
/// applied to this one slice.
pub fn decode_hdr_slice(raw: &RawMip, z: u32) -> Result<HdrImage, TexError> {
    if z >= raw.depth {
        return Err(TexError::InvalidHdrLayer {
            layer: z,
            layers: raw.depth,
        });
    }

    let sizes = mip::MipSizes {
        width: raw.width,
        height: raw.height,
        depth: 1,
    };
    let total_elements = mip::hdr_element_count(sizes)?;
    let layer_len = mip::buffer_size_for(raw.format, sizes)?;

    let start = layer_len
        .checked_mul(z as usize)
        .ok_or(TexError::SizeOverflow)?;
    let end = start.checked_add(layer_len).ok_or(TexError::SizeOverflow)?;
    let layer_bytes = raw
        .bytes
        .get(start..end)
        .ok_or_else(|| TexError::Truncated {
            detail: format!("mip data shorter than slice {z} of {}", raw.depth),
        })?;

    let mut rgba = vec![0f32; total_elements];
    hdr_decode::decode_hdr_slice(raw.format, raw.width, raw.height, layer_bytes, &mut rgba)?;

    Ok(HdrImage {
        width: raw.width,
        height: raw.height,
        rgba,
        format: raw.format,
        mip_level: raw.mip_level,
        layers: 1,
        is_cube: raw.is_cube,
    })
}

/// [`decode_hdr_slice`]'s counterpart for a format [`crate::format::VTexFormat::
/// is_high_dynamic_range`] doesn't cover -- e.g. a probe volume's BC7 `..._dlshd` atlas, which is
/// block-compressed but carries an ordinary LDR `[0,1]` value (sun visibility), not float HDR
/// data. Decodes `raw`'s `z`-th face/slice through the same per-format block decoders [`decode`]
/// uses (`decode::decode_mip`), one slice at a time like [`decode_hdr_slice`] (large volumes are
/// read slice by slice, never whole).
pub fn decode_raw_mip_slice_ldr(raw: &RawMip, z: u32) -> Result<DecodedImage, TexError> {
    if z >= raw.depth {
        return Err(TexError::InvalidHdrLayer {
            layer: z,
            layers: raw.depth,
        });
    }

    // Same crafted-header cap as `decode_hdr_slice`, checked before `decode_mip` allocates its
    // output: a block-compressed format's decoded RGBA8 bytes can be far larger than its raw
    // bytes (e.g. BC1's 0.5 bytes/pixel compressed vs 4 bytes/pixel decoded, an 8x expansion), so
    // `buffer_size_for`'s 1 GiB cap on the *compressed* slice doesn't bound the decoded size.
    let decoded_bytes = u64::from(raw.width)
        .checked_mul(u64::from(raw.height))
        .and_then(|v| v.checked_mul(4))
        .ok_or(TexError::SizeOverflow)?;
    if decoded_bytes > mip::MAX_BUFFER_SIZE {
        return Err(TexError::SizeTooLarge {
            requested: decoded_bytes,
            limit: mip::MAX_BUFFER_SIZE,
        });
    }

    let sizes = mip::MipSizes {
        width: raw.width,
        height: raw.height,
        depth: 1,
    };
    let layer_len = mip::buffer_size_for(raw.format, sizes)?;
    let start = layer_len
        .checked_mul(z as usize)
        .ok_or(TexError::SizeOverflow)?;
    let end = start.checked_add(layer_len).ok_or(TexError::SizeOverflow)?;
    let layer_bytes = raw
        .bytes
        .get(start..end)
        .ok_or_else(|| TexError::Truncated {
            detail: format!("mip data shorter than slice {z} of {}", raw.depth),
        })?;

    let rgba = decode::decode_mip(raw.format, raw.width, raw.height, layer_bytes)?;

    Ok(DecodedImage {
        width: raw.width,
        height: raw.height,
        rgba,
        format: raw.format,
        mip_level: raw.mip_level,
        truncated_to_first_layer: raw.depth > 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_bytes_rejects_non_resource_input() {
        assert!(decode_bytes(b"not a resource", 1024).is_err());
    }

    #[test]
    fn decode_hdr_bytes_rejects_non_resource_input() {
        assert!(decode_hdr_bytes(b"not a resource", 1024).is_err());
    }

    #[test]
    fn decode_raw_mip_bytes_rejects_non_resource_input() {
        assert!(decode_raw_mip_bytes(b"not a resource", 0).is_err());
    }

    /// Wraps a vtex DATA block's bytes in a minimal, otherwise-empty
    /// resource container (file header + one-entry block table), the same
    /// offset convention `s2fmt::resource::Resource::parse` uses everywhere
    /// (each offset relative to its own field's position).
    fn resource_bytes(data_block: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes()); // file_size, patched below
        out.extend_from_slice(&12u16.to_le_bytes()); // header_version
        out.extend_from_slice(&0u16.to_le_bytes()); // resource version
        out.extend_from_slice(&8u32.to_le_bytes()); // block_offset (table starts right after block_count)
        out.extend_from_slice(&1u32.to_le_bytes()); // block_count
        out.extend_from_slice(b"DATA");
        out.extend_from_slice(&8u32.to_le_bytes()); // rel_offset (payload starts right after size)
        out.extend_from_slice(&(data_block.len() as u32).to_le_bytes());
        out.extend_from_slice(data_block);
        let file_size = out.len() as u32;
        out[0..4].copy_from_slice(&file_size.to_le_bytes());
        out
    }

    /// A vtex header declaring BC6H 16384x16384, depth 4, one mip level --
    /// `s6f3a2b_hdr.md`'s crafted case (~17 GiB of decoded floats). No extra
    /// data, so no mip payload is needed: the size check below must reject
    /// this from the header alone, before ever looking for mip bytes.
    fn crafted_oversized_hdr_header() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes()); // version
        b.extend_from_slice(&0u16.to_le_bytes()); // flags
        b.extend_from_slice(&[0u8; 16]); // reflectivity
        b.extend_from_slice(&16384u16.to_le_bytes()); // width
        b.extend_from_slice(&16384u16.to_le_bytes()); // height
        b.extend_from_slice(&4u16.to_le_bytes()); // depth
        b.push(19); // format = Bc6H
        b.push(1); // num_mip_levels
        b.extend_from_slice(&0u32.to_le_bytes()); // picmip0res
        b.extend_from_slice(&0u32.to_le_bytes()); // extraDataOffset
        b.extend_from_slice(&0u32.to_le_bytes()); // extraDataCount
        b
    }

    #[test]
    fn decode_hdr_bytes_rejects_a_crafted_oversized_volume_before_allocating() {
        let bytes = resource_bytes(&crafted_oversized_hdr_header());
        let err = decode_hdr_bytes(&bytes, header::MAX_SIDE).unwrap_err();
        assert!(
            matches!(err, TexError::SizeTooLarge { .. }),
            "expected SizeTooLarge, got {err:?}"
        );
    }

    /// A crafted `RawMip` (change item 14, `s6f3a4_lighting.md`): its decoded RGBA8 output would
    /// be ~1.6 GiB even though the block-compressed input is tiny -- must be rejected before
    /// `decode_mip` allocates, not just left to `buffer_size_for`'s cap on the *compressed* bytes.
    #[test]
    fn decode_raw_mip_slice_ldr_rejects_an_oversized_decode_before_allocating() {
        let raw = RawMip {
            width: 20000,
            height: 20000,
            depth: 1,
            format: VTexFormat::Dxt1,
            mip_level: 0,
            is_cube: false,
            bytes: Vec::new(),
        };
        let err = decode_raw_mip_slice_ldr(&raw, 0).unwrap_err();
        assert!(
            matches!(err, TexError::SizeTooLarge { .. }),
            "expected SizeTooLarge, got {err:?}"
        );
    }
}
