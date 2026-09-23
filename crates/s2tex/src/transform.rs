//! Post-decode pixel transforms the texture compiler applies before ever
//! writing a mip to disk, undone here in the same order the reference does
//! (Texture.cs:1295-1349, `TextureDecoders/Common.cs:80-199`).
//!
//! Operates on an RGBA8 buffer in `[R, G, B, A]` byte order throughout
//! (this crate's canonical order — see `decode.rs`), unlike the reference,
//! which reads/writes a `BGRA8888` `SKBitmap` byte-for-byte; every formula
//! below is channel-semantic (red, green, blue, alpha), not byte-order
//! specific, so it ports directly.

use crate::format::VTexFormat;

/// Which post-decode conversions a texture's compiler dependencies call
/// for, resolved from REDI/RED2 (`redi::resolve_codec`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextureCodec {
    pub ycocg: bool,
    /// YCoCg's Co/Cg/Scale channels are themselves sRGB-gamma-encoded
    /// (skybox cubemaps only; Texture.cs:1327-1331).
    pub ycocg_srgb: bool,
    pub hemi_oct_rb: bool,
    pub normalize_normals: bool,
    /// DXT5's alpha channel holds normal X, swapped with red before any of
    /// the above run (Texture.cs:1333-1335).
    pub dxt5nm: bool,
}

impl TextureCodec {
    fn is_noop(self) -> bool {
        self == TextureCodec::default()
    }
}

/// Resolves the special-dependency codec flags the same way
/// `RetrieveCodecFromResourceEditInfo` does once the raw flags are known
/// (Texture.cs:1327-1345): cubemap YCoCg is sRGB, DXT5 normal maps swap R/A,
/// and a BC7 HemiOct roughness map already reconstructs a full normal so
/// its separate Z-reconstruction flag is redundant and dropped.
pub fn resolve(
    format: VTexFormat,
    is_cube: bool,
    ycocg: bool,
    mut normalize_normals: bool,
    hemi_oct_rb: bool,
) -> TextureCodec {
    let ycocg_srgb = ycocg && is_cube;
    let dxt5nm = format == VTexFormat::Dxt5 && normalize_normals;
    if format == VTexFormat::Bc7 && hemi_oct_rb && normalize_normals {
        normalize_normals = false;
    }
    TextureCodec {
        ycocg,
        ycocg_srgb,
        hemi_oct_rb,
        normalize_normals,
        dxt5nm,
    }
}

