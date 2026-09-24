//! World lightmaps: raw BC-block export straight from `m_lightMaps` (`s6f3a4_lighting.md` change
//! item 1) -- no re-encoding, the browser uploads these as compressed WebGL2 textures.
//!
//! Ground truth: `World.cs:28-37`/`WorldLoader.cs:474-545` (`m_lightMaps` file list, sampler
//! names); facts (sizes, formats, mip counts) cross-checked against `REPORT.md` §1 on real
//! Mirage data.

use sha2::{Digest, Sha256};

use crate::source::{Sources, compiled_path};

/// One raw lightmap file this export writes next to `render.glb`.
pub struct LightmapFile {
    /// `render_lm_<name>_<mip>.bin`.
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub mip_level: u32,
    pub sha256: String,
    /// `bytes.len()` (§8's `byteLength`), kept alongside `bytes` so `export.rs` can report it
    /// after `bytes` has already been moved into `extra_files`.
    pub byte_length: usize,
}

/// The WebGL2 compressed-texture name for a raw block format this export can hand the browser
/// unmodified, `None` for anything else (§3's fix: shared with `environment.rs`'s sky cube, so
/// both raw-block writers agree on the same mapping and the same "unmapped -> don't write a file"
/// rule -- an unmapped format used to be labelled `"unknown"` and written anyway).
pub(crate) fn compressed_format_name(format: s2tex::VTexFormat) -> Option<&'static str> {
    match format {
        s2tex::VTexFormat::Bc6H => Some("BC6H_UF16"),
        s2tex::VTexFormat::Bc7 => Some("BC7"),
        s2tex::VTexFormat::Ati1N => Some("BC4"),
        s2tex::VTexFormat::Ati2N => Some("BC5"),
        s2tex::VTexFormat::Dxt1 => Some("BC1"),
        s2tex::VTexFormat::Dxt5 => Some("BC3"),
        _ => None,
    }
}

/// Finds the `m_lightMaps` entry whose basename (without extension) matches `name`
/// (`s6f3a4_lighting.md`: "выбор файлов -- по `m_lightMaps` мира, не по списку каталога").
fn find_by_basename<'a>(light_maps: &'a [String], name: &str) -> Option<&'a str> {
    light_maps.iter().find_map(|p| {
        let base = p.rsplit('/').next().unwrap_or(p);
        let base = base.strip_suffix(".vtex").unwrap_or(base);
        base.eq_ignore_ascii_case(name).then_some(p.as_str())
    })
}

fn load_raw(
    sources: &Sources,
    path: &str,
    level: u32,
    out_name: &str,
) -> Result<LightmapFile, String> {
    let compiled = compiled_path(path);
    let bytes = sources
        .read(&compiled)
        .ok_or_else(|| format!("{compiled}: not found"))?;
    let raw = s2tex::decode_raw_mip_bytes(&bytes, level)
        .map_err(|e| format!("{compiled}: failed to read mip {level}: {e}"))?;
    let Some(format) = compressed_format_name(raw.format) else {
        return Err(format!(
            "{compiled}: format {:?} has no known WebGL2 compressed-texture mapping, not writing a raw block file",
            raw.format
        ));
    };
    let mut hasher = Sha256::new();
    hasher.update(&raw.bytes);
    let sha256 = format!("{:x}", hasher.finalize());
    Ok(LightmapFile {
        file_name: format!("render_lm_{out_name}_{level}.bin"),
        byte_length: raw.bytes.len(),
        bytes: raw.bytes,
        format,
        width: raw.width,
        height: raw.height,
        mip_level: level,
        sha256,
    })
}

