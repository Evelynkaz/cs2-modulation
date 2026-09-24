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
use std::time::SystemTime;

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

    pub fn has_render(&self) -> bool {
        self.dir.join("render.glb").is_file()
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
    /// `render.glb` exists in this map's cache directory (`s6f3b_viewer3d.md` F3b-1a).
    pub has_render: bool,
    /// `render.json`'s own `formatVersion`, when `has_render` and the file parses; `None`
    /// otherwise (including a `render.glb` written without a readable `render.json` yet).
    pub render_version: Option<u32>,
    pub build: String,
    /// True when there's no complete cache directory for this map's current `.vpk` hash (or
    /// when there's no configured game install to compare against at all).
    pub stale: bool,
}

/// A memoized VPK hash, valid as long as the file's `(modified, len)` identity - the same pair
/// `routes.rs`'s radar ETag uses - hasn't changed since it was computed.
struct VpkHashMemo {
    modified: SystemTime,
    len: u64,
    sha256: String,
}

pub struct MapRegistry {
    cache_root: PathBuf,
    entries: Mutex<HashMap<String, Arc<MapEntry>>>,
    /// Map VPK path -> its last-hashed sha256, so `discover` (called on every `/api/maps` and
    /// `/api/config` request) stats each VPK instead of sha256-ing gigabytes of it every time.
    vpk_hashes: Mutex<HashMap<PathBuf, VpkHashMemo>>,
}

impl MapRegistry {
    pub fn new(cache_root: PathBuf) -> Self {
        MapRegistry {
            cache_root,
            entries: Mutex::new(HashMap::new()),
            vpk_hashes: Mutex::new(HashMap::new()),
        }
    }

