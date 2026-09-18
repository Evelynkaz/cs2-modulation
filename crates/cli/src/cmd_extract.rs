//! `cs2mod extract|info|export-obj` commands.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use extract::build::{ExtractOptions, Extraction, extract_map};
use extract::cache;
use extract::game::GameInstall;
use extract::report::ExtractReport;
use geom::{filter, obj};

use crate::game_path::{ensure_write_allowed, find_git_root};

/// Resolves the game directory: `--game`, else `CS2_GAME_DIR`.
fn resolve_game_dir(game: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(g) = game {
        return Ok(g.to_path_buf());
    }
    std::env::var_os("CS2_GAME_DIR")
        .map(PathBuf::from)
        .context("no --game given and CS2_GAME_DIR is not set")
}

/// The nearest ancestor of the current directory containing `.git`, or the current directory
/// itself if none is found, joined with `cache`.
fn default_cache_dir() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let root = find_git_root(&cwd).unwrap_or_else(|| cwd.clone());
    Ok(root.join("cache"))
}

pub(crate) fn resolve_cache_root(cache: Option<&Path>) -> anyhow::Result<PathBuf> {
    match cache {
        Some(c) => Ok(c.to_path_buf()),
        None => default_cache_dir(),
    }
}

pub(crate) fn install_for(game: Option<&Path>) -> anyhow::Result<GameInstall> {
    let dir = resolve_game_dir(game)?;
    GameInstall::new(&dir)
        .with_context(|| format!("failed to use game directory {}", dir.display()))
}

/// Sums every attribute's triangle count from the report (avoids needing to load the mesh just
/// to print a total).
fn total_triangles(report: &ExtractReport) -> usize {
    report
        .triangles_per_attribute
        .iter()
        .map(|a| a.triangles)
        .sum()
}

fn print_summary(
    out: &mut impl Write,
    dir: &Path,
    report: &ExtractReport,
    nav_areas: Option<usize>,
    timing_ms: u64,
) -> io::Result<()> {
    writeln!(out, "cache dir: {}", dir.display())?;
    writeln!(out, "triangles: {}", total_triangles(report))?;
    writeln!(out, "attributes:")?;
    for a in &report.triangles_per_attribute {
        writeln!(
            out,
            "  [{}] {}{}: {} (as={:?} exclude={:?})",
            a.index,
            a.name,
            if a.synthetic { " (synthetic)" } else { "" },
            a.triangles,
            a.interact_as,
            a.interact_exclude
        )?;
    }
    writeln!(out, "merged entities: {}", report.merged_entities.len())?;
    writeln!(out, "skipped entities by reason:")?;
    for (reason, count) in &report.skipped_by_reason {
        writeln!(out, "  {reason}: {count}")?;
    }
    writeln!(
        out,
        "static props: {} total, {} with phys, {} without phys (unique models), {} models not found, {} load errors (unique models)",
        report.static_props_total,
        report.static_props_with_phys,
        report.static_props_without_phys,
        report.static_props_models_not_found,
        report.static_props_load_errors
    )?;
    if let Some(n) = nav_areas {
        writeln!(out, "nav areas: {n}")?;
    } else {
        writeln!(out, "nav areas: (none)")?;
    }
    writeln!(out, "time: {timing_ms} ms")?;
    Ok(())
}

fn print_highlights(out: &mut impl Write, report: &ExtractReport) -> io::Result<()> {
    writeln!(
        out,
        "world hull flags histogram: {:?}",
        report.world_hull_flags_histogram
    )?;
    writeln!(out, "spheres by source: {:?}", report.spheres_by_source)?;
    writeln!(out, "capsules by source: {:?}", report.capsules_by_source)?;
    writeln!(
        out,
        "bind poses (non-identity): {}",
        report.bind_pose_non_identity_count
    )?;
    writeln!(
        out,
        "degenerate triangles skipped: {}",
        report.degenerate_triangles_skipped
    )?;
    writeln!(
        out,
        "surface indices out of range: {}",
        report.surface_indices_out_of_range
    )?;
    writeln!(out, "aggregates skipped: {}", report.aggregates_skipped)?;
    writeln!(
        out,
        "point_template entities: {}",
        report.point_template_count
    )?;
    writeln!(
        out,
        "entity lumps with child lumps: {}",
        report.lumps_with_child_lumps
    )?;
    writeln!(
        out,
        "passable glass entities: {}",
        report.passable_glass.len()
    )?;
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {w}")?;
        }
    }
    Ok(())
}

