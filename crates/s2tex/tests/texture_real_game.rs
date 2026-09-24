//! Tests against a real CS2 install. Ignored by default; run with
//! `CS2_GAME_DIR` pointing at `...\game\csgo`, e.g.:
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2tex --release --test texture_real_game -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use s2fmt::resource::Resource;
use s2fmt::vpk::{Vpk, VpkEntry};
use s2tex::TexError;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test, e.g. \
             `set CS2_GAME_DIR=D:\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo`"
        ),
    }
}

const BUDGET: u32 = 1024;

/// A short, stable label for a decode failure, bucketed by error kind (and,
/// for the two kinds where it matters, by which format/value) -- exactly
/// the "успехи и отказы по причинам" the spec's real-archive test asks for.
fn failure_reason(e: &TexError) -> String {
    match e {
        TexError::Resource(_) => "resource_parse".to_string(),
        TexError::MissingDataBlock => "missing_data_block".to_string(),
        TexError::Truncated { .. } => "truncated".to_string(),
        TexError::UnsupportedVersion { actual } => format!("unsupported_version:{actual}"),
        TexError::UnknownFormat { raw } => format!("unknown_format:{raw}"),
        TexError::UnsupportedFormat { format } => format!("unsupported_format:{format:?}"),
        TexError::InvalidExtraData { .. } => "invalid_extra_data".to_string(),
        TexError::InvalidDimensions { .. } => "invalid_dimensions".to_string(),
        TexError::DimensionsTooLarge { .. } => "dimensions_too_large".to_string(),
        TexError::InvalidMipCount => "invalid_mip_count".to_string(),
        TexError::TooManyExtraData { .. } => "too_many_extra_data".to_string(),
        TexError::TooManyMips { .. } => "too_many_mips".to_string(),
        TexError::InvalidMipLevel { .. } => "invalid_mip_level".to_string(),
        TexError::SizeTooLarge { .. } => "size_too_large".to_string(),
        TexError::SizeOverflow => "size_overflow".to_string(),
        TexError::Lz4(_) => "lz4".to_string(),
        TexError::BlockDecode { format, .. } => format!("block_decode:{format:?}"),
        TexError::JpegDecode(_) => "jpeg_decode".to_string(),
        TexError::PngDecode(_) => "png_decode".to_string(),
        TexError::JpegEncode(_) => "jpeg_encode".to_string(),
        TexError::PngEncode(_) => "png_encode".to_string(),
        TexError::EncodeDimensionsTooLarge { .. } => "encode_dimensions_too_large".to_string(),
        TexError::EncodeBufferSizeMismatch { .. } => "encode_buffer_size_mismatch".to_string(),
        TexError::InvalidHdrLayer { .. } => "invalid_hdr_layer".to_string(),
    }
}

struct ScanStats {
    format_histogram: BTreeMap<String, usize>,
    failure_reasons: BTreeMap<String, usize>,
    successes: usize,
    failures: usize,
    encoded_bytes: u64,
    truncated_to_first_layer: usize,
    /// A handful of successfully decoded, deterministically-picked
    /// thumbnails for the contact sheet.
    thumbnails: Vec<(String, u32, u32, Vec<u8>)>,
    de_mirage_total: usize,
    de_mirage_failures: usize,
}