fn clamp_color(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn clamp_unit(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn to_clamped_ldr(v: f32) -> u8 {
    (clamp_unit(v) * 255.0 + 0.5) as u8
}

/// sRGB gamma -> linear, per channel (`ColorSpace.SrgbGammaToLinear`).
fn srgb_gamma_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Swaps the R and A channels of every pixel (`Common.cs`'s `SwapRA`,
/// Texture.cs:1333-1335's DXT5 normal map convention).
fn swap_r_a(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 3);
    }
}

/// Reconstructs normalized RGB from Co/Cg/Scale/Y (Common.cs:142-151).
fn decode_ycocg(px: &mut [u8], linearize: bool) {
    let mut r = f32::from(px[0]) / 255.0;
    let mut g = f32::from(px[1]) / 255.0;
    let mut b = f32::from(px[2]) / 255.0;
    if linearize {
        r = srgb_gamma_to_linear(r);
        g = srgb_gamma_to_linear(g);
        b = srgb_gamma_to_linear(b);
    }
    let y = f32::from(px[3]) / 255.0;

    let scale = (b * (255.0 / 8.0)) + 1.0;
    let co = (r - (128.0 / 255.0)) / scale;
    let cg = (g - (128.0 / 255.0)) / scale;

    px[0] = to_clamped_ldr(y + co - cg);
    px[1] = to_clamped_ldr(y + cg);
    px[2] = to_clamped_ldr(y - co - cg);
    px[3] = 255;
}

/// Hemi-octahedron encoded normal in R/G, widened to a full RGB normal; the
/// blue channel it displaces (packed roughness) moves to alpha
/// (Common.cs:163-174).
fn decode_hemi_oct(px: &mut [u8]) {
    let r = f32::from(px[0]);
    let g = f32::from(px[1]);

    let nx = ((r + g) / 255.0) - 1.003_922;
    let ny = (r - g) / 255.0;
    let nz = 1.0 - nx.abs() - ny.abs();
    let l = (nx * nx + ny * ny + nz * nz).sqrt();

    px[3] = px[2]; // b (packed roughness) to alpha
    px[0] = ((nx / l * 0.5 + 0.5) * 255.0) as u8;
    px[1] = ((ny / l * 0.5 + 0.5) * 255.0) as u8;
    px[2] = ((nz / l * 0.5 + 0.5) * 255.0) as u8;
}

/// Derives normal Z from X/Y stored (premultiplied) in R/G
/// (Common.cs:153-161). `deriveB` matches the reference's `(int)MathF.Sqrt`
/// of a value that can be negative for an out-of-gamut X/Y pair: the sqrt
/// of a negative is NaN, and both C#'s and Rust's float-to-int casts send
/// NaN to `0`, so this ports as a direct `as i32` with no extra branch.
fn reconstruct_normal_z(px: &mut [u8]) {
    let swizzle_r = i32::from(px[0]) * 2 - 255;
    let swizzle_g = i32::from(px[1]) * 2 - 255;
    let under_sqrt = (255 * 255 - swizzle_r * swizzle_r - swizzle_g * swizzle_g) as f32;
    let derive_b = under_sqrt.sqrt() as i32;

    px[0] = clamp_color(swizzle_r / 2 + 128);
    px[1] = clamp_color(swizzle_g / 2 + 128);
    px[2] = clamp_color(derive_b / 2 + 128);
}

/// Applies `codec`'s conversions to an RGBA8 buffer in place
/// (`Common.ApplyTextureConversions`).
pub fn apply(rgba: &mut [u8], codec: TextureCodec) {
    if codec.is_noop() {
        return;
    }
    if codec.dxt5nm {
        swap_r_a(rgba);
    }
    for px in rgba.as_chunks_mut::<4>().0 {
        if codec.ycocg {
            decode_ycocg(px, codec.ycocg_srgb);
        }
        if codec.hemi_oct_rb {
            decode_hemi_oct(px);
        }
        if codec.normalize_normals {
            reconstruct_normal_z(px);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_r_a_exchanges_channels() {
        let mut rgba = vec![10, 20, 30, 40];
        swap_r_a(&mut rgba);
        assert_eq!(rgba, vec![40, 20, 30, 10]);
    }

    #[test]
    fn ycocg_identity_when_luma_only() {
        // Co=Cg=0 (both at the 128 midpoint), scale doesn't matter at 0
        // offset, Y=128/255: result should be a flat gray at ~128.
        let mut px = [128u8, 128, 0, 128];
        decode_ycocg(&mut px, false);
        assert_eq!(px[3], 255);
        assert!((px[0] as i16 - 128).abs() <= 1);
        assert!((px[1] as i16 - 128).abs() <= 1);
        assert!((px[2] as i16 - 128).abs() <= 1);
    }

    #[test]
    fn ycocg_matches_reference_at_known_nonzero_values() {
        // Non-degenerate Co/Cg, verified against Common.cs's formula by
        // hand: scale=1 here (b=0), co=(160-128)/255, cg=(100-128)/255,
        // y=128/255, giving r=y+co-cg=188/255, g=y+cg=100/255,
        // b=y-co-cg=124/255 -- each an exact integer over 255, so no
        // rounding slack is needed.
        let mut px = [160u8, 100, 0, 128];
        decode_ycocg(&mut px, false);
        assert_eq!(px, [188, 100, 124, 255]);

        // Same Y/Co/Cg numerators but scale=2 (b=8 -> b*(255/8)+1=2), which
        // exercises the scale divisor the all-zero-Co/Cg test above cannot:
        // co=(160-128)/255/2=16/255, cg=(100-128)/255/2=-14/255,
        // r=(128+16+14)/255=158/255, g=(128-14)/255=114/255,
        // b=(128-16+14)/255=126/255.
        let mut px = [160u8, 100, 8, 128];
        decode_ycocg(&mut px, false);
        assert_eq!(px, [158, 114, 126, 255]);
    }

    #[test]
    fn reconstruct_normal_z_forward_facing_is_flat_blue() {
        // R=G=128 (unit circle center) -> X=Y=1, deriveB maxed, normal
        // should reconstruct to (128,128,255)-ish (forward-facing).
        let mut px = [128u8, 128, 0, 255];
        reconstruct_normal_z(&mut px);
        assert_eq!(px[0], 128);
        assert_eq!(px[1], 128);
        assert_eq!(px[2], 255);
    }

    #[test]
    fn reconstruct_normal_z_out_of_gamut_does_not_panic_or_produce_nan_byte() {
        // R=G=255 pushes X=Y=1 each, so X^2+Y^2 > 1: negative under the
        // sqrt, must clamp to 0 rather than propagate NaN.
        let mut px = [255u8, 255, 0, 255];
        reconstruct_normal_z(&mut px);
        assert_eq!(px[2], 128); // (0/2)+128
    }

    #[test]
    fn hemi_oct_moves_blue_to_alpha() {
        let mut px = [128u8, 128, 200, 0];
        decode_hemi_oct(&mut px);
        assert_eq!(px[3], 200);
    }

    #[test]
    fn hemi_oct_matches_reference_at_known_values() {
        // R=G=128 -> nx/ny both land within a float epsilon of 0 (the
        // `-1.003_922` constant vs the true 256/255 differs by ~4.3e-7),
        // pushing nz within that same epsilon of 1: R and G both resolve to
        // 127 (0.5 * 255 truncated), and B to 255 with the epsilon possibly
        // rounding it down by one -- the ±1 this test allows on B only.
        let mut px = [128u8, 128, 200, 0];
        decode_hemi_oct(&mut px);
        assert_eq!(px[0], 127);
        assert_eq!(px[1], 127);
        assert!((i16::from(px[2]) - 255).abs() <= 1, "b={}", px[2]);
        assert_eq!(px[3], 200);
    }

    #[test]
    fn hemi_oct_matches_reference_at_asymmetric_values() {
        // R != G, so a flipped ny sign or a swapped R/G would both be
        // caught here (the R=G=128 case above leaves ny exactly 0, which
        // can't distinguish either mistake): nx=(200+100)/255-1.003922,
        // ny=(200-100)/255, nz=1-|nx|-|ny|, normalised then packed to
        // bytes as in `decode_hemi_oct`; a flipped ny sign would give
        // G=45 instead of 209.
        let mut px = [200u8, 100, 50, 0];
        decode_hemi_oct(&mut px);
        assert_eq!(px, [163, 209, 218, 50]);
    }

    #[test]
    fn resolve_drops_normalize_normals_when_bc7_hemi_oct_both_set() {
        let codec = resolve(VTexFormat::Bc7, false, false, true, true);
        assert!(codec.hemi_oct_rb);
        assert!(!codec.normalize_normals);
    }

    #[test]
    fn resolve_keeps_dxt5_normalize_normals_as_dxt5nm() {
        let codec = resolve(VTexFormat::Dxt5, false, false, true, false);
        assert!(codec.dxt5nm);
        assert!(codec.normalize_normals);
    }

    #[test]
    fn resolve_marks_cube_ycocg_as_srgb() {
        let codec = resolve(VTexFormat::Dxt5, true, true, false, false);
        assert!(codec.ycocg);
        assert!(codec.ycocg_srgb);
    }

    #[test]
    fn apply_is_noop_for_default_codec() {
        let mut rgba = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let before = rgba.clone();
        apply(&mut rgba, TextureCodec::default());
        assert_eq!(rgba, before);
    }
}
