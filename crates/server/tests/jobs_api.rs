//! `POST /api/jobs/{kind}`, `GET /api/jobs[/{id}]`, `DELETE /api/jobs/{id}` router-level tests
//! (`s6e_progress_jobs.md`'s "Часть E3" `tests` section), against a small synthetic map. No
//! network port opened.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use extract::build::Extraction;
use extract::cache;
use extract::game::GameInstall;
use extract::mapdata::StandSpotsState;
use extract::report::{ExtractMeta, ExtractReport, NavAreaDump, NavAreasDump};
use geom::filter;
use geom::grid::UniformGrid;
use geom::math::V3;
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use server::AppState;
use server::config::AppConfig;
use solver::standspots;

/// A `std::env::temp_dir()` subdirectory unique to one test, removed (recursively) on drop, even
/// on panic.
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
        "cs2mod_server_jobs_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn fake_install(root: &Path) -> GameInstall {
    fs::create_dir_all(root.join("maps")).unwrap();
    fs::write(root.join("steam.inf"), "ClientVersion=2000908\n").unwrap();
    fs::write(root.join("pak01_dir.vpk"), b"pak bytes").unwrap();
    fs::write(root.join("maps").join("de_test.vpk"), b"fake vpk bytes").unwrap();
    GameInstall::new(root).unwrap()
}

/// A flat floor at z=0, `-half..half` on both axes.
fn flat_floor_mesh(half: f32) -> CollisionMesh {
    let mut mesh = CollisionMesh::new();
    let attr = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let obj = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldMesh,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    mesh.push_triangles(
        &[
            [-half, -half, 0.0],
            [half, -half, 0.0],
            [half, half, 0.0],
            [-half, half, 0.0],
        ],
        &[[0, 1, 2], [0, 2, 3]],
        attr,
        |_| SurfaceProperty::NONE,
        obj,
    )
    .unwrap();
    mesh
}

fn nav_area(half: f32) -> NavAreaDump {
    NavAreaDump {
        id: 0,
        hull_index: 0,
        attribute_flags: 0,
        corners: vec![
            [-half, -half, 0.0],
            [half, -half, 0.0],
            [half, half, 0.0],
            [-half, half, 0.0],
        ],
        connections: Vec::new(),
        ladders_above: Vec::new(),
        ladders_below: Vec::new(),
    }
}

/// Writes a complete `<map>` cache directory with a flat floor and one nav area covering it, of
/// side `2*half`, under `cache_root` (a fake install root nested inside it, cleaned up along with
/// everything else).
fn sample_cache_dir(cache_root: &Path, map: &str, half: f32) {
    let root = cache_root.join("_install_root");
    fs::create_dir_all(&root).unwrap();
    let install = fake_install(&root);
    fs::write(root.join("maps").join(format!("{map}.vpk")), b"fake vpk").unwrap();
    let vpk_sha256 = cache::sha256_file(&install.map_vpk(map)).unwrap();
    let extraction = Extraction {
        mesh: flat_floor_mesh(half),
        entities: Vec::new(),
        nav: Some(NavAreasDump {
            version: 1,
            sub_version: 0,
            generation_hulls: Vec::new(),
            areas: vec![nav_area(half)],
            ladders: Vec::new(),
        }),
        report: ExtractReport::default(),
        meta: ExtractMeta {
            map: map.to_string(),
            game_build: "2000908".to_string(),
            extractor_version: extract::EXTRACTOR_VERSION,
            map_vpk_sha256: vpk_sha256,
            shared_vpk_sha256: Vec::new(),
            created_utc: cache::now_utc_rfc3339(),
            timing_ms: 0,
        },
    };
    cache::save_extraction(cache_root, &extraction, false).unwrap();
}

fn state_over(cache_root: &Path) -> Arc<AppState> {
    let config_path = cache_root.join("_config").join("config.json");
    fs::create_dir_all(cache_root.join("_config")).unwrap();
    let viewer_dir = cache_root.join("_viewer_empty");
    fs::create_dir_all(&viewer_dir).unwrap();
    Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root.to_path_buf(),
        viewer_dir,
    ))
}

