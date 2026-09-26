//! Per-map extraction cache: `<cache_root>/maps/<map>/<sha12>-x<version>/`, written atomically
//! (`<dir>.tmp-<pid>` then renamed into place). The directory is keyed by the map VPK's content
//! hash and the extractor version alone - not the game build - so a CS2 patch that leaves a
//! map's `.vpk` untouched leaves its cache untouched too; the build a directory was extracted
//! from is still recorded in `manifest.json`. Directories from before this change, named
//! `<build>-<sha12>-x<version>`, are still recognized as a fallback (see `find_legacy_dir`), read
//! in place and never renamed or copied.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use geom::cgeo;
use geom::mesh::CollisionMesh;

use crate::ExtractError;
use crate::build::{EXTRACTOR_VERSION, Extraction};
use crate::game::GameInstall;
use crate::report::ExtractMeta;

/// `manifest.json`'s shape: [`ExtractMeta`] plus every other file's name and byte size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub meta: ExtractMeta,
    pub files: Vec<(String, u64)>,
}

/// Streamed SHA-256 of a file's contents, as a lowercase hex string.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The current UTC time as a minimal RFC3339 timestamp (`2026-09-17T12:34:56Z`), without pulling
/// in a date/time crate.
pub fn now_utc_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a proleptic-Gregorian
/// `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn cache_key_dir(cache_root: &Path, map: &str, sha12: &str) -> PathBuf {
    cache_root
        .join("maps")
        .join(map)
        .join(format!("{sha12}-x{EXTRACTOR_VERSION}"))
}

/// The cache directory a freshly-built [`Extraction`]'s own metadata maps to, without hashing
/// the map VPK again (it was already hashed once, while building [`ExtractMeta`]).
fn cache_key_dir_from_meta(cache_root: &Path, meta: &ExtractMeta) -> PathBuf {
    let sha12 = &meta.map_vpk_sha256[..meta.map_vpk_sha256.len().min(12)];
    cache_key_dir(cache_root, &meta.map, sha12)
}

/// A pre-existing `<build>-<sha12>-x<version>` directory (the naming scheme used before the
/// cache key stopped including the build) under `cache_root/maps/<map>/`, whose hash and
/// extractor version match `sha12` and are complete - the first such directory found, in
/// arbitrary order, since at most one is expected to exist per `(sha12, version)` pair. Only
/// consulted when [`cache_key_dir`]'s own (new-form) directory isn't a complete cache; found
/// directories are read as-is, never renamed or copied into the new form.
fn find_legacy_dir(cache_root: &Path, map: &str, sha12: &str) -> Option<PathBuf> {
    let map_dir = cache_root.join("maps").join(map);
    let suffix = format!("-{sha12}-x{EXTRACTOR_VERSION}");
    let read = fs::read_dir(&map_dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_match = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&suffix));
        if is_match && is_complete(&path) {
            return Some(path);
        }
    }
    None
}

/// True if `dir` has a readable, current-version manifest and every file it lists is present
/// with the recorded size.
pub fn is_complete(dir: &Path) -> bool {
    let manifest_path = dir.join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
        return false;
    };
    if manifest.meta.extractor_version != EXTRACTOR_VERSION {
        return false;
    }
    manifest.files.iter().all(|(name, size)| {
        fs::metadata(dir.join(name))
            .map(|m| m.len() == *size)
            .unwrap_or(false)
    })
}

/// Returns the cache directory for `map`'s current `.vpk` content if it exists and is complete
/// (every file the manifest lists present with the recorded size, and the manifest's own
/// extractor version matches). Hashing the map VPK every call is acceptable (this is a
/// report-time cost); callers with a tighter budget (e.g. the map registry, listing many maps
/// per request) should use [`find_cached_with_hash`] with a memoized hash instead.
pub fn find_cached(
    cache_root: &Path,
    install: &GameInstall,
    map: &str,
) -> Result<Option<PathBuf>, ExtractError> {
    let vpk_path = install.map_vpk(map);
    let hash = sha256_file(&vpk_path).map_err(|source| ExtractError::Io {
        path: vpk_path,
        source,
    })?;
    find_cached_with_hash(cache_root, install, map, &hash)
}