/// Picks `irradiance`'s default (non-`--lightmap-quality high`) mip level (§1's fix): reads the
/// header and takes the first level whose width is `<= 4096`, capped to the texture's last level
/// if none are (a texture whose mip chain is coarser than that, e.g. only 8192 then 4096 like
/// Mirage, still lands on 4096; one with no mip below 4096 at all falls back to its smallest).
/// `high_quality` always picks level `0` (the full-resolution mip) without reading anything.
fn pick_irradiance_level(sources: &Sources, path: &str, high_quality: bool) -> Result<u32, String> {
    if high_quality {
        return Ok(0);
    }
    let compiled = compiled_path(path);
    let bytes = sources
        .read(&compiled)
        .ok_or_else(|| format!("{compiled}: not found"))?;
    let resource = s2fmt::resource::Resource::parse(bytes)
        .map_err(|e| format!("{compiled}: failed to parse: {e}"))?;
    let data_block = resource
        .block(s2fmt::resource::FourCC::DATA)
        .ok_or_else(|| format!("{compiled}: no DATA block"))?;
    let header = s2tex::header::parse(resource.block_bytes(data_block))
        .map_err(|e| format!("{compiled}: failed to read header: {e}"))?;
    let num_levels = u32::from(header.num_mip_levels);
    for level in 0..num_levels {
        let width = u32::from(header.width)
            .checked_shr(level)
            .unwrap_or(0)
            .max(1);
        if width <= 4096 {
            return Ok(level);
        }
    }
    Ok(num_levels.saturating_sub(1))
}

/// Every raw lightmap file this export writes (§1): `irradiance` at [`pick_irradiance_level`]'s
/// mip (`0` for `--lightmap-quality high`), `direct_light_shadows` at mip 0 (always full
/// resolution -- small even uncompressed, REPORT.md §8), `directional_irradiance` at its only mip
/// (0). Entries this map's `m_lightMaps` doesn't list are silently absent, not an error (a map
/// without baked lighting has none of these).
pub fn export_files(
    sources: &Sources,
    light_maps: &[String],
    high_quality: bool,
) -> (Vec<LightmapFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut missing = Vec::new();

    if let Some(path) = find_by_basename(light_maps, "irradiance") {
        match pick_irradiance_level(sources, path, high_quality) {
            Ok(level) => match load_raw(sources, path, level, "irradiance") {
                Ok(f) => files.push(f),
                Err(e) => missing.push(e),
            },
            Err(e) => missing.push(e),
        }
    }

    for (basename, out_name, level) in [
        ("direct_light_shadows", "direct_light_shadows", 0),
        ("directional_irradiance", "directional_irradiance", 0),
    ] {
        let Some(path) = find_by_basename(light_maps, basename) else {
            continue;
        };
        match load_raw(sources, path, level, out_name) {
            Ok(f) => files.push(f),
            Err(e) => missing.push(e),
        }
    }
    (files, missing)
}

/// RGBM8 range for the irradiance PNG fallback: p99.9 of Mirage's irradiance texels is 5.0 and
/// the max is 31.9 (REPORT.md §1's histogram), so `8` covers the overwhelming majority of texels
/// exactly and only clips the brightest ~0.1% (a fallback path only used when the browser lacks
/// `EXT_texture_compression_bptc`).
const IRRADIANCE_RGBM_RANGE: f32 = 8.0;

/// Encodes `rgba` (linear HDR, `width*height` `[r,g,b,a]` floats) as RGBM8: `m =
/// clamp(max(r,g,b)/range, 1/255, 1)`, stored channels `= clamp(c/(range*m), 0, 1)`, alpha `= m`.
/// `m` is then quantized up to the exact value its 8-bit alpha byte will decode to (`ceil`, not
/// `round`) *before* dividing the colour channels by it -- dividing by the continuous `m` but
/// storing `round(m*255)/255` as alpha leaves the two inconsistent, up to ~33% relative error on
/// dark texels where alpha's 8 bits of precision matter most.
/// `pub(crate)`: shared with `environment.rs`'s sky face fallback (§9), which encodes the same way.
pub(crate) fn encode_rgbm8(rgba: &[f32], range: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        let maxc = px[0].max(px[1]).max(px[2]).max(0.0);
        let m = (maxc / range).clamp(1.0 / 255.0, 1.0);
        let m = ((m * 255.0).ceil() / 255.0).min(1.0);
        for &c in &px[0..3] {
            out.push(((c / (range * m)).clamp(0.0, 1.0) * 255.0).round() as u8);
        }
        out.push((m * 255.0).round() as u8);
    }
    out
}

