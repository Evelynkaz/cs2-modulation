//! Router-level tests, `oneshot`'d against synthetic cache dirs -- no network port opened
//! (`tower::ServiceExt::oneshot`), matching `s6c_server.md`'s test list.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use extract::build::Extraction;
use extract::cache;
use extract::game::GameInstall;
use extract::report::{EntityRecord, ExtractMeta, ExtractReport, NavAreaDump, NavAreasDump};
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
        "cs2mod_server_api_test_{name}_{}",
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

fn square(min: (f32, f32), max: (f32, f32), z: f32) -> Vec<[f32; 3]> {
    vec![
        [min.0, min.1, z],
        [max.0, min.1, z],
        [max.0, max.1, z],
        [min.0, max.1, z],
    ]
}

fn nav_area(id: u32, corners: Vec<[f32; 3]>) -> NavAreaDump {
    NavAreaDump {
        id,
        hull_index: 0,
        attribute_flags: 0,
        corners,
        connections: Vec::new(),
        ladders_above: Vec::new(),
        ladders_below: Vec::new(),
    }
}

fn one_triangle_mesh() -> CollisionMesh {
    let mut mesh = CollisionMesh::new();
    let a = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let o = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldHull,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    mesh.push_triangles(
        &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        &[[0, 1, 2]],
        a,
        |_| SurfaceProperty::NONE,
        o,
    )
    .unwrap();
    mesh
}

/// A large flat floor at `z=0` (two triangles, CCW from above so the normal points +Z) - big
/// enough that a grenade thrown from well above its center bounces and settles without ever
/// leaving the floor's footprint.
fn floor_mesh() -> CollisionMesh {
    let mut mesh = CollisionMesh::new();
    let a = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let o = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldHull,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    mesh.push_triangles(
        &[
            [-5000.0, -5000.0, 0.0],
            [5000.0, -5000.0, 0.0],
            [5000.0, 5000.0, 0.0],
            [-5000.0, 5000.0, 0.0],
        ],
        &[[0, 1, 2], [0, 2, 3]],
        a,
        |_| SurfaceProperty::NONE,
        o,
    )
    .unwrap();
    mesh
}

