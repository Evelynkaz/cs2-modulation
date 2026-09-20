//! A registry of extracted maps, built from the cache directory the server was pointed at:
//! ported in spirit from `MapRegistry.cs`, but scanning disk directly rather than needing a
//! live `GameInstall` (`MapRegistry.cs:89-104` enumerates `data/*.s2geo`; we have no such
//! self-describing single file, so each map's newest complete build subdirectory under
//! `cache/maps/<map>/` stands in for it). Mesh/player colliders build lazily on first use, like
//! `MapEntry`'s `Lazy<TriangleCollider>` fields (`MapRegistry.cs:46-58`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

use extract::cache;
use extract::game::GameInstall;
use extract::mapdata::{self, MapBundle, StandSpotsState};
use extract::report::EntityRecord;
use geom::collider::{Collider, ColliderError};
use geom::filter;
use geom::grid::UniformGrid;
use geom::math::V3;

use crate::config;
use crate::mesh_payload;

/// Grid cell size shared by every collider this crate builds, matching every other caller in
/// the workspace (`crates/solver`, `crates/radar`, ...).
const COLLIDER_CELL_SIZE: f32 = 128.0;
/// How far above/below a spawn to search for its resting floor (`ServeCommand.cs:102`).
const SPAWN_DROP_SEARCH: f32 = 256.0;

/// `(T spawns, CT spawns)`, each grounded onto the floor.
type GroundedSpawns = (Vec<[f32; 3]>, Vec<[f32; 3]>);

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown map (see /api/maps)")]
    UnknownMap,
    #[error("failed to load cached map data: {0}")]
    Load(#[from] extract::ExtractError),
    #[error("failed to build collider: {0}")]
    Collider(#[from] ColliderError),
}

/// One loaded map's mesh/nav/spawn/callout data plus its lazily-built colliders and derived
/// payloads.
pub struct MapEntry {
    pub map: String,
    pub dir: PathBuf,
    pub bundle: MapBundle,
    /// First 16 hex chars of the mesh file's own sha256, so re-extracting a map (which rewrites
    /// `world.cgeo`) invalidates every ETag that named the old mesh.
    pub etag: String,
    /// `env_cs_place` entities, unmerged (`(name, origin)`); `/api/callouts` groups these itself,
    /// matching `ServeCommand.cs:504-529`.
    pub places: Vec<(String, [f32; 3])>,
    grenade_collider: OnceLock<Arc<UniformGrid>>,
    player_collider: OnceLock<Arc<UniformGrid>>,
    mesh_payload: OnceLock<Arc<Vec<u8>>>,
    grounded_spawns: OnceLock<Arc<GroundedSpawns>>,
}

impl MapEntry {
    /// The grenade-solid collider (`entry.Collider` in `MapRegistry.cs:46-50`), built on first
    /// use and cached for the process lifetime.
    pub fn grenade_collider(&self) -> Result<Arc<UniformGrid>, ColliderError> {
        if let Some(c) = self.grenade_collider.get() {
            return Ok(c.clone());
        }
        let mask = filter::grenade_mask(&self.bundle.mesh);
        let grid = Arc::new(UniformGrid::build(
            &self.bundle.mesh,
            &mask,
            None,
            COLLIDER_CELL_SIZE,
        )?);
        let _ = self.grenade_collider.set(grid.clone());
        Ok(grid)
    }

    /// The player-solid twin (`entry.PlayerCollider` in `MapRegistry.cs:53-58`).
    pub fn player_collider(&self) -> Result<Arc<UniformGrid>, ColliderError> {
        if let Some(c) = self.player_collider.get() {
            return Ok(c.clone());
        }
        let mask = filter::player_mask(&self.bundle.mesh);
        let grid = Arc::new(UniformGrid::build(
            &self.bundle.mesh,
            &mask,
            None,
            COLLIDER_CELL_SIZE,
        )?);
        let _ = self.player_collider.set(grid.clone());
        Ok(grid)
    }

    /// The `SM3D` mesh payload (`mesh_payload::sm3d`), built once and cached.
    pub fn mesh_payload(&self) -> Arc<Vec<u8>> {
        if let Some(p) = self.mesh_payload.get() {
            return p.clone();
        }
        let payload = Arc::new(mesh_payload::sm3d(&self.bundle.mesh));
        match self.mesh_payload.set(payload.clone()) {
            Ok(()) => payload,
            Err(_) => self.mesh_payload.get().expect("just set").clone(),
        }
    }

    /// Spawn positions dropped onto the floor with a straight-down ray, memoized
    /// (`ServeCommand.cs:104-120`).
    pub fn grounded_spawns(&self) -> Result<Arc<GroundedSpawns>, ColliderError> {
        if let Some(v) = self.grounded_spawns.get() {
            return Ok(v.clone());
        }
        let collider = self.player_collider()?;
        let drop = |pts: &[V3]| -> Vec<[f32; 3]> {
            pts.iter()
                .map(|p| {
                    let from = V3::new(p.x, p.y, p.z + 8.0);
                    let to = V3::new(p.x, p.y, p.z - SPAWN_DROP_SEARCH);
                    match collider.first_hit_ray(from, to) {
                        Some(hit) => [p.x, p.y, from.z + (to.z - from.z) * hit.t],
                        None => p.to_array(),
                    }
                })
                .collect()
        };
        let result = Arc::new((drop(&self.bundle.spawns.t), drop(&self.bundle.spawns.ct)));
        let _ = self.grounded_spawns.set(result.clone());
        Ok(result)
    }

    pub fn has_glass(&self) -> bool {
        self.bundle
            .mesh
            .attributes
            .iter()
            .any(|a| a.name == "EntityBreakable")
    }

    pub fn has_doors(&self) -> bool {
        self.bundle
            .mesh
            .attributes
            .iter()
            .any(|a| a.name == "EntityDoor")
    }

    pub fn has_lineups(&self) -> bool {
        !self.bundle.nav_areas.is_empty()
    }

    pub fn has_stand_spots(&self) -> bool {
        matches!(self.bundle.stand_spots, StandSpotsState::Loaded(_))
    }

    pub fn has_radar(&self) -> bool {
        self.dir.join("viewer-map.png").is_file()
    }
}

/// `GET /api/maps`' shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapSummary {
    pub map: String,
    pub has_lineups: bool,
    pub has_stand_spots: bool,
    pub has_radar: bool,
    pub has_glass: bool,
    pub has_doors: bool,
    pub build: String,
    /// True when the build directory picked for this map doesn't match what
    /// `extract::cache::find_cached` would pick for the currently configured game install (or
    /// when there's no configured game install to compare against at all).
    pub stale: bool,
}

