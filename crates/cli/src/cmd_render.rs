//! `cs2mod export-glb` (§7): writes `render.glb`/`render.json` into a map's extraction cache
//! directory, auto-extracting first if there is no cache yet (mirrors `cmd_extract`'s own
//! auto-extract behaviour).

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::Context;
use extract::build::{ExtractOptions, extract_map};
use extract::cache;
use s2render::export::{ExportOptions, export_and_write};
use s2render::source::Sources;

use crate::cmd_extract::{install_for, resolve_cache_root};

/// `cs2mod export-glb <MAP> [--max-texture 1024] [--force] [--lightmap-quality high] [--game]
/// [--cache]`.
#[allow(clippy::too_many_arguments)]
pub fn export_glb(
    map: &str,
    max_texture: u32,
    lightmap_quality_high: bool,
    force: bool,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<()> {
    let install = install_for(game)?;
    let cache_root = resolve_cache_root(cache)?;

    let dir = match cache::find_cached(&cache_root, &install, map)? {
        Some(dir) => dir,
        None => {
            println!("no cache for {map} matching its current .vpk; extracting first");
            let extraction = extract_map(&install, map, &ExtractOptions::default())
                .with_context(|| format!("failed to extract {map}"))?;
            cache::save_extraction(&cache_root, &extraction, false)
                .with_context(|| format!("failed to write cache for {map}"))?
        }
    };

    let glb_path = dir.join("render.glb");
    let json_path = dir.join("render.json");
    if !force && glb_path.is_file() && json_path.is_file() {
        println!(
            "{} already exists (use --force to overwrite)",
            glb_path.display()
        );
        return Ok(());
    }

    let map_vpk_path = install.map_vpk(map);
    let sources = Sources::open(&map_vpk_path, &install.csgo_dir)
        .with_context(|| format!("failed to open sources for {map}"))?;

    let start = Instant::now();
    let options = ExportOptions {
        max_texture,
        lightmap_quality_high,
    };
    let result = export_and_write(
        &sources,
        &options,
        &dir,
        &AtomicBool::new(false),
        |_stage| {},
    )
    .with_context(|| format!("failed to export {map}"))?;
    let elapsed_ms = start.elapsed().as_millis();

    let json_text =
        serde_json::to_string_pretty(&result.report).context("failed to serialize render.json")?;
    let extra_bytes: usize = result
        .extra_files
        .iter()
        .map(|(_, bytes)| bytes.len())
        .sum();

    let counts = &result.report["counts"];
    println!("wrote {} ({} bytes)", glb_path.display(), result.glb.len());
    println!("wrote {} ({} bytes)", json_path.display(), json_text.len());
    for (name, bytes) in &result.extra_files {
        println!("wrote {} ({} bytes)", dir.join(name).display(), bytes.len());
    }
    println!(
        "nodes={} meshes={} materials={} textures={} triangles={} geometryBytes={} textureBytes={} extraFileBytes={}",
        counts["nodes"],
        counts["meshes"],
        counts["materials"],
        counts["textures"],
        counts["triangles"],
        counts["geometryBytes"],
        counts["textureBytes"],
        extra_bytes
    );
    println!(
        "lightmapDrawCalls={} lightmapTriangles={} probeDrawCalls={} probeTriangles={} unlitDrawCalls={} unlitTriangles={}",
        counts["lightmapDrawCalls"],
        counts["lightmapTriangles"],
        counts["probeDrawCalls"],
        counts["probeTriangles"],
        counts["unlitDrawCalls"],
        counts["unlitTriangles"],
    );
    println!("time: {elapsed_ms} ms");
    Ok(())
}