/// `cs2mod extract <MAP>...`. Prints one summary block per map; a failing map prints its error
/// and extraction continues with the next map.
pub fn extract(
    maps: &[String],
    game: Option<&Path>,
    cache: Option<&Path>,
    force: bool,
) -> anyhow::Result<()> {
    let install = install_for(game)?;
    let cache_root = resolve_cache_root(cache)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut any_failed = false;

    for map in maps {
        writeln!(out, "=== {map} ===")?;
        let result = (|| -> anyhow::Result<()> {
            if !force && let Some(dir) = cache::find_cached(&cache_root, &install, map)? {
                writeln!(out, "reusing cache")?;
                let report = cache::load_report(&dir)?;
                let nav_areas = if dir.join("nav.json").is_file() {
                    let text = std::fs::read_to_string(dir.join("nav.json"))?;
                    let nav: extract::report::NavAreasDump = serde_json::from_str(&text)?;
                    Some(nav.areas.len())
                } else {
                    None
                };
                let manifest = cache::load_manifest(&dir)?;
                print_summary(&mut out, &dir, &report, nav_areas, manifest.meta.timing_ms)?;
                return Ok(());
            }

            let extraction: Extraction = extract_map(&install, map, &ExtractOptions::default())
                .with_context(|| format!("failed to extract {map}"))?;
            let dir = cache::save_extraction(&cache_root, &extraction, force)
                .with_context(|| format!("failed to write cache for {map}"))?;
            let nav_areas = extraction.nav.as_ref().map(|n| n.areas.len());
            print_summary(
                &mut out,
                &dir,
                &extraction.report,
                nav_areas,
                extraction.meta.timing_ms,
            )?;
            Ok(())
        })();

        if let Err(e) = result {
            writeln!(out, "ERROR: {e:?}")?;
            any_failed = true;
        }
    }

    if any_failed {
        anyhow::bail!("one or more maps failed to extract");
    }
    Ok(())
}

/// `cs2mod info <MAP>`. Prints the manifest and report highlights for the current build's cache,
/// or how to run `extract` if there is none.
pub fn info(map: &str, game: Option<&Path>, cache: Option<&Path>) -> anyhow::Result<()> {
    let install = install_for(game)?;
    let cache_root = resolve_cache_root(cache)?;

    let Some(dir) = cache::find_cached(&cache_root, &install, map)? else {
        println!(
            "no cache for {map} on this build; run `cs2mod extract {map}` first (--game/--cache as needed)"
        );
        return Ok(());
    };

    let manifest = cache::load_manifest(&dir)?;
    let report = cache::load_report(&dir)?;
    let nav_areas = if dir.join("nav.json").is_file() {
        let text = std::fs::read_to_string(dir.join("nav.json"))?;
        let nav: extract::report::NavAreasDump = serde_json::from_str(&text)?;
        Some(nav.areas.len())
    } else {
        None
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "map: {}", manifest.meta.map)?;
    writeln!(out, "game build: {}", manifest.meta.game_build)?;
    writeln!(
        out,
        "extractor version: {}",
        manifest.meta.extractor_version
    )?;
    writeln!(out, "map vpk sha256: {}", manifest.meta.map_vpk_sha256)?;
    writeln!(out, "created (UTC): {}", manifest.meta.created_utc)?;
    print_summary(&mut out, &dir, &report, nav_areas, manifest.meta.timing_ms)?;
    print_highlights(&mut out, &report)?;
    Ok(())
}

