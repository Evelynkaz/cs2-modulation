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
