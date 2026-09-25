//! HTTP routing: config, read-only map data, and static hosting (`viewer/`, the radar PNG).
//! Ported in spirit from `ServeCommand.cs`'s route table and `StaticAssetServer.cs`; every
//! citation below names the exact lines this endpoint's behaviour comes from.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, ReadBuf};

use geom::collider::Collider;
use geom::math::V3;

use crate::AppState;
use crate::config::{self, AppConfig};
use crate::jobs;
use crate::physics;
use crate::registry::{MapEntry, RegistryError};
use crate::solve;

/// `ServeCommand.cs:244`: the exact text every "no such map" 404 uses.
pub(crate) const UNKNOWN_MAP_ERROR: &str = "unknown map (see /api/maps)";
/// `/api/config`'s `PUT` body: a settings patch, not a solve query, so this is generous but not
/// unbounded.
const MAX_CONFIG_BODY: usize = 4 * 1024;

/// `LineupSolver.Origins.cs` constants named in `ServeCommand.cs:230-240`, reused by
/// `/api/levels`.
const LEVEL_SEPARATION: f32 = 128.0;
const LEVEL_FLOOR_CLEARANCE: f32 = 8.0;
const LEVEL_HEADROOM: f32 = 64.0;
const LEVEL_SNAP_UP: f32 = 32.0;
const LEVEL_SNAP_DOWN: f32 = 32.0;
const LEVEL_NAME_RADIUS: f32 = 900.0;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/config",
            get(get_config)
                .put(put_config)
                .layer(DefaultBodyLimit::max(MAX_CONFIG_BODY))
                .layer(middleware::from_fn(config_body_limit_error)),
        )
        .route("/api/maps", get(get_maps))
        .route("/api/spawns", get(get_spawns))
        .route("/api/callouts", get(get_callouts))
        .route("/api/levels", get(get_levels))
        .route("/api/mesh", get(get_mesh))
        .route("/api/radar", get(get_radar))
        .route(
            "/api/lineup",
            post(post_lineup)
                .layer(DefaultBodyLimit::max(solve::MAX_LINEUP_BODY))
                .layer(middleware::from_fn(lineup_body_limit_error)),
        )
        .route("/api/trajectory", get(physics::get_trajectory))
        .route("/api/lineup-one", get(physics::get_lineup_one))
        .route("/api/slack", get(physics::get_slack))
        .route("/api/smoke", get(physics::get_smoke))
        .route("/api/jobs", get(jobs::get_jobs))
        .route(
            "/api/jobs/{id}",
            get(jobs::get_job).delete(jobs::delete_job),
        )
        .route("/api/jobs/extract", post(jobs::post_extract))
        .route("/api/jobs/standspots", post(jobs::post_standspots))
        .route("/api/jobs/viewerdata", post(jobs::post_viewerdata))
        .route("/data/maps/{map}/viewer-map.png", get(get_radar_png))
        .route(
            "/data/maps/{map}/render_tex/{file}",
            get(get_render_tex_asset),
        )
        .route("/data/maps/{map}/{file}", get(get_render_asset))
        .route("/", get(get_index))
        .route("/viewer/{*rest}", get(get_viewer_asset))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// `ServeCommand.cs:444-455`: baseline hardening headers on every response.
async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    headers.insert("Referrer-Policy", HeaderValue::from_static("same-origin"));
    resp
}

/// `DefaultBodyLimit::max(MAX_CONFIG_BODY)` rejects an oversized `/api/config` body before the
/// handler ever runs, with axum's own plain-text rejection body; this rewrites that into our
/// `{"error": ...}` shape.
async fn config_body_limit_error(req: axum::extract::Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    resp
}

/// Same rewrite as `config_body_limit_error`, but `s6d_solve_api.md` wants `POST /api/lineup`'s
/// oversized body to read 400, not axum's default 413.
async fn lineup_body_limit_error(req: axum::extract::Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return api_error(StatusCode::BAD_REQUEST, "request body too large");
    }
    resp
}

pub(crate) fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub(crate) fn unknown_map_error() -> Response {
    api_error(StatusCode::NOT_FOUND, UNKNOWN_MAP_ERROR)
}