fn router_over(cache_root: &Path) -> Router {
    server::routes::router(state_over(cache_root))
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

async fn post_job(router: &Router, kind: &str, map: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/jobs/{kind}"))
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "map": map }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

async fn get_job_stream(router: &Router, id: &str) -> (StatusCode, String) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/jobs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn delete_job(router: &Router, id: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/jobs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[tokio::test]
async fn standspots_on_unknown_map_errors_in_the_stream_not_a_panic() {
    let cache_root = temp_dir("unknown_std");
    sample_cache_dir(&cache_root, "de_test", 200.0);
    let router = router_over(&cache_root);

    let (status, body) = post_job(&router, "standspots", "de_nope").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["job"].as_str().unwrap().to_string();

    // The stream may still be catching up to `Running`; poll until it reports a terminal line.
    let mut last_text = String::new();
    for _ in 0..50 {
        let (status, text) = get_job_stream(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        last_text = text;
        if last_text.contains("\"error\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        last_text.contains("unknown map"),
        "expected an error line, got {last_text:?}"
    );
}

#[tokio::test]
async fn repeated_post_returns_the_same_job_id() {
    let cache_root = temp_dir("dedupe");
    sample_cache_dir(&cache_root, "de_test", 1500.0);
    let router = router_over(&cache_root);

    let (status_a, body_a) = post_job(&router, "standspots", "de_test").await;
    assert_eq!(status_a, StatusCode::ACCEPTED);
    let (status_b, body_b) = post_job(&router, "standspots", "de_test").await;
    assert_eq!(status_b, StatusCode::ACCEPTED);
    assert_eq!(body_a["job"], body_b["job"], "expected the same job id");
}

#[tokio::test]
async fn get_after_completion_replays_the_stored_state() {
    let cache_root = temp_dir("replay");
    sample_cache_dir(&cache_root, "de_test", 200.0);
    let router = router_over(&cache_root);

    let (_status, maps_before) = get(&router, "/api/maps").await;
    let before = maps_before
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["map"] == "de_test")
        .expect("de_test in /api/maps");
    assert_eq!(before["hasStandSpots"], json!(false));

    let (status, body) = post_job(&router, "standspots", "de_test").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["job"].as_str().unwrap().to_string();

    let mut last_text = String::new();
    for _ in 0..200 {
        let (status, text) = get_job_stream(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        last_text = text.clone();
        if text.contains("\"result\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        last_text.contains("\"result\""),
        "job never finished: {last_text:?}"
    );
    assert!(cache_root.join("maps").join("de_test").exists());

    // The map registry sees the new stand spots without a server restart.
    let (_status, maps_after) = get(&router, "/api/maps").await;
    let after = maps_after
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["map"] == "de_test")
        .expect("de_test in /api/maps");
    assert_eq!(after["hasStandSpots"], json!(true));

    // A fresh `GET` after completion replays the same stored state without recomputing.
    let (status, text2) = get_job_stream(&router, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text2.contains("\"result\""));
}

/// A `MapEntry` fetched from the registry before the job sees stand spots loaded afterwards -
/// `registry::MapRegistry::reload_stand_spots`, not just `/api/maps`' file-presence check.
#[tokio::test]
async fn registry_entry_sees_stand_spots_after_the_job() {
    let cache_root = temp_dir("registry_reload");
    sample_cache_dir(&cache_root, "de_test", 200.0);
    let state = state_over(&cache_root);
    let router = server::routes::router(state.clone());

    let entry_before = state.registry.get("de_test", None).unwrap();
    assert!(!entry_before.has_stand_spots());

    let (status, body) = post_job(&router, "standspots", "de_test").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["job"].as_str().unwrap().to_string();

    let mut last_text = String::new();
    for _ in 0..200 {
        let (status, text) = get_job_stream(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        last_text = text.clone();
        if text.contains("\"result\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        last_text.contains("\"result\""),
        "job never finished: {last_text:?}"
    );

    let entry_after = state.registry.get("de_test", None).unwrap();
    assert!(matches!(
        entry_after.bundle.stand_spots,
        StandSpotsState::Loaded(_)
    ));
}

/// `DELETE` while a `standspots` job is still scanning: the job ends up `cancelled` and no
/// `standspots.json.tmp-*` is left behind. Uses a wide-enough region that the scan is still
/// running by the time the (immediately following) `DELETE` request lands.
#[tokio::test]
async fn delete_during_standspots_cancels_and_leaves_no_temp_files() {
    let cache_root = temp_dir("cancel");
    sample_cache_dir(&cache_root, "de_big", 6000.0);
    let router = router_over(&cache_root);

    let (status, body) = post_job(&router, "standspots", "de_big").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["job"].as_str().unwrap().to_string();

    let (del_status, _del_body) = delete_job(&router, &id).await;
    assert_eq!(del_status, StatusCode::OK);

    let mut last_text = String::new();
    for _ in 0..500 {
        let (status, text) = get_job_stream(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        last_text = text.clone();
        if text.contains("\"status\":\"cancelled\"") || text.contains("\"result\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        last_text.contains("\"status\":\"cancelled\""),
        "expected the job to end up cancelled (it may have finished before the DELETE landed - \
         widen the test map): {last_text:?}"
    );

    let build_dir = fs::read_dir(cache_root.join("maps").join("de_big"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut leftovers = Vec::new();
    for entry in fs::read_dir(&build_dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(".tmp-") {
            leftovers.push(name);
        }
    }
    assert!(
        leftovers.is_empty(),
        "left temp files behind: {leftovers:?}"
    );
    assert!(
        !build_dir.join("standspots.json").exists(),
        "a cancelled standspots job must not write standspots.json"
    );
}

/// Real de_mirage: `POST /api/jobs/standspots` through the API must report the same count as an
/// expected count computed independently in this test the way the CLI does it
/// (`crates/cli/src/cmd_solver.rs:71-103`: load the mesh and nav areas from the resolved cache
/// dir, build a player-mask `UniformGrid` at cell 128, run `solver::standspots::compute` over the
/// mesh bounds at step 16) - not from the job's own payload or the file it writes, so a drift of
/// the job's step or collider mask away from the CLI would fail this test. The cache file the job
/// writes must also load back (`extract::mapdata::load_stand_spots` as `Loaded`) with that same
/// count. Needs `CS2_GAME_DIR` and a prior `cs2mod extract de_mirage`.
#[tokio::test]
#[ignore = "needs CS2_GAME_DIR"]
async fn de_mirage_standspots_job_matches_cli_and_writes_a_loadable_cache() {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let cache_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache");

    let config_dir = temp_dir("real_mirage_config");
    let config_path = config_dir.join("config.json");
    let viewer_dir = temp_dir("real_mirage_viewer_empty");
    let state = Arc::new(
        AppState::new(
            config_path,
            AppConfig::default(),
            cache_root.clone(),
            viewer_dir.to_path_buf(),
        )
        .with_overrides(Some(PathBuf::from(&game_dir)), None, None),
    );
    let router = server::routes::router(state);

    let (status, body) = post_job(&router, "standspots", "de_mirage").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["job"].as_str().unwrap().to_string();

    let mut last_text = String::new();
    for _ in 0..3000 {
        let (status, text) = get_job_stream(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        last_text = text.clone();
        if text.contains("\"result\"") || text.contains("\"error\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        last_text.contains("\"result\""),
        "job did not complete: {last_text}"
    );
    let last_line = last_text.lines().last().expect("at least one line");
    let last_value: Value = serde_json::from_str(last_line).unwrap();

    let install = GameInstall::new(PathBuf::from(&game_dir)).expect("game install");
    let dir = cache::find_cached(&cache_root, &install, "de_mirage")
        .expect("find_cached")
        .expect("cache dir for de_mirage");

    // The expected count, computed independently the way the CLI does (cmd_solver.rs:71-103):
    // load the mesh and nav areas from the resolved cache dir, build a player-mask `UniformGrid`
    // at cell 128, and scan at step 16.
    let mesh = cache::load_mesh(&dir).expect("load_mesh");
    let nav_areas = extract::mapdata::load_nav_areas(&dir).expect("load_nav_areas");
    let (min, max) = mesh.bounds().expect("de_mirage mesh has triangles");
    let (min, max) = (V3::from_array(min), V3::from_array(max));
    let mask = filter::player_mask(&mesh);
    let collider = UniformGrid::build(&mesh, &mask, None, 128.0).expect("build player collider");
    let expected = standspots::compute(&collider, &nav_areas, min, max, 16.0, None).len();

    assert_eq!(last_value["result"]["count"], json!(expected));
    match extract::mapdata::load_stand_spots(&dir) {
        StandSpotsState::Loaded(file) => {
            assert_eq!(file.spots.len(), expected);
        }
        other => panic!("expected Loaded stand spots, got {other:?}"),
    }
}
