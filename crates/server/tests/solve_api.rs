//! `POST /api/lineup` router-level tests, `oneshot`'d against a small synthetic flat-floor map
//! (`s6d_solve_api.md`'s `tests` section). No network port opened.

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
        "cs2mod_server_solve_test_{name}_{}",
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

/// A flat floor at z=0, `-half..half` on both axes - enough open ground that some throw always
/// settles near where it was thrown, matching `solver::target::tests::flat_plane`.
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

/// Writes a complete `de_test` cache directory with a flat floor and one nav area covering it,
/// under `cache_root` (a fake install root nested inside it, cleaned up along with everything
/// else).
fn sample_cache_dir(cache_root: &Path) {
    let root = cache_root.join("_install_root");
    fs::create_dir_all(&root).unwrap();
    let install = fake_install(&root);
    let vpk_sha256 = cache::sha256_file(&install.map_vpk("de_test")).unwrap();
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
            map: "de_test".to_string(),
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

fn router_over(cache_root: &Path) -> Router {
    let config_path = cache_root.join("_config").join("config.json");
    fs::create_dir_all(cache_root.join("_config")).unwrap();
    let viewer_dir = cache_root.join("_viewer_empty");
    fs::create_dir_all(&viewer_dir).unwrap();
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root.to_path_buf(),
        viewer_dir,
    ));
    server::routes::router(state)
}

/// Like `router_over`, but with the progress-stream caps overridden - used to exercise
/// truncation without a real 200k-point sweep.
fn router_over_with_limits(
    cache_root: &Path,
    max_stream_points: usize,
    max_points_per_line: usize,
) -> Router {
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
    state.max_stream_points = max_stream_points;
    state.max_points_per_line = max_points_per_line;
    server::routes::router(Arc::new(state))
}

async fn post_lineup(router: &Router, body: &Value) -> (StatusCode, Vec<u8>) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

fn base_target() -> Value {
    json!({ "map": "de_test", "target": [0.0, 0.0, 0.0], "origin": [0.0, 0.0], "scope": "exact" })
}

fn solve_cache_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("solves")
}

fn count_cache_files(cache_root: &Path) -> usize {
    fs::read_dir(solve_cache_dir(cache_root))
        .map(|read| {
            read.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn validation_errors_are_400_with_expected_text() {
    let cache_root = temp_dir("validate");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "tolerance": 0.5 }),
            "tolerance must be between",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "origin": [0.0,0.0], "originReach": 5.0 }),
            "originReach must be between",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "minStability": 0.0 }),
            "minStability must be between",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "types": [] }),
            "types must be a non-empty array",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "strengths": [0.3] }),
            "strengths must be a non-empty array",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "broken": ["glass", "doors", "glass"] }),
            "broken must be an array",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "scope": "exact" }),
            "scope \"exact\" needs an origin",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "origin": [0.0, 0.0],
                "originArea": [[100.0,100.0],[300.0,100.0],[300.0,300.0],[100.0,300.0]] }),
            "mutually exclusive",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "scope": "spawns",
                "originArea": [[100.0,100.0],[300.0,100.0],[300.0,300.0],[100.0,300.0]] }),
            "mutually exclusive",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "originArea": [[100.0,100.0],[300.0,300.0]] }),
            "between 3 and 64 vertices",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "originArea": [[100.0,100.0],[300.0,100.0],"bad"] }),
            "must be [x,y] with finite numbers",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "originArea": [[0.0,0.0],[9000.0,0.0],[9000.0,100.0],[0.0,100.0]] }),
            "outside the map bounds",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "originArea": [[0.0,0.0],[100.0,0.0],[200.0,0.0]] }),
            "non-zero area",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "originArea": [[100.0,100.0],[300.0,100.0],[300.0,300.0],[100.0,300.0]],
                "zMin": 100.0, "zMax": 0.0 }),
            "zMin must not be greater than zMax",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0],
                "targetArea": [[100.0,100.0],[300.0,100.0],[300.0,300.0],[100.0,300.0]] }),
            "mutually exclusive",
        ),
        (
            json!({ "map": "de_test",
                "targetArea": [[100.0,100.0],[300.0,300.0]] }),
            "between 3 and 64 vertices",
        ),
        (
            json!({ "map": "de_test",
                "targetArea": [[100.0,100.0],[300.0,100.0],"bad"] }),
            "must be [x,y] with finite numbers",
        ),
        (
            json!({ "map": "de_test",
                "targetArea": [[0.0,0.0],[9000.0,0.0],[9000.0,100.0],[0.0,100.0]] }),
            "outside the map bounds",
        ),
        (
            json!({ "map": "de_test",
                "targetArea": [[0.0,0.0],[100.0,0.0],[200.0,0.0]] }),
            "non-zero area",
        ),
        (
            json!({ "map": "de_test",
                "targetArea": [[100.0,100.0],[300.0,100.0],[300.0,300.0],[100.0,300.0]],
                "targetZMin": 100.0, "targetZMax": 0.0 }),
            "targetZMin must not be greater than targetZMax",
        ),
        (
            json!({ "map": "de_test", "target": [0.0, 0.0], "targetZMin": "bad" }),
            "targetZMin must be a finite number",
        ),
    ];
    for (body, expected) in cases {
        let (status, bytes) = post_lineup(&router, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value["error"].as_str().unwrap().contains(expected),
            "{body}: got {value}"
        );
    }
}

