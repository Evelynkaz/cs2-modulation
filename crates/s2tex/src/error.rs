//! Errors from decoding or encoding a texture. Hostile input (a corrupted
//! or adversarial `vtex_c`) must produce an `Err` here, never a panic; see
//! `crates/s2tex/src/mip.rs`'s size caps and the constraint in
//! `s6f3a2_tex.md`.

use crate::format::VTexFormat;

#[derive(Debug, thiserror::Error)]
pub enum TexError {
    #[error("failed to parse the resource container: {0}")]
    Resource(#[from] s2fmt::resource::ResourceError),
    #[error("resource has no DATA block")]
    MissingDataBlock,
    #[error("truncated or malformed texture data: {detail}")]
    Truncated { detail: String },
    #[error("unsupported vtex version {actual} (expected 1)")]
    UnsupportedVersion { actual: u16 },
    #[error("unknown texture format id {raw}")]
    UnknownFormat { raw: u8 },
    #[error("texture format {format:?} is not supported for decoding")]
    UnsupportedFormat { format: VTexFormat },
    #[error("invalid extra data: {detail}")]
    InvalidExtraData { detail: String },
    #[error("width/height must be non-zero, got {width}x{height}")]
    InvalidDimensions { width: u16, height: u16 },
    #[error("dimension {width}x{height}x{depth} exceeds the sanity limit of {max} per side")]
    DimensionsTooLarge {
        width: u16,
        height: u16,
        depth: u16,
        max: u32,
    },
    #[error("mip level count must be non-zero")]
    InvalidMipCount,
    #[error("{count} extra data entries exceeds the sanity limit of {limit}")]
    TooManyExtraData { count: u32, limit: u32 },
    #[error("{count} compressed mip sizes exceeds the sanity limit of {limit}")]
    TooManyMips { count: u32, limit: u32 },
    #[error("mip level {level} does not exist (texture has {num_mip_levels})")]
    InvalidMipLevel { level: u32, num_mip_levels: u8 },
    #[error("computed buffer size {requested} exceeds the sanity limit of {limit} bytes")]
    SizeTooLarge { requested: u64, limit: u64 },
    #[error("mip size arithmetic overflowed")]
    SizeOverflow,
    #[error("failed to LZ4-decompress a mip level: {0}")]
    Lz4(#[from] s2fmt::compress::DecompressError),
    #[error("block decode failed for {format:?}: {message}")]
    BlockDecode {
        format: VTexFormat,
        message: &'static str,
    },
    #[error("failed to decode embedded JPEG: {0}")]
    JpegDecode(String),
    #[error("failed to decode embedded PNG: {0}")]
    PngDecode(#[from] png::DecodingError),
    #[error("failed to encode JPEG: {0}")]
    JpegEncode(#[from] jpeg_encoder::EncodingError),
    #[error("failed to encode PNG: {0}")]
    PngEncode(#[from] png::EncodingError),
    #[error("image dimensions {width}x{height} exceed the encoder's {max} limit per side")]
    EncodeDimensionsTooLarge { width: u32, height: u32, max: u32 },
    #[error("rgba buffer is {actual} bytes, expected {expected} for {width}x{height}")]
    EncodeBufferSizeMismatch {
        width: u32,
        height: u32,
        expected: u64,
        actual: usize,
    },
    #[error("HDR layer {layer} does not exist (image has {layers})")]
    InvalidHdrLayer { layer: u32, layers: u32 },
}
