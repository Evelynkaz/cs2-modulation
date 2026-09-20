//! `extract::mapdata`: no CS2 install needed -- every test builds a synthetic cache directory.

use std::fs;
use std::path::{Path, PathBuf};

use extract::build::Extraction;
use extract::cache;
use extract::game::GameInstall;
use extract::mapdata::{StandSpotFile, StandSpotJson, StandSpotsState};
use extract::report::{EntityRecord, ExtractMeta, ExtractReport, NavAreaDump, NavAreasDump};
use geom::mesh::CollisionMesh;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cs2mod_extract_mapdata_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `GameInstall` and cache dir with just enough on disk (`maps/<map>.vpk`, `pak01_dir.vpk`,
/// `steam.inf`) for `cache::save_extraction`/`find_cached` to key off it, matching
/// `cache.rs`'s own test helper.
fn fake_install(root: &Path) -> GameInstall {
    fs::create_dir_all(root.join("maps")).unwrap();
    fs::write(root.join("steam.inf"), "ClientVersion=2000908\n").unwrap();
    fs::write(root.join("pak01_dir.vpk"), b"pak bytes").unwrap();
    fs::write(root.join("maps").join("de_test.vpk"), b"fake vpk bytes").unwrap();
    GameInstall::new(root).unwrap()
}

/// Writes a minimal complete cache directory for `de_test` (mesh + entities + report +
/// manifest, no nav/standspots) and returns its path.
fn sample_cache_dir(root: &Path, cache_root: &Path, entities: Vec<EntityRecord>) -> PathBuf {
    let install = fake_install(root);
    let vpk_sha256 = cache::sha256_file(&install.map_vpk("de_test")).unwrap();
    let extraction = Extraction {
        mesh: CollisionMesh::new(),
        entities,
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
    cache::save_extraction(cache_root, &extraction, false).unwrap()
}

fn entity(classname: &str, targetname: Option<&str>, origin: [f32; 3]) -> EntityRecord {
    EntityRecord {
        classname: classname.to_string(),
        targetname: targetname.map(str::to_string),
        origin,
        angles: [0.0, 0.0, 0.0],
        model: None,
        hammer_id: None,
        properties: serde_json::Map::new(),
        from_child_lump: None,
    }
}

#[test]
fn missing_nav_json_is_an_empty_vec_not_an_error() {
    let root = temp_dir("missing_nav_root");
    let cache_root = temp_dir("missing_nav_cache");
    let dir = sample_cache_dir(&root, &cache_root, Vec::new());

    let nav_areas = extract::load_nav_areas(&dir).unwrap();
    assert!(nav_areas.is_empty());

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_root).ok();
}

#[test]
fn load_nav_areas_keeps_only_hull_index_zero() {
    let root = temp_dir("nav_happy_root");
    let cache_root = temp_dir("nav_happy_cache");
    let dir = sample_cache_dir(&root, &cache_root, Vec::new());
    let hull0_corners: Vec<[f32; 3]> = vec![
        [0.0, 0.0, 0.0],
        [64.0, 0.0, 0.0],
        [64.0, 64.0, 0.0],
        [0.0, 64.0, 0.0],
    ];
    let hull1_corners: Vec<[f32; 3]> = vec![
        [100.0, 100.0, 0.0],
        [164.0, 100.0, 0.0],
        [164.0, 164.0, 0.0],
        [100.0, 164.0, 0.0],
    ];
    let dump = NavAreasDump {
        version: 0,
        sub_version: 0,
        generation_hulls: Vec::new(),
        areas: vec![
            NavAreaDump {
                id: 1,
                hull_index: 0,
                attribute_flags: 0,
                corners: hull0_corners.clone(),
                connections: Vec::new(),
                ladders_above: Vec::new(),
                ladders_below: Vec::new(),
            },
            NavAreaDump {
                id: 2,
                hull_index: 1,
                attribute_flags: 0,
                corners: hull1_corners,
                connections: Vec::new(),
                ladders_above: Vec::new(),
                ladders_below: Vec::new(),
            },
        ],
        ladders: Vec::new(),
    };
    fs::write(dir.join("nav.json"), serde_json::to_string(&dump).unwrap()).unwrap();

    let nav_areas = extract::load_nav_areas(&dir).unwrap();
    assert_eq!(nav_areas.len(), 1);
    let corners: Vec<[f32; 3]> = nav_areas[0].iter().map(|v| v.to_array()).collect();
    assert_eq!(corners, hull0_corners);

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_root).ok();
}