/// [`find_cached`], but given the map VPK's sha256 rather than hashing it again - for a caller
/// that already has (or has memoized) the hash. `install` is kept in the signature for symmetry
/// with [`find_cached`] and left unused; the cache key no longer depends on the game build.
/// Tries the new-form directory first, then falls back to a matching legacy one
/// ([`find_legacy_dir`]).
pub fn find_cached_with_hash(
    cache_root: &Path,
    _install: &GameInstall,
    map: &str,
    vpk_sha256: &str,
) -> Result<Option<PathBuf>, ExtractError> {
    let sha12 = &vpk_sha256[..vpk_sha256.len().min(12)];
    let dir = cache_key_dir(cache_root, map, sha12);
    if is_complete(&dir) {
        return Ok(Some(dir));
    }
    Ok(find_legacy_dir(cache_root, map, sha12))
}

fn write_json<T: Serialize>(
    path: &Path,
    value: &T,
    files: &mut Vec<(String, u64)>,
    name: &str,
) -> Result<(), ExtractError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| ExtractError::Other(format!("failed to serialize {name}: {e}")))?;
    fs::write(path, text.as_bytes()).map_err(|source| ExtractError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    files.push((name.to_string(), text.len() as u64));
    Ok(())
}

/// Writes `extraction` into the cache, atomically: builds the directory as
/// `<dir>.tmp-<pid>`, then renames it into place. The target directory is derived from
/// `extraction.meta` (no re-hashing of the map VPK: it was already hashed once while building
/// that metadata). If the final directory already exists, `force` is false, and it's already
/// complete, this is a no-op that returns the existing directory unchanged; an *incomplete*
/// existing directory (e.g. a file was deleted from it) is always rebuilt, `force` or not.
pub fn save_extraction(
    cache_root: &Path,
    extraction: &Extraction,
    force: bool,
) -> Result<PathBuf, ExtractError> {
    let dir = cache_key_dir_from_meta(cache_root, &extraction.meta);
    if dir.exists() {
        if !force && is_complete(&dir) {
            return Ok(dir);
        }
        fs::remove_dir_all(&dir).map_err(|source| ExtractError::Io {
            path: dir.clone(),
            source,
        })?;
    }

    let parent = dir.parent().expect("cache dir always has a parent");
    fs::create_dir_all(parent).map_err(|source| ExtractError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let tmp_name = format!(
        "{}.tmp-{}",
        dir.file_name()
            .expect("cache dir has a name")
            .to_string_lossy(),
        std::process::id()
    );
    let tmp_dir = parent.join(tmp_name);
    if tmp_dir.exists() {
        let _ = fs::remove_dir_all(&tmp_dir);
    }
    fs::create_dir_all(&tmp_dir).map_err(|source| ExtractError::Io {
        path: tmp_dir.clone(),
        source,
    })?;

    let result = (|| -> Result<Vec<(String, u64)>, ExtractError> {
        let mut files = Vec::new();

        let shared_vpk_sha256_json = serde_json::to_string(&extraction.meta.shared_vpk_sha256)
            .unwrap_or_else(|_| "[]".to_string());
        let cgeo_meta: Vec<(String, String)> = vec![
            ("map".to_string(), extraction.meta.map.clone()),
            ("game_build".to_string(), extraction.meta.game_build.clone()),
            (
                "extractor_version".to_string(),
                extraction.meta.extractor_version.to_string(),
            ),
            (
                "created_utc".to_string(),
                extraction.meta.created_utc.clone(),
            ),
            (
                "map_vpk_sha256".to_string(),
                extraction.meta.map_vpk_sha256.clone(),
            ),
            ("shared_vpk_sha256".to_string(), shared_vpk_sha256_json),
            (
                "timing_ms".to_string(),
                extraction.meta.timing_ms.to_string(),
            ),
        ];
        let cgeo_path = tmp_dir.join("world.cgeo");
        cgeo::save_cgeo(&cgeo_path, &extraction.mesh, &cgeo_meta)
            .map_err(|e| ExtractError::Other(format!("failed to write world.cgeo: {e}")))?;
        let cgeo_len = fs::metadata(&cgeo_path)
            .map_err(|source| ExtractError::Io {
                path: cgeo_path.clone(),
                source,
            })?
            .len();
        files.push(("world.cgeo".to_string(), cgeo_len));

        write_json(
            &tmp_dir.join("entities.json"),
            &extraction.entities,
            &mut files,
            "entities.json",
        )?;
        if let Some(nav) = &extraction.nav {
            write_json(&tmp_dir.join("nav.json"), nav, &mut files, "nav.json")?;
        }
        write_json(
            &tmp_dir.join("report.json"),
            &extraction.report,
            &mut files,
            "report.json",
        )?;

        let manifest = Manifest {
            meta: extraction.meta.clone(),
            files: files.clone(),
        };
        write_json(
            &tmp_dir.join("manifest.json"),
            &manifest,
            &mut files,
            "manifest.json",
        )?;

        Ok(files)
    })();

    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    match fs::rename(&tmp_dir, &dir) {
        Ok(()) => Ok(dir),
        Err(source) => {
            // On Windows, renaming onto an existing directory fails outright (unlike POSIX's
            // atomic replace). If something else already finished writing a complete cache dir
            // here (e.g. a concurrent extract), that's success, not a failure: just drop our own
            // (redundant) temp copy and hand back the existing dir.
            if is_complete(&dir) {
                let _ = fs::remove_dir_all(&tmp_dir);
                return Ok(dir);
            }
            let _ = fs::remove_dir_all(&tmp_dir);
            Err(ExtractError::Io {
                path: dir.clone(),
                source,
            })
        }
    }
}

/// Loads a cached mesh (`world.cgeo`) from a cache directory.
pub fn load_mesh(dir: &Path) -> Result<CollisionMesh, ExtractError> {
    let path = dir.join("world.cgeo");
    let (mesh, _meta) = cgeo::load_cgeo(&path)
        .map_err(|e| ExtractError::Other(format!("failed to load {}: {e}", path.display())))?;
    Ok(mesh)
}

/// Loads a cache directory's manifest.
pub fn load_manifest(dir: &Path) -> Result<Manifest, ExtractError> {
    let path = dir.join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|source| ExtractError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&text)
        .map_err(|e| ExtractError::Other(format!("failed to parse {}: {e}", path.display())))
}