/// A target far below the mesh used to abort the whole process (negative voxel-grid `nz`
/// wrapping the cell-count product); it must now be a plain 400.
#[tokio::test]
async fn target_far_below_mesh_z_is_400() {
    let cache_root = temp_dir("below_z");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);
    let (status, bytes) = post_lineup(
        &router,
        &json!({ "map": "de_test", "target": [0.0, 0.0, -1400.0] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("target z is outside the map bounds"),
        "{value}"
    );
}

/// A target just inside the widened z range (mesh z is 0 on the flat floor, margin is 512)
/// still solves normally, not rejected by the z check.
#[tokio::test]
async fn target_just_inside_the_z_margin_still_solves() {
    let cache_root = temp_dir("inside_z");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);
    let (status, bytes) = post_lineup(
        &router,
        &json!({ "map": "de_test", "target": [0.0, 0.0, -500.0] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let last_line = text.lines().rfind(|l| !l.is_empty()).unwrap();
    let last: Value = serde_json::from_str(last_line).unwrap();
    assert!(last.get("result").is_some(), "solve failed: {last}");
}

#[tokio::test]
async fn oversized_non_json_and_wrong_content_type() {
    let cache_root = temp_dir("bad_bodies");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let big = "x".repeat(8 * 1024);
    let (status, _) = post_lineup(&router, &json!({ "map": "de_test", "pad": big })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from(base_target().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn unknown_map_is_404() {
    let cache_root = temp_dir("unknown");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);
    let (status, bytes) =
        post_lineup(&router, &json!({ "map": "nope", "target": [0.0, 0.0] })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"], json!("unknown map (see /api/maps)"));
}

#[tokio::test]
async fn non_smoke_grenade_is_501() {
    let cache_root = temp_dir("flash");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);
    let mut body = base_target();
    body["grenade"] = json!("flash");
    let (status, bytes) = post_lineup(&router, &body).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["error"].as_str().unwrap().contains("smoke"));
}

/// Every line is valid JSON; the first is a `phase`, the last is `result`, and everything between
/// is a `phase`/`checked`/`verified`/`progress-truncated` line (all numbers finite).
fn assert_well_formed_ndjson(bytes: &[u8]) -> Value {
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.len() >= 2,
        "expected at least a phase and a result line, got {lines:?}"
    );
    let mut saw_checked = false;
    let mut saw_verified = false;
    for line in &lines {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert!(
            v.get("phase").is_some()
                || v.get("result").is_some()
                || v.get("error").is_some()
                || v.get("checked").is_some()
                || v.get("verified").is_some(),
            "unexpected line shape: {line}"
        );
        if let Some(points) = v.get("checked").and_then(Value::as_array) {
            saw_checked = true;
            assert_finite_points(points, line);
        }
        if let Some(points) = v.get("verified").and_then(Value::as_array) {
            saw_verified = true;
            assert_finite_points(points, line);
        }
    }
    assert!(saw_checked, "expected at least one checked line");
    assert!(saw_verified, "expected at least one verified line");
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first.get("phase").is_some(), "first line was {first}");
    let last: Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
    assert!(
        last.get("result").is_some() || last.get("error").is_some(),
        "last line was {last}"
    );
    // Every `checked`/`verified` (and `phase`) line comes before `result`/`error`.
    let last_idx = lines.len() - 1;
    for (i, line) in lines.iter().enumerate().take(last_idx) {
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(
            v.get("result").is_none() && v.get("error").is_none(),
            "line {i} was a result/error before the last line: {line}"
        );
    }
    last
}

fn assert_finite_points(points: &[Value], line: &str) {
    for p in points {
        let arr = p
            .as_array()
            .unwrap_or_else(|| panic!("{line}: not an array"));
        assert_eq!(arr.len(), 4, "{line}: expected [x,y,z,n]");
        for n in arr {
            let f = n.as_f64().unwrap_or_else(|| panic!("{line}: not a number"));
            assert!(f.is_finite(), "{line}: non-finite number");
        }
    }
}

#[tokio::test]
async fn successful_stream_then_cache_hit_then_distinct_key_on_tolerance() {
    let cache_root = temp_dir("stream");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let mut body = base_target();
    body["tolerance"] = json!(300.0);
    let (status, bytes) = post_lineup(&router, &body).await;
    assert_eq!(status, StatusCode::OK);
    let last = assert_well_formed_ndjson(&bytes);
    assert!(last.get("result").is_some(), "solve failed: {last}");
    let result = &last["result"];
    assert!(
        result["lineups"].as_array().is_some_and(|a| !a.is_empty()),
        "expected at least one lineup on an open flat floor: {result}"
    );
    assert_eq!(count_cache_files(&cache_root), 1);

    // The identical query again answers with one line (the cache hit) and the same content.
    let (status2, bytes2) = post_lineup(&router, &body).await;
    assert_eq!(status2, StatusCode::OK);
    let text2 = String::from_utf8(bytes2.to_vec()).unwrap();
    let lines2: Vec<&str> = text2.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines2.len(),
        1,
        "cache hit must be a single line: {lines2:?}"
    );
    let cached: Value = serde_json::from_str(lines2[0]).unwrap();
    assert_eq!(cached["result"], *result);

    // Same query except `tolerance` -> a different cache file.
    let mut other = body.clone();
    other["tolerance"] = json!(260.0);
    let (status3, bytes3) = post_lineup(&router, &other).await;
    assert_eq!(status3, StatusCode::OK);
    assert_well_formed_ndjson(&bytes3);
    assert_eq!(count_cache_files(&cache_root), 2);
}

/// `s6e_progress_jobs.md`'s memory-bound requirement: even a slow reader cannot make the
/// `checked`/`verified` queue grow without limit, because the producer side (`solve.rs`'s
/// `on_origin`/`on_candidate`) itself stops sending past a fixed per-solve point budget and a
/// fixed per-line cap - checked here with both caps set to something a synthetic map's own solve
/// actually exceeds (10 points total, 4 per line), so the test would fail if either cap were
/// removed. The truncated run's `result` still matches an unbounded run's, proving the caps never
/// touch the solve itself.
#[tokio::test]
async fn progress_point_stream_is_bounded() {
    let mut body = base_target();
    body["tolerance"] = json!(300.0);

    let unbounded_cache_root = temp_dir("bounded_reference");
    sample_cache_dir(&unbounded_cache_root);
    let unbounded_router = router_over(&unbounded_cache_root);
    let (status, unbounded_bytes) = post_lineup(&unbounded_router, &body).await;
    assert_eq!(status, StatusCode::OK);
    let unbounded_last = assert_well_formed_ndjson(&unbounded_bytes);
    let unbounded_lineups = &unbounded_last["result"]["lineups"];

    let cache_root = temp_dir("bounded");
    sample_cache_dir(&cache_root);
    let router = router_over_with_limits(&cache_root, 10, 4);
    let (status, bytes) = post_lineup(&router, &body).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes.to_vec()).unwrap();

    let mut total_points = 0usize;
    let mut truncated_lines = 0usize;
    let mut result_lineups = None;
    for line in text.lines().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_str(line).unwrap();
        if let Some(points) = v.get("checked").and_then(Value::as_array) {
            assert!(points.len() <= 4, "{line}: more than 4 points in one line");
            total_points += points.len();
        }
        if let Some(points) = v.get("verified").and_then(Value::as_array) {
            assert!(points.len() <= 4, "{line}: more than 4 points in one line");
            total_points += points.len();
        }
        if v.get("phase").and_then(Value::as_str) == Some("progress-truncated") {
            truncated_lines += 1;
        }
        if let Some(result) = v.get("result") {
            result_lineups = Some(result["lineups"].clone());
        }
    }
    assert!(total_points > 0, "expected at least one progress point");
    assert!(
        total_points <= 10,
        "progress stream sent {total_points} points, over the per-solve budget"
    );
    assert_eq!(
        truncated_lines, 1,
        "expected exactly one progress-truncated line"
    );
    assert_eq!(
        result_lineups.expect("expected a result line"),
        *unbounded_lineups,
        "truncating progress points must not change the solved result"
    );
}

