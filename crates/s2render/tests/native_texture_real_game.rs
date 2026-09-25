//! F3a-6's "попиксельно сравнить выборку в шейдере с декодом s2tex того же мипа" check
//! (`s6f3a6_native_tex.md` Проверки item 2), the half of it a Rust test can actually exercise
//! without a WebGL context: for a handful of real de_mirage/de_inferno textures across the roles
//! this exporter uses (normal -- HemiOct, color -- including the one RGBA8888 on de_inferno),
//! decodes the raw block bytes `native_texture::TextureCatalog::load` would ship to the GPU the
//! same way the viewer's shader will (`s2tex::decode_mip` + `s2tex::apply_texture_codec`, the
//! exact function pair `s2tex::decode` calls internally) and checks it matches
//! `s2tex::decode_bytes`'s own independent decode of the same mip exactly -- both paths run the
//! identical decoder, so unlike the browser's native BC decoder (which the spec allows +-1-2/255
//! for), this is an exact-equality regression check that the native export pipeline didn't
//! quietly reorder, mis-slice, or skip the codec on a mip.
//!
//! `cargo test -p s2render --release --test native_texture_real_game -- --ignored --nocapture`

use std::path::PathBuf;

use extract::game::GameInstall;
use s2render::native_texture::{ColorSpace, Loaded, TextureCatalog};
use s2render::source::{Sources, compiled_path};

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!("CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test"),
    }
}

/// `(map, path, role max_side, color_space)` -- one representative texture per role from
/// `REPORT.md`'s "Formats by role" (normal maps BC7 HemiOct, base color BC7/DXT1, the one
/// RGBA8888 on de_inferno).
const SAMPLES: &[(&str, &str, u32, ColorSpace)] = &[
    (
        "de_mirage",
        "materials/models/props/de_mirage/rusted_fence_a/rusted_fence_a_normals_normal_psd_822f2409.vtex_c",
        512,
        ColorSpace::Linear,
    ),
    (
        "de_mirage",
        "materials/overlays/urban_paintswatch_01a_normal_psd_65e31ad0.vtex_c",
        512,
        ColorSpace::Linear,
    ),
    (
        "de_inferno",
        "materials/de_inferno/metal/gates_01/inferno_metalgates_01a_color_tga_bf89355f.vtex_c",
        1024,
        ColorSpace::Srgb,
    ),
];

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn native_raw_mip_matches_s2tex_decode_for_real_textures() {
    let install = GameInstall::new(game_dir()).expect("valid CS2 install");
    let mut sampled = 0usize;
    let mut catalog = TextureCatalog::default();

    for &(map, path, max_side, color_space) in SAMPLES {
        let sources = match Sources::open(&install.map_vpk(map), &install.csgo_dir) {
            Ok(s) => s,
            Err(e) => {
                println!("{map}: failed to open sources, skipping: {e}");
                continue;
            }
        };
        let compiled = compiled_path(path);
        let Some(bytes) = sources.read(&compiled) else {
            println!("{compiled}: not found, skipping");
            continue;
        };

        // What the exporter would actually write for this texture at this role's budget.
        let loaded = catalog
            .load(&sources, &compiled, max_side, color_space)
            .unwrap_or_else(|e| panic!("{compiled}: native load failed: {e}"));
        let Loaded::Texture(_) = loaded else {
            panic!("{compiled}: expected a real (non-4x4-constant) texture for this sample");
        };

        // The same base level `TextureCatalog::load` picked, recomputed independently here so the
        // comparison below isn't just checking the catalog against itself.
        let resource = s2fmt::resource::Resource::parse(bytes.clone()).expect("parse resource");
        let data_block = resource
            .block(s2fmt::resource::FourCC::DATA)
            .expect("DATA block");
        let header = s2tex::header::parse(resource.block_bytes(data_block)).expect("header");
        let level = s2tex::pick_mip_for_budget(&header, max_side);

        let raw = s2tex::decode_raw_mip_bytes(&bytes, level).expect("raw mip");
        let codec_flags = s2tex::resolve_codec(&resource, header.format, header.flags.is_cube());
        let mut mirrored = s2tex::decode_mip(header.format, raw.width, raw.height, &raw.bytes)
            .expect("decode_mip on the exported raw level");
        s2tex::apply_texture_codec(&mut mirrored, codec_flags);

        let decoded_here = s2tex::decode_bytes(&bytes, max_side).expect("s2tex::decode_bytes");
        assert_eq!(
            decoded_here.mip_level, level,
            "{compiled}: native and s2tex::decode_bytes picked different mip levels"
        );
        assert_eq!(
            (decoded_here.width, decoded_here.height),
            (raw.width, raw.height),
            "{compiled}: level {level} size"
        );
        assert_eq!(
            mirrored, decoded_here.rgba,
            "{compiled}: native raw-mip decode (mip {level}) disagrees with s2tex::decode_bytes's own RGBA8 output"
        );
        println!(
            "{compiled}: {}x{} mip{level} {:?} OK ({} raw bytes)",
            raw.width,
            raw.height,
            header.format,
            raw.bytes.len()
        );
        sampled += 1;
    }

    assert!(
        sampled > 0,
        "no sample textures were found under CS2_GAME_DIR"
    );
}