/// Parses `"minx,miny,minz,maxx,maxy,maxz"` into a region box.
fn parse_region(spec: &str) -> anyhow::Result<([f32; 3], [f32; 3])> {
    let parts: Vec<f32> = spec
        .split(',')
        .map(|s| s.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| {
            format!("invalid --region {spec:?}: expected 6 comma-separated numbers")
        })?;
    anyhow::ensure!(
        parts.len() == 6,
        "invalid --region {spec:?}: expected 6 comma-separated numbers, got {}",
        parts.len()
    );
    Ok((
        [parts[0], parts[1], parts[2]],
        [parts[3], parts[4], parts[5]],
    ))
}

/// Loads `map`'s cached mesh for the current build, auto-extracting first if there is none yet.
pub(crate) fn load_or_extract_mesh(
    map: &str,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<(geom::mesh::CollisionMesh, PathBuf)> {
    let install = install_for(game)?;
    let cache_root = resolve_cache_root(cache)?;

    let dir = match cache::find_cached(&cache_root, &install, map)? {
        Some(dir) => dir,
        None => {
            let extraction = extract_map(&install, map, &ExtractOptions::default())
                .with_context(|| format!("failed to extract {map}"))?;
            cache::save_extraction(&cache_root, &extraction, false)
                .with_context(|| format!("failed to write cache for {map}"))?
        }
    };
    let mesh = cache::load_mesh(&dir)
        .with_context(|| format!("failed to load cached mesh from {}", dir.display()))?;
    Ok((mesh, dir))
}

/// `cs2mod export-obj <MAP> --out <FILE.obj>`. Auto-extracts into the cache if there is none yet.
#[allow(clippy::too_many_arguments)]
pub fn export_obj(
    map: &str,
    out_path: &Path,
    filter_spec: &str,
    region: Option<&str>,
    y_up: bool,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<()> {
    ensure_write_allowed(out_path)?;
    let install = install_for(game)?;
    let cache_root = resolve_cache_root(cache)?;

    let dir = match cache::find_cached(&cache_root, &install, map)? {
        Some(dir) => dir,
        None => {
            println!("no cache for {map} on this build; extracting first");
            let extraction = extract_map(&install, map, &ExtractOptions::default())
                .with_context(|| format!("failed to extract {map}"))?;
            cache::save_extraction(&cache_root, &extraction, false)
                .with_context(|| format!("failed to write cache for {map}"))?
        }
    };

    let mesh = cache::load_mesh(&dir)
        .with_context(|| format!("failed to load cached mesh from {}", dir.display()))?;
    let mask = filter::parse_filter(filter_spec, &mesh)
        .with_context(|| format!("invalid --filter {filter_spec:?}"))?;
    let region = region.map(parse_region).transpose()?;

    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mtl_path = out_path.with_extension("mtl");
    let mtl_name = mtl_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("out.mtl")
        .to_string();

    let mut obj_file = std::fs::File::create(out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;
    let mut mtl_file = std::fs::File::create(&mtl_path)
        .with_context(|| format!("failed to create {}", mtl_path.display()))?;

    let stats = obj::write_obj(
        &mesh,
        Some(&mask),
        region,
        &mut obj_file,
        Some((&mut mtl_file, &mtl_name)),
        obj::ObjOptions { swap_to_y_up: y_up },
    )
    .context("failed to write OBJ")?;

    let obj_len = obj_file.metadata().map(|m| m.len()).unwrap_or(0);
    let mtl_len = mtl_file.metadata().map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {} ({} bytes, {} triangles, {} vertices, {} groups)",
        out_path.display(),
        obj_len,
        stats.triangles_written,
        stats.vertices_written,
        stats.groups_written
    );
    println!("wrote {} ({} bytes)", mtl_path.display(), mtl_len);
    Ok(())
}