    /// `path`'s sha256, from the memo if its `(modified, len)` identity still matches what was
    /// last hashed, else freshly computed (and the memo updated) - so a changed VPK is always
    /// re-hashed, never silently reported stale-but-cached.
    fn hashed_vpk(&self, path: &Path) -> std::io::Result<String> {
        let meta = fs::metadata(path)?;
        let modified = meta.modified()?;
        let len = meta.len();
        if let Some(memo) = self.vpk_hashes.lock().unwrap().get(path)
            && memo.modified == modified
            && memo.len == len
        {
            return Ok(memo.sha256.clone());
        }
        let sha256 = cache::sha256_file(path)?;
        self.vpk_hashes.lock().unwrap().insert(
            path.to_path_buf(),
            VpkHashMemo {
                modified,
                len,
                sha256: sha256.clone(),
            },
        );
        Ok(sha256)
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// `(map name, cache dir, stale)` for every map that has at least one complete cache
    /// subdirectory. Missing/empty cache -> an empty list, never an error. When `game_dir`
    /// validates as a real CS2 install, the directory for the VPK's current hash is picked via
    /// `extract::cache::find_cached_with_hash` fed a memoized VPK hash (`hashed_vpk`) - the same
    /// selection a live extraction would make, without re-sha256-ing every map's VPK on every
    /// call; `stale` is true exactly when no complete directory exists for that hash, in which
    /// case the newest-write-time pick is shown instead (so something is still listed). Without a
    /// usable `game_dir`, the newest-write-time pick is used and every map is flagged `stale`
    /// (nothing to compare against).
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
                Some(install) => {
                    let vpk_path = install.map_vpk(name);
                    match self.hashed_vpk(&vpk_path).ok().and_then(|hash| {
                        cache::find_cached_with_hash(&self.cache_root, install, name, &hash).ok()
                    }) {
                        Some(Some(dir)) => (Some(dir), false),
                        _ => (mtime_pick, true),
                    }
                }
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

/// The subdirectory under `map_dir` (named `<sha12>-x<version>`, or `<build>-<sha12>-x<version>`
/// for a pre-existing legacy directory) with the most recent modification time among those that
/// are complete (`extract::cache::is_complete`): an incomplete directory (e.g. missing
/// `world.cgeo`) is skipped entirely rather than shadowing an older, complete one. A directory
/// still named `<...>.tmp-<pid>` (`extract::cache::save_extraction`'s in-progress name) is
/// skipped too, even if it happens to already satisfy `is_complete` - it's mid-write and about to
/// be renamed away, or torn down entirely.
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
    let has_render = dir.join("render.glb").is_file();
    Some(MapSummary {
        map: map.to_string(),
        has_lineups: dir.join("nav.json").is_file(),
        has_stand_spots: dir.join("standspots.json").is_file(),
        has_radar: dir.join("viewer-map.png").is_file(),
        has_glass,
        has_doors,
        has_render,
        render_version: has_render.then(|| render_json_version(dir)).flatten(),
        build: manifest.meta.game_build,
        stale,
    })
}

/// `render.json`'s `formatVersion` field, if the file is present and parses (`s6f3a3_map.md`
/// §6). Any read/parse failure -> `None`, same "no error, just missing" treatment as the rest of
/// this function's optional fields.
fn render_json_version(dir: &Path) -> Option<u32> {
    let text = fs::read_to_string(dir.join("render.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("formatVersion")
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as u32)
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

#[cfg(test)]
mod tests {
    use super::*;
    use extract::build::Extraction;
    use extract::report::{ExtractMeta, ExtractReport};
    use geom::mesh::CollisionMesh;

    /// A `std::env::temp_dir()` subdirectory unique to one test, removed (recursively) on drop,
    /// even on panic.
    struct TempDir(PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "cs2mod_registry_test_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn fake_install(root: &Path, vpk_bytes: &[u8]) -> GameInstall {
        fs::create_dir_all(root.join("maps")).unwrap();
        fs::write(root.join("steam.inf"), "ClientVersion=2000908\n").unwrap();
        fs::write(root.join("pak01_dir.vpk"), b"pak bytes").unwrap();
        fs::write(root.join("maps").join("de_test.vpk"), vpk_bytes).unwrap();
        GameInstall::new(root).unwrap()
    }

    /// Writes a complete cache directory for `de_test`, keyed off `install`'s current VPK bytes,
    /// and returns the cache root.
    fn sample_cache_root(root: &Path, install: &GameInstall) -> PathBuf {
        let cache_root = root.join("cache");
        let vpk_sha256 = cache::sha256_file(&install.map_vpk("de_test")).unwrap();
        let extraction = Extraction {
            mesh: CollisionMesh::new(),
            entities: Vec::new(),
            nav: None,
            report: ExtractReport::default(),
            meta: ExtractMeta {
                map: "de_test".to_string(),
                game_build: "2000908".to_string(),
                extractor_version: extract::EXTRACTOR_VERSION,
                map_vpk_sha256: vpk_sha256,
                shared_vpk_sha256: Vec::new(),
                created_utc: cache::now_utc_rfc3339(),
                timing_ms: 0,
            },
        };
        cache::save_extraction(&cache_root, &extraction, false).unwrap();
        cache_root
    }

    fn de_test_stale(registry: &MapRegistry, root: &Path) -> bool {
        registry
            .maps(Some(root))
            .into_iter()
            .find(|s| s.map == "de_test")
            .expect("de_test in /api/maps")
            .stale
    }

    /// A second `maps()` call must not re-hash a VPK whose `(modified, len)` identity hasn't
    /// changed: overwriting the file's bytes (same length) but restoring its exact original
    /// modification time leaves the memoized hash - and therefore the cache dir it resolves to -
    /// unchanged, so `stale` stays `false` even though the on-disk bytes no longer match what was
    /// actually cached.
    #[test]
    fn second_call_reuses_memoized_hash() {
        let root = temp_dir("memo_root");
        let install = fake_install(&root, b"original vpk bytes");
        let cache_root = sample_cache_root(&root, &install);
        let registry = MapRegistry::new(cache_root);

        assert!(!de_test_stale(&registry, &root), "freshly cached, matches");

        let vpk_path = install.map_vpk("de_test");
        let original_modified = fs::metadata(&vpk_path).unwrap().modified().unwrap();
        // Same length (18 bytes) as "original vpk bytes", different content.
        fs::write(&vpk_path, b"replaced-vpk-bytz!").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&vpk_path)
            .unwrap()
            .set_modified(original_modified)
            .unwrap();

        assert!(
            !de_test_stale(&registry, &root),
            "identity unchanged - the stale memoized hash (not the new bytes) must still be used"
        );
    }

    /// A VPK whose `(modified, len)` identity did change must be re-hashed, and a real content
    /// change flips `stale` back to `true` (the memoized cache dir no longer matches).
    #[test]
    fn changed_identity_is_rehashed_and_flips_stale() {
        let root = temp_dir("rehash_root");
        let install = fake_install(&root, b"original vpk bytes");
        let cache_root = sample_cache_root(&root, &install);
        let registry = MapRegistry::new(cache_root);

        assert!(!de_test_stale(&registry, &root), "freshly cached, matches");

        let vpk_path = install.map_vpk("de_test");
        // Different length, and no mtime override - a real, naturally-observed identity change.
        fs::write(&vpk_path, b"a completely different, longer vpk payload").unwrap();

        assert!(
            de_test_stale(&registry, &root),
            "changed identity must be re-hashed, and the new hash doesn't match the cached dir"
        );
    }
}