/// Loads a cache directory's report.
pub fn load_report(dir: &Path) -> Result<crate::report::ExtractReport, ExtractError> {
    let path = dir.join("report.json");
    let text = fs::read_to_string(&path).map_err(|source| ExtractError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&text)
        .map_err(|e| ExtractError::Other(format!("failed to parse {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir =
            std::env::temp_dir().join(format!("extract_cache_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        // 1970 and 1971 are not leap years (365 days each), so day 730 is 1972-01-01; 1972 is a
        // leap year, so Feb has 29 days: day 730+31 = 761 is 1972-02-01, day 789 is 1972-02-29
        // (the leap day), and day 790 rolls over to 1972-03-01.
        assert_eq!(civil_from_days(761), (1972, 2, 1));
        assert_eq!(civil_from_days(789), (1972, 2, 29));
        assert_eq!(civil_from_days(790), (1972, 3, 1));
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_utc_rfc3339();
        assert_eq!(s.len(), 20);
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
        assert_eq!(s.as_bytes()[19], b'Z');
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let dir = temp_dir("sha");
        let path = dir.join("abc.txt");
        fs::write(&path, b"abc").unwrap();
        let hash = sha256_file(&path).unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn fake_install(root: &Path, build: &str) -> GameInstall {
        fs::create_dir_all(root.join("maps")).unwrap();
        fs::write(root.join("steam.inf"), format!("ClientVersion={build}\n")).unwrap();
        fs::write(root.join("maps").join("de_test.vpk"), b"fake vpk bytes").unwrap();
        GameInstall {
            csgo_dir: root.to_path_buf(),
        }
    }

    /// `extraction.meta.map_vpk_sha256` must match what `find_cached`/`compute_key` computes
    /// from `install`'s actual map VPK bytes, since `save_extraction` derives its target
    /// directory purely from `meta` while `find_cached` re-hashes the file -- both must agree on
    /// the same cache dir for a real extraction.
    fn sample_extraction(install: &GameInstall, map: &str) -> Extraction {
        let vpk_sha256 = sha256_file(&install.map_vpk(map)).unwrap();
        Extraction {
            mesh: CollisionMesh::new(),
            entities: Vec::new(),
            nav: None,
            report: crate::report::ExtractReport::default(),
            meta: ExtractMeta {
                map: map.to_string(),
                game_build: "2000908".to_string(),
                extractor_version: EXTRACTOR_VERSION,
                map_vpk_sha256: vpk_sha256,
                shared_vpk_sha256: Vec::new(),
                created_utc: now_utc_rfc3339(),
                timing_ms: 0,
            },
        }
    }

    #[test]
    fn save_find_and_replace_round_trip() {
        let root = temp_dir("round_trip_root");
        let cache_root = temp_dir("round_trip_cache");
        let install = fake_install(&root, "2000908");

        assert!(
            find_cached(&cache_root, &install, "de_test")
                .unwrap()
                .is_none()
        );

        let extraction = sample_extraction(&install, "de_test");
        let dir = save_extraction(&cache_root, &extraction, false).unwrap();
        assert!(dir.join("manifest.json").is_file());
        assert!(dir.join("world.cgeo").is_file());
        assert!(dir.join("entities.json").is_file());
        assert!(dir.join("report.json").is_file());
        assert!(!dir.join("nav.json").exists());

        let found = find_cached(&cache_root, &install, "de_test").unwrap();
        assert_eq!(found.as_deref(), Some(dir.as_path()));

        // Without --force, re-saving a complete cache is a no-op that returns the existing dir
        // unchanged.
        let marker = dir.join("marker.txt");
        fs::write(&marker, b"unchanged").unwrap();
        let dir2 = save_extraction(&cache_root, &extraction, false).unwrap();
        assert_eq!(dir2, dir);
        assert!(marker.exists());

        // --force replaces the directory (the marker file must be gone afterwards).
        let dir3 = save_extraction(&cache_root, &extraction, true).unwrap();
        assert_eq!(dir3, dir);
        assert!(!marker.exists());

        let (mesh, _) = cgeo::load_cgeo(dir.join("world.cgeo")).unwrap();
        assert_eq!(mesh.vertices.len(), 0);
    }

    #[test]
    fn incomplete_cache_is_repaired_without_force() {
        let root = temp_dir("repair_root");
        let cache_root = temp_dir("repair_cache");
        let install = fake_install(&root, "2000908");
        let extraction = sample_extraction(&install, "de_test");

        let dir = save_extraction(&cache_root, &extraction, false).unwrap();
        assert!(
            find_cached(&cache_root, &install, "de_test")
                .unwrap()
                .is_some()
        );

        // Delete world.cgeo: the cache dir still exists but is now incomplete.
        fs::remove_file(dir.join("world.cgeo")).unwrap();
        assert!(!is_complete(&dir));
        assert!(
            find_cached(&cache_root, &install, "de_test")
                .unwrap()
                .is_none()
        );

        // Without --force, save_extraction must notice the dir is incomplete and rebuild it
        // rather than treating its mere existence as "already cached".
        let repaired = save_extraction(&cache_root, &extraction, false).unwrap();
        assert_eq!(repaired, dir);
        assert!(dir.join("world.cgeo").is_file());
        assert!(is_complete(&dir));
    }

    #[test]
    fn cache_dir_name_matches_sha_and_version() {
        let dir = cache_key_dir(Path::new("/cache"), "de_mirage", "abcdef012345");
        assert_eq!(
            dir,
            Path::new(&format!(
                "/cache/maps/de_mirage/abcdef012345-x{EXTRACTOR_VERSION}"
            ))
        );
    }

    /// Writes a directory `is_complete` accepts (a readable, current-version manifest listing no
    /// files to additionally check for the presence of) - a minimal stand-in for a cache
    /// directory, for tests that exercise directory-selection logic rather than the files
    /// `save_extraction` actually writes.
    fn write_bare_complete_dir(dir: &Path, map: &str, build: &str, vpk_sha256: &str) {
        fs::create_dir_all(dir).unwrap();
        let manifest = Manifest {
            meta: ExtractMeta {
                map: map.to_string(),
                game_build: build.to_string(),
                extractor_version: EXTRACTOR_VERSION,
                map_vpk_sha256: vpk_sha256.to_string(),
                shared_vpk_sha256: Vec::new(),
                created_utc: now_utc_rfc3339(),
                timing_ms: 0,
            },
            files: Vec::new(),
        };
        let text = serde_json::to_string_pretty(&manifest).unwrap();
        fs::write(dir.join("manifest.json"), text).unwrap();
    }

    #[test]
    fn new_form_dir_is_found_by_hash() {
        let cache_root = temp_dir("newform");
        let sha = "a".repeat(64);
        let sha12 = &sha[..12];
        let dir = cache_key_dir(&cache_root, "de_mirage", sha12);
        write_bare_complete_dir(&dir, "de_mirage", "2000914", &sha);

        let install = fake_install(&cache_root.join("game"), "2000914");
        let found = find_cached_with_hash(&cache_root, &install, "de_mirage", &sha).unwrap();
        assert_eq!(found.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn legacy_dir_is_found_as_a_fallback_when_no_new_form_dir_exists() {
        let cache_root = temp_dir("legacy");
        let sha = "c".repeat(64);
        let sha12 = &sha[..12];

        let legacy_dir = cache_root
            .join("maps")
            .join("de_mirage")
            .join(format!("2000908-{sha12}-x{EXTRACTOR_VERSION}"));
        write_bare_complete_dir(&legacy_dir, "de_mirage", "2000908", &sha);

        let install = fake_install(&cache_root.join("game"), "2000914");
        let found = find_cached_with_hash(&cache_root, &install, "de_mirage", &sha).unwrap();
        assert_eq!(found.as_deref(), Some(legacy_dir.as_path()));
    }

    #[test]
    fn a_dir_with_a_different_hash_does_not_match() {
        let cache_root = temp_dir("wronghash");
        let sha_cached = "d".repeat(64);
        let sha_current = "e".repeat(64);
        let sha12 = &sha_cached[..12];

        let dir = cache_key_dir(&cache_root, "de_mirage", sha12);
        write_bare_complete_dir(&dir, "de_mirage", "2000908", &sha_cached);

        let install = fake_install(&cache_root.join("game"), "2000914");
        let found =
            find_cached_with_hash(&cache_root, &install, "de_mirage", &sha_current).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn incomplete_new_form_dir_does_not_shadow_a_complete_legacy_one() {
        let cache_root = temp_dir("incomplete");
        let sha = "b".repeat(64);
        let sha12 = &sha[..12];

        // New-form dir: manifest present, but claims a file that isn't there.
        let new_dir = cache_key_dir(&cache_root, "de_mirage", sha12);
        write_bare_complete_dir(&new_dir, "de_mirage", "2000914", &sha);
        let mut manifest: Manifest =
            serde_json::from_str(&fs::read_to_string(new_dir.join("manifest.json")).unwrap())
                .unwrap();
        manifest.files.push(("world.cgeo".to_string(), 4));
        fs::write(
            new_dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert!(!is_complete(&new_dir));

        // A complete legacy dir for the same hash and extractor version.
        let legacy_dir = cache_root
            .join("maps")
            .join("de_mirage")
            .join(format!("2000908-{sha12}-x{EXTRACTOR_VERSION}"));
        write_bare_complete_dir(&legacy_dir, "de_mirage", "2000908", &sha);

        let install = fake_install(&cache_root.join("game"), "2000914");
        let found = find_cached_with_hash(&cache_root, &install, "de_mirage", &sha).unwrap();
        assert_eq!(found.as_deref(), Some(legacy_dir.as_path()));
    }

    /// Real game, real repo cache: a map whose `.vpk` hasn't changed since a previous CS2 patch
    /// must still resolve through `find_cached`, without re-extraction - the main observable
    /// outcome of dropping the build from the cache key. Needs `CS2_GAME_DIR` and an existing
    /// `de_dust2` cache directory (any build) under `<repo>/cache`.
    #[test]
    #[ignore = "needs CS2_GAME_DIR"]
    fn unchanged_map_finds_a_cache_from_a_previous_build() {
        let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
        let install = GameInstall::new(PathBuf::from(&game_dir)).expect("valid CS2 install");
        let cache_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("cache");

        let found = find_cached(&cache_root, &install, "de_dust2")
            .expect("find_cached")
            .expect("expected an existing cache for de_dust2 (its .vpk hasn't changed)");
        let name = found.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !name.starts_with(&format!("{}-", install.build_id().unwrap())),
            "expected a cache dir from a previous build to be reused, got {name}"
        );
    }
}
