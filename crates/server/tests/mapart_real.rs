//! Real-game `/api/overview` and `/api/mapart` tests, `oneshot`'d over the router (no network
//! port); needs `CS2_GAME_DIR` (`s6p_map_art.md` change item 4). Unlike `api_real.rs`, these read
//! straight from pak01 and need no pre-extracted map cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        "cs2mod_server_mapart_real_test_{name}_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn router() -> (axum::Router, TempDir) {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let cache_root = temp_dir("cache");
    let config_path = cache_root.join("_config").join("config.json");
    fs::create_dir_all(cache_root.join("_config")).unwrap();
    let viewer_dir = cache_root.join("_viewer_empty");
    fs::create_dir_all(&viewer_dir).unwrap();
    let state = Arc::new(
        AppState::new(
            config_path,
            AppConfig::default(),
            cache_root.to_path_buf(),
            viewer_dir,
        )
        .with_overrides(Some(PathBuf::from(game_dir)), None, None),
    );
    (server::routes::router(state), cache_root)
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

/// A PNG's `(width, height)` from its `IHDR` chunk, without pulling in a decoder.
fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[1..4], b"PNG", "not a PNG");
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (width, height)
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR"]
async fn overview_parses_de_mirage_and_de_nuke() {
    let (router, _cache) = router();

    let (status, _headers, body) = get(&router, "/api/overview?map=de_mirage").await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["posX"], serde_json::json!(-3230.0));
    assert_eq!(value["posY"], serde_json::json!(1713.0));
    assert_eq!(value["scale"], serde_json::json!(5.0));
    assert_eq!(value["imageSize"], serde_json::json!([1024, 1024]));
    let sections = value["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0]["name"], serde_json::json!("default"));
    assert_eq!(value["points"]["ctSpawn"], serde_json::json!([0.28, 0.70]));
    assert_eq!(value["points"]["tSpawn"], serde_json::json!([0.87, 0.36]));

    let (status, _headers, body) = get(&router, "/api/overview?map=de_nuke").await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["posX"], serde_json::json!(-3453.0));
    assert_eq!(value["posY"], serde_json::json!(2887.0));
    assert_eq!(value["scale"], serde_json::json!(7.0));
    let sections = value["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0]["name"], serde_json::json!("default"));
    assert_eq!(sections[0]["radar"], serde_json::json!("default"));
    assert_eq!(sections[1]["name"], serde_json::json!("lower"));
    assert_eq!(sections[1]["radar"], serde_json::json!("lower"));
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR"]
async fn screenshot_and_both_nuke_radars_decode_to_expected_sizes() {
    let (router, _cache) = router();

    let (status, headers, body) = get(&router, "/api/mapart?map=de_nuke&kind=screenshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(png_dimensions(&body), (1280, 720));

    let (status, _headers, body) = get(&router, "/api/mapart?map=de_nuke&kind=radar").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(png_dimensions(&body), (1024, 1024));

    let (status, _headers, body) =
        get(&router, "/api/mapart?map=de_nuke&kind=radar&section=lower").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(png_dimensions(&body), (1024, 1024));
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR"]
async fn mapart_is_cached_on_disk_and_served_again_identically() {
    let (router, cache) = router();

    let (status, _headers, first) = get(&router, "/api/mapart?map=de_mirage&kind=radar").await;
    assert_eq!(status, StatusCode::OK);
    let png_path = cache.join("art").join("de_mirage").join("radar.png");
    assert!(png_path.is_file());

    let (status, _headers, second) = get(&router, "/api/mapart?map=de_mirage&kind=radar").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second);
}