/// Box-downsamples one `width x height` RGBA float image by 2x2 (rounding an odd dimension up, so
/// the last row/column of texels is weighted the same as every other pair rather than dropped).
/// Shared by the irradiance (§7) and sky (§9) PNG fallbacks: both need a specific pixel budget a
/// texture's own coarse mip chain can't always hit exactly (e.g. Mirage's irradiance only has
/// 8192/4096, no 2048), so this halves the largest mip that's still `>=` the target until it fits.
pub(crate) fn box_downsample_2x2(rgba: &[f32], width: u32, height: u32) -> (Vec<f32>, u32, u32) {
    let new_w = width.div_ceil(2).max(1);
    let new_h = height.div_ceil(2).max(1);
    let mut out = vec![0f32; (new_w * new_h * 4) as usize];
    for y in 0..new_h {
        for x in 0..new_w {
            let mut acc = [0f32; 4];
            let mut n = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let sx = (x * 2 + dx).min(width - 1);
                    let sy = (y * 2 + dy).min(height - 1);
                    let idx = ((sy * width + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += rgba[idx + c];
                    }
                    n += 1.0;
                }
            }
            let out_idx = ((y * new_w + x) * 4) as usize;
            for c in 0..4 {
                out[out_idx + c] = acc[c] / n;
            }
        }
    }
    (out, new_w, new_h)
}

/// One fallback PNG this export writes when the browser lacks the compressed-texture extension
/// for a lightmap's real format (§1's "запасной путь").
pub struct FallbackFile {
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// `bytes.len()` (§8's `byteLength`).
    pub byte_length: usize,
    /// `"RGBM8"` or `"R8"` (§8): how to read the channels back into a light value.
    pub encoding: &'static str,
    /// `Some(range)` only for an RGBM8 encoding (§8's `rgbmRange`).
    pub rgbm_range: Option<f32>,
    /// §8's decode text, e.g. `"linear = rgb*a*8; PNG is linear, not sRGB"` for irradiance, or the
    /// shadow fallback's own channel/visibility note.
    pub decode: &'static str,
}

