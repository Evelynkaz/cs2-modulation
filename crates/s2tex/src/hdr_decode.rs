//! Decodes one HDR mip slice's already-extracted, already-decompressed
//! bytes to linear-light RGBA f32 (`s6f3a2b_hdr.md` change items 1-2;
//! `TextureDecoders/DecodeR16F.cs` and its neighbours). Mirrors
//! `decode::decode_mip`'s shape, but for the float formats plus BC6H, and
//! writes into a caller-provided output buffer instead of returning a fresh
//! `Vec` (`s6f3a2b_hdr.md`'s "avoid the double peak" note -- see
//! `bc6h::decode_image`'s doc comment for why that matters most for BC6H).
//!
//! Each raw (non-block) decoder below fills only the channels the format
//! actually stores and leaves the rest at the reference's own fixed
//! defaults -- unused color channels `0.0`, alpha `1.0` unless the format
//! carries its own alpha -- matching `DecodeR16F.cs`/`DecodeRG1616F.cs`/
//! `DecodeRG3232F.cs`/`DecodeRGB323232F.cs`'s `DecodeHdr` methods exactly
//! (each constructs an `SKColorF` with the missing channels as literal `0f`,
//! and `SKColorF`'s omitted alpha argument defaults to `1`).

use crate::error::TexError;
use crate::format::VTexFormat;
use crate::half_float::f16_to_f32;

fn checked_len(data: &[u8], needed: usize, format: VTexFormat) -> Result<&[u8], TexError> {
    data.get(..needed).ok_or_else(|| TexError::Truncated {
        detail: format!("{format:?} data is {} bytes, need {needed}", data.len()),
    })
}

fn decode_r16f(data: &[u8], width: u32, height: u32, out: &mut [f32]) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 2, VTexFormat::R16F)?;
    for (px, out_px) in data
        .as_chunks::<2>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f16_to_f32(u16::from_le_bytes(*px));
        out_px.copy_from_slice(&[r, 0.0, 0.0, 1.0]);
    }
    Ok(())
}

fn decode_rg1616f(data: &[u8], width: u32, height: u32, out: &mut [f32]) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 4, VTexFormat::Rg1616F)?;
    for (px, out_px) in data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
        let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
        out_px.copy_from_slice(&[r, g, 0.0, 1.0]);
    }
    Ok(())
}

fn decode_rgba16161616f(
    data: &[u8],
    width: u32,
    height: u32,
    out: &mut [f32],
) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 8, VTexFormat::Rgba16161616F)?;
    for (px, out_px) in data
        .as_chunks::<8>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f16_to_f32(u16::from_le_bytes([px[0], px[1]]));
        let g = f16_to_f32(u16::from_le_bytes([px[2], px[3]]));
        let b = f16_to_f32(u16::from_le_bytes([px[4], px[5]]));
        let a = f16_to_f32(u16::from_le_bytes([px[6], px[7]]));
        out_px.copy_from_slice(&[r, g, b, a]);
    }
    Ok(())
}

fn decode_r32f(data: &[u8], width: u32, height: u32, out: &mut [f32]) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 4, VTexFormat::R32F)?;
    for (px, out_px) in data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        out_px.copy_from_slice(&[f32::from_le_bytes(*px), 0.0, 0.0, 1.0]);
    }
    Ok(())
}

fn decode_rg3232f(data: &[u8], width: u32, height: u32, out: &mut [f32]) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 8, VTexFormat::Rg3232F)?;
    for (px, out_px) in data
        .as_chunks::<8>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f32::from_le_bytes(px[0..4].try_into().unwrap());
        let g = f32::from_le_bytes(px[4..8].try_into().unwrap());
        out_px.copy_from_slice(&[r, g, 0.0, 1.0]);
    }
    Ok(())
}

fn decode_rgb323232f(
    data: &[u8],
    width: u32,
    height: u32,
    out: &mut [f32],
) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 12, VTexFormat::Rgb323232F)?;
    for (px, out_px) in data
        .as_chunks::<12>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f32::from_le_bytes(px[0..4].try_into().unwrap());
        let g = f32::from_le_bytes(px[4..8].try_into().unwrap());
        let b = f32::from_le_bytes(px[8..12].try_into().unwrap());
        out_px.copy_from_slice(&[r, g, b, 1.0]);
    }
    Ok(())
}

fn decode_rgba32323232f(
    data: &[u8],
    width: u32,
    height: u32,
    out: &mut [f32],
) -> Result<(), TexError> {
    let count = (width as usize) * (height as usize);
    let data = checked_len(data, count * 16, VTexFormat::Rgba32323232F)?;
    for (px, out_px) in data
        .as_chunks::<16>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let r = f32::from_le_bytes(px[0..4].try_into().unwrap());
        let g = f32::from_le_bytes(px[4..8].try_into().unwrap());
        let b = f32::from_le_bytes(px[8..12].try_into().unwrap());
        let a = f32::from_le_bytes(px[12..16].try_into().unwrap());
        out_px.copy_from_slice(&[r, g, b, a]);
    }
    Ok(())
}