fn thumbnail(rgba: &[u8], w: u32, h: u32, out_size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (out_size * out_size * 4) as usize];
    for y in 0..out_size {
        for x in 0..out_size {
            let sx = (x * w / out_size).min(w - 1);
            let sy = (y * h / out_size).min(h - 1);
            let src = ((sy * w + sx) * 4) as usize;
            let dst = ((y * out_size + x) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    out
}

/// Decodes and encodes every entry in `entries`, folding results into
/// `stats`. `de_mirage_paths` additionally tallies `stats.de_mirage_*` so
/// the map's own surface textures (all in formats this crate must support)
/// can be checked separately from the whole-archive sample (which
/// legitimately includes lightmap/skybox-only formats this crate doesn't
/// support by design).
fn scan(
    vpk: &Vpk,
    entries: &[&VpkEntry],
    de_mirage_paths: &std::collections::HashSet<String>,
    stats: &mut ScanStats,
    thumb_every: usize,
) {
    for (i, entry) in entries.iter().enumerate() {
        let is_de_mirage = de_mirage_paths.contains(&entry.path);
        if is_de_mirage {
            stats.de_mirage_total += 1;
        }
        let fail = |stats: &mut ScanStats, reason: String| {
            stats.failures += 1;
            *stats.failure_reasons.entry(reason).or_insert(0) += 1;
            if is_de_mirage {
                stats.de_mirage_failures += 1;
            }
        };

        let bytes = match vpk.read(entry) {
            Ok(b) => b,
            Err(_) => {
                fail(stats, "vpk_read".to_string());
                continue;
            }
        };
        let resource = match Resource::parse(bytes.clone()) {
            Ok(r) => r,
            Err(_) => {
                fail(stats, "resource_parse".to_string());
                continue;
            }
        };

        match s2tex::decode(&resource, &bytes, BUDGET) {
            Ok(image) => {
                stats.successes += 1;
                *stats
                    .format_histogram
                    .entry(format!("{:?}", image.format))
                    .or_insert(0) += 1;
                if image.truncated_to_first_layer {
                    stats.truncated_to_first_layer += 1;
                }
                if let Ok(encoded) = image.encode(85) {
                    stats.encoded_bytes += encoded.bytes.len() as u64;
                }
                if i % thumb_every == 0 && stats.thumbnails.len() < 40 {
                    let thumb = thumbnail(&image.rgba, image.width, image.height, 96);
                    stats.thumbnails.push((entry.path.clone(), 96, 96, thumb));
                }
            }
            Err(e) => fail(stats, failure_reason(&e)),
        }
    }
}

fn write_contact_sheet(thumbnails: &[(String, u32, u32, Vec<u8>)], path: &Path) {
    const CELL: u32 = 96;
    const PAD: u32 = 2;
    const COLS: u32 = 8;
    let rows = (thumbnails.len() as u32).div_ceil(COLS).max(1);
    let width = COLS * (CELL + PAD) + PAD;
    let height = rows * (CELL + PAD) + PAD;
    let mut canvas = vec![0u8; (width * height * 4) as usize];
    for px in canvas.as_chunks_mut::<4>().0 {
        *px = [40, 40, 40, 255]; // dark gray background, opaque
    }
    for (i, (_, w, h, thumb)) in thumbnails.iter().enumerate() {
        let col = i as u32 % COLS;
        let row = i as u32 / COLS;
        let ox = PAD + col * (CELL + PAD);
        let oy = PAD + row * (CELL + PAD);
        for y in 0..*h {
            for x in 0..*w {
                let src = ((y * w + x) * 4) as usize;
                let dst = (((oy + y) * width + (ox + x)) * 4) as usize;
                canvas[dst..dst + 4].copy_from_slice(&thumb[src..src + 4]);
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create contact sheet directory");
    }
    let file = std::fs::File::create(path).expect("create contact sheet file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write contact sheet header");
    writer
        .write_image_data(&canvas)
        .expect("write contact sheet pixels");
}

#[test]
#[ignore = "needs CS2_GAME_DIR; slow, run with --release"]
fn de_mirage_and_pak01_sample_decode() {
    let path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

    let de_mirage: Vec<&VpkEntry> = vpk
        .entries()
        .filter(|e| e.path.starts_with("materials/de_mirage/") && e.path.ends_with(".vtex_c"))
        .collect();

    let all_vtex: Vec<&VpkEntry> = vpk
        .entries()
        .filter(|e| e.path.ends_with(".vtex_c"))
        .collect();
    let want = 2000usize.min(all_vtex.len());
    let stride = (all_vtex.len() / want).max(1);
    let sample: Vec<&VpkEntry> = all_vtex
        .iter()
        .step_by(stride)
        .take(want)
        .copied()
        .collect();

    let de_mirage_paths: std::collections::HashSet<String> =
        de_mirage.iter().map(|e| e.path.clone()).collect();

    // De-duplicate by path so an entry sampled into both groups is only
    // decoded once for the combined stats.
    let mut seen = std::collections::HashSet::new();
    let mut combined: Vec<&VpkEntry> = Vec::new();
    for e in de_mirage.iter().chain(sample.iter()) {
        if seen.insert(e.path.clone()) {
            combined.push(e);
        }
    }

    println!(
        "=== scanning {} de_mirage + {} sampled ({} unique) vtex_c entries, budget={BUDGET} ===",
        de_mirage.len(),
        sample.len(),
        combined.len()
    );

    let mut stats = ScanStats {
        format_histogram: BTreeMap::new(),
        failure_reasons: BTreeMap::new(),
        successes: 0,
        failures: 0,
        encoded_bytes: 0,
        truncated_to_first_layer: 0,
        thumbnails: Vec::new(),
        de_mirage_total: 0,
        de_mirage_failures: 0,
    };

    let thumb_every = (combined.len() / 40).max(1);
    let start = Instant::now();
    scan(&vpk, &combined, &de_mirage_paths, &mut stats, thumb_every);
    let elapsed = start.elapsed();

    println!("total entries: {}", combined.len());
    println!("successes: {}", stats.successes);
    println!("failures: {}", stats.failures);
    println!(
        "truncated_to_first_layer (cube/array/volume): {}",
        stats.truncated_to_first_layer
    );
    println!("total decode+encode time: {elapsed:?}");
    println!("total encoded (JPEG/PNG) bytes: {}", stats.encoded_bytes);
    println!("format histogram:");
    for (format, count) in &stats.format_histogram {
        println!("  {format}: {count}");
    }
    println!("failure reasons:");
    for (reason, count) in &stats.failure_reasons {
        println!("  {reason}: {count}");
    }

    let contact_sheet_path = PathBuf::from(r"D:\porject\modulator-work\scratch\s2tex_contact.png");
    write_contact_sheet(&stats.thumbnails, &contact_sheet_path);
    println!("contact sheet: {}", contact_sheet_path.display());

    println!(
        "de_mirage: {}/{} failed to decode",
        stats.de_mirage_failures, stats.de_mirage_total
    );

    assert!(
        stats.successes > 0,
        "expected at least one texture to decode"
    );
    // Every de_mirage surface texture uses a format this crate supports
    // (BC1/BC3/BC7/RGBA8888/etc, confirmed by the format histogram above);
    // a near-total failure here would mean the decoder itself is broken,
    // as opposed to the whole-archive sample legitimately hitting
    // lightmap/skybox-only formats (BC6H, ETC2, ...) this crate doesn't
    // support by design.
    let de_mirage_failure_rate =
        stats.de_mirage_failures as f64 / stats.de_mirage_total.max(1) as f64;
    assert!(
        de_mirage_failure_rate < 0.05,
        "{}/{} de_mirage textures failed to decode (>5%)",
        stats.de_mirage_failures,
        stats.de_mirage_total
    );
}

/// Decodes a VRF-exported reference PNG to RGBA8, the same way
/// `decode::decode_raw_png` normalizes a vtex's own embedded PNG (kept as a
/// separate copy here since that function is a private implementation
/// detail of the crate, not part of its public surface).
fn load_reference_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let pixels = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|px| [px[0], px[1], px[2], 255])
            .collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|px| [px[0], px[0], px[0], px[1]])
            .collect(),
        png::ColorType::Indexed => unreachable!("normalize_to_color8 expands palettes"),
    };
    (info.width, info.height, rgba)
}

