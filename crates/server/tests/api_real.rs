//! Real de_mirage smoke tests over the router (`tower::ServiceExt::oneshot`, no network port);
//! needs `CS2_GAME_DIR` (to resolve the current build) plus a pre-populated `cache/` for
//! de_mirage (`cs2mod extract de_mirage`). `s6c_server.md`'s "real, ignored" test list.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use extract::cache;
use extract::game::GameInstall;
use server::AppState;
use server::config::AppConfig;

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

fn router() -> axum::Router {
    // Confirms de_mirage is actually cached for the current build before handing the registry a
    // cache root that has it (`find_cached` re-hashes the map VPK against `CS2_GAME_DIR`).
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    cache::find_cached(&cache_root(), &install, "de_mirage")
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for de_mirage; run `cs2mod extract de_mirage` first"));

    let config_path = std::env::temp_dir().join(format!(
        "cs2mod-server-real-test-config-{}.json",
        std::process::id()
    ));
    let viewer_dir = std::env::temp_dir().join(format!(
        "cs2mod-server-real-test-viewer-{}",
        std::process::id()
    ));
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root(),
        viewer_dir,
    ));
    server::routes::router(state)
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR and a de_mirage cache entry"]
async fn maps_lists_de_mirage() {
    let router = router();
    let (status, _headers, body) = get(&router, "/api/maps").await;
    assert_eq!(status, StatusCode::OK);
    let maps: Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = maps
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["map"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"de_mirage"), "{names:?}");
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR and a de_mirage cache entry"]
async fn mesh_payload_size_matches_triangle_groups() {
    let router = router();
    let (status, _headers, body) = get(&router, "/api/mesh?map=de_mirage").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[0..4], b"SM3D");
    let vertex_count = i32::from_le_bytes(body[8..12].try_into().unwrap());
    let world_count = i32::from_le_bytes(body[12..16].try_into().unwrap());
    let phantom_count = i32::from_le_bytes(body[16..20].try_into().unwrap());
    let door_count = i32::from_le_bytes(body[20..24].try_into().unwrap());
    let breakable_count = i32::from_le_bytes(body[24..28].try_into().unwrap());
    assert!(vertex_count > 0);
    let total_indices = world_count + phantom_count + door_count + breakable_count;
    assert!(total_indices > 0);
    assert_eq!(total_indices % 3, 0, "index groups must be whole triangles");
    let expected_len = 28 + vertex_count as usize * 12 + total_indices as usize * 4;
    assert_eq!(body.len(), expected_len);
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR and a de_mirage cache entry"]
async fn levels_at_a_known_mid_point_returns_at_least_one() {
    let router = router();
    // Mid, from the target-solver parity corpus's query `c` (`crates/solver/tests/target_real.rs`).
    let (status, _headers, body) =
        get(&router, "/api/levels?map=de_mirage&x=-662.5&y=-1612.5").await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    let levels = value["levels"].as_array().unwrap();
    assert!(!levels.is_empty());
}
