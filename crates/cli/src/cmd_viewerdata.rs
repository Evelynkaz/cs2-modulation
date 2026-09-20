//! `cs2mod viewerdata <MAP>`: renders the 2D radar PNG and `viewer-map.json` header used by the
//! (future) web viewer, ported from `ViewerDataCommand.cs`.

use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use extract::cache;
use extract::report::EntityRecord;
use radar::{RadarOptions, ViewerMap};

use crate::cmd_extract::load_or_extract_mesh;

/// `minx,miny,maxx,maxy` -> `[x0,y0,x1,y1]`.
fn parse_region(spec: &str) -> anyhow::Result<[f32; 4]> {
    let parts: Vec<f32> = spec
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| {
            format!("invalid --region {spec:?}: expected 4 comma-separated numbers")
        })?;
    anyhow::ensure!(
        parts.len() == 4,
        "invalid --region {spec:?}: expected 4 comma-separated numbers, got {}",
        parts.len()
    );
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

/// `entities.json` missing -> empty list, same "no error" treatment `extract::load_spawns` gives
/// it: a map without entities still renders, just without callouts.
fn load_entities(dir: &Path) -> anyhow::Result<Vec<EntityRecord>> {
    let path = dir.join("entities.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Renders `viewer-map.png` and `viewer-map.json` for `map`. Without `--force`, skips the work
/// only if both output files already exist; if either is missing, it re-renders and overwrites
/// whichever of the two is still present.
#[allow(clippy::too_many_arguments)]
pub fn viewerdata(
    map: &str,
    pixel_size: f32,
    region: Option<&str>,
    force: bool,
    game: Option<&Path>,
    cache_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let (mesh, dir) = load_or_extract_mesh(map, game, cache_dir)?;
    let png_path = dir.join("viewer-map.png");
    let json_path = dir.join("viewer-map.json");

    if !force && png_path.is_file() && json_path.is_file() {
        println!(
            "{} and {} already exist; pass --force to overwrite",
            png_path.display(),
            json_path.display()
        );
        return Ok(());
    }

    let manifest = cache::load_manifest(&dir)?;
    let nav_areas = extract::load_nav_areas(&dir)?;
    let entities = load_entities(&dir)?;
    let region = region.map(parse_region).transpose()?;

    let opts = RadarOptions { pixel_size, region };

    let start = Instant::now();
    let image = radar::render(&mesh, &nav_areas, &opts).context("failed to render radar")?;
    let elapsed = start.elapsed();

    let callouts = radar::callouts(&entities, image.region);
    let viewer_map = ViewerMap {
        map: manifest.meta.map.clone(),
        build: manifest.meta.game_build.clone(),
        region: image.region,
        image: "viewer-map.png".to_string(),
        pixel_size,
        callouts,
    };

    // Atomic write: a temp file per output, then rename into place, as elsewhere in this
    // codebase (`extract::save_stand_spots`).
    let png_tmp = dir.join(format!("viewer-map.png.tmp-{}", std::process::id()));
    let json_tmp = dir.join(format!("viewer-map.json.tmp-{}", std::process::id()));
    if let Err(e) = radar::write_png(&image, &png_tmp) {
        let _ = std::fs::remove_file(&png_tmp);
        return Err(anyhow::Error::new(e).context("failed to write viewer-map.png"));
    }
    std::fs::rename(&png_tmp, &png_path).with_context(|| {
        let _ = std::fs::remove_file(&png_tmp);
        format!("failed to rename into {}", png_path.display())
    })?;
    let json_text =
        serde_json::to_string(&viewer_map).context("failed to serialize viewer-map.json")?;
    if let Err(e) = std::fs::write(&json_tmp, json_text) {
        let _ = std::fs::remove_file(&json_tmp);
        return Err(
            anyhow::Error::new(e).context(format!("failed to write {}", json_tmp.display()))
        );
    }
    std::fs::rename(&json_tmp, &json_path).with_context(|| {
        let _ = std::fs::remove_file(&json_tmp);
        format!("failed to rename into {}", json_path.display())
    })?;

    println!(
        "wrote {} ({}x{}) and {} ({} callouts) in {:.1}s",
        png_path.display(),
        image.width,
        image.height,
        json_path.display(),
        viewer_map.callouts.len(),
        elapsed.as_secs_f64()
    );
    Ok(())
}
