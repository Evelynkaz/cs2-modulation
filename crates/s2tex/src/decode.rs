//! Decodes one mip's already-extracted, already-decompressed bytes into
//! RGBA8 (`[R, G, B, A]` byte order throughout this crate), per format
//! (Texture.cs:836-899, `TextureDecoders/*.cs`).

use crate::error::TexError;
use crate::format::VTexFormat;
use crate::header::MAX_SIDE;

/// `texture2ddecoder`'s block decoders write packed `u32`s built as
/// `u32::from_le_bytes([b, g, r, a])` (its `color()` helper): `to_le_bytes`
/// always returns that same `[b, g, r, a]` order (it's endian-fixed, not
/// host-endian), regardless of the host's own byte order, so this reorder
/// is portable.
fn bgra_u32_to_rgba8(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for &p in pixels {
        let [b, g, r, a] = p.to_le_bytes();
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

fn decode_block_format(
    format: VTexFormat,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, TexError> {
    let mut pixels = vec![0u32; (width as usize) * (height as usize)];
    let decode_result = match format {
        VTexFormat::Dxt1 => {
            texture2ddecoder::decode_bc1(data, width as usize, height as usize, &mut pixels)
        }
        VTexFormat::Dxt5 => {
            texture2ddecoder::decode_bc3(data, width as usize, height as usize, &mut pixels)
        }
        VTexFormat::Ati1N => {
            texture2ddecoder::decode_bc4(data, width as usize, height as usize, &mut pixels)
        }
        VTexFormat::Ati2N => {
            texture2ddecoder::decode_bc5(data, width as usize, height as usize, &mut pixels)
        }
        VTexFormat::Bc7 => {
            texture2ddecoder::decode_bc7(data, width as usize, height as usize, &mut pixels)
        }
        _ => unreachable!("decode_block_format only called for the BCn formats above"),
    };
    decode_result.map_err(|message| TexError::BlockDecode { format, message })?;
    Ok(bgra_u32_to_rgba8(&pixels))
}

fn decode_rgba8888(data: &[u8], pixel_count: usize) -> Vec<u8> {
    // Already `[R, G, B, A]` on disk (Texture.cs's `DecodeRGBA8888`): a
    // straight copy.
    data[..pixel_count * 4].to_vec()
}

fn decode_bgra8888(data: &[u8], pixel_count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixel_count * 4);
    for px in data[..pixel_count * 4].as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

fn decode_i8(data: &[u8], pixel_count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixel_count * 4);
    for &i in &data[..pixel_count] {
        out.extend_from_slice(&[i, i, i, 255]);
    }
    out
}

fn decode_ia88(data: &[u8], pixel_count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixel_count * 4);
    for px in data[..pixel_count * 2].as_chunks::<2>().0 {
        let (intensity, alpha) = (px[0], px[1]);
        out.extend_from_slice(&[intensity, intensity, intensity, alpha]);
    }
    out
}

/// Decodes an embedded whole-image blob (JPEG/PNG): `data` is the DATA
/// block's payload from `mip_data_offset` onward, not clipped to the
/// image's own encoded length -- both decoders stop at their own
/// terminator/declared size, so trailing bytes (there shouldn't be any) are
/// harmless (Texture.cs:739-747's `IsRawJpeg`/`IsRawPng` path).
fn decode_raw_jpeg(data: &[u8]) -> Result<(u32, u32, Vec<u8>), TexError> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(data, options);
    let pixels = decoder
        .decode()
        .map_err(|e| TexError::JpegDecode(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| TexError::JpegDecode("no image info after successful decode".to_string()))?;
    let pixel_count = usize::from(info.width) * usize::from(info.height);
    let rgba = expand_to_rgba(&pixels, pixel_count)?;
    Ok((u32::from(info.width), u32::from(info.height), rgba))
}

/// zune-jpeg's requested output colorspace (`RGBA` above) is advisory, not
/// guaranteed: a single-component (grayscale) or three-component (no alpha)
/// JPEG comes back at 1 or 3 bytes/pixel regardless of the request, silently
/// breaking this crate's documented `width * height * 4` promise and making
/// callers index out of bounds. Widens by the buffer's *actual* length
/// rather than trusting the request.
fn expand_to_rgba(pixels: &[u8], pixel_count: usize) -> Result<Vec<u8>, TexError> {
    if pixels.len() == pixel_count * 4 {
        return Ok(pixels.to_vec());
    }
    if pixels.len() == pixel_count {
        let mut out = Vec::with_capacity(pixel_count * 4);
        for &g in pixels {
            out.extend_from_slice(&[g, g, g, 255]);
        }
        return Ok(out);
    }
    if pixels.len() == pixel_count * 3 {
        let mut out = Vec::with_capacity(pixel_count * 4);
        for px in pixels.as_chunks::<3>().0 {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        return Ok(out);
    }
    Err(TexError::JpegDecode(format!(
        "decoded {} bytes for {pixel_count} pixels: expected 1, 3 or 4 bytes/pixel",
        pixels.len()
    )))
}

fn decode_raw_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), TexError> {
    let mut decoder = png::Decoder::new(data);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    // The png crate sizes its output buffer from IHDR alone, with no cap of
    // its own: an adversarial IHDR (e.g. 4,000,000 x 4,000,000) would abort
    // the whole process trying to allocate that buffer below. Reject before
    // any allocation, the same MAX_SIDE the vtex header itself is held to.
    let info = reader.info();
    if info.width > MAX_SIDE || info.height > MAX_SIDE {
        return Err(TexError::SizeTooLarge {
            requested: u64::from(info.width.max(info.height)),
            limit: u64::from(MAX_SIDE),
        });
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let (width, height) = (info.width, info.height);
    let pixels = &buf[..info.buffer_size()];

    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(pixels.len() / 3 * 4);
            for px in pixels.as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(pixels.len() * 4);
            for &g in pixels {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixels.len() * 2);
            for px in pixels.as_chunks::<2>().0 {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        png::ColorType::Indexed => {
            unreachable!("Transformations::normalize_to_color8 expands palettes to RGB(A)")
        }
    };
    Ok((width, height, rgba))
}

/// Decodes an embedded JPEG/PNG (`format.is_raw_image()`); the whole image
/// is mip 0, there is no mip chain to pick from.
pub fn decode_raw_image(format: VTexFormat, data: &[u8]) -> Result<(u32, u32, Vec<u8>), TexError> {
    if format.is_raw_jpeg() {
        decode_raw_jpeg(data)
    } else if format.is_raw_png() {
        decode_raw_png(data)
    } else {
        Err(TexError::UnsupportedFormat { format })
    }
}

/// Decodes one 2D mip slice (`width` x `height`, already trimmed to a
/// single cubemap face/array layer by the caller) to RGBA8.
pub fn decode_mip(
    format: VTexFormat,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, TexError> {
    match format {
        VTexFormat::Dxt1
        | VTexFormat::Dxt5
        | VTexFormat::Ati1N
        | VTexFormat::Ati2N
        | VTexFormat::Bc7 => decode_block_format(format, width, height, data),
        VTexFormat::Rgba8888 => Ok(decode_rgba8888(data, (width * height) as usize)),
        VTexFormat::Bgra8888 => Ok(decode_bgra8888(data, (width * height) as usize)),
        VTexFormat::I8 => Ok(decode_i8(data, (width * height) as usize)),
        VTexFormat::Ia88 => Ok(decode_ia88(data, (width * height) as usize)),
        // BC6H and every other HDR/exotic format (R16*, RGBA16161616*, R32F
        // family, ETC2/EAC, WebP): out of scope for a browser-bound RGBA8
        // preview -- maps use these almost exclusively for cubemaps and
        // lightmaps, which this crate doesn't export (s6f3_common.md).
        other => Err(TexError::UnsupportedFormat { format: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba8888_is_a_straight_copy() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(decode_rgba8888(&data, 2), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn bgra8888_swaps_r_and_b() {
        let data = [10, 20, 30, 40]; // B,G,R,A on disk
        assert_eq!(decode_bgra8888(&data, 1), vec![30, 20, 10, 40]);
    }

    #[test]
    fn i8_replicates_to_gray_opaque() {
        assert_eq!(decode_i8(&[100], 1), vec![100, 100, 100, 255]);
    }

    #[test]
    fn ia88_replicates_intensity_keeps_alpha() {
        assert_eq!(decode_ia88(&[100, 200], 1), vec![100, 100, 100, 200]);
    }

    #[test]
    fn bc1_solid_color_block_decodes() {
        // A single DXT1 block encoding solid RGB565 white: both endpoints
        // equal (opaque 4-color mode), all index bits 0 -> every texel is
        // color0.
        let white565: u16 = 0xFFFF;
        let mut block = Vec::new();
        block.extend_from_slice(&white565.to_le_bytes());
        block.extend_from_slice(&white565.to_le_bytes());
        block.extend_from_slice(&[0u8; 4]); // indices
        let out = decode_mip(VTexFormat::Dxt1, 4, 4, &block).unwrap();
        assert_eq!(out.len(), 4 * 4 * 4);
        for px in out.as_chunks::<4>().0 {
            assert_eq!(px, &[255, 255, 255, 255]);
        }
    }

    #[test]
    fn unsupported_format_errors_cleanly() {
        assert!(matches!(
            decode_mip(VTexFormat::Bc6H, 4, 4, &[]),
            Err(TexError::UnsupportedFormat {
                format: VTexFormat::Bc6H
            })
        ));
    }

    #[test]
    fn truncated_block_data_errors_not_panics() {
        assert!(decode_mip(VTexFormat::Dxt1, 4, 4, &[0u8; 2]).is_err());
    }

    #[test]
    fn grayscale_jpeg_is_expanded_to_rgba() {
        let mut jpg = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut jpg, 90);
        encoder
            .encode(&[128u8; 64], 8, 8, jpeg_encoder::ColorType::Luma)
            .unwrap();
        let (width, height, rgba) = decode_raw_jpeg(&jpg).unwrap();
        assert_eq!((width, height), (8, 8));
        assert_eq!(rgba.len(), 8 * 8 * 4);
        assert_eq!(&rgba[0..4], &[128, 128, 128, 255]);
    }

    /// Builds a minimal but CRC-valid PNG with an arbitrary IHDR
    /// width/height (8-bit RGBA, no interlace) and a one-byte stored zlib
    /// block, the same shape a real vtex_c's embedded PNG has (Texture.cs's
    /// `IsRawPng` path decodes exactly this kind of buffer).
    fn png_with_ihdr(width: u32, height: u32) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(ty);
            hasher.update(data);
            out.extend_from_slice(ty);
            out.extend_from_slice(data);
            out.extend_from_slice(&hasher.finalize().to_be_bytes());
        }

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit depth, RGBA, no interlace
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(
            &mut png,
            b"IDAT",
            &[
                0x78, 0x01, 0x01, 0x01, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x01, 0x00, 0x01,
            ],
        );
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn oversized_png_ihdr_is_rejected_before_allocation() {
        // The 141-byte vtex_c that motivated this fix declared exactly this
        // IHDR (RGBA8, 4,000,000 x 4,000,000): reading its output buffer
        // size before this fix aborts the whole process, uncatchable.
        let png = png_with_ihdr(4_000_000, 4_000_000);
        let err = decode_raw_png(&png).unwrap_err();
        assert!(matches!(err, TexError::SizeTooLarge { .. }));
    }
}
