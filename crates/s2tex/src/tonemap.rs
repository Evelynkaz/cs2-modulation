//! Tone-maps a linear HDR image down to an sRGB-ish LDR RGBA8 preview
//! (`s6f3a2b_hdr.md` change item 4).
//!
//! Ported from `TextureDecoders/DecodeRGBA16161616F.cs`'s `DecodeLdr` --
//! the one HDR decoder in the reference with a real tone-mapping curve
//! (global Reinhard, keyed by the image's own log-average luminance, in a
//! luma/chroma split) rather than a bare `[0,1]` clamp (`DecodeBCn.cs`'s and
//! the other `DecodeHdr`-only formats' LDR fallback is exactly that bare
//! clamp, `Common.ToClampedLdrColor`). Reused here for every HDR format
//! (including BC6H) since it gives a legible preview for genuinely
//! high-range pixels, where a plain clamp just blows out to white.

/// Tone-maps `rgba` (`width * height * 4` linear floats) to `width * height
/// * 4` sRGB-ish `u8`s. Alpha passes through the reference's own plain
/// `[0,1]` clamp (HDR formats' alpha, where present, isn't radiometric).
pub fn preview_rgba8(width: u32, height: u32, rgba: &[f32]) -> Vec<u8> {
    let pixel_count = (width as usize) * (height as usize);
    debug_assert_eq!(rgba.len(), pixel_count * 4);
    let pixels = rgba.as_chunks::<4>().0;

    let mut log_sum = 0f32;
    for px in pixels {
        let lum = px[0] * 0.299 + px[1] * 0.587 + px[2] * 0.114;
        log_sum += lum.max(f32::EPSILON).ln();
    }
    let log_avg = (log_sum / (pixels.len().max(1) as f32)).exp();

    let mut out = Vec::with_capacity(pixel_count * 4);
    for px in pixels {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let y = r * 0.299 + g * 0.587 + b * 0.114;
        let u = (b - y) * 0.565;
        let v = (r - y) * 0.713;

        let mut mul = 4.0 * y / log_avg;
        mul /= 1.0 + mul;
        mul /= y.max(f32::EPSILON);

        let rr = ((y + 1.403 * v) * mul).max(0.0).powf(2.25);
        let gg = ((y - 0.344 * u - 0.714 * v) * mul).max(0.0).powf(2.25);
        let bb = ((y + 1.770 * u) * mul).max(0.0).powf(2.25);

        out.push(to_clamped_ldr(rr));
        out.push(to_clamped_ldr(gg));
        out.push(to_clamped_ldr(bb));
        out.push(to_clamped_ldr(a));
    }
    out
}

fn to_clamped_ldr(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_gray_image_tonemaps_to_a_visible_gray() {
        let rgba = [0.18f32, 0.18, 0.18, 1.0].repeat(4);
        let out = preview_rgba8(2, 2, &rgba);
        assert_eq!(out.len(), 2 * 2 * 4);
        for px in out.as_chunks::<4>().0 {
            assert!(px[0] > 0 && px[0] < 255, "r={}", px[0]);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn black_image_stays_black() {
        let rgba = vec![0.0f32, 0.0, 0.0, 1.0];
        let out = preview_rgba8(1, 1, &rgba);
        assert_eq!(out, vec![0, 0, 0, 255]);
    }

    #[test]
    fn very_bright_pixel_does_not_panic_and_stays_in_range() {
        // A pathologically bright pixel: the tone-map's NaN-prone division
        // steps (`s6f3a2b_hdr.md`'s float edge cases) must still land on a
        // valid, non-black byte for every channel, not silently saturate to
        // 0 via a stray NaN.
        let rgba = vec![1000.0f32, 1000.0, 1000.0, 1.0];
        let out = preview_rgba8(1, 1, &rgba);
        assert_eq!(out.len(), 4);
        assert_eq!(out[3], 255);
        assert!(out[0] > 0 && out[1] > 0 && out[2] > 0);
    }

    #[test]
    fn alpha_passes_through_clamped() {
        let rgba = vec![0.1f32, 0.1, 0.1, 0.5];
        let out = preview_rgba8(1, 1, &rgba);
        assert_eq!(out[3], 128);
    }
}