#[test]
fn malformed_nav_json_is_an_error() {
    let root = temp_dir("bad_nav_root");
    let cache_root = temp_dir("bad_nav_cache");
    let dir = sample_cache_dir(&root, &cache_root, Vec::new());
    fs::write(dir.join("nav.json"), b"not valid json").unwrap();

    let err = extract::load_nav_areas(&dir).unwrap_err();
    assert!(err.to_string().contains("nav.json"));

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_root).ok();
}

#[test]
fn spawns_filters_2v2_wingman_spawns() {
    let root = temp_dir("spawns_root");
    let cache_root = temp_dir("spawns_cache");
    let entities = vec![
        entity("info_player_terrorist", None, [1.0, 2.0, 3.0]),
        entity(
            "info_player_terrorist",
            Some("[PR1]spawnpoints.2v2"),
            [4.0, 5.0, 6.0],
        ),
        entity("info_player_counterterrorist", None, [7.0, 8.0, 9.0]),
        entity("func_door", None, [0.0, 0.0, 0.0]),
    ];
    let dir = sample_cache_dir(&root, &cache_root, entities);

    let spawns = extract::load_spawns(&dir).unwrap();
    assert_eq!(spawns.t.len(), 1);
    assert_eq!(spawns.t[0], geom::math::V3::new(1.0, 2.0, 3.0));
    assert_eq!(spawns.ct.len(), 1);
    assert_eq!(spawns.ct[0], geom::math::V3::new(7.0, 8.0, 9.0));

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_root).ok();
}

fn sample_stand_spot_file() -> StandSpotFile {
    StandSpotFile {
        version: extract::STANDSPOTS_VERSION,
        map: "de_test".to_string(),
        step: 24.0,
        spots: vec![StandSpotJson {
            feet: [1.0, 2.0, 3.0],
            stance: "Standing".to_string(),
            nav: true,
        }],
    }
}

#[test]
fn stand_spots_missing() {
    let dir = temp_dir("standspots_missing");
    assert!(matches!(
        extract::load_stand_spots(&dir),
        StandSpotsState::Missing
    ));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stand_spots_unreadable() {
    let dir = temp_dir("standspots_unreadable");
    fs::write(dir.join("standspots.json"), b"not valid json").unwrap();
    assert!(matches!(
        extract::load_stand_spots(&dir),
        StandSpotsState::Unreadable
    ));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stand_spots_stale() {
    let dir = temp_dir("standspots_stale");
    let mut file = sample_stand_spot_file();
    file.version = extract::STANDSPOTS_VERSION + 1;
    fs::write(
        dir.join("standspots.json"),
        serde_json::to_string(&file).unwrap(),
    )
    .unwrap();
    match extract::load_stand_spots(&dir) {
        StandSpotsState::Stale { version } => assert_eq!(version, extract::STANDSPOTS_VERSION + 1),
        other => panic!("expected Stale, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stand_spots_loaded_round_trips_through_save() {
    let dir = temp_dir("standspots_roundtrip");
    let file = sample_stand_spot_file();
    extract::save_stand_spots(&dir, &file).unwrap();
    assert!(dir.join("standspots.json").is_file());
    assert!(!dir.join("standspots.json.tmp").exists());

    match extract::load_stand_spots(&dir) {
        StandSpotsState::Loaded(loaded) => {
            assert_eq!(loaded.version, file.version);
            assert_eq!(loaded.map, file.map);
            assert_eq!(loaded.step, file.step);
            assert_eq!(loaded.spots.len(), 1);
            assert_eq!(loaded.spots[0].feet, [1.0, 2.0, 3.0]);
        }
        other => panic!("expected Loaded, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_stand_spots_overwrites_an_existing_file() {
    let dir = temp_dir("standspots_overwrite");
    extract::save_stand_spots(&dir, &sample_stand_spot_file()).unwrap();

    let mut overwrite = sample_stand_spot_file();
    overwrite.step = 32.0;
    extract::save_stand_spots(&dir, &overwrite).unwrap();

    match extract::load_stand_spots(&dir) {
        StandSpotsState::Loaded(loaded) => assert_eq!(loaded.step, 32.0),
        other => panic!("expected Loaded, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}