/// Writes a complete cache directory for `de_test` with the given mesh/nav/entities under
/// `cache_root` (a fake install root nested inside it, cleaned up along with everything else),
/// and returns the map's own cache dir.
fn sample_cache_dir(
    cache_root: &Path,
    mesh: CollisionMesh,
    nav: Option<NavAreasDump>,
    entities: Vec<EntityRecord>,
) -> PathBuf {
    let root = cache_root.join("_install_root");
    fs::create_dir_all(&root).unwrap();
    let install = fake_install(&root);
    let vpk_sha256 = cache::sha256_file(&install.map_vpk("de_test")).unwrap();
    let extraction = Extraction {
        mesh,
        entities,
        nav,
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
async fn config_starts_unconfigured_and_maps_empty_on_empty_cache() {
    let cache_root = temp_dir("empty_cache");
    let router = router_over(&cache_root);

    let (status, body) = get(&router, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], json!(false));
    assert_eq!(body["maps"], json!([]));
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));

    let (status, body) = get(&router, "/api/maps").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn put_config_with_bad_path_400_and_does_not_corrupt_saved_config() {
    let base = temp_dir("put_bad");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_dir = base.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    let state = Arc::new(AppState::new(
        config_path.clone(),
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    // First, a valid PUT (no game dir) that only sets the theme, so there is a saved config to
    // check isn't corrupted afterwards.
    let good = json!({ "theme": "dark" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(good.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(config_path.is_file());
    let saved_before = fs::read_to_string(&config_path).unwrap();

    let bad = json!({ "gameDir": "C:/this/does/not/exist/at/all" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bad.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("does not look like")
    );

    // The bad PUT must not have touched the config file the good PUT wrote.
    let saved_after = fs::read_to_string(&config_path).unwrap();
    assert_eq!(saved_before, saved_after);
}

#[tokio::test]
async fn put_config_with_good_game_dir_200_and_written_to_disk() {
    let base = temp_dir("put_good");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_dir = base.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    let game_root = base.join("game");
    fs::create_dir_all(game_root.join("maps")).unwrap();
    fs::write(game_root.join("pak01_dir.vpk"), b"x").unwrap();
    fs::write(game_root.join("maps").join("de_test.vpk"), b"x").unwrap();
    fs::write(game_root.join("steam.inf"), b"ClientVersion=2000908\n").unwrap();

    let state = Arc::new(AppState::new(
        config_path.clone(),
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    let body = json!({ "gameDir": game_root.display().to_string() });
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["configured"], json!(true));
    assert_eq!(value["gameBuild"], json!("2000908"));
    assert!(config_path.is_file());
}

#[tokio::test]
async fn unknown_map_is_404_everywhere() {
    let cache_root = temp_dir("unknown_map_cache");
    let router = router_over(&cache_root);

    for uri in [
        "/api/spawns?map=nope",
        "/api/callouts?map=nope",
        "/api/levels?map=nope&x=0&y=0",
        "/api/mesh?map=nope",
        "/api/radar?map=nope",
        "/data/maps/nope/viewer-map.png",
    ] {
        let (status, body) = get(&router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["error"], json!("unknown map (see /api/maps)"), "{uri}");
    }
}

#[tokio::test]
async fn mesh_header_and_conditional_304() {
    let cache_root = temp_dir("mesh");
    let _dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/mesh?map=de_test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[0..4], b"SM3D");
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert_eq!(version, 2);
    let vertex_count = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let world_count = i32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let phantom_count = i32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let door_count = i32::from_le_bytes(bytes[20..24].try_into().unwrap());
    let breakable_count = i32::from_le_bytes(bytes[24..28].try_into().unwrap());
    assert_eq!(vertex_count, 3);
    assert_eq!(world_count, 3); // one triangle, all "Default"
    assert_eq!(phantom_count, 0);
    assert_eq!(door_count, 0);
    assert_eq!(breakable_count, 0);
    let expected_len = 28 + vertex_count as usize * 12 + world_count as usize * 4;
    assert_eq!(bytes.len(), expected_len);

    // Repeat with If-None-Match set to the ETag just returned: must come back 304.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/mesh?map=de_test")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn static_traversal_outside_viewer_dir_is_404_not_a_file() {
    let base = temp_dir("static");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_root = base.join("viewer_root");
    let viewer_dir = viewer_root.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    fs::write(viewer_dir.join("index.html"), b"<html>viewer</html>").unwrap();
    fs::write(viewer_root.join("secret.txt"), b"do not serve me").unwrap();

    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    // A real file inside viewer/ serves fine.
    let resp = router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Escaping to a sibling file must not succeed.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/viewer/../secret.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.starts_with(b"do not serve me"));
}

#[tokio::test]
async fn missing_index_html_serves_stub() {
    let cache_root = temp_dir("stub_cache");
    let router = router_over(&cache_root);
    let resp = router
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("part F") || text.contains("6F"));
}

#[tokio::test]
async fn put_config_with_missing_cache_dir_is_400() {
    let base = temp_dir("put_bad_cache_dir");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_dir = base.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    let body = json!({ "cacheDir": "C:/this/does/not/exist/at/all" });
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_config_body_too_large_is_413_with_json_error() {
    let base = temp_dir("put_large");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_dir = base.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    let big_theme = "x".repeat(8 * 1024);
    let body = json!({ "theme": big_theme }).to_string();
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"], json!("request body too large"));
}

#[tokio::test]
async fn security_headers_present_on_json_binary_and_404() {
    let cache_root = temp_dir("headers");
    let _dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    for uri in ["/api/maps", "/api/mesh?map=de_test", "/api/spawns?map=nope"] {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff",
            "{uri}"
        );
        assert_eq!(
            resp.headers().get("X-Frame-Options").unwrap(),
            "DENY",
            "{uri}"
        );
        assert_eq!(
            resp.headers().get("Referrer-Policy").unwrap(),
            "same-origin",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn mesh_mismatched_etag_is_200_and_distinct_meshes_have_distinct_etags() {
    let cache_root = temp_dir("etag_a");
    let _dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/mesh?map=de_test")
                .header(header::IF_NONE_MATCH, "\"not-the-real-tag\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let etag_a = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let mut other_mesh = CollisionMesh::new();
    let a = other_mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let o = other_mesh.add_object(MeshObject {
        kind: ObjectKind::WorldHull,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    other_mesh
        .push_triangles(
            &[[5.0, 5.0, 5.0], [6.0, 5.0, 5.0], [5.0, 6.0, 5.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
    let cache_root_b = temp_dir("etag_b");
    let _dir_b = sample_cache_dir(&cache_root_b, other_mesh, None, Vec::new());
    let router_b = router_over(&cache_root_b);
    let resp_b = router_b
        .oneshot(
            Request::builder()
                .uri("/api/mesh?map=de_test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag_b = resp_b
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(etag_a, etag_b);
}

#[tokio::test]
async fn static_traversal_variants_are_rejected() {
    let base = temp_dir("static2");
    let cache_root = base.join("cache");
    fs::create_dir_all(&cache_root).unwrap();
    let config_path = base.join("config").join("config.json");
    fs::create_dir_all(base.join("config")).unwrap();
    let viewer_root = base.join("viewer_root");
    let viewer_dir = viewer_root.join("viewer");
    fs::create_dir_all(&viewer_dir).unwrap();
    fs::write(viewer_dir.join("index.html"), b"<html>viewer</html>").unwrap();
    // A sibling file whose name has "viewer" as a string prefix - catches a switch from
    // component-wise `Path::starts_with` to a raw string-prefix comparison.
    fs::write(viewer_root.join("viewer-evil.txt"), b"do not serve me").unwrap();

    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root,
        viewer_dir,
    ));
    let router = server::routes::router(state);

    for uri in [
        "/viewer/../viewer-evil.txt",
        "/viewer/..%2fviewer-evil.txt",
        "/viewer/C:/Windows/win.ini",
    ] {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!bytes.starts_with(b"do not serve me"), "{uri}");
    }
}

#[tokio::test]
async fn radar_png_success_path() {
    let cache_root = temp_dir("radarpng");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::write(dir.join("viewer-map.png"), b"fake png bytes").unwrap();
    let router = router_over(&cache_root);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/data/maps/de_test/viewer-map.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get(header::ETAG).is_some());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"fake png bytes");
}

#[tokio::test]
async fn render_asset_success_path_streams_bytes_with_etag_and_304() {
    let cache_root = temp_dir("render_glb");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::write(dir.join("render.glb"), b"fake glb bytes").unwrap();
    let router = router_over(&cache_root);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/data/maps/de_test/render.glb")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "model/gltf-binary"
    );
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"fake glb bytes");

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/data/maps/de_test/render.glb")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn render_asset_name_whitelist_rejects_everything_else() {
    let cache_root = temp_dir("render_whitelist");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::write(dir.join("render.glb"), b"fake glb bytes").unwrap();
    fs::write(dir.join("world.cgeo"), b"do not serve me").unwrap();
    let router = router_over(&cache_root);

    for uri in [
        "/data/maps/de_test/world.cgeo",
        "/data/maps/de_test/render.glb.tmp-1",
        "/data/maps/de_test/render_..bin",
        "/data/maps/de_test/render_%2e%2e.bin",
    ] {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!bytes.starts_with(b"do not serve me"), "{uri}");
    }
}

// `s6f3a6_native_tex.md` change item 1: `render_tex/<sha12>.bin`, a strict `^[0-9a-f]{12}\.bin$`
// whitelist in its own subdirectory (unlike every other `render*` asset, which lives flat in the
// map cache dir).
#[tokio::test]
async fn render_tex_success_path_streams_bytes_with_etag_and_304() {
    let cache_root = temp_dir("render_tex_ok");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::create_dir_all(dir.join("render_tex")).unwrap();
    fs::write(
        dir.join("render_tex").join("0123456789ab.bin"),
        b"fake bc7 blocks",
    )
    .unwrap();
    let router = router_over(&cache_root);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/data/maps/de_test/render_tex/0123456789ab.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"fake bc7 blocks");

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/data/maps/de_test/render_tex/0123456789ab.bin")
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn render_tex_name_whitelist_rejects_everything_else() {
    let cache_root = temp_dir("render_tex_whitelist");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::create_dir_all(dir.join("render_tex")).unwrap();
    fs::write(
        dir.join("render_tex").join("0123456789ab.bin"),
        b"do not serve via a bad name",
    )
    .unwrap();
    // A file directly in the map cache dir (not under render_tex/) that a hostile request might
    // try to reach via `..`, plus the general shape of `is_render_tex_name`'s own rejections.
    fs::write(dir.join("world.cgeo"), b"do not serve me either").unwrap();
    let router = router_over(&cache_root);

    for uri in [
        // Wrong length (11 hex chars, not 12).
        "/data/maps/de_test/render_tex/0123456789a.bin",
        // Uppercase hex is not in `[0-9a-f]`.
        "/data/maps/de_test/render_tex/0123456789AB.bin",
        // Wrong extension.
        "/data/maps/de_test/render_tex/0123456789ab.png",
        // No extension at all.
        "/data/maps/de_test/render_tex/0123456789ab",
        // Path traversal out of the map cache dir.
        "/data/maps/de_test/render_tex/..%2f..%2fworld.cgeo",
        "/data/maps/de_test/render_tex/..%2fworld.cgeo",
    ] {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!bytes.starts_with(b"do not serve"), "{uri}");
    }
}

#[tokio::test]
async fn render_json_reports_missing_render_with_404() {
    let cache_root = temp_dir("render_missing");
    let _dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    let (status, body) = get(&router, "/data/maps/de_test/render.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("render assets not built yet")
    );
}

#[tokio::test]
async fn maps_has_render_and_render_version() {
    let cache_root = temp_dir("render_summary");
    let dir = sample_cache_dir(&cache_root, one_triangle_mesh(), None, Vec::new());
    fs::write(dir.join("render.glb"), b"fake glb bytes").unwrap();
    fs::write(dir.join("render.json"), r#"{"formatVersion":1}"#).unwrap();
    let router = router_over(&cache_root);

    let (status, body) = get(&router, "/api/maps").await;
    assert_eq!(status, StatusCode::OK);
    let maps = body.as_array().unwrap();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0]["hasRender"], json!(true));
    assert_eq!(maps[0]["renderVersion"], json!(1));
}

// `s6f3b_viewer3d.md` F3b-1b: the viewer draws bounce marks from `/api/trajectory`'s own
// `contacts`, not a Z-local-minimum guess - a near-vertical throw onto a flat floor bounces
// (losing 55% of its speed per `ThrowConstants::default().elasticity` each time) several times
// before settling, so `contacts` must be non-empty and agree with `bounces`.
#[tokio::test]
async fn trajectory_contacts_match_recorded_bounces_on_a_flat_floor() {
    let cache_root = temp_dir("traj_contacts");
    let _dir = sample_cache_dir(&cache_root, floor_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    let (status, body) = get(
        &router,
        "/api/trajectory?map=de_test&x=0&y=0&z=200&type=Stand&pitch=80&yaw=0&strength=1&runDeg=0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bounces = body["bounces"].as_u64().expect("bounces");
    assert!(bounces > 0, "expected at least one bounce: {body}");
    let contacts = body["contacts"].as_array().expect("contacts array");
    assert_eq!(contacts.len() as u64, bounces, "{body}");
    for c in contacts {
        let pt = c.as_array().expect("contact triple");
        let cz = pt[2].as_f64().expect("contact z");
        assert!(cz.abs() < 20.0, "contact {pt:?} not on the z=0 floor");
    }
}

// `s6i_render_job_areas3d.md` change item 4: `/api/smoke` used to bounds-check only x/y - a
// finite but wildly out-of-range z fell through to the physics computation below instead of a
// clean 400, unlike `target`'s own z check in `solve.rs`.
#[tokio::test]
async fn smoke_rejects_z_far_outside_the_mesh_bounds() {
    let cache_root = temp_dir("smoke_z_bounds");
    let _dir = sample_cache_dir(&cache_root, floor_mesh(), None, Vec::new());
    let router = router_over(&cache_root);

    let (status, body) = get(&router, "/api/smoke?map=de_test&x=0&y=0&z=5000").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("outside the map bounds"),
        "{body}"
    );
}

#[tokio::test]
async fn levels_two_stacked_floors_bottom_to_top() {
    let nav = NavAreasDump {
        version: 1,
        sub_version: 0,
        generation_hulls: Vec::new(),
        areas: vec![
            nav_area(0, square((-50.0, -50.0), (50.0, 50.0), 0.0)),
            nav_area(1, square((-50.0, -50.0), (50.0, 50.0), 200.0)),
        ],
        ladders: Vec::new(),
    };
    let cache_root = temp_dir("levels");
    let _dir = sample_cache_dir(&cache_root, CollisionMesh::new(), Some(nav), Vec::new());
    let router = router_over(&cache_root);

    let (status, body) = get(&router, "/api/levels?map=de_test&x=0&y=0").await;
    assert_eq!(status, StatusCode::OK);
    let levels = body["levels"].as_array().unwrap();
    assert_eq!(levels.len(), 2);
    assert_eq!(levels[0]["z"].as_f64().unwrap(), 0.0);
    assert_eq!(levels[1]["z"].as_f64().unwrap(), 200.0);
}