pub struct MapRegistry {
    cache_root: PathBuf,
    entries: Mutex<HashMap<String, Arc<MapEntry>>>,
}

impl MapRegistry {
    pub fn new(cache_root: PathBuf) -> Self {
        MapRegistry {
            cache_root,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// `(map name, build dir, stale)` for every map that has at least one complete build
    /// subdirectory. Missing/empty cache -> an empty list, never an error. When `game_dir`
    /// validates as a real CS2 install, the current build is picked via
    /// `extract::cache::find_cached` (the same selection a live extraction would make); `stale`
    /// then flags a mismatch against the newest-write-time pick. Without a usable `game_dir`,
    /// the newest-write-time pick is used and every map is flagged `stale` (nothing to compare
    /// against).
    fn discover(&self, game_dir: Option<&Path>) -> Vec<(String, PathBuf, bool)> {
        let maps_dir = self.cache_root.join("maps");
        let Ok(read) = fs::read_dir(&maps_dir) else {
            return Vec::new();
        };
        let install = game_dir
            .and_then(|d| config::validate_game_dir(d).ok())
            .and_then(|info| GameInstall::new(&info.csgo_dir).ok());
        let mut out = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let mtime_pick = newest_complete_dir(&path);
            let (chosen, stale) = match &install {
                Some(install) => match cache::find_cached(&self.cache_root, install, name) {
                    Ok(Some(dir)) => {
                        let stale = mtime_pick.as_deref() != Some(dir.as_path());
                        (Some(dir), stale)
                    }
                    _ => (mtime_pick, true),
                },
                None => (mtime_pick, true),
            };
            if let Some(dir) = chosen {
                out.push((name.to_string(), dir, stale));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Every discovered map, sorted by name (Ordinal), built from cheap per-file reads (no mesh
    /// or nav loaded); a map whose manifest fails to read (a corrupt cache dir) is silently
    /// dropped rather than failing the whole listing, so one broken map cannot take the others
    /// down with it.
    pub fn maps(&self, game_dir: Option<&Path>) -> Vec<MapSummary> {
        self.discover(game_dir)
            .into_iter()
            .filter_map(|(name, dir, stale)| map_summary(&name, &dir, stale))
            .collect()
    }

    pub fn get(&self, map: &str, game_dir: Option<&Path>) -> Result<Arc<MapEntry>, RegistryError> {
        if let Some(entry) = self.entries.lock().unwrap().get(map) {
            return Ok(entry.clone());
        }
        let dir = self
            .discover(game_dir)
            .into_iter()
            .find(|(name, _, _)| name == map)
            .map(|(_, d, _)| d)
            .ok_or(RegistryError::UnknownMap)?;
        self.load(map, &dir)
    }

    /// Drops `map`'s cached entry, if loaded, so the next `get` rebuilds it from disk
    /// (`jobs.rs`, after a background job writes new data for it - a fresh extraction, stand
    /// spots, or a radar render - so it's visible without restarting the server).
    pub fn invalidate(&self, map: &str) {
        self.entries.lock().unwrap().remove(map);
    }

    /// Replaces `map`'s cached stand-spot state alone by re-reading `standspots.json`, leaving
    /// its mesh, ETag and already-built colliders untouched (`jobs.rs`, after a `standspots` job;
    /// unlike `invalidate`, this doesn't force the next request to re-hash `world.cgeo`). A no-op
    /// if `map` isn't loaded yet.
    pub fn reload_stand_spots(&self, map: &str) {
        let mut entries = self.entries.lock().unwrap();
        let Some(old) = entries.get(map).cloned() else {
            return;
        };
        let stand_spots = mapdata::load_stand_spots(&old.dir);
        let bundle = MapBundle {
            map: old.bundle.map.clone(),
            dir: old.bundle.dir.clone(),
            mesh: old.bundle.mesh.clone(),
            nav_areas: old.bundle.nav_areas.clone(),
            spawns: old.bundle.spawns.clone(),
            stand_spots,
        };
        let new_entry = Arc::new(MapEntry {
            map: old.map.clone(),
            dir: old.dir.clone(),
            bundle,
            etag: old.etag.clone(),
            places: old.places.clone(),
            grenade_collider: clone_once(&old.grenade_collider),
            player_collider: clone_once(&old.player_collider),
            mesh_payload: clone_once(&old.mesh_payload),
            grounded_spawns: clone_once(&old.grounded_spawns),
        });
        entries.insert(map.to_string(), new_entry);
    }

    fn load(&self, map: &str, dir: &Path) -> Result<Arc<MapEntry>, RegistryError> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.get(map) {
            return Ok(e.clone());
        }
        let bundle = mapdata::load_bundle(dir)?;
        let etag = mesh_etag(dir)?;
        let places = load_places(dir);
        let entry = Arc::new(MapEntry {
            map: map.to_string(),
            dir: dir.to_path_buf(),
            bundle,
            etag,
            places,
            grenade_collider: OnceLock::new(),
            player_collider: OnceLock::new(),
            mesh_payload: OnceLock::new(),
            grounded_spawns: OnceLock::new(),
        });
        entries.insert(map.to_string(), entry.clone());
        Ok(entry)
    }
}

/// The build subdirectory under `map_dir` (named `<build>-<sha12>-x<version>`) with the most
/// recent modification time among those that are complete (`extract::cache::is_complete`): an
/// incomplete directory (e.g. missing `world.cgeo`) is skipped entirely rather than shadowing an
/// older, complete one. A directory still named `<build>.tmp-<pid>` (`extract::cache::
/// save_extraction`'s in-progress name) is skipped too, even if it happens to already satisfy
/// `is_complete` - it's mid-write and about to be renamed away, or torn down entirely.
fn newest_complete_dir(map_dir: &Path) -> Option<PathBuf> {
    let read = fs::read_dir(map_dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in read.flatten() {
        let path = entry.path();
        let is_tmp = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".tmp-"));
        if is_tmp || !cache::is_complete(&path) {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Builds a [`MapSummary`] from cheap per-file reads under `dir`, without loading the mesh or
/// nav data: `report.json`'s `triangles_per_attribute` names give `hasGlass`/`hasDoors`, file
/// presence gives `hasLineups`/`hasStandSpots`/`hasRadar`, and `manifest.json` gives the build.
/// `None` if `manifest.json` can't be read.
fn map_summary(map: &str, dir: &Path, stale: bool) -> Option<MapSummary> {
    let manifest = cache::load_manifest(dir).ok()?;
    let (has_glass, has_doors) = match cache::load_report(dir) {
        Ok(report) => (
            report
                .triangles_per_attribute
                .iter()
                .any(|a| a.name == "EntityBreakable"),
            report
                .triangles_per_attribute
                .iter()
                .any(|a| a.name == "EntityDoor"),
        ),
        Err(_) => (false, false),
    };
    Some(MapSummary {
        map: map.to_string(),
        has_lineups: dir.join("nav.json").is_file(),
        has_stand_spots: dir.join("standspots.json").is_file(),
        has_radar: dir.join("viewer-map.png").is_file(),
        has_glass,
        has_doors,
        build: manifest.meta.game_build,
        stale,
    })
}

/// A fresh `OnceLock` holding a clone of `cell`'s value, if it was already set - used by
/// `reload_stand_spots` to carry an already-built collider/payload over to the replacement
/// `MapEntry` instead of rebuilding it.
fn clone_once<T: Clone>(cell: &OnceLock<T>) -> OnceLock<T> {
    let new = OnceLock::new();
    if let Some(v) = cell.get() {
        let _ = new.set(v.clone());
    }
    new
}

/// First 16 hex chars of the mesh file's own sha256.
fn mesh_etag(dir: &Path) -> Result<String, RegistryError> {
    let path = dir.join("world.cgeo");
    let full = cache::sha256_file(&path)
        .map_err(|source| RegistryError::Load(extract::ExtractError::Io { path, source }))?;
    Ok(full[..16].to_string())
}

/// `env_cs_place` entities from `entities.json`, unmerged. Missing file -> no error, an empty
/// list (a map without entities still has no callouts, same "no error" treatment
/// `extract::load_spawns` gives it).
fn load_places(dir: &Path) -> Vec<(String, [f32; 3])> {
    let path = dir.join("entities.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(entities) = serde_json::from_str::<Vec<EntityRecord>>(&text) else {
        return Vec::new();
    };
    entities
        .iter()
        .filter(|e| e.classname == "env_cs_place")
        .filter_map(|e| {
            let name = e.properties.get("place_name")?.as_str()?;
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), e.origin))
        })
        .collect()
}
