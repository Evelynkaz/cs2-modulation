//! Loads a map's cache directory (mesh, nav areas, spawns, stand spots) into one bundle, shared
//! by the CLI and the server.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use geom::math::V3;
use geom::mesh::CollisionMesh;

use crate::ExtractError;
use crate::cache;
use crate::report::{EntityRecord, NavAreasDump};

/// Bump when `solver::standspots::compute`'s output for the same inputs would change, so a stale
/// cache from an older build gets recomputed rather than silently reused.
pub const STANDSPOTS_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandSpotJson {
    pub feet: [f32; 3],
    pub stance: String,
    pub nav: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandSpotFile {
    pub version: u32,
    pub map: String,
    pub step: f32,
    pub spots: Vec<StandSpotJson>,
}

/// Why stand spots aren't loaded - the CLI and server print/show their own text for each case.
#[derive(Debug, Clone)]
pub enum StandSpotsState {
    Loaded(StandSpotFile),
    Missing,                // no standspots.json
    Stale { version: u32 }, // version didn't match STANDSPOTS_VERSION
    Unreadable,             // unreadable or unparsable
}

#[derive(Debug, Clone)]
pub struct Spawns {
    pub t: Vec<V3>,
    pub ct: Vec<V3>,
}

pub struct MapBundle {
    pub map: String,
    pub dir: PathBuf,
    pub mesh: CollisionMesh,
    pub nav_areas: Vec<Vec<V3>>, // only hull_index == 0
    pub spawns: Spawns,
    pub stand_spots: StandSpotsState,
}

/// Loads every piece of `dir`'s cache used by a solve: mesh, nav areas, spawns and stand spots.
/// The map name comes from the cache manifest, not a caller-supplied string.
pub fn load_bundle(dir: &Path) -> Result<MapBundle, ExtractError> {
    let manifest = cache::load_manifest(dir)?;
    let mesh = cache::load_mesh(dir)?;
    let nav_areas = load_nav_areas(dir)?;
    let spawns = load_spawns(dir)?;
    let stand_spots = load_stand_spots(dir);
    Ok(MapBundle {
        map: manifest.meta.map,
        dir: dir.to_path_buf(),
        mesh,
        nav_areas,
        spawns,
        stand_spots,
    })
}

/// `nav.json` missing -> no error, an empty set (a nav-less map still solves off spawns/manual
/// origins); present but unparsable -> an error naming the file. Only `hull_index == 0` areas
/// (the default player hull) are returned.
pub fn load_nav_areas(dir: &Path) -> Result<Vec<Vec<V3>>, ExtractError> {
    let nav_path = dir.join("nav.json");
    if !nav_path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&nav_path).map_err(|source| ExtractError::Io {
        path: nav_path.clone(),
        source,
    })?;
    let nav: NavAreasDump = serde_json::from_str(&text)
        .map_err(|e| ExtractError::Other(format!("failed to parse {}: {e}", nav_path.display())))?;
    Ok(nav
        .areas
        .iter()
        .filter(|a| a.hull_index == 0)
        .map(|a| a.corners.iter().map(|c| V3::from_array(*c)).collect())
        .collect())
}

/// `entities.json` missing -> empty spawns (no error). `MapRegistry.cs:347-353`: Wingman (2v2)
/// spawns are tagged `[PR#]spawnpoints.2v2`, sit in walled-off areas and are disabled
/// (`enabled=0`) in Defusal - not real round-start spots, so they're filtered out here.
pub fn load_spawns(dir: &Path) -> Result<Spawns, ExtractError> {
    let entities_path = dir.join("entities.json");
    let mut t = Vec::new();
    let mut ct = Vec::new();
    if entities_path.is_file() {
        let text = fs::read_to_string(&entities_path).map_err(|source| ExtractError::Io {
            path: entities_path.clone(),
            source,
        })?;
        let entities: Vec<EntityRecord> = serde_json::from_str(&text).map_err(|e| {
            ExtractError::Other(format!("failed to parse {}: {e}", entities_path.display()))
        })?;
        for e in &entities {
            let bucket = match e.classname.as_str() {
                "info_player_terrorist" => &mut t,
                "info_player_counterterrorist" => &mut ct,
                _ => continue,
            };
            if e.targetname
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains("2v2"))
            {
                continue;
            }
            bucket.push(V3::new(e.origin[0], e.origin[1], e.origin[2]));
        }
    }
    Ok(Spawns { t, ct })
}

/// Loads `standspots.json` if present, current-version and parsable; every other case is
/// reported through [`StandSpotsState`] rather than as an error, since the caller always has a
/// working fallback (nav-mesh origins).
pub fn load_stand_spots(dir: &Path) -> StandSpotsState {
    let path = dir.join("standspots.json");
    if !path.is_file() {
        return StandSpotsState::Missing;
    }
    let cached: Option<StandSpotFile> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    match cached {
        Some(cached) if cached.version == STANDSPOTS_VERSION => StandSpotsState::Loaded(cached),
        Some(cached) => StandSpotsState::Stale {
            version: cached.version,
        },
        // Unreadable or unparsable (a truncated/corrupt file, a format from before this port
        // existed, ...): the same "not usable" fallback as a stale version, not a hard failure.
        None => StandSpotsState::Unreadable,
    }
}

/// Writes `standspots.json` via a temp file rename: the compute pass takes long enough to be
/// interrupted, and a truncated standspots file would otherwise greet the next run. The temp
/// file name carries the process id (as `cache.rs`'s `save_extraction` already does) because,
/// unlike the CLI, the server can have the background `standspots` task running twice for the
/// same map: a fixed name would let the second writer truncate the first's still-open temp file
/// out from under it.
pub fn save_stand_spots(dir: &Path, file: &StandSpotFile) -> Result<(), ExtractError> {
    let path = dir.join("standspots.json");
    let tmp_path = dir.join(format!("standspots.json.tmp-{}", std::process::id()));
    let text = serde_json::to_string(file)
        .map_err(|e| ExtractError::Other(format!("failed to serialize standspots.json: {e}")))?;
    fs::write(&tmp_path, text).map_err(|source| ExtractError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    fs::rename(&tmp_path, &path).map_err(|source| {
        let _ = fs::remove_file(&tmp_path);
        ExtractError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(())
}