#[tokio::test]
async fn client_disconnect_does_not_write_the_cache() {
    let cache_root = temp_dir("cancel");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    // First run the same query to completion and time it: a fixed short sleep (as this test used
    // to do) stays green even if `cancel` is never armed, as long as the solve happens to still be
    // running when the sleep ends - it proves nothing about cancellation actually working. Timing
    // an uncancelled run of the same shape gives a wait (2x that) long enough that, if the second
    // run's cancellation did *not* stop the solve, it would have finished and cached by then.
    let timing_body = base_target();
    let start = std::time::Instant::now();
    let (status, _) = post_lineup(&router, &timing_body).await;
    assert_eq!(status, StatusCode::OK);
    let t = start.elapsed().max(Duration::from_millis(20));
    assert_eq!(count_cache_files(&cache_root), 1);

    // Same shape, different `tolerance` so this can't just be served from the warm-up's cache
    // entry.
    let mut cancel_body = base_target();
    cancel_body["tolerance"] = json!(81.0);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(cancel_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drop the response (and its streaming body) without reading it - the equivalent of a client
    // closing the connection before the solve finished.
    drop(resp);
    tokio::time::sleep(t * 2).await;

    assert_eq!(
        count_cache_files(&cache_root),
        1,
        "a cancelled solve must not write a cache file (only the warm-up's should exist)"
    );
}

/// `s6g_origin_area.md`: `originArea` must actually cut down which stand spots the solve
/// considers - a polygon covering a quarter of the nav area finds fewer `origins` than an
/// unrestricted solve over the same map, and a distinct cache key from it (`de_test`'s nav area
/// spans -600..600 on both axes).
#[tokio::test]
async fn origin_area_limits_candidate_origins() {
    let cache_root = temp_dir("origin_area");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let unrestricted = json!({ "map": "de_test", "target": [0.0, 0.0, 0.0] });
    let (status, bytes) = post_lineup(&router, &unrestricted).await;
    assert_eq!(status, StatusCode::OK);
    let last = assert_well_formed_ndjson(&bytes);
    let unrestricted_origins = last["result"]["origins"].as_u64().unwrap();
    assert!(unrestricted_origins > 0, "{last}");

    let area = json!({ "map": "de_test", "target": [0.0, 0.0, 0.0],
        "originArea": [[0.0,0.0],[300.0,0.0],[300.0,300.0],[0.0,300.0]] });
    let (status2, bytes2) = post_lineup(&router, &area).await;
    assert_eq!(status2, StatusCode::OK);
    let last2 = assert_well_formed_ndjson(&bytes2);
    let area_origins = last2["result"]["origins"].as_u64().unwrap();
    assert!(
        area_origins > 0 && area_origins < unrestricted_origins,
        "expected 0 < area_origins ({area_origins}) < unrestricted_origins ({unrestricted_origins})"
    );

    assert_eq!(
        count_cache_files(&cache_root),
        2,
        "the area query must not collide with the unrestricted one on cache key"
    );
}

/// Even-odd point-in-polygon, mirroring `solver::target::point_in_area_polygon` for this test's
/// own containment checks (kept independent so a bug in the server's `insideTargetArea` flag
/// can't hide behind reusing the exact same implementation it is meant to check).
fn point_in_polygon(polygon: &[[f64; 2]], x: f64, y: f64) -> bool {
    let n = polygon.len();
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (polygon[i][0], polygon[i][1]);
        let (xj, yj) = (polygon[j][0], polygon[j][1]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// `s6g2_target_area.md`: `targetArea` must actually restrict where a lineup's grenade comes to
/// rest, every returned lineup's `insideTargetArea` flag must read `true`, and the cache key must
/// not collide with an equivalent point-target query.
#[tokio::test]
async fn target_area_restricts_rest_points_and_flags_them() {
    let cache_root = temp_dir("target_area");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let point = json!({ "map": "de_test", "target": [200.0, 200.0, 0.0], "tolerance": 80.0 });
    let (status, bytes) = post_lineup(&router, &point).await;
    assert_eq!(status, StatusCode::OK);
    let last = assert_well_formed_ndjson(&bytes);
    let point_lineups = last["result"]["lineups"].as_array().unwrap();
    assert!(!point_lineups.is_empty(), "{last}");
    assert!(
        point_lineups[0].get("insideTargetArea").is_none(),
        "a point-target solve must not carry insideTargetArea at all: {}",
        point_lineups[0]
    );

    let polygon = [
        [100.0, 100.0],
        [300.0, 100.0],
        [300.0, 300.0],
        [100.0, 300.0],
    ];
    let area = json!({ "map": "de_test", "targetArea": polygon });
    let (status2, bytes2) = post_lineup(&router, &area).await;
    assert_eq!(status2, StatusCode::OK);
    let last2 = assert_well_formed_ndjson(&bytes2);
    let result2 = &last2["result"];
    let area_lineups = result2["lineups"].as_array().unwrap();
    assert!(!area_lineups.is_empty(), "{result2}");
    for l in area_lineups {
        assert_eq!(l["insideTargetArea"], json!(true), "{l}");
        let rest = l["rest"].as_array().unwrap();
        let (rx, ry) = (rest[0].as_f64().unwrap(), rest[1].as_f64().unwrap());
        assert!(
            point_in_polygon(&polygon, rx, ry),
            "rest point ({rx},{ry}) must be inside the polygon: {l}"
        );
    }

    assert_eq!(
        count_cache_files(&cache_root),
        2,
        "the target-area query must not collide with the point-target one on cache key"
    );
}

/// `s6g2_target_area.md`'s check 3: an origin area and a target area at once - both constraints
/// hold simultaneously (every origin inside the throw area, every rest point inside the landing
/// area).
#[tokio::test]
async fn origin_area_and_target_area_combine() {
    let cache_root = temp_dir("both_areas");
    sample_cache_dir(&cache_root);
    let router = router_over(&cache_root);

    let origin_polygon = [
        [-500.0, -500.0],
        [-200.0, -500.0],
        [-200.0, -200.0],
        [-500.0, -200.0],
    ];
    let target_polygon = [
        [100.0, 100.0],
        [300.0, 100.0],
        [300.0, 300.0],
        [100.0, 300.0],
    ];
    let body = json!({
        "map": "de_test",
        "originArea": origin_polygon,
        "targetArea": target_polygon,
    });
    let (status, bytes) = post_lineup(&router, &body).await;
    assert_eq!(status, StatusCode::OK);
    let last = assert_well_formed_ndjson(&bytes);
    let result = &last["result"];
    let lineups = result["lineups"].as_array().unwrap();
    assert!(!lineups.is_empty(), "{result}");
    for l in lineups {
        assert_eq!(l["insideTargetArea"], json!(true), "{l}");
        let rest = l["rest"].as_array().unwrap();
        assert!(
            point_in_polygon(
                &target_polygon,
                rest[0].as_f64().unwrap(),
                rest[1].as_f64().unwrap()
            ),
            "{l}"
        );
        let feet = l["feet"].as_array().unwrap();
        assert!(
            point_in_polygon(
                &origin_polygon,
                feet[0].as_f64().unwrap(),
                feet[1].as_f64().unwrap()
            ),
            "{l}"
        );
    }
}