fn registry_error_response(e: RegistryError) -> Response {
    match e {
        RegistryError::UnknownMap => unknown_map_error(),
        other => api_error(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

/// This run's game directory: the CLI's `--game` override if given, else the persisted config's,
/// as used to pick the current build (`registry::MapRegistry`) and to compute `configured`.
pub(crate) fn effective_game_dir(state: &AppState) -> Option<PathBuf> {
    state
        .game_override
        .clone()
        .or_else(|| state.config.lock().unwrap().game_dir.clone())
}

pub(crate) fn get_entry(state: &AppState, map: &str) -> Result<Arc<MapEntry>, Box<Response>> {
    let game_dir = effective_game_dir(state);
    state
        .registry
        .get(map, game_dir.as_deref())
        .map_err(|e| Box::new(registry_error_response(e)))
}

// ---- /api/lineup ---------------------------------------------------------------------------------

/// Whether `<cache>/maps` has at least one subdirectory - a cheap stand-in for "some map is
/// extracted" that does not touch the VPKs (unlike `MapRegistry::maps`, which may still have to
/// sha256 an unmemoized cached map's VPK to build its list).
fn has_extracted_maps(state: &AppState) -> bool {
    let Ok(read) = std::fs::read_dir(state.registry.cache_root().join("maps")) else {
        return false;
    };
    read.flatten().any(|e| e.path().is_dir())
}

/// Everything before the NDJSON stream starts (`s6d_solve_api.md`'s pre-stream status codes):
/// no maps extracted yet, wrong content type, an oversized/non-JSON body (the size cap is the
/// `DefaultBodyLimit` layer above), an unknown map or one without nav data. `solve::post_lineup`
/// takes over from validation onward.
async fn post_lineup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let game_dir = effective_game_dir(&state);
    // The cheap check covers the common case (some map is extracted); only fall back to the
    // expensive, VPK-hashing `maps()` when it says there is nothing, so a request that is about
    // to be served entirely from the solve cache never pays for it.
    if !has_extracted_maps(&state) && state.registry.maps(game_dir.as_deref()).is_empty() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no maps extracted yet - run `cs2mod extract <map>` first",
        );
    }
    let content_type_ok = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/json"));
    if !content_type_ok {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        );
    }
    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "body must be valid JSON"),
    };
    let Some(map) = body_json.get("map").and_then(|v| v.as_str()) else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    solve::post_lineup(state, entry, body_json).await
}

#[derive(Debug, Deserialize)]
struct MapQuery {
    map: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LevelsQuery {
    map: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

pub(crate) fn parse_finite(s: Option<&str>) -> Option<f32> {
    let v: f32 = s?.trim().parse().ok()?;
    v.is_finite().then_some(v)
}

// ---- /api/config -----------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResponse {
    configured: bool,
    game_dir: Option<String>,
    game_dir_adjusted: bool,
    cache_dir: Option<String>,
    port: u16,
    last_map: Option<String>,
    theme: Option<String>,
    game_build: Option<String>,
    maps: Vec<crate::registry::MapSummary>,
    /// True when `cache_dir` (as saved) no longer matches the cache directory the live registry
    /// was started with - it takes a restart of `cs2mod serve` to pick up the new one.
    restart_required: bool,
}

fn config_response(state: &AppState, cfg: &AppConfig) -> Response {
    let effective_game_dir = state.game_override.clone().or_else(|| cfg.game_dir.clone());
    let (configured, game_build, game_dir_adjusted) = match &effective_game_dir {
        Some(dir) => match config::validate_game_dir(dir) {
            Ok(info) => (true, Some(info.build), info.adjusted),
            Err(_) => (false, None, false),
        },
        None => (false, None, false),
    };
    let live_cache_root = state.registry.cache_root();
    let cache_dir_response = cfg
        .cache_dir
        .clone()
        .unwrap_or_else(|| live_cache_root.to_path_buf());
    let restart_required = cfg
        .cache_dir
        .as_deref()
        .is_some_and(|d| d != live_cache_root);
    let port = state.port_override.unwrap_or(cfg.port);
    Json(ConfigResponse {
        configured,
        game_dir: effective_game_dir.as_ref().map(|p| p.display().to_string()),
        game_dir_adjusted,
        cache_dir: Some(cache_dir_response.display().to_string()),
        port,
        last_map: cfg.last_map.clone(),
        theme: cfg.theme.clone(),
        game_build,
        maps: state.registry.maps(effective_game_dir.as_deref()),
        restart_required,
    })
    .into_response()
}

async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config.lock().unwrap().clone();
    config_response(&state, &cfg)
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConfigPatch {
    game_dir: Option<String>,
    cache_dir: Option<String>,
    port: Option<u16>,
    last_map: Option<String>,
    theme: Option<String>,
}

async fn put_config(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    if body.len() > MAX_CONFIG_BODY {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let patch: ConfigPatch = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "body must be valid JSON"),
    };

