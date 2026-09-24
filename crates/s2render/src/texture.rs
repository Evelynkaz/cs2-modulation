//! Texture loading + encoding for the map export: `s2tex` decodes a `vtex_c` down to the budget's
//! mip, this module picks the browser-facing format (§5) and hands back the encoded bytes ready
//! to embed in the `.glb`.
//!
//! Base color and normal maps both end up as JPEG at the budget's quality (normals with alpha
//! forced to `255` first, dropping roughness, specifically so `DecodedImage::encode`'s own
//! significant-alpha check routes them to JPEG rather than PNG -- masks/significant-alpha
//! textures still fall through to PNG automatically, that decision already lives in `s2tex`).
//! The JPEG-vs-PNG size/quality comparison behind this choice is measured out-of-repo (this
//! crate adds no new dependency for it) and reported in the F3a-3 receipt; q90 stays the default
//! (`ExportOptions::jpeg_quality`) because it's a good size/error trade-off for normal maps too,
//! not just base color: measured on three real maps at the 1024 budget, reconstructed-normal
//! angle error at q90 is mean 1.17-2.20 degrees, p95 2.76-5.57 degrees, p99 3.65-7.80 degrees, max
//! 7.4-15.2 degrees, while PNG (lossless but 4.4-7x larger) is the only way to drop that error to
//! zero; q95 only cuts the error 15-25% for 60-90% more bytes, not worth the budget it costs
//! elsewhere.

use s2tex::encode::EncodedFormat;

use crate::source::Sources;

#[derive(Debug, thiserror::Error)]
pub enum TextureError {
    #[error("{path}: not found in any source VPK")]
    Missing { path: String },
    #[error("{path}: failed to decode: {source}")]
    Decode {
        path: String,
        source: s2tex::TexError,
    },
    #[error("{path}: failed to encode: {source}")]
    Encode {
        path: String,
        source: s2tex::TexError,
    },
}

/// Texture budget: mip size cap and JPEG quality (§5, §7's `--max-texture`).
#[derive(Debug, Clone, Copy)]
pub struct TextureBudget {
    pub max_side: u32,
    pub jpeg_quality: u8,
}

impl Default for TextureBudget {
    fn default() -> Self {
        TextureBudget {
            max_side: 1024,
            jpeg_quality: 90,
        }
    }
}

pub struct EncodedTexture {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Decodes `path` (already `_c`-suffixed) via `s2tex` at `budget.max_side`, and encodes it for
/// the browser. `is_normal` drops the alpha channel (roughness) to `255` first (§4/§5).
pub fn load_and_encode(
    sources: &Sources,
    path: &str,
    budget: TextureBudget,
    is_normal: bool,
) -> Result<EncodedTexture, TextureError> {
    let bytes = sources.read(path).ok_or_else(|| TextureError::Missing {
        path: path.to_string(),
    })?;
    let mut decoded =
        s2tex::decode_bytes(&bytes, budget.max_side).map_err(|source| TextureError::Decode {
            path: path.to_string(),
            source,
        })?;

    if is_normal {
        for px in decoded.rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    let encoded = decoded
        .encode(budget.jpeg_quality)
        .map_err(|source| TextureError::Encode {
            path: path.to_string(),
            source,
        })?;
    let mime_type = match encoded.format {
        EncodedFormat::Jpeg => "image/jpeg",
        EncodedFormat::Png => "image/png",
    };

    Ok(EncodedTexture {
        bytes: encoded.bytes,
        mime_type,
        width: decoded.width,
        height: decoded.height,
    })
}
