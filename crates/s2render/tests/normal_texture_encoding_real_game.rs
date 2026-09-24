//! §5's JPEG-vs-PNG measurement for normal maps: decodes a sample of de_mirage's actual normal
//! textures and compares both encodings' byte sizes at the production budget/quality. Only a
//! size comparison -- there is no JPEG decoder in this dependency graph to also measure
//! reconstructed-normal angle error without adding one, so that half of §5's "по размеру и по
//! ошибке угла нормали" is argued qualitatively in the F3a-3 receipt instead (JPEG's block-DCT +
//! chroma subsampling systematically distort the unit-length (X,Y,Z) triple a normal map stores,
//! which is exactly the failure mode `#[cfg(test)]`-only export.rs code sidesteps by encoding
//! normals as JPEG too (`texture.rs`'s own doc comment) only after flattening alpha to 255 -- see
//! the receipt for the full reasoning).
//!
//! `cargo test -p s2render --release --test normal_texture_encoding_real_game -- --ignored --nocapture`

use std::path::PathBuf;

use s2fmt::vpk::Vpk;
use s2tex::encode::encode as encode_image;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!("CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test"),
    }
}

/// A handful of real, varied de_mirage normal maps (picked from `materials_de_mirage.csv`'s
/// `normal_param` column): brick, plaster, foliage, metal.
const SAMPLE_NORMALS: &[&str] = &[
    "materials/models/props/de_mirage/rusted_fence_a/rusted_fence_a_normals_normal_psd_822f2409.vtex_c",
    "materials/models/props_foliage/mall_trees_branches03_normal_psd_89549f78.vtex_c",
    "materials/overlays/urban_paintswatch_01a_normal_psd_65e31ad0.vtex_c",
    "materials/de_train/train_cement_stain_01_vmat_g_tlayer1normalroughness_7ab89ad6.vtex_c",
];

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn jpeg_vs_png_size_for_real_normal_maps() {
    let csgo_dir = game_dir();
    let map_vpk = Vpk::open(csgo_dir.join("maps").join("de_mirage.vpk")).expect("open map vpk");
    let pak01 = Vpk::open(csgo_dir.join("pak01_dir.vpk")).expect("open pak01");

    let mut total_jpeg = 0usize;
    let mut total_png = 0usize;
    let mut sampled = 0usize;

    for path in SAMPLE_NORMALS {
        let entry = map_vpk.find(path).or_else(|| pak01.find(path));
        let Some(entry) = entry else {
            println!("{path}: not found, skipping");
            continue;
        };
        let vpk = if map_vpk.find(path).is_some() {
            &map_vpk
        } else {
            &pak01
        };
        let bytes = vpk.read(entry).expect("read texture");
        let mut decoded = s2tex::decode_bytes(&bytes, 1024).expect("decode normal texture");
        for px in decoded.rgba.as_chunks_mut::<4>().0 {
            px[3] = 255; // drop roughness, same as texture.rs's `load_and_encode`.
        }

        let jpeg = encode_image(&decoded.rgba, decoded.width, decoded.height, 90, false)
            .expect("jpeg encode");
        let png = encode_image(&decoded.rgba, decoded.width, decoded.height, 90, true)
            .expect("png encode");

        println!(
            "{path}: {}x{} jpeg_q90={} bytes png={} bytes (jpeg is {:.1}% of png)",
            decoded.width,
            decoded.height,
            jpeg.bytes.len(),
            png.bytes.len(),
            100.0 * jpeg.bytes.len() as f64 / png.bytes.len() as f64
        );
        total_jpeg += jpeg.bytes.len();
        total_png += png.bytes.len();
        sampled += 1;
    }

    assert!(sampled > 0, "no sample normal textures were found");
    println!(
        "totals over {sampled} textures: jpeg_q90={total_jpeg} bytes, png={total_png} bytes, jpeg/png={:.2}",
        total_jpeg as f64 / total_png as f64
    );
}