    let mut cfg = state.config.lock().unwrap().clone();
    if let Some(game_dir) = &patch.game_dir {
        match config::validate_game_dir(Path::new(game_dir)) {
            Ok(info) => cfg.game_dir = Some(info.csgo_dir),
            Err(message) => return api_error(StatusCode::BAD_REQUEST, message),
        }
    }
    if let Some(cache_dir) = &patch.cache_dir {
        let path = PathBuf::from(cache_dir);
        if !path.is_dir() {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("{} is not an existing directory", path.display()),
            );
        }
        cfg.cache_dir = Some(path);
    }
    if let Some(port) = patch.port {
        cfg.port = port;
    }
    if let Some(last_map) = &patch.last_map {
        cfg.last_map = Some(last_map.clone());
    }
    if let Some(theme) = &patch.theme {
        cfg.theme = Some(theme.clone());
    }

    if let Err(e) = config::save_at(&state.config_path, &cfg) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to save config: {e}"),
        );
    }
    *state.config.lock().unwrap() = cfg.clone();
    config_response(&state, &cfg)
}

// ---- /api/maps ---------------------------------------------------------------------------------

async fn get_maps(State(state): State<Arc<AppState>>) -> Response {
    let game_dir = effective_game_dir(&state);
    Json(state.registry.maps(game_dir.as_deref())).into_response()
}

// ---- /api/spawns --------------------------------------------------------------------------------

async fn get_spawns(State(state): State<Arc<AppState>>, Query(q): Query<MapQuery>) -> Response {
    let Some(map) = q.map else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    match entry.grounded_spawns() {
        Ok(s) => Json(json!({ "t": s.0, "ct": s.1 })).into_response(),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("spawn data unreadable - re-extract the map's entities: {e}"),
        ),
    }
}

// ---- /api/callouts ------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct Callout {
    name: String,
    pos: [f32; 3],
    parts: usize,
}

async fn get_callouts(State(state): State<Arc<AppState>>, Query(q): Query<MapQuery>) -> Response {
    let Some(map) = q.map else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    // `ServeCommand.cs:510-527`: grouped case-insensitively, position averaged, count kept.
    let mut groups: std::collections::HashMap<String, (String, f64, f64, f64, usize)> =
        std::collections::HashMap::new();
    for (name, origin) in &entry.places {
        let key = name.to_ascii_lowercase();
        let g = groups
            .entry(key)
            .or_insert_with(|| (name.clone(), 0.0, 0.0, 0.0, 0));
        g.1 += origin[0] as f64;
        g.2 += origin[1] as f64;
        g.3 += origin[2] as f64;
        g.4 += 1;
    }
    let mut callouts: Vec<Callout> = groups
        .into_values()
        .map(|(name, sx, sy, sz, n)| Callout {
            name,
            pos: [
                (sx / n as f64) as f32,
                (sy / n as f64) as f32,
                (sz / n as f64) as f32,
            ],
            parts: n,
        })
        .collect();
    callouts.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    Json(json!({ "callouts": callouts })).into_response()
}

// ---- /api/levels --------------------------------------------------------------------------------