/// Per-pixel worst absolute difference (over R/G/B; alpha is uniformly 255
/// in both images for the plain color/normal textures compared here) and
/// how many pixels exceed `tolerance`.
fn worst_diff(a: &[u8], b: &[u8], tolerance: i16) -> (i16, usize) {
    let mut worst = 0i16;
    let mut over = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()) {
        let mut pixel_worst = 0i16;
        for c in 0..3 {
            let d = (i16::from(pa[c]) - i16::from(pb[c])).abs();
            pixel_worst = pixel_worst.max(d);
        }
        worst = worst.max(pixel_worst);
        if pixel_worst > tolerance {
            over += 1;
        }
    }
    (worst, over)
}

#[test]
#[ignore = "needs CS2_GAME_DIR and a VRF reference export"]
fn de_mirage_matches_vrf_reference_export() {
    let vrf_dir = PathBuf::from(r"D:\porject\modulator-work\scratch\vrf_export\de_mirage");
    if !vrf_dir.is_dir() {
        println!("no VRF reference export at {vrf_dir:?}; skipping comparison");
        return;
    }

    let path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

    // Three ordinary color textures and two normal maps, all under
    // materials/de_mirage/base/, each confirmed present in the reference
    // export's PNGs by de_mirage_summary.json.
    const CASES: &[&str] = &[
        "base_1_diffuse_blend_psd_37bb7bd3",
        "base_mid_ver1_diffuse_color_psd_a1ba91d5",
        "base_top_ver1_diffuse_color_psd_41ae5e62",
        "base_mid_ver1_normal_psd_fdce5b8c",
        "base_top_ver1_normal_psd_6397a1fd",
    ];

    // BC1/BC3/BC7 block decode is fully specified by the format (fixed
    // endpoint interpolation weights), so texture2ddecoder and VRF's
    // TinyBCSharp should agree almost exactly; a couple of LSBs of
    // rounding slack in the endpoint/weight math is the only legitimate
    // source of difference, hence a tight tolerance.
    const TOLERANCE: i16 = 3;

    let mut worst_overall = 0i16;
    for stem in CASES {
        let vtex_path = format!("materials/de_mirage/base/{stem}.vtex_c");
        let bytes = vpk
            .read_path(&vtex_path)
            .unwrap_or_else(|e| panic!("failed to read {vtex_path}: {e}"));
        let resource = Resource::parse(bytes.clone()).expect("Resource::parse");
        let ours = s2tex::decode(&resource, &bytes, s2tex::header::MAX_SIDE)
            .unwrap_or_else(|e| panic!("failed to decode {vtex_path}: {e}"));

        let png_path = vrf_dir.join(format!("{stem}.png"));
        let (ref_w, ref_h, reference) = load_reference_png(&png_path);

        assert_eq!(
            (ours.width, ours.height),
            (ref_w, ref_h),
            "{stem}: dimension mismatch"
        );
        let (worst, over) = worst_diff(&ours.rgba, &reference, TOLERANCE);
        println!(
            "{stem}: {}x{} worst_diff={worst} pixels_over_tolerance={over}/{}",
            ours.width,
            ours.height,
            ours.width * ours.height
        );
        worst_overall = worst_overall.max(worst);
        assert!(
            worst <= TOLERANCE,
            "{stem}: worst per-channel difference {worst} exceeds tolerance {TOLERANCE}"
        );
    }
    println!("worst difference across all cases: {worst_overall}");
}