/// Decodes one 2D mip slice (already trimmed to a single cubemap
/// face/array layer) to linear-light RGBA f32 into `out`, which must be
/// exactly `width * height * 4` floats (`debug_assert`ed). Formats not
/// listed here (including the plain, non-`F` 16-bit integer formats
/// `R16`/`RG1616`/`RGBA16161616`, which `s6f3a2b_hdr.md`'s change item 2
/// doesn't ask for and this crate's real-archive scan never found in use)
/// return [`TexError::UnsupportedFormat`].
pub(crate) fn decode_hdr_slice(
    format: VTexFormat,
    width: u32,
    height: u32,
    data: &[u8],
    out: &mut [f32],
) -> Result<(), TexError> {
    debug_assert_eq!(out.len(), (width as usize) * (height as usize) * 4);
    match format {
        VTexFormat::Bc6H => crate::bc6h::decode_image(data, width, height, out),
        VTexFormat::R16F => decode_r16f(data, width, height, out),
        VTexFormat::Rg1616F => decode_rg1616f(data, width, height, out),
        VTexFormat::Rgba16161616F => decode_rgba16161616f(data, width, height, out),
        VTexFormat::R32F => decode_r32f(data, width, height, out),
        VTexFormat::Rg3232F => decode_rg3232f(data, width, height, out),
        VTexFormat::Rgb323232F => decode_rgb323232f(data, width, height, out),
        VTexFormat::Rgba32323232F => decode_rgba32323232f(data, width, height, out),
        other => Err(TexError::UnsupportedFormat { format: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r16f_fills_only_red_alpha_one() {
        let half_one = 0x3C00u16.to_le_bytes();
        let mut out = vec![0f32; 4];
        decode_r16f(&half_one, 1, 1, &mut out).unwrap();
        assert_eq!(out, vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn rg1616f_fills_red_green_alpha_one() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x3C00u16.to_le_bytes()); // R=1.0
        data.extend_from_slice(&0x4000u16.to_le_bytes()); // G=2.0
        let mut out = vec![0f32; 4];
        decode_rg1616f(&data, 1, 1, &mut out).unwrap();
        assert_eq!(out, vec![1.0, 2.0, 0.0, 1.0]);
    }

    #[test]
    fn rgba16161616f_fills_all_four_channels() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x3C00u16.to_le_bytes()); // R=1.0
        data.extend_from_slice(&0x4000u16.to_le_bytes()); // G=2.0
        data.extend_from_slice(&0xBC00u16.to_le_bytes()); // B=-1.0
        data.extend_from_slice(&0x0000u16.to_le_bytes()); // A=0.0
        let mut out = vec![0f32; 4];
        decode_rgba16161616f(&data, 1, 1, &mut out).unwrap();
        assert_eq!(out, vec![1.0, 2.0, -1.0, 0.0]);
    }

    #[test]
    fn r32f_and_rgba32323232f_are_a_straight_reinterpret() {
        let data = 12.5f32.to_le_bytes();
        let mut out = vec![0f32; 4];
        decode_r32f(&data, 1, 1, &mut out).unwrap();
        assert_eq!(out, vec![12.5, 0.0, 0.0, 1.0]);

        let mut rgba_data = Vec::new();
        for v in [1.0f32, 2.0, 3.0, 4.0] {
            rgba_data.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = vec![0f32; 4];
        decode_rgba32323232f(&rgba_data, 1, 1, &mut out).unwrap();
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn truncated_data_errors_not_panics() {
        let mut out = vec![0f32; 4];
        assert!(decode_r16f(&[0u8], 1, 1, &mut out).is_err());
        let mut out = vec![0f32; 4];
        assert!(decode_rgba16161616f(&[0u8; 4], 1, 1, &mut out).is_err());
        let mut out = vec![0f32; 4];
        assert!(decode_rgba32323232f(&[0u8; 4], 1, 1, &mut out).is_err());
    }

    #[test]
    fn non_float_16bit_formats_are_unsupported() {
        let mut out = vec![0f32; 4];
        for format in [
            VTexFormat::R16,
            VTexFormat::Rg1616,
            VTexFormat::Rgba16161616,
        ] {
            assert!(matches!(
                decode_hdr_slice(format, 1, 1, &[0u8; 16], &mut out),
                Err(TexError::UnsupportedFormat { .. })
            ));
        }
    }
}
