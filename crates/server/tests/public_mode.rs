//! `s6t_public_mode.md` route tests: public-mode 403s/filtering/429, and the local-mode `Host`
//! header (DNS-rebinding) guard. `tower::ServiceExt::oneshot`, no network port opened.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use extract::build::Extraction;
use extract::cache;
use extract::game::GameInstall;
use extract::report::{ExtractMeta, ExtractReport, NavAreaDump, NavAreasDump};
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use server::AppState;
use server::config::AppConfig;

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
        "cs2mod_server_public_mode_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn fake_install(root: &Path, map: &str) -> GameInstall {
    fs::create_dir_all(root.join("maps")).unwrap();
    fs::write(root.join("steam.inf"), "ClientVersion=2000908\n").unwrap();
    fs::write(root.join("pak01_dir.vpk"), b"pak bytes").unwrap();
    fs::write(
        root.join("maps").join(format!("{map}.vpk")),
        b"fake vpk bytes",
    )
    .unwrap();
    GameInstall::new(root).unwrap()
}

/// A flat floor at z=0, big enough to give the queue-overflow test's lineup query somewhere to
/// land (same idiom as `solve_api.rs::flat_floor_mesh`).
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

/// Writes a complete cache directory for `map` under `cache_root` (its own install root nested
/// inside it), with nav data so `has_lineups` is true; `ready` additionally writes
/// `standspots.json`/`viewer-map.png` so `has_stand_spots`/`has_radar` are true too (`MapSummary`
/// only checks these two files' presence, not their contents - `registry.rs::map_summary`).
fn sample_cache_dir(cache_root: &Path, map: &str, ready: bool) -> PathBuf {
    let root = cache_root.join(format!("_install_{map}"));
    fs::create_dir_all(&root).unwrap();
    let install = fake_install(&root, map);
    let vpk_sha256 = cache::sha256_file(&install.map_vpk(map)).unwrap();
    let half = 600.0;
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
    let dir = cache::save_extraction(cache_root, &extraction, false).unwrap();
    if ready {
        fs::write(dir.join("standspots.json"), b"{}").unwrap();
        fs::write(dir.join("viewer-map.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    }
    dir
}

fn state_over(cache_root: &Path, public: bool, bound_port: u16) -> AppState {
    let config_path = cache_root.join("_config").join("config.json");
    fs::create_dir_all(cache_root.join("_config")).unwrap();
    let viewer_dir = cache_root.join("_viewer_empty");
    fs::create_dir_all(&viewer_dir).unwrap();
    let mut state = AppState::new(
        config_path,
        AppConfig::default(),
        cache_root.to_path_buf(),
        viewer_dir,
    );
    state.public = public;
    state.bound_port = bound_port;
    state
}

fn router_over(cache_root: &Path, public: bool) -> Router {
    server::routes::router(Arc::new(state_over(cache_root, public, 0)))
}

async fn request(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    request(
        router,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn get_with_host(router: &Router, uri: &str, host: &str) -> (StatusCode, Value) {
    request(
        router,
        Request::builder()
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

// ---- public mode: 403s ---------------------------------------------------------------------

#[tokio::test]
async fn put_config_is_403_in_public_mode() {
    let cache_root = temp_dir("put_config_public");
    let router = router_over(&cache_root, true);
    let (status, body) = request(
        &router,
        Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "theme": "dark" }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        json!("В демо-режиме настройки менять нельзя.")
    );
}

#[tokio::test]
async fn post_extract_is_403_in_public_mode() {
    let cache_root = temp_dir("post_extract_public");
    let router = router_over(&cache_root, true);
    let (status, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/jobs/extract")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "map": "de_test" }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        json!("В демо-режиме карты готовит только автор.")
    );
}

#[tokio::test]
async fn delete_job_is_403_in_public_mode() {
    let cache_root = temp_dir("delete_job_public");
    let router = router_over(&cache_root, true);
    let (status, body) = request(
        &router,
        Request::builder()
            .method("DELETE")
            .uri("/api/jobs/job-00000001")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"],
        json!("В демо-режиме карты готовит только автор.")
    );
}

#[tokio::test]
async fn job_stream_is_404_in_public_mode() {
    let cache_root = temp_dir("job_stream_public");
    let router = router_over(&cache_root, true);
    let (status, _) = get(&router, "/api/jobs/job-00000001").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_jobs_is_empty_array_in_public_mode() {
    let cache_root = temp_dir("get_jobs_public");
    let router = router_over(&cache_root, true);
    let (status, body) = get(&router, "/api/jobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

// ---- public mode: /api/config strips local paths ------------------------------------------

#[tokio::test]
async fn config_strips_local_paths_and_reports_public() {
    let cache_root = temp_dir("config_public");
    let install_root = cache_root.join("_game");
    fake_install(&install_root, "de_test");
    let explicit_cache_dir = cache_root.join("_explicit_cache_dir");
    fs::create_dir_all(&explicit_cache_dir).unwrap();
    let cfg = AppConfig {
        cache_dir: Some(explicit_cache_dir),
        ..AppConfig::default()
    };
    let config_path = cache_root.join("_config").join("config.json");
    fs::create_dir_all(cache_root.join("_config")).unwrap();
    let viewer_dir = cache_root.join("_viewer_empty");
    fs::create_dir_all(&viewer_dir).unwrap();
    let mut state = AppState::new(config_path, cfg, cache_root.to_path_buf(), viewer_dir)
        .with_overrides(Some(install_root), None, None);
    state.public = true;
    let router = server::routes::router(Arc::new(state));

    let (status, body) = get(&router, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], json!(true));
    assert_eq!(body["gameDir"], Value::Null);
    assert_eq!(body["cacheDir"], Value::Null);
    assert_eq!(body["public"], json!(true));
    assert_eq!(
        body["downloadUrl"],
        json!("https://github.com/Evelynkaz/cs2-modulation/releases")
    );
}

// ---- public mode: /api/maps and /api/config's own `maps` are filtered ---------------------

#[tokio::test]
async fn maps_are_filtered_to_fully_prepared_in_public_mode_only() {
    let cache_root = temp_dir("maps_filtered");
    sample_cache_dir(&cache_root, "de_ready", true);
    sample_cache_dir(&cache_root, "de_partial", false);

    let public_router = router_over(&cache_root, true);
    let (status, body) = get(&public_router, "/api/maps").await;
    assert_eq!(status, StatusCode::OK);
    let maps: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["map"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(maps, vec!["de_ready".to_string()]);

    let (status, body) = get(&public_router, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    let config_maps: Vec<String> = body["maps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["map"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(config_maps, vec!["de_ready".to_string()]);

    let local_router = router_over(&cache_root, false);
    let (status, body) = get(&local_router, "/api/maps").await;
    assert_eq!(status, StatusCode::OK);
    let mut maps: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["map"].as_str().unwrap().to_string())
        .collect();
    maps.sort();
    assert_eq!(maps, vec!["de_partial".to_string(), "de_ready".to_string()]);
}

// ---- public mode: the dedicated solve queue's own (tiny, test-overridden) cap --------------

#[tokio::test]
async fn lineup_429_on_public_queue_overflow_with_a_tiny_queue() {
    let cache_root = temp_dir("public_queue_overflow");
    sample_cache_dir(&cache_root, "de_test", true);
    let mut state = state_over(&cache_root, true, 0);
    state.public_queue_cap = 0;
    let router = server::routes::router(Arc::new(state));

    let (status, body) = request(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/lineup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "map": "de_test", "target": [0.0, 0.0] }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["error"],
        json!("Сервер занят другими поисками, попробуйте через минуту.")
    );
}

// ---- local mode: the `Host` DNS-rebinding guard --------------------------------------------

#[tokio::test]
async fn host_check_allows_the_three_local_hostnames() {
    let cache_root = temp_dir("host_allowed");
    let router = server::routes::router(Arc::new(state_over(&cache_root, false, 18099)));
    for host in ["127.0.0.1:18099", "localhost:18099", "[::1]:18099"] {
        let (status, _) = get_with_host(&router, "/api/maps", host).await;
        assert_eq!(status, StatusCode::OK, "host {host} should be allowed");
    }
}

#[tokio::test]
async fn host_check_rejects_an_unknown_host_in_local_mode() {
    let cache_root = temp_dir("host_rejected");
    let router = server::routes::router(Arc::new(state_over(&cache_root, false, 18099)));
    let (status, body) = get_with_host(&router, "/api/maps", "evil.example").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], json!("invalid Host header"));

    // A right hostname but a wrong port is just as much a rebinding vector - also rejected.
    let (status, _) = get_with_host(&router, "/api/maps", "127.0.0.1:9999").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn host_check_is_skipped_entirely_in_public_mode() {
    let cache_root = temp_dir("host_public_skips");
    let router = server::routes::router(Arc::new(state_over(&cache_root, true, 18099)));
    let (status, _) = get_with_host(&router, "/api/maps", "evil.example").await;
    assert_eq!(status, StatusCode::OK);
}

// ---- follow-up security review: art.rs never leaks a local path in public mode -------------

#[tokio::test]
async fn overview_500_in_public_mode_has_no_local_path() {
    let cache_root = temp_dir("overview_public_no_path");
    let install_root = cache_root.join("_game");
    // `fake_install`'s `pak01_dir.vpk` is 9 garbage bytes, not a real VPK - `Vpk::open` fails
    // reading it, which is exactly the path-bearing error `art::cached_pak01` used to return
    // verbatim.
    fake_install(&install_root, "de_test");
    let state =
        state_over(&cache_root, true, 0).with_overrides(Some(install_root.clone()), None, None);
    let router = server::routes::router(Arc::new(state));

    let (status, body) = get(&router, "/api/overview?map=de_test").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let msg = body["error"].as_str().unwrap();
    assert_eq!(msg, "map art unavailable");
    let install_str = install_root.display().to_string();
    assert!(!msg.contains(&install_str), "{msg}");
    assert!(!msg.contains(':') && !msg.contains('\\'), "{msg}");
}

// ---- follow-up security review item 3: a stalled (connected but unread) client -------------

#[tokio::test]
async fn public_solve_permit_released_when_client_never_reads_the_stream() {
    let cache_root = temp_dir("stalled_client");
    sample_cache_dir(&cache_root, "de_test", true);
    let mut state = state_over(&cache_root, true, 0);
    // Shortened so the test doesn't need to wait anywhere near the real 120s/10s defaults.
    state.public_solve_time_limit = Duration::from_millis(300);
    state.send_timeout = Duration::from_millis(200);
    let state = Arc::new(state);
    let router = server::routes::router(state.clone());

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "map": "de_test", "target": [0.0, 0.0] }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Never read `resp`'s NDJSON body - the same "connected but not reading" stall a reverse
    // proxy with `proxy_buffering off` passes straight through to this server. Without the
    // `send_timeout` wrap, the background solve task would block forever inside `send_line`,
    // holding `public_solve_semaphore`'s only permit and never noticing `public_solve_time_limit`.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        state.public_solve_semaphore.available_permits(),
        1,
        "the compute permit must be released once a stalled client's solve gives up"
    );
    drop(resp);
}