// ---------------------------------------------------------------------
// F3a-2b (`s6f3a2b_hdr.md`): BC6H and float HDR formats.
// ---------------------------------------------------------------------

/// A short label for an HDR decode failure, by error kind/format --
/// `failure_reason`'s counterpart for [`s2tex::decode_hdr_bytes`].
fn hdr_failure_reason(e: &TexError) -> String {
    match e {
        TexError::UnsupportedFormat { format } => format!("not_hdr_format:{format:?}"),
        other => failure_reason(other),
    }
}

/// Scans `entries` with [`s2tex::decode_hdr_bytes`], folding results into
/// `stats`. A texture whose format isn't one of the HDR ones this crate
/// supports (`hdr_decode::decode_hdr_slice`'s list, plus every raw
/// JPEG/PNG/WebP format) reports `UnsupportedFormat` and is *not* counted
/// as a failure -- it means "not an HDR texture", the expected outcome for
/// the vast majority of `pak01`'s vtex_c population (`s6f3a2b_hdr.md`'s
/// real-archive test is about the BC6H/float subset, not every texture).
struct HdrScanStats {
    format_histogram: BTreeMap<String, usize>,
    failure_reasons: BTreeMap<String, usize>,
    hdr_successes: usize,
    hdr_failures: usize,
    not_hdr_format: usize,
}

