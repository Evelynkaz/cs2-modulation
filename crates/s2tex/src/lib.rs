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

mod budget;
mod decode;
pub mod encode;
mod error;
pub mod format;
pub mod header;
mod mip;
mod reader;
mod redi;
mod transform;

pub use encode::{Encoded, EncodedFormat, alpha_is_significant, encode as encode_image};
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_bytes_rejects_non_resource_input() {
        assert!(decode_bytes(b"not a resource", 1024).is_err());
    }
}