async fn get_levels(State(state): State<Arc<AppState>>, Query(q): Query<LevelsQuery>) -> Response {
    let Some(map) = q.map else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    if entry.bundle.nav_areas.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "map has no nav data (see /api/maps)",
        );
    }
    let (Some(x), Some(y)) = (parse_finite(q.x.as_deref()), parse_finite(q.y.as_deref())) else {
        return api_error(StatusCode::BAD_REQUEST, "non-finite coordinate");
    };

    // `LineupSolver.NavGroundLevels(strict: true)`, `ServeCommand.cs:928-932`.
    let mut levels = solver::nav_ground::nav_ground_levels(
        &entry.bundle.nav_areas,
        x,
        y,
        LEVEL_SEPARATION,
        true,
    );

    // `ServeCommand.cs:940-952`: drop levels with no headroom above them, unless that would
    // leave nothing.
    if levels.len() > 1 {
        let collider = match entry.grenade_collider() {
            Ok(c) => c,
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        let open: Vec<f32> = levels
            .iter()
            .copied()
            .filter(|&z| {
                collider
                    .first_hit_ray(
                        V3::new(x, y, z + LEVEL_FLOOR_CLEARANCE),
                        V3::new(x, y, z + LEVEL_HEADROOM),
                    )
                    .is_none()
            })
            .collect();
        if !open.is_empty() {
            levels = open;
        }
    }

    // `ServeCommand.cs:953-961`: snap each level onto the surface the player hull actually rests
    // on.
    let player_collider = match entry.player_collider() {
        Ok(c) => c,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let levels: Vec<f32> = levels
        .into_iter()
        .map(|z| {
            solver::origins::floor_under_hull(
                player_collider.as_ref(),
                V3::new(x, y, z),
                LEVEL_SNAP_UP,
                LEVEL_SNAP_DOWN,
            )
            .unwrap_or(z)
        })
        .collect();

    // `ServeCommand.cs:962-980`: name each level by the nearest `env_cs_place`, measured at
    // eye height.
    let levels_json: Vec<serde_json::Value> = levels
        .into_iter()
        .map(|z| {
            let best = entry
                .places
                .iter()
                .filter_map(|(name, origin)| {
                    let dx = origin[0] - x;
                    let dy = origin[1] - y;
                    let dz = origin[2] - (z + 64.0);
                    let d = (dx * dx + dy * dy + dz * dz).sqrt();
                    (d < LEVEL_NAME_RADIUS).then_some((name.clone(), d))
                })
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            json!({ "z": z, "name": best.map(|(n, _)| n) })
        })
        .collect();
    Json(json!({ "levels": levels_json })).into_response()
}

// ---- /api/mesh ----------------------------------------------------------------------------------

async fn get_mesh(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MapQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let etag = format!("\"{}\"", entry.etag);
    let matched = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(&etag));
    if matched {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_etag_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let payload = entry.mesh_payload();
    let mut resp = ((*payload).clone()).into_response();
    set_etag_headers(resp.headers_mut(), &etag);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp
}

fn set_etag_headers(headers: &mut HeaderMap, etag: &str) {
    headers.insert(header::ETAG, HeaderValue::from_str(etag).unwrap());
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
}

/// A file's own identity as an ETag - modification time plus size, like
/// `StaticAssetServer.cs:108-112` - not the mesh's content hash, so it actually changes when the
/// file it names does (`viewer-map.json`/`viewer-map.png` aren't `world.cgeo`).
fn file_identity_etag(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let millis = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(format!("\"{millis:x}-{:x}\"", meta.len()))
}

pub(crate) fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(etag))
}

// ---- /api/radar and the radar PNG ----------------------------------------------------------------

async fn get_radar(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MapQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let path = entry.dir.join("viewer-map.json");
    let Some(etag) = file_identity_etag(&path) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "radar not built yet for this map (run `cs2mod viewerdata`)",
        );
    };
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_etag_headers(resp.headers_mut(), &etag);
        return resp;
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut resp = ([(header::CONTENT_TYPE, "application/json")], bytes).into_response();
            set_etag_headers(resp.headers_mut(), &etag);
            resp
        }
        Err(_) => api_error(
            StatusCode::NOT_FOUND,
            "radar not built yet for this map (run `cs2mod viewerdata`)",
        ),
    }
}

async fn get_radar_png(
    State(state): State<Arc<AppState>>,
    AxPath(map): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let path = entry.dir.join("viewer-map.png");
    let Some(etag) = file_identity_etag(&path) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "radar image not built yet for this map (run `cs2mod viewerdata`)",
        );
    };
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_etag_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "radar image not built yet for this map (run `cs2mod viewerdata`)",
            );
        }
    };
    let mut resp = (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        bytes,
    )
        .into_response();
    set_etag_headers(resp.headers_mut(), &etag);
    resp
}

// ---- render*: the textured 3D export (`s6f3b_viewer3d.md` F3b-1a) -------------------------------

/// Whitelists exactly the file names `s6f3b_viewer3d.md` names for the map cache directory's
/// `render*` files - never a path, never anything the CLI's export step didn't itself write.
fn is_render_asset_name(name: &str) -> bool {
    if matches!(name, "render.glb" | "render.json" | "render_sky.glb") {
        return true;
    }
    let Some(rest) = name.strip_prefix("render_") else {
        return false;
    };
    let Some((stem, ext)) = rest.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && matches!(ext, "bin" | "png")
}

fn render_asset_content_type(name: &str) -> &'static str {
    if name.ends_with(".glb") {
        "model/gltf-binary"
    } else if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".png") {
        "image/png"
    } else {
        "application/octet-stream"
    }
}

/// `64 KiB` read chunks for [`FileStream`] - large enough to keep syscall overhead low for a
/// multi-megabyte `render.glb`, small enough not to balloon memory per concurrent download.
const FILE_STREAM_CHUNK: usize = 64 * 1024;