fn scan_hdr(vpk: &Vpk, entries: &[&VpkEntry], stats: &mut HdrScanStats) {
    for entry in entries {
        let bytes = match vpk.read(entry) {
            Ok(b) => b,
            Err(_) => {
                stats.hdr_failures += 1;
                *stats
                    .failure_reasons
                    .entry("vpk_read".to_string())
                    .or_insert(0) += 1;
                continue;
            }
        };
        match s2tex::decode_hdr_bytes(&bytes, BUDGET) {
            Ok(image) => {
                stats.hdr_successes += 1;
                *stats
                    .format_histogram
                    .entry(format!("{:?}", image.format))
                    .or_insert(0) += 1;
            }
            Err(TexError::UnsupportedFormat { .. }) => stats.not_hdr_format += 1,
            Err(e) => {
                stats.hdr_failures += 1;
                *stats
                    .failure_reasons
                    .entry(hdr_failure_reason(&e))
                    .or_insert(0) += 1;
            }
        }
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR; slow, run with --release"]
fn hdr_and_float_real_archive_scan() {
    let pak01_path = game_dir().join("pak01_dir.vpk");
    let pak01 =
        Vpk::open(&pak01_path).unwrap_or_else(|e| panic!("failed to open {pak01_path:?}: {e}"));

    let all_vtex: Vec<&VpkEntry> = pak01
        .entries()
        .filter(|e| e.path.ends_with(".vtex_c"))
        .collect();
    let want = 2000usize.min(all_vtex.len());
    let stride = (all_vtex.len() / want).max(1);
    let sample: Vec<&VpkEntry> = all_vtex
        .iter()
        .step_by(stride)
        .take(want)
        .copied()
        .collect();

    println!(
        "=== pak01 sample: {} vtex_c entries, HDR budget={BUDGET} ===",
        sample.len()
    );
    let mut pak01_stats = HdrScanStats {
        format_histogram: BTreeMap::new(),
        failure_reasons: BTreeMap::new(),
        hdr_successes: 0,
        hdr_failures: 0,
        not_hdr_format: 0,
    };
    let start = Instant::now();
    scan_hdr(&pak01, &sample, &mut pak01_stats);
    let elapsed = start.elapsed();
    println!("elapsed: {elapsed:?}");
    println!(
        "hdr successes: {}, hdr failures: {}, not-HDR-format: {}",
        pak01_stats.hdr_successes, pak01_stats.hdr_failures, pak01_stats.not_hdr_format
    );
    println!("format histogram: {:?}", pak01_stats.format_histogram);
    println!("failure reasons: {:?}", pak01_stats.failure_reasons);

    // The map's own known HDR/float set (`s6f3a2b_hdr.md`'s task facts):
    // the sky cube (pak01) and de_mirage's lightmaps (its own map VPK) --
    // every BC6H texture the coordinator's data survey actually found.
    let sky_path = "materials/skybox/sky_de_mirage_exr_71e5f2a1.vtex_c";
    let sky_entry = pak01
        .entries()
        .find(|e| e.path == sky_path)
        .unwrap_or_else(|| panic!("{sky_path} not found in pak01"));

    let mirage_vpk_path = game_dir().join("maps").join("de_mirage.vpk");
    let mirage_vpk = Vpk::open(&mirage_vpk_path)
        .unwrap_or_else(|e| panic!("failed to open {mirage_vpk_path:?}: {e}"));
    let lightmap_paths = [
        "maps/de_mirage/lightmaps/irradiance.vtex_c",
        "maps/de_mirage/lightmaps/directional_irradiance.vtex_c",
        "maps/de_mirage/lightmaps/direct_light_shadows.vtex_c",
        "maps/de_mirage/lightmaps/env_light_probe_volume_atlas.vtex_c",
        "maps/de_mirage/lightmaps/env_light_probe_volume_atlas_dlshd.vtex_c",
    ];
    let lightmap_entries: Vec<&VpkEntry> = lightmap_paths
        .iter()
        .map(|p| {
            mirage_vpk
                .entries()
                .find(|e| &e.path == p)
                .unwrap_or_else(|| panic!("{p} not found in maps/de_mirage.vpk"))
        })
        .collect();

    println!("=== known de_mirage HDR/lightmap set ===");
    let mut known_stats = HdrScanStats {
        format_histogram: BTreeMap::new(),
        failure_reasons: BTreeMap::new(),
        hdr_successes: 0,
        hdr_failures: 0,
        not_hdr_format: 0,
    };
    scan_hdr(&pak01, &[sky_entry], &mut known_stats);
    let start = Instant::now();
    scan_hdr(&mirage_vpk, &lightmap_entries, &mut known_stats);
    let elapsed = start.elapsed();
    println!("elapsed (lightmap set): {elapsed:?}");
    println!(
        "hdr successes: {}, hdr failures: {}, not-HDR-format (expected for DXT1/ATI1N/BC7): {}",
        known_stats.hdr_successes, known_stats.hdr_failures, known_stats.not_hdr_format
    );
    println!("format histogram: {:?}", known_stats.format_histogram);
    println!("failure reasons: {:?}", known_stats.failure_reasons);

    // The sky and irradiance textures are both BC6H and must decode.
    assert!(
        known_stats.hdr_successes >= 2,
        "expected the sky cube and irradiance lightmap to both decode as HDR"
    );
    assert_eq!(
        known_stats.hdr_failures, 0,
        "no known HDR texture should fail to decode"
    );
}

/// Reads an EXR's first layer to interleaved RGBA f32
/// (`exr::image::FlatSamples::values_as_f32` widens whichever sample type
/// the file actually stores -- `f16`, `f32` or `u32` -- so this doesn't need
/// to know VRF's own export precision ahead of time).
fn load_exr_rgba(path: &Path) -> (u32, u32, Vec<f32>) {
    let image = exr::prelude::read_all_flat_layers_from_file(path)
        .unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
    let layer = &image.layer_data[0];
    let width = layer.size.0 as u32;
    let height = layer.size.1 as u32;
    let pixel_count = (width as usize) * (height as usize);

    let channel = |name: &str| -> Vec<f32> {
        let samples = &layer
            .channel_data
            .list
            .iter()
            .find(|c| c.name.to_string() == name)
            .unwrap_or_else(|| panic!("{path:?} has no {name} channel"))
            .sample_data;
        samples.values_as_f32().collect()
    };
    let (r, g, b, a) = (channel("R"), channel("G"), channel("B"), channel("A"));

    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for i in 0..pixel_count {
        rgba.extend_from_slice(&[r[i], g[i], b[i], a[i]]);
    }
    (width, height, rgba)
}

/// Worst absolute and relative (`|ours-ref| / max(|ref|, 1.0)`) difference
/// over R/G/B, sampled at every `stride`-th pixel (both images are
/// megapixel-plus; a stride keeps this test's runtime reasonable while
/// still covering the whole image). The `max(|ref|, 1.0)` floor keeps the
/// relative metric meaningful near black, where an absolute BC6H
/// quantization step can otherwise look like a huge relative error.
fn worst_hdr_diff(
    a: &[f32],
    b: &[f32],
    width: u32,
    height: u32,
    stride: usize,
) -> (f32, f32, usize) {
    let mut worst_abs = 0f32;
    let mut worst_rel = 0f32;
    let mut sampled = 0usize;
    for i in (0..(width as usize) * (height as usize)).step_by(stride) {
        sampled += 1;
        for c in 0..3 {
            let (av, bv) = (a[i * 4 + c], b[i * 4 + c]);
            let diff = (av - bv).abs();
            worst_abs = worst_abs.max(diff);
            worst_rel = worst_rel.max(diff / bv.abs().max(1.0));
        }
    }
    (worst_abs, worst_rel, sampled)
}

/// `s6f3a2b_hdr.md`: "для неба Mirage ... и одной карты освещения сравнить
/// с выходом эталона" -- the lightmap half. `irradiance.vtex_c` decodes as
/// a plain 2D BC6H image (no cubemap reprojection involved, unlike the sky
/// -- see `sky_cube_reference_stats_and_contact_sheet`), so this compares
/// full-resolution pixels directly against VRF's own `.exr` export.
#[test]
#[ignore = "needs CS2_GAME_DIR and the VRF reference export"]
fn irradiance_matches_vrf_reference_export() {
    let vrf_path = PathBuf::from(
        r"D:\porject\modulator-work\scratch\f3_lighting\lmdec\maps\de_mirage\lightmaps\irradiance.exr",
    );
    if !vrf_path.is_file() {
        println!("no VRF reference export at {vrf_path:?}; skipping comparison");
        return;
    }

    let mirage_vpk_path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&mirage_vpk_path)
        .unwrap_or_else(|e| panic!("failed to open {mirage_vpk_path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/lightmaps/irradiance.vtex_c")
        .expect("read irradiance.vtex_c");

    let ours = s2tex::decode_hdr_bytes(&bytes, s2tex::header::MAX_SIDE).expect("decode_hdr_bytes");
    println!(
        "ours: {}x{} layers={} format={:?} mip_level={}",
        ours.width, ours.height, ours.layers, ours.format, ours.mip_level
    );

    let (ref_w, ref_h, reference) = load_exr_rgba(&vrf_path);
    assert_eq!(
        (ours.width, ours.height),
        (ref_w, ref_h),
        "dimension mismatch"
    );

    // Half-float endpoints, exact bit-for-bit widening on both sides (see
    // this crate's `half_float` module and TinyBCSharp's own `(float)Half`)
    // -- 1e-3 relative tolerance is generous headroom for interpolation
    // rounding order, not precision loss (`s6f3a2b_hdr.md`'s own estimate).
    const RELATIVE_TOLERANCE: f32 = 1e-3;
    const STRIDE: usize = 97; // coprime-ish with 8192 for a spread sample

    let (worst_abs, worst_rel, sampled) =
        worst_hdr_diff(&ours.rgba, &reference, ours.width, ours.height, STRIDE);
    println!("sampled {sampled} pixels: worst_abs={worst_abs} worst_rel={worst_rel}");
    assert!(
        worst_rel <= RELATIVE_TOLERANCE,
        "worst relative difference {worst_rel} exceeds tolerance {RELATIVE_TOLERANCE}"
    );
}

/// `s6f3a2b_hdr.md`'s coordinator follow-up: the light probe volume atlas
/// is a BC6H **3D volume** texture (164x152x384), and [`s2tex::decode_hdr`]
/// must return every depth slice, not just the first, so a caller can
/// sample it per vertex. Spot-checks a handful of z-slices against VRF's
/// own per-slice `.exr` exports.
#[test]
#[ignore = "needs CS2_GAME_DIR and the VRF reference export"]
fn probe_volume_atlas_matches_vrf_reference_export() {
    let vrf_dir = PathBuf::from(
        r"D:\porject\modulator-work\scratch\f3_lighting\lpvdec\maps\de_mirage\lightmaps",
    );
    if !vrf_dir.is_dir() {
        println!("no VRF reference export at {vrf_dir:?}; skipping comparison");
        return;
    }

    let mirage_vpk_path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&mirage_vpk_path)
        .unwrap_or_else(|e| panic!("failed to open {mirage_vpk_path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/lightmaps/env_light_probe_volume_atlas.vtex_c")
        .expect("read env_light_probe_volume_atlas.vtex_c");

    let ours = s2tex::decode_hdr_bytes(&bytes, s2tex::header::MAX_SIDE).expect("decode_hdr_bytes");
    println!(
        "ours: {}x{} layers={} format={:?} mip_level={}",
        ours.width, ours.height, ours.layers, ours.format, ours.mip_level
    );
    assert_eq!((ours.width, ours.height), (164, 152));
    assert_eq!(
        ours.layers, 384,
        "expected every depth slice, not just the first"
    );

    const RELATIVE_TOLERANCE: f32 = 1e-3;
    let mut worst_abs_overall = 0f32;
    let mut worst_rel_overall = 0f32;
    for z in [0u32, 50, 100, 200, 383] {
        let vrf_path = vrf_dir.join(format!("env_light_probe_volume_atlas_z{z:03}.exr"));
        let (ref_w, ref_h, reference) = load_exr_rgba(&vrf_path);
        assert_eq!(
            (ref_w, ref_h),
            (ours.width, ours.height),
            "slice {z} dimension mismatch"
        );

        let per_layer = (ours.width as usize) * (ours.height as usize) * 4;
        let start = per_layer * (z as usize);
        let ours_layer = &ours.rgba[start..start + per_layer];

        let (worst_abs, worst_rel, sampled) =
            worst_hdr_diff(ours_layer, &reference, ours.width, ours.height, 1);
        println!("z={z}: sampled {sampled} pixels worst_abs={worst_abs} worst_rel={worst_rel}");
        worst_abs_overall = worst_abs_overall.max(worst_abs);
        worst_rel_overall = worst_rel_overall.max(worst_rel);
        assert!(
            worst_rel <= RELATIVE_TOLERANCE,
            "slice {z}: worst relative difference {worst_rel} exceeds tolerance {RELATIVE_TOLERANCE}"
        );
    }
    println!("worst across sampled slices: abs={worst_abs_overall} rel={worst_rel_overall}");
}

/// `s6f3a2b_hdr.md`'s change item 3: `decode_raw_mip`'s success path had no
/// test. These three buffer sizes are VRF's own "Mip level N - buffer size"
/// values (`D:\porject\modulator-work\scratch\f3_lighting\lm_data_blocks.txt`),
/// so this pins the crate's mip-size/LZ4 math against the reference on real
/// files rather than synthetic ones. `level` `0` is the largest,
/// full-resolution mip (`irradiance`'s level 1 is the first downsample,
/// 4096 from an 8192 base).
#[test]
#[ignore = "needs CS2_GAME_DIR; slow, run with --release"]
fn decode_raw_mip_matches_vrf_buffer_sizes() {
    let pak01_path = game_dir().join("pak01_dir.vpk");
    let pak01 =
        Vpk::open(&pak01_path).unwrap_or_else(|e| panic!("failed to open {pak01_path:?}: {e}"));
    let sky_bytes = pak01
        .read_path("materials/skybox/sky_de_mirage_exr_71e5f2a1.vtex_c")
        .expect("read sky_de_mirage_exr_71e5f2a1.vtex_c");
    let sky = s2tex::decode_raw_mip_bytes(&sky_bytes, 0).expect("decode_raw_mip_bytes sky level 0");
    assert_eq!((sky.width, sky.height, sky.depth), (512, 512, 6));
    assert_eq!(sky.format, s2tex::VTexFormat::Bc6H);
    assert_eq!(sky.bytes.len(), 1_572_864);

    let mirage_vpk_path = game_dir().join("maps").join("de_mirage.vpk");
    let mirage_vpk = Vpk::open(&mirage_vpk_path)
        .unwrap_or_else(|e| panic!("failed to open {mirage_vpk_path:?}: {e}"));

    let irradiance_bytes = mirage_vpk
        .read_path("maps/de_mirage/lightmaps/irradiance.vtex_c")
        .expect("read irradiance.vtex_c");
    let irradiance = s2tex::decode_raw_mip_bytes(&irradiance_bytes, 1)
        .expect("decode_raw_mip_bytes irradiance level 1");
    assert_eq!(
        (irradiance.width, irradiance.height, irradiance.depth),
        (4096, 4096, 1)
    );
    assert_eq!(irradiance.format, s2tex::VTexFormat::Bc6H);
    assert_eq!(irradiance.bytes.len(), 16_777_216);

    let atlas_bytes = mirage_vpk
        .read_path("maps/de_mirage/lightmaps/env_light_probe_volume_atlas.vtex_c")
        .expect("read env_light_probe_volume_atlas.vtex_c");
    let atlas = s2tex::decode_raw_mip_bytes(&atlas_bytes, 0)
        .expect("decode_raw_mip_bytes env_light_probe_volume_atlas level 0");
    assert_eq!((atlas.width, atlas.height, atlas.depth), (164, 152, 384));
    assert_eq!(atlas.format, s2tex::VTexFormat::Bc6H);
    assert_eq!(atlas.bytes.len(), 9_572_352);
}

/// `s6f3a2b_hdr.md`'s per-slice API: decoding one z-slice via
/// `decode_raw_mip_bytes` + `decode_hdr_slice` must agree bit-for-bit with
/// the same slice out of the whole-texture [`s2tex::decode_hdr_bytes`] path
/// -- both run the exact same per-format decode over the exact same bytes,
/// so the worst difference is expected to be exactly 0 (the VRF cross-check
/// with its own 1e-3 tolerance is the separate
/// `probe_volume_atlas_matches_vrf_reference_export` test).
#[test]
#[ignore = "needs CS2_GAME_DIR; slow, run with --release"]
fn decode_hdr_slice_matches_the_whole_texture_path() {
    let mirage_vpk_path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&mirage_vpk_path)
        .unwrap_or_else(|e| panic!("failed to open {mirage_vpk_path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/lightmaps/env_light_probe_volume_atlas.vtex_c")
        .expect("read env_light_probe_volume_atlas.vtex_c");

    let whole = s2tex::decode_hdr_bytes(&bytes, s2tex::header::MAX_SIDE).expect("decode_hdr_bytes");
    let raw = s2tex::decode_raw_mip_bytes(&bytes, whole.mip_level).expect("decode_raw_mip_bytes");

    let per_layer = (whole.width as usize) * (whole.height as usize) * 4;
    for z in [0u32, 383] {
        let slice = s2tex::decode_hdr_slice(&raw, z)
            .unwrap_or_else(|e| panic!("decode_hdr_slice z={z}: {e}"));
        assert_eq!((slice.width, slice.height), (whole.width, whole.height));
        let whole_layer = &whole.rgba[per_layer * (z as usize)..per_layer * (z as usize + 1)];
        let mut worst = 0f32;
        for (a, b) in slice.rgba.iter().zip(whole_layer.iter()) {
            worst = worst.max((a - b).abs());
        }
        println!("z={z}: worst difference vs the whole-texture path = {worst}");
        assert_eq!(worst, 0.0, "z={z}: expected bit-identical output");
    }
}

/// `s6f3a2b_hdr.md`'s contact sheet ("6 граней неба Mirage после
/// тонкомпрессии") plus a reference sanity check. VRF's own HDR export for
/// a cubemap reprojects all 6 faces into a single equirectangular
/// (latlong) image with bilinear cross-face sampling
/// (`TextureExtract.cs`'s `CreateLatLongFromCubemapFaces`) rather than
/// writing the 6 faces separately, so a pixel-exact per-face comparison
/// isn't available without reimplementing that resampling; instead this
/// checks that the reprojected reference and our own 6 raw faces agree on
/// the aggregate radiance range (max and mean over the luminance channel),
/// which a lossy bilinear resample can shift slightly but not by an order
/// of magnitude -- a structural cross-check, not a bit-exact one.
#[test]
#[ignore = "needs CS2_GAME_DIR and the VRF reference export"]
fn sky_cube_reference_stats_and_contact_sheet() {
    let sky_path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&sky_path).unwrap_or_else(|e| panic!("failed to open {sky_path:?}: {e}"));
    let bytes = vpk
        .read_path("materials/skybox/sky_de_mirage_exr_71e5f2a1.vtex_c")
        .expect("read sky_de_mirage_exr_71e5f2a1.vtex_c");

    let ours = s2tex::decode_hdr_bytes(&bytes, s2tex::header::MAX_SIDE).expect("decode_hdr_bytes");
    println!(
        "ours: {}x{} layers={} format={:?} mip_level={} is_cube={}",
        ours.width, ours.height, ours.layers, ours.format, ours.mip_level, ours.is_cube
    );
    assert_eq!(ours.layers, 6, "expected all 6 cube faces");

    // Contact sheet: 6 faces, tone-mapped, side by side.
    const CELL: u32 = 128;
    let mut canvas = vec![0u8; (CELL * 6 * CELL * 4) as usize];
    let mut our_max_luma = 0f32;
    let mut our_luma_sum = 0f64;
    let per_layer = (ours.width as usize) * (ours.height as usize) * 4;
    for face in 0..6u32 {
        let layer = &ours.rgba[per_layer * (face as usize)..per_layer * (face as usize + 1)];
        let preview = s2tex::tonemap::preview_rgba8(ours.width, ours.height, layer);
        for y in 0..CELL {
            for x in 0..CELL {
                let sx = x * ours.width / CELL;
                let sy = y * ours.height / CELL;
                let src = ((sy * ours.width + sx) * 4) as usize;
                let dst = ((y * (CELL * 6) + (face * CELL + x)) * 4) as usize;
                canvas[dst..dst + 4].copy_from_slice(&preview[src..src + 4]);
            }
        }

        for px in layer.as_chunks::<4>().0 {
            let luma = px[0] * 0.299 + px[1] * 0.587 + px[2] * 0.114;
            our_max_luma = our_max_luma.max(luma);
            our_luma_sum += f64::from(luma);
        }
    }
    let our_mean_luma = our_luma_sum / f64::from((ours.width * ours.height * 6) as u32);
    println!("ours: max_luma={our_max_luma} mean_luma={our_mean_luma}");

    let contact_sheet_path = PathBuf::from(r"D:\porject\modulator-work\scratch\s2tex_sky.png");
    if let Some(parent) = contact_sheet_path.parent() {
        std::fs::create_dir_all(parent).expect("create contact sheet directory");
    }
    let file = std::fs::File::create(&contact_sheet_path).expect("create contact sheet file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), CELL * 6, CELL);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write contact sheet header");
    writer
        .write_image_data(&canvas)
        .expect("write contact sheet pixels");
    println!("contact sheet: {}", contact_sheet_path.display());

    let vrf_path = PathBuf::from(
        r"D:\porject\modulator-work\scratch\f3_lighting\skytex\materials\skybox\sky_de_mirage_exr_71e5f2a1.exr",
    );
    if !vrf_path.is_file() {
        println!("no VRF reference export at {vrf_path:?}; skipping the reference stats check");
        return;
    }
    let (_, _, reference) = load_exr_rgba(&vrf_path);
    let mut ref_max_luma = 0f32;
    let mut ref_luma_sum = 0f64;
    for px in reference.as_chunks::<4>().0 {
        let luma = px[0] * 0.299 + px[1] * 0.587 + px[2] * 0.114;
        ref_max_luma = ref_max_luma.max(luma);
        ref_luma_sum += f64::from(luma);
    }
    let ref_mean_luma = ref_luma_sum / (reference.len() / 4) as f64;
    println!("reference (latlong): max_luma={ref_max_luma} mean_luma={ref_mean_luma}");

    // A bilinear latlong resample redistributes energy across faces but
    // shouldn't change the overall exposure by more than a small factor;
    // 3x is a loose, structural bound (documented above), not a precision
    // claim.
    assert!(
        our_max_luma > ref_max_luma / 3.0 && our_max_luma < ref_max_luma * 3.0,
        "max luma {our_max_luma} vs reference {ref_max_luma} differ by more than 3x"
    );
    assert!(
        our_mean_luma > ref_mean_luma / 3.0 && our_mean_luma < ref_mean_luma * 3.0,
        "mean luma {our_mean_luma} vs reference {ref_mean_luma} differ by more than 3x"
    );
}
