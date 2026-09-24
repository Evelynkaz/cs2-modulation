//! Encodes an RGBA8 image for the browser: JPEG for opaque color textures,
//! PNG when alpha carries real information (`s6f3a2_tex.md` change item 6).

use crate::error::TexError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedFormat {
    Jpeg,
    Png,
}

#[derive(Debug)]
pub struct Encoded {
    pub format: EncodedFormat,
    pub bytes: Vec<u8>,
}

/// True if any pixel's alpha isn't fully opaque. A texture whose alpha is
/// uniformly 255 gains nothing from PNG's lossless encoding over JPEG.
pub fn alpha_is_significant(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>().0.iter().any(|px| px[3] != 255)
}

const MAX_ENCODE_SIDE: u32 = u16::MAX as u32;

/// Rejects hostile/mismatched inputs before either encoder touches them:
/// `jpeg-encoder` truncates `width`/`height` to `u16` silently, and both
/// encoders only check that the buffer is *long enough*, not that it's
/// exactly `width * height * channels`.
fn validate_dimensions(
    data: &[u8],
    width: u32,
    height: u32,
    channels: u64,
) -> Result<(), TexError> {
    if width > MAX_ENCODE_SIDE || height > MAX_ENCODE_SIDE {
        return Err(TexError::EncodeDimensionsTooLarge {
            width,
            height,
            max: MAX_ENCODE_SIDE,
        });
    }
    let expected = u64::from(width) * u64::from(height) * channels;
    if data.len() as u64 != expected {
        return Err(TexError::EncodeBufferSizeMismatch {
            width,
            height,
            expected,
            actual: data.len(),
        });
    }
    Ok(())
}

fn encode_jpeg(rgba: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>, TexError> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    // `ColorType::Rgba` ignores the alpha channel during encoding (the
    // caller already decided this image is opaque via
    // `alpha_is_significant`), so no separate RGB copy is needed.
    encoder.encode(
        rgba,
        width as u16,
        height as u16,
        jpeg_encoder::ColorType::Rgba,
    )?;
    Ok(out)
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, TexError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(out)
}

/// Encodes `rgba` (`width * height * 4` bytes) as PNG when `lossless` is set
/// (masks and other non-color-graded formats, `format::VTexFormat::is_mask`)
/// or alpha is significant, JPEG otherwise.
pub fn encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    jpeg_quality: u8,
    lossless: bool,
) -> Result<Encoded, TexError> {
    validate_dimensions(rgba, width, height, 4)?;
    if lossless || alpha_is_significant(rgba) {
        Ok(Encoded {
            format: EncodedFormat::Png,
            bytes: encode_png(rgba, width, height)?,
        })
    } else {
        Ok(Encoded {
            format: EncodedFormat::Jpeg,
            bytes: encode_jpeg(rgba, width, height, jpeg_quality)?,
        })
    }
}

fn encode_jpeg_gray(
    gray: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, TexError> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.encode(
        gray,
        width as u16,
        height as u16,
        jpeg_encoder::ColorType::Luma,
    )?;
    Ok(out)
}

fn encode_png_gray(gray: &[u8], width: u32, height: u32) -> Result<Vec<u8>, TexError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(gray)?;
    }
    Ok(out)
}

/// Encodes a single-channel (one byte per pixel) image: JPEG luma at `jpeg_quality` normally, or
/// an 8-bit grayscale PNG when `lossless` is set (a mask channel that would band under JPEG's
/// chroma-adjacent quantization -- `s6f3a4_lighting.md` change item 11).
pub fn encode_gray(
    gray: &[u8],
    width: u32,
    height: u32,
    jpeg_quality: u8,
    lossless: bool,
) -> Result<Encoded, TexError> {
    validate_dimensions(gray, width, height, 1)?;
    if lossless {
        Ok(Encoded {
            format: EncodedFormat::Png,
            bytes: encode_png_gray(gray, width, height)?,
        })
    } else {
        Ok(Encoded {
            format: EncodedFormat::Jpeg,
            bytes: encode_jpeg_gray(gray, width, height, jpeg_quality)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_alpha_is_not_significant() {
        let rgba = vec![1, 2, 3, 255, 4, 5, 6, 255];
        assert!(!alpha_is_significant(&rgba));
    }

    #[test]
    fn any_non_255_alpha_is_significant() {
        let rgba = vec![1, 2, 3, 255, 4, 5, 6, 254];
        assert!(alpha_is_significant(&rgba));
    }

    #[test]
    fn opaque_image_encodes_as_jpeg() {
        let rgba = [200u8, 100, 50, 255].repeat(16); // 4x4
        let encoded = encode(&rgba, 4, 4, 85, false).unwrap();
        assert_eq!(encoded.format, EncodedFormat::Jpeg);
        assert!(!encoded.bytes.is_empty());
    }

    #[test]
    fn transparent_image_encodes_as_png() {
        let mut rgba = [200u8, 100, 50, 255].repeat(16);
        rgba[3] = 128; // first pixel's alpha
        let encoded = encode(&rgba, 4, 4, 85, false).unwrap();
        assert_eq!(encoded.format, EncodedFormat::Png);
        assert_eq!(&encoded.bytes[1..4], b"PNG");
    }

    #[test]
    fn lossless_opaque_image_encodes_as_png() {
        let rgba = [200u8, 100, 50, 255].repeat(16); // 4x4, fully opaque
        let encoded = encode(&rgba, 4, 4, 85, true).unwrap();
        assert_eq!(encoded.format, EncodedFormat::Png);
        assert_eq!(&encoded.bytes[1..4], b"PNG");
    }

    #[test]
    fn oversized_dimensions_are_rejected() {
        let rgba = vec![0u8; 4];
        let err = encode(&rgba, u32::from(u16::MAX) + 1, 1, 85, false).unwrap_err();
        assert!(matches!(err, TexError::EncodeDimensionsTooLarge { .. }));
    }

    #[test]
    fn mismatched_buffer_length_is_rejected() {
        let rgba = vec![0u8; 3]; // not width * height * 4
        let err = encode(&rgba, 4, 4, 85, false).unwrap_err();
        assert!(matches!(err, TexError::EncodeBufferSizeMismatch { .. }));
    }

    #[test]
    fn encode_gray_not_lossless_is_jpeg_luma() {
        let gray = vec![128u8; 16]; // 4x4, one byte per pixel
        let encoded = encode_gray(&gray, 4, 4, 90, false).unwrap();
        assert_eq!(encoded.format, EncodedFormat::Jpeg);
        assert!(!encoded.bytes.is_empty());
    }

    #[test]
    fn encode_gray_lossless_is_png_grayscale() {
        let gray = vec![128u8; 16];
        let encoded = encode_gray(&gray, 4, 4, 90, true).unwrap();
        assert_eq!(encoded.format, EncodedFormat::Png);
        assert_eq!(&encoded.bytes[1..4], b"PNG");
    }

    #[test]
    fn encode_gray_mismatched_buffer_length_is_rejected() {
        let gray = vec![0u8; 3]; // not width * height
        let err = encode_gray(&gray, 4, 4, 90, false).unwrap_err();
        assert!(matches!(err, TexError::EncodeBufferSizeMismatch { .. }));
    }
}