/// Streams an open file as a `Body` without ever holding the whole thing in memory
/// (`s6f3b_viewer3d.md`: "отдача потоком без чтения целиком в память"), the same hand-rolled
/// `Stream` idiom `solve.rs::RxStream` uses over a channel, here over `tokio::fs::File` directly.
struct FileStream {
    file: tokio::fs::File,
    buf: Box<[u8; FILE_STREAM_CHUNK]>,
}

impl futures_core::Stream for FileStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut read_buf = ReadBuf::new(this.buf.as_mut_slice());
        match Pin::new(&mut this.file).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                if n == 0 {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Bytes::copy_from_slice(read_buf.filled()))))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Streams `path` as `content_type`, with the same ETag/304/chunked-read behaviour every
/// `render*` asset uses (`get_render_asset`, `get_render_tex_asset`) - factored out so
/// `s6f3a6_native_tex.md`'s `render_tex/<sha12>.bin` subdirectory doesn't duplicate it.
async fn stream_cache_file(
    path: &std::path::Path,
    content_type: &'static str,
    headers: &HeaderMap,
) -> Response {
    let Some(etag) = file_identity_etag(path) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "render assets not built yet for this map (run `cs2mod export-glb`)",
        );
    };
    if if_none_match_hits(headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_etag_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let file_handle = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "render assets not built yet for this map (run `cs2mod export-glb`)",
            );
        }
    };
    let content_length = file_handle.metadata().await.ok().map(|m| m.len());
    let body = Body::from_stream(FileStream {
        file: file_handle,
        buf: Box::new([0u8; FILE_STREAM_CHUNK]),
    });
    let mut resp = Response::new(body);
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Some(len) = content_length {
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&len.to_string()).unwrap(),
        );
    }
    set_etag_headers(resp.headers_mut(), &etag);
    resp
}

async fn get_render_asset(
    State(state): State<Arc<AppState>>,
    AxPath((map, file)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_render_asset_name(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let path = entry.dir.join(&file);
    stream_cache_file(&path, render_asset_content_type(&file), &headers).await
}

/// Strict whitelist for `render_tex/<sha12>.bin` (`s6f3a6_native_tex.md` change item 1): exactly
/// 12 lowercase hex characters, `.bin` - the file name `native_texture::TextureCatalog::load`
/// itself always writes (first 6 bytes of a SHA-256 digest), never a path.
fn is_render_tex_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".bin") else {
        return false;
    };
    stem.len() == 12
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

async fn get_render_tex_asset(
    State(state): State<Arc<AppState>>,
    AxPath((map, file)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_render_tex_name(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let entry = match get_entry(&state, &map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let path = entry.dir.join("render_tex").join(&file);
    stream_cache_file(&path, "application/octet-stream", &headers).await
}

// ---- static: viewer/ ----------------------------------------------------------------------------

const STUB_INDEX_HTML: &str = "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>cs2-modulation</title></head>\n<body><h1>cs2-modulation</h1>\n<p>The web viewer isn't built yet (see stage 6, part F). The API is up; try \
<code>/api/maps</code>.</p></body></html>\n";

fn stub_index_html() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        STUB_INDEX_HTML,
    )
        .into_response()
}

/// Resolves `relative` under `root`, refusing anything that canonicalizes outside it
/// (`StaticAssetServer.cs:18-28`).
fn resolve_static(root: &Path, relative: &str) -> Option<PathBuf> {
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let joined = root.join(relative);
    let canonical = std::fs::canonicalize(&joined).ok()?;
    if canonical.starts_with(&canonical_root) && canonical.is_file() {
        Some(canonical)
    } else {
        None
    }
}

/// `StaticAssetServer.cs:33-65`: content type by extension, `no-cache` for html/js/css.
fn serve_file(path: &Path) -> Response {
    let Ok(bytes) = std::fs::read(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content_type = match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "json" => "application/json",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => "application/octet-stream",
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    if matches!(ext.as_str(), "html" | "js" | "mjs" | "css") {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    resp
}

async fn get_index(State(state): State<Arc<AppState>>) -> Response {
    match resolve_static(&state.viewer_dir, "index.html") {
        Some(path) => serve_file(&path),
        None => stub_index_html(),
    }
}

async fn get_viewer_asset(
    State(state): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
) -> Response {
    if rest.is_empty() || rest == "index.html" {
        return get_index(State(state)).await;
    }
    match resolve_static(&state.viewer_dir, &rest) {
        Some(path) => serve_file(&path),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