/// The two always-generated fallback PNGs (§1: "генерировать всегда, они маленькие"): irradiance
/// as RGBM8 2048² (§7: box-downsampled 2x2 from whatever coarser mip the texture actually has,
/// e.g. Mirage's 4096² -- measured ~7.2 MB, vs 22.8 MB for the un-downsampled 4096² this used to
/// write) and `direct_light_shadows` as an 8-bit grayscale (R channel) 4096² PNG.
pub fn export_fallback_files(
    sources: &Sources,
    light_maps: &[String],
) -> (Vec<FallbackFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut missing = Vec::new();

    if let Some(path) = find_by_basename(light_maps, "irradiance") {
        let compiled = compiled_path(path);
        match sources.read(&compiled) {
            Some(bytes) => match s2tex::decode_hdr_bytes(&bytes, 2048) {
                Ok(hdr) => {
                    let mut rgba = hdr.rgba;
                    let mut w = hdr.width;
                    let mut h = hdr.height;
                    while w > 2048 || h > 2048 {
                        let (down, nw, nh) = box_downsample_2x2(&rgba, w, h);
                        rgba = down;
                        w = nw;
                        h = nh;
                    }
                    let rgba8 = encode_rgbm8(&rgba, IRRADIANCE_RGBM_RANGE);
                    match s2tex::encode_image(&rgba8, w, h, 90, true) {
                        Ok(enc) => files.push(FallbackFile {
                            file_name: "render_lm_irradiance_fallback.png".to_string(),
                            byte_length: enc.bytes.len(),
                            bytes: enc.bytes,
                            width: w,
                            height: h,
                            encoding: "RGBM8",
                            rgbm_range: Some(IRRADIANCE_RGBM_RANGE),
                            decode: "linear = rgb*a*8; PNG is linear, not sRGB",
                        }),
                        Err(e) => {
                            missing.push(format!("{compiled}: failed to encode RGBM fallback: {e}"))
                        }
                    }
                }
                Err(e) => missing.push(format!("{compiled}: failed to decode HDR fallback: {e}")),
            },
            None => missing.push(format!("{compiled}: not found")),
        }
    }

    if let Some(path) = find_by_basename(light_maps, "direct_light_shadows") {
        let compiled = compiled_path(path);
        match sources.read(&compiled) {
            Some(bytes) => match s2tex::decode_bytes(&bytes, 4096) {
                Ok(decoded) => match s2tex::encode_image(
                    &decoded.rgba,
                    decoded.width,
                    decoded.height,
                    90,
                    true,
                ) {
                    Ok(enc) => files.push(FallbackFile {
                        file_name: "render_lm_direct_light_shadows_fallback.png".to_string(),
                        byte_length: enc.bytes.len(),
                        bytes: enc.bytes,
                        width: decoded.width,
                        height: decoded.height,
                        encoding: "RGBA8",
                        rgbm_range: None,
                        decode: "same channels as the raw file; vis = 1 - channel[bakedShadowChannel]",
                    }),
                    Err(e) => {
                        missing.push(format!("{compiled}: failed to encode shadow fallback: {e}"))
                    }
                },
                Err(e) => {
                    missing.push(format!("{compiled}: failed to decode shadow fallback: {e}"))
                }
            },
            None => missing.push(format!("{compiled}: not found")),
        }
    }

    (files, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_basename_matches_case_insensitively_and_ignores_directory() {
        let list = vec![
            "maps/de_mirage/lightmaps/irradiance.vtex".to_string(),
            "maps/de_mirage/lightmaps/Direct_Light_Shadows.vtex".to_string(),
        ];
        assert_eq!(
            find_by_basename(&list, "irradiance"),
            Some("maps/de_mirage/lightmaps/irradiance.vtex")
        );
        assert_eq!(
            find_by_basename(&list, "direct_light_shadows"),
            Some("maps/de_mirage/lightmaps/Direct_Light_Shadows.vtex")
        );
        assert_eq!(find_by_basename(&list, "debug_chart_color"), None);
    }

    #[test]
    fn rgbm8_round_trips_within_quantization_error() {
        let rgba = [2.0f32, 4.0, 1.0, 1.0]; // one texel, well within range 8
        let encoded = encode_rgbm8(&rgba, 8.0);
        let m = f32::from(encoded[3]) / 255.0;
        let decoded = [
            f32::from(encoded[0]) / 255.0 * 8.0 * m,
            f32::from(encoded[1]) / 255.0 * 8.0 * m,
            f32::from(encoded[2]) / 255.0 * 8.0 * m,
        ];
        for (got, want) in decoded.iter().zip([2.0, 4.0, 1.0]) {
            assert!((got - want).abs() < 0.05, "{decoded:?}");
        }
    }

    /// §5's fix: quantizing `m` up to its own alpha byte *before* dividing the colour channels by
    /// it (rather than dividing by the continuous `m` and separately rounding alpha) keeps the two
    /// consistent -- previously up to ~33% relative error on dark texels, where alpha's 8 bits of
    /// precision matter most.
    #[test]
    fn rgbm8_dark_texels_round_trip_within_one_percent_relative() {
        for &v in &[0.05f32, 0.3, 1.7, 7.9] {
            let encoded = encode_rgbm8(&[v, v, v, 1.0], 8.0);
            let m = f32::from(encoded[3]) / 255.0;
            let decoded = f32::from(encoded[0]) / 255.0 * 8.0 * m;
            let rel_err = (decoded - v).abs() / v;
            assert!(rel_err < 0.01, "v={v} decoded={decoded} rel_err={rel_err}");
        }
    }

    #[test]
    fn compressed_format_name_maps_known_formats_and_rejects_unknown() {
        assert_eq!(
            compressed_format_name(s2tex::VTexFormat::Bc6H),
            Some("BC6H_UF16")
        );
        assert_eq!(
            compressed_format_name(s2tex::VTexFormat::Ati1N),
            Some("BC4")
        );
        assert_eq!(
            compressed_format_name(s2tex::VTexFormat::Ati2N),
            Some("BC5")
        );
        assert_eq!(compressed_format_name(s2tex::VTexFormat::Bc7), Some("BC7"));
        assert_eq!(compressed_format_name(s2tex::VTexFormat::Dxt1), Some("BC1"));
        assert_eq!(compressed_format_name(s2tex::VTexFormat::Dxt5), Some("BC3"));
        assert_eq!(compressed_format_name(s2tex::VTexFormat::Rgba8888), None);
    }

    #[test]
    fn box_downsample_2x2_halves_dimensions_and_averages() {
        // 2x2 image, four distinct values -> one output texel, the mean.
        #[rustfmt::skip]
        let rgba = [
            0.0, 0.0, 0.0, 0.0,   4.0, 0.0, 0.0, 0.0,
            0.0, 4.0, 0.0, 0.0,   0.0, 0.0, 4.0, 0.0,
        ];
        let (out, w, h) = box_downsample_2x2(&rgba, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![1.0, 1.0, 1.0, 0.0]);
    }
}
