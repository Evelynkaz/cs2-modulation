//! `cs2mod export-glb` (§7): writes `render.glb`/`render.json` into a map's extraction cache
//! directory, auto-extracting first if there is no cache yet (mirrors `cmd_extract`'s own
//! auto-extract behaviour).

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, bail};
use extract::build::{ExtractOptions, extract_map};
use extract::cache;
use s2render::export::{ExportOptions, export_map};
use s2render::source::Sources;

use crate::cmd_extract::{install_for, resolve_cache_root};

/// `cs2mod export-glb <MAP> [--max-texture 1024] [--force] [--lightmap-quality high] [--game]
/// [--cache]`.
#[allow(clippy::too_many_arguments)]
pub fn export_glb(
    map: &str,
    max_texture: u32,
    jpeg_quality: u8,
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
        jpeg_quality,
        lightmap_quality_high,
    };
    let result =
        export_map(&sources, &options).with_context(|| format!("failed to export {map}"))?;
    let elapsed_ms = start.elapsed().as_millis();

    write_atomic(&glb_path, &result.glb)?;
    let json_text =
        serde_json::to_string_pretty(&result.report).context("failed to serialize render.json")?;
    write_atomic(&json_path, json_text.as_bytes())?;
    let mut extra_bytes = 0usize;
    for (name, bytes) in &result.extra_files {
        extra_bytes += bytes.len();
        write_atomic(&dir.join(name), bytes)?;
    }

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

/// Writes `bytes` to `path` via a temp file + rename, matching this codebase's other cache
/// writers (`extract::cache::save_extraction`, `extract::mapdata::save_stand_spots`).
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp_path = Path::new(&tmp_name);

    if let Err(source) = fs::write(tmp_path, bytes) {
        let _ = fs::remove_file(tmp_path);
        bail!("failed to write {}: {source}", tmp_path.display());
    }
    match fs::rename(tmp_path, path) {
        Ok(()) => Ok(()),
        Err(source) => {
            let _ = fs::remove_file(tmp_path);
            bail!(
                "failed to rename {} to {}: {source}",
                tmp_path.display(),
                path.display()
            );
        }
    }
}
