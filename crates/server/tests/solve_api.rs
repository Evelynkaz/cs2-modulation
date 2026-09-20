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

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cs2mod_server_solve_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
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

/// Writes a complete `de_test` cache directory with a flat floor and one nav area covering it.
fn sample_cache_dir(name: &str) -> PathBuf {
    let root = temp_dir(&format!("{name}_root"));
    let cache_root = temp_dir(&format!("{name}_cache"));
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
    cache::save_extraction(&cache_root, &extraction, false).unwrap();
    cache_root
}

fn router_over(name: &str, cache_root: PathBuf) -> Router {
    let config_path = temp_dir(&format!("{name}_config")).join("config.json");
    let viewer_dir = temp_dir(&format!("{name}_viewer_empty"));
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    server::routes::router(state)
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
    let cache_root = sample_cache_dir("validate");
    let router = router_over("validate", cache_root);

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

#[tokio::test]
async fn oversized_non_json_and_wrong_content_type() {
    let cache_root = sample_cache_dir("bad_bodies");
    let router = router_over("bad_bodies", cache_root);

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
    let cache_root = sample_cache_dir("unknown");
    let router = router_over("unknown", cache_root);
    let (status, bytes) =
        post_lineup(&router, &json!({ "map": "nope", "target": [0.0, 0.0] })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"], json!("unknown map (see /api/maps)"));
}

#[tokio::test]
async fn non_smoke_grenade_is_501() {
    let cache_root = sample_cache_dir("flash");
    let router = router_over("flash", cache_root);
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
    let cache_root = sample_cache_dir("stream");
    let router = router_over("stream", cache_root.clone());

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
/// `on_origin`/`on_candidate`) itself stops sending past a fixed per-solve point budget
/// (`MAX_STREAM_POINTS`, 200_000) - checked here structurally, by counting every point actually
/// emitted on a synthetic map's own (small) solve and asserting it stays far under that budget.
#[tokio::test]
async fn progress_point_stream_is_bounded() {
    let cache_root = sample_cache_dir("bounded");
    let router = router_over("bounded", cache_root);

    let mut body = base_target();
    body["tolerance"] = json!(300.0);
    let (status, bytes) = post_lineup(&router, &body).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut total_points = 0usize;
    for line in text.lines().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_str(line).unwrap();
        if let Some(points) = v.get("checked").and_then(Value::as_array) {
            total_points += points.len();
        }
        if let Some(points) = v.get("verified").and_then(Value::as_array) {
            total_points += points.len();
        }
    }
    assert!(total_points > 0, "expected at least one progress point");
    assert!(
        total_points <= 200_000,
        "progress stream sent {total_points} points, over the per-solve budget"
    );
}

#[tokio::test]
async fn client_disconnect_does_not_write_the_cache() {
    let cache_root = sample_cache_dir("cancel");
    let router = router_over("cancel", cache_root.clone());

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
