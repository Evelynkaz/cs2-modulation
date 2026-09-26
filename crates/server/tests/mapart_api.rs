//! `/api/overview` and `/api/mapart` validation, `oneshot`'d against an unconfigured (no game
//! directory) `AppState` - no network port opened (`s6p_map_art.md` change item 4: "route tests
//! for validation (bad map/section -> 400)"). Game-gated success-path tests live in
//! `mapart_real.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

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
        "cs2mod_server_mapart_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
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

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
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

#[tokio::test]
async fn overview_missing_map_is_400() {
    let cache_root = temp_dir("overview_missing_map");
    let router = router_over(&cache_root);
    let (status, body) = get(&router, "/api/overview").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], Value::String("map is required".to_string()));
}

#[tokio::test]
async fn overview_bad_map_is_400() {
    let cache_root = temp_dir("overview_bad_map");
    let router = router_over(&cache_root);
    for uri in [
        "/api/overview?map=..%2fetc",
        "/api/overview?map=de%20nuke",
        "/api/overview?map=",
    ] {
        let (status, _body) = get(&router, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn overview_without_game_dir_configured_is_400() {
    let cache_root = temp_dir("overview_no_game_dir");
    let router = router_over(&cache_root);
    let (status, body) = get(&router, "/api/overview?map=de_mirage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no game directory configured"),
        "{body}"
    );
}

#[tokio::test]
async fn mapart_bad_kind_is_400() {
    let cache_root = temp_dir("mapart_bad_kind");
    let router = router_over(&cache_root);
    let (status, _body) = get(&router, "/api/mapart?map=de_mirage&kind=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn mapart_bad_section_is_400() {
    let cache_root = temp_dir("mapart_bad_section");
    let router = router_over(&cache_root);
    for uri in [
        "/api/mapart?map=de_mirage&kind=radar&section=..%2f..%2fetc",
        "/api/mapart?map=de_mirage&kind=radar&section=lower%2f..",
    ] {
        let (status, _body) = get(&router, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn mapart_bad_map_is_400() {
    let cache_root = temp_dir("mapart_bad_map");
    let router = router_over(&cache_root);
    let (status, _body) = get(&router, "/api/mapart?map=..%2fetc&kind=radar").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn mapart_without_game_dir_configured_is_400() {
    let cache_root = temp_dir("mapart_no_game_dir");
    let router = router_over(&cache_root);
    let (status, body) = get(&router, "/api/mapart?map=de_mirage&kind=radar").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no game directory configured"),
        "{body}"
    );
}
