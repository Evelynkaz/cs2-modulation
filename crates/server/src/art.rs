//! Official map art from the game (`s6p_map_art.md`): `GET /api/overview` (the parsed
//! `resource/overviews/<map>.txt`, HLTV's world<->radar-image transform) and `GET /api/mapart`
//! (screenshot/radar PNGs, decoded on demand from pak01's `vtex_c` textures via `s2fmt`'s VPK
//! reader and `s2tex`'s decoder, and cached as plain PNG files).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use extract::game::GameInstall;
use s2fmt::resource::{FourCC, Resource};
use s2fmt::vpk::{Vpk, VpkEntry};

use crate::AppState;
use crate::config;
use crate::kv1;
use crate::routes::{api_error, effective_game_dir};

/// Disambiguates [`write_art_cache`]'s temp file name across concurrent requests in one process:
/// `std::process::id()` alone collides when two requests for different maps/kinds race inside the
/// same server, since both would otherwise share `<stem>.png.tmp-<pid>`.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `ServeCommand.cs`-style message, matching `jobs.rs::start_job`'s wording exactly so the
/// viewer's existing "no game directory" handling recognises this error too.
const NO_GAME_DIR_ERROR: &str =
    "no game directory configured - open the setup page (or PUT /api/config) and set one first";

/// Follow-up security review item 1: every art-decode/hash/parse failure in this file flows
/// through here. The real detail always goes to the server's own stderr, but the HTTP response
/// only repeats it in local mode - `Vpk`/`GameInstall`/`s2fmt`/`s2tex` error `Display`s routinely
/// embed a local filesystem path (reproduced with `--game "...\game"`: `GameInstall::new`'s own
/// "does not look like a CS2 game directory" message names it directly), which must never reach a
/// public visitor.
fn art_internal_error(state: &AppState, detail: impl std::fmt::Display) -> Response {
    eprintln!("map art error: {detail}");
    if state.public {
        api_error(StatusCode::INTERNAL_SERVER_ERROR, "map art unavailable")
    } else {
        api_error(StatusCode::INTERNAL_SERVER_ERROR, detail.to_string())
    }
}

/// `effective_game_dir`'s raw value re-validated/adjusted the same way every other game-dir
/// consumer does (`registry.rs::discover`) - a CLI `--game` override may still be the install root
/// (e.g. `...\game`, not `...\game\csgo`), which `GameInstall::new` alone rejects outright
/// (follow-up security review item 1). An invalid/missing directory reads the same as unconfigured;
/// its own `validate_game_dir` message would otherwise be the one path-bearing 400 in this file.
fn resolve_game_dir(state: &AppState) -> Result<PathBuf, Box<Response>> {
    let Some(game_dir) = effective_game_dir(state) else {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            NO_GAME_DIR_ERROR,
        )));
    };
    config::validate_game_dir(&game_dir)
        .map(|info| info.csgo_dir)
        .map_err(|_| Box::new(api_error(StatusCode::BAD_REQUEST, NO_GAME_DIR_ERROR)))
}

/// `map`/`section` may only be plain alphanumerics/underscore (`s6p_map_art.md` change item 2):
/// enough to rule out path traversal once either is joined into a cache path or a VPK entry name,
/// and matching how every map name and `verticalsections` key pak01 actually uses is spelled.
fn is_safe_ident(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `Box`ed error, matching `routes.rs::get_entry`'s own `Result<_, Box<Response>>` - `Response`
/// itself is large enough to trip `clippy::result_large_err`.
fn pak01_path(state: &AppState, game_dir: &Path) -> Result<PathBuf, Box<Response>> {
    let install = GameInstall::new(game_dir).map_err(|e| Box::new(art_internal_error(state, e)))?;
    Ok(install.csgo_dir.join("pak01_dir.vpk"))
}

/// Follow-up security review round 3, item 2: `get_overview`/`get_mapart`'s shared pak01 handle,
/// reused while `state.pak01_cache`'s stored path and identity still match `path`/`identity` - an
/// unknown map/section name that passes `is_safe_ident` now costs one in-memory index lookup
/// instead of reopening (and re-indexing) the whole VPK per request. Keyed by path too, not
/// identity alone: the game folder can move to another install whose pak01_dir.vpk bytes hash
/// identically, and the cached `Vpk` handle opens its pak01_NNN archives lazily from its own
/// directory, so serving it for a different path would read archives from the wrong install. The
/// lock is held across the open itself too, so several requests racing a cold cache open pak01 once
/// between them, not once each.
fn cached_pak01(state: &AppState, path: &Path, identity: &str) -> Result<Arc<Vpk>, Box<Response>> {
    let mut guard = state.pak01_cache.lock().unwrap();
    if let Some((cached_path, cached_identity, vpk)) = guard.as_ref()
        && cached_path == path
        && cached_identity == identity
    {
        return Ok(vpk.clone());
    }
    let vpk = Vpk::open(path).map_err(|e| {
        Box::new(art_internal_error(
            state,
            format!("failed to open {}: {e}", path.display()),
        ))
    })?;
    state
        .pak01_open_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let vpk = Arc::new(vpk);
    *guard = Some((path.to_path_buf(), identity.to_string(), vpk.clone()));
    Ok(vpk)
}

/// Follow-up security review round 3, item 3 (applied evenly to [`get_overview`] too): in public
/// mode, bounds concurrent map-art computation (pak01 opens/reads, texture decode/encode) with the
/// same `public_physics_semaphore` `physics.rs::spawn_compute_heavy` uses for grenade colliders -
/// reused rather than a dedicated semaphore since both guard the same "how much CPU/disk can one
/// public visitor's request cost" question. The permit is owned and moved into the blocking closure
/// itself (round 3, item 1's fix), so a client disconnect can't free it while the compute keeps
/// running.
async fn spawn_art_compute<F, T>(state: &AppState, compute: F) -> Result<T, Box<Response>>
where
    F: FnOnce() -> Result<T, Box<Response>> + Send + 'static,
    T: Send + 'static,
{
    let permit = if state.public {
        Some(
            state
                .public_physics_semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore never closed"),
        )
    } else {
        None
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        compute()
    })
    .await
    {
        Ok(result) => result,
        Err(e) => Err(Box::new(art_internal_error(
            state,
            format!("map art task panicked: {e}"),
        ))),
    }
}

/// The pak01 entry name for `map`'s radar image: `<m>_radar_psd` for `section == "default"`,
/// `<m>_<section>_radar_psd` otherwise (`s6p_map_art.md` change item 2).
fn radar_entry_path(map: &str, section: &str) -> String {
    if section.eq_ignore_ascii_case("default") {
        format!("panorama/images/overheadmaps/{map}_radar_psd.vtex_c")
    } else {
        format!("panorama/images/overheadmaps/{map}_{section}_radar_psd.vtex_c")
    }
}

/// `map`/`section`'s radar entry: `_radar_psd` ([`radar_entry_path`]) if pak01 has it, else the
/// same name with `_radar_tga` instead - some maps (e.g. `rush_001`) only ship the tga-sourced
/// radar, never a psd-sourced one. Returns the entry found together with the exact path it lives
/// at, so a caller's error message names the file that's actually missing.
fn find_radar_entry<'a>(vpk: &'a Vpk, map: &str, section: &str) -> Option<(&'a VpkEntry, String)> {
    let psd_path = radar_entry_path(map, section);
    if let Some(entry) = vpk.find(&psd_path) {
        return Some((entry, psd_path));
    }
    let tga_path = psd_path.replace("_radar_psd.vtex_c", "_radar_tga.vtex_c");
    vpk.find(&tga_path).map(|entry| (entry, tga_path))
}

/// `<m>_radar_psd.vtex_c`'s (or its `_radar_tga` fallback's, see [`find_radar_entry`]) own header
/// dimensions (no pixel decode) - the radar image's true pixel size, needed by the viewer's
/// world<->image transform (`s6p_map_art.md`: "1024^2 typically - verify against the decoded
/// size"). Returns a 404 `Response`, not a 500, when neither name exists: that's an
/// unknown/unshipped map, a caller error, not a server fault.
fn radar_image_size(state: &AppState, vpk: &Vpk, map: &str) -> Result<(u32, u32), Box<Response>> {
    let Some((entry, entry_path)) = find_radar_entry(vpk, map, "default") else {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            format!("no radar image for {map} (checked _radar_psd and _radar_tga)"),
        )));
    };
    let bytes = vpk
        .read_verified(entry)
        .map_err(|e| Box::new(art_internal_error(state, e)))?;
    let resource = Resource::parse(bytes).map_err(|e| Box::new(art_internal_error(state, e)))?;
    let data_block = resource.block(FourCC::DATA).ok_or_else(|| {
        Box::new(art_internal_error(
            state,
            format!("{entry_path}: no DATA block"),
        ))
    })?;
    let header = s2tex::header::parse(resource.block_bytes(data_block))
        .map_err(|e| Box::new(art_internal_error(state, e)))?;
    Ok((u32::from(header.width), u32::from(header.height)))
}

/// Parses `value.get(key)` as a lenient number (`s6p_map_art.md`: `"7 "` must parse), trimming
/// whitespace before `f64::parse`.
fn number(value: &kv1::Value, key: &str) -> Option<f64> {
    value.get(key)?.as_str()?.trim().parse().ok()
}

/// Turns a leading run of uppercase letters into a camelCase-friendly prefix: `CTSpawn` ->
/// `ctSpawn`, `TSpawn` -> `tSpawn`, `Hostage1` -> `hostage1`; a name that doesn't start with an
/// uppercase letter (`bombA`, `bombB`) is left unchanged. More than one leading uppercase letter
/// keeps the last of the run capitalised, since it starts the next word (`CTSpawn`'s "Spawn").
fn decapitalize_leading_acronym(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut upper_run = 0;
    while upper_run < chars.len() && chars[upper_run].is_ascii_uppercase() {
        upper_run += 1;
    }
    let decap_count = match upper_run {
        0 => 0,
        1 => 1,
        n if n == chars.len() => n,
        n => n - 1,
    };
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i < decap_count {
                c.to_ascii_lowercase()
            } else {
                *c
            }
        })
        .collect()
}

/// The overview's `sections` array: one entry per `verticalsections` child (ordered by descending
/// `AltitudeMax`, highest/`"default"` first), or a single boundless `"default"` section when the
/// map has no `verticalsections` block at all (`s6p_map_art.md` change item 1).
fn build_sections(root: &kv1::Value) -> Vec<Value> {
    let mut sections: Vec<(String, Option<f64>, Option<f64>)> = root
        .get("verticalsections")
        .and_then(kv1::Value::as_block)
        .map(|pairs| {
            pairs
                .iter()
                .map(|(name, section)| {
                    (
                        name.clone(),
                        number(section, "AltitudeMin"),
                        number(section, "AltitudeMax"),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if sections.is_empty() {
        sections.push(("default".to_string(), None, None));
    }
    sections.sort_by(|a, b| {
        b.2.unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.2.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sections
        .into_iter()
        .map(|(name, altitude_min, altitude_max)| {
            json!({
                "name": name,
                "altitudeMin": altitude_min,
                "altitudeMax": altitude_max,
                "radar": name,
            })
        })
        .collect()
}

/// The overview's `points` object: every top-level `<Name>_x`/`<Name>_y` pair (`CTSpawn`,
/// `TSpawn`, `bombA`, `bombB`, `Hostage1`.. on a hostage map, ...), keyed by
/// [`decapitalize_leading_acronym`]. `pos_x`/`pos_y` are the map origin, already surfaced as
/// `posX`/`posY`, and are excluded here even though they share the same `_x`/`_y` shape.
fn build_points(root: &kv1::Value) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(pairs) = root.as_block() {
        for (key, _) in pairs {
            let Some(base) = key.strip_suffix("_x") else {
                continue;
            };
            if base.eq_ignore_ascii_case("pos") {
                continue;
            }
            let y_key = format!("{base}_y");
            if let (Some(x), Some(y)) = (number(root, key), number(root, &y_key)) {
                map.insert(decapitalize_leading_acronym(base), json!([x, y]));
            }
        }
    }
    Value::Object(map)
}

#[derive(Debug, Deserialize)]
pub(crate) struct OverviewQuery {
    map: Option<String>,
}

/// `get_overview`'s own blocking computation (follow-up security review item 5): resolves pak01's
/// identity (`MapRegistry::hashed_vpk`, itself memoized by the file's own `(modified, len)`, so
/// this is cheap unless pak01 actually changed) and reuses `state.overview_cache`'s cached JSON
/// while it still matches, else opens pak01, parses the overview `.txt` fresh, and updates the
/// cache. Entirely blocking I/O/CPU work - never called outside `spawn_blocking` (`get_overview`).
fn compute_overview(state: &AppState, csgo_dir: &Path, map: &str) -> Result<Value, Box<Response>> {
    let pak01 = pak01_path(state, csgo_dir)?;
    let identity = state.registry.hashed_vpk(&pak01).map_err(|e| {
        Box::new(art_internal_error(
            state,
            format!("failed to hash {}: {e}", pak01.display()),
        ))
    })?;
    if let Some((cached_identity, cached)) = state.overview_cache.lock().unwrap().get(map)
        && *cached_identity == identity
    {
        return Ok(cached.clone());
    }

    let vpk = cached_pak01(state, &pak01, &identity)?;
    let overview_path = format!("resource/overviews/{map}.txt");
    let Some(entry) = vpk.find(&overview_path) else {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            format!("no overview for {map} (see /api/maps)"),
        )));
    };
    let bytes = vpk
        .read_verified(entry)
        .map_err(|e| Box::new(art_internal_error(state, e)))?;
    let text = String::from_utf8_lossy(&bytes);
    let root = kv1::parse(&text).map_err(|e| {
        Box::new(art_internal_error(
            state,
            format!("failed to parse {overview_path}: {e}"),
        ))
    })?;

    let (Some(pos_x), Some(pos_y), Some(scale)) = (
        number(&root, "pos_x"),
        number(&root, "pos_y"),
        number(&root, "scale"),
    ) else {
        return Err(Box::new(art_internal_error(
            state,
            format!("{overview_path}: missing pos_x/pos_y/scale"),
        )));
    };

    let image_size = radar_image_size(state, &vpk, map)?;

    let value = json!({
        "posX": pos_x,
        "posY": pos_y,
        "scale": scale,
        "imageSize": [image_size.0, image_size.1],
        "sections": build_sections(&root),
        "points": build_points(&root),
    });
    state
        .overview_cache
        .lock()
        .unwrap()
        .insert(map.to_string(), (identity, value.clone()));
    Ok(value)
}

pub(crate) async fn get_overview(
    State(state): State<Arc<AppState>>,
    Query(q): Query<OverviewQuery>,
) -> Response {
    let Some(map) = q.map else {
        return api_error(StatusCode::BAD_REQUEST, "map is required");
    };
    if !is_safe_ident(&map) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "map must be alphanumerics/underscore only",
        );
    }
    let csgo_dir = match resolve_game_dir(&state) {
        Ok(d) => d,
        Err(r) => return *r,
    };

    let state_for_blocking = state.clone();
    let result = spawn_art_compute(&state, move || {
        compute_overview(&state_for_blocking, &csgo_dir, &map)
    })
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(r) => *r,
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct MapArtQuery {
    map: Option<String>,
    kind: Option<String>,
    section: Option<String>,
}

/// pak01's screenshot entry for `map`, at 720p, falling back to 1080p then 360p
/// (`s6p_map_art.md` change item 2).
fn find_screenshot(vpk: &Vpk, map: &str) -> Option<Vec<u8>> {
    for resolution in ["720p", "1080p", "360p"] {
        let path = format!("panorama/images/map_icons/screenshots/{resolution}/{map}_png.vtex_c");
        if let Some(entry) = vpk.find(&path) {
            return vpk.read_verified(entry).ok();
        }
    }
    None
}

fn art_cache_paths(state: &AppState, map: &str, file_stem: &str) -> (PathBuf, PathBuf) {
    let dir = state.registry.cache_root().join("art").join(map);
    (
        dir.join(format!("{file_stem}.png")),
        dir.join(format!("{file_stem}.png.identity")),
    )
}

/// Writes `png_bytes` to `png_path` (via a same-directory temp file, then rename - the same
/// atomic-write idiom `config::save_at`/`extract::cache::save_extraction` use) and `sha12` to
/// `identity_path` right after, so a reader never sees a PNG whose identity file doesn't match it
/// yet.
fn write_art_cache(
    png_path: &Path,
    identity_path: &Path,
    png_bytes: &[u8],
    sha12: &str,
) -> std::io::Result<()> {
    if let Some(parent) = png_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = png_path.with_file_name(format!(
        "{}.tmp-{}-{}",
        png_path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::write(&tmp_path, png_bytes)?;
    if let Err(e) = fs::rename(&tmp_path, png_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    fs::write(identity_path, sha12)
}

fn png_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// `get_mapart`'s own blocking computation (follow-up security review round 3, item 3): serves the
/// on-disk art cache when pak01's identity still matches, else decodes+encodes fresh via
/// [`cached_pak01`] and refreshes the cache. Entirely blocking I/O/CPU work - never called outside
/// `spawn_blocking` (`get_mapart`, via [`spawn_art_compute`]).
fn compute_mapart(
    state: &AppState,
    csgo_dir: &Path,
    map: &str,
    kind: &str,
    section: &str,
) -> Result<Vec<u8>, Box<Response>> {
    let file_stem = if kind == "screenshot" {
        "screenshot".to_string()
    } else if section.eq_ignore_ascii_case("default") {
        "radar".to_string()
    } else {
        format!("radar_{section}")
    };
    let (png_path, identity_path) = art_cache_paths(state, map, &file_stem);

    let pak01 = pak01_path(state, csgo_dir)?;
    // The registry's own memoized-by-(modified, len) VPK hash (`MapRegistry::hashed_vpk`),
    // reused here so a game update (which changes pak01's bytes) invalidates the art cache the
    // same way it invalidates a stale map cache dir, without re-hashing a multi-GB VPK on every
    // request (`s6p_map_art.md` change item 2).
    let identity = state.registry.hashed_vpk(&pak01).map_err(|e| {
        Box::new(art_internal_error(
            state,
            format!("failed to hash {}: {e}", pak01.display()),
        ))
    })?;
    let sha12 = &identity[..identity.len().min(12)];

    if fs::read_to_string(&identity_path)
        .ok()
        .as_deref()
        .map(str::trim)
        == Some(sha12)
        && let Ok(cached) = fs::read(&png_path)
    {
        return Ok(cached);
    }

    let vpk = cached_pak01(state, &pak01, &identity)?;

    let vtex_bytes = if kind == "screenshot" {
        match find_screenshot(&vpk, map) {
            Some(b) => b,
            None => {
                return Err(Box::new(api_error(
                    StatusCode::NOT_FOUND,
                    format!("no screenshot for {map}"),
                )));
            }
        }
    } else {
        // `_radar_psd` first, `_radar_tga` fallback (`find_radar_entry`) - some maps (e.g.
        // `rush_001`) only ship the tga-sourced radar.
        let Some((entry, _entry_path)) = find_radar_entry(&vpk, map, section) else {
            return Err(Box::new(api_error(
                StatusCode::NOT_FOUND,
                format!("no radar image for {map}/{section} (checked _radar_psd and _radar_tga)"),
            )));
        };
        match vpk.read_verified(entry) {
            Ok(b) => b,
            Err(e) => return Err(Box::new(art_internal_error(state, e))),
        }
    };

    let decoded = match s2tex::decode_bytes(&vtex_bytes, s2tex::header::MAX_SIDE) {
        Ok(d) => d,
        Err(e) => return Err(Box::new(art_internal_error(state, e))),
    };
    // Always PNG, lossless, regardless of alpha (`s6p_map_art.md` change item 2) - unlike
    // `s2tex::DecodedImage::encode`, which would pick JPEG for an opaque screenshot.
    let encoded = match s2tex::encode_image(&decoded.rgba, decoded.width, decoded.height, 90, true)
    {
        Ok(e) => e,
        Err(e) => return Err(Box::new(art_internal_error(state, e))),
    };

    if let Err(e) = write_art_cache(&png_path, &identity_path, &encoded.bytes, sha12) {
        // Not fatal: the freshly-decoded bytes are still served below even if the cache write
        // failed (e.g. a read-only cache dir) - the next request just decodes again.
        eprintln!(
            "warning: failed to write art cache {}: {e}",
            png_path.display()
        );
    }

    Ok(encoded.bytes)
}

pub(crate) async fn get_mapart(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MapArtQuery>,
) -> Response {
    let Some(map) = q.map else {
        return api_error(StatusCode::BAD_REQUEST, "map is required");
    };
    if !is_safe_ident(&map) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "map must be alphanumerics/underscore only",
        );
    }
    let Some(kind) = q.kind else {
        return api_error(StatusCode::BAD_REQUEST, "kind is required");
    };
    if kind != "screenshot" && kind != "radar" {
        return api_error(StatusCode::BAD_REQUEST, "kind must be screenshot or radar");
    }
    let section = q.section.unwrap_or_else(|| "default".to_string());
    if !is_safe_ident(&section) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "section must be alphanumerics/underscore only",
        );
    }

    let csgo_dir = match resolve_game_dir(&state) {
        Ok(d) => d,
        Err(r) => return *r,
    };

    let state_for_blocking = state.clone();
    let result = spawn_art_compute(&state, move || {
        compute_mapart(&state_for_blocking, &csgo_dir, &map, &kind, &section)
    })
    .await;
    match result {
        Ok(bytes) => png_response(bytes),
        Err(r) => *r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decapitalizes_leading_acronyms_like_the_known_overview_keys() {
        assert_eq!(decapitalize_leading_acronym("CTSpawn"), "ctSpawn");
        assert_eq!(decapitalize_leading_acronym("TSpawn"), "tSpawn");
        assert_eq!(decapitalize_leading_acronym("Hostage1"), "hostage1");
        assert_eq!(decapitalize_leading_acronym("bombA"), "bombA");
        assert_eq!(decapitalize_leading_acronym("bombB"), "bombB");
    }

    #[test]
    fn build_points_excludes_pos_and_pairs_x_y() {
        let root = kv1::parse(
            r#""m" { "pos_x" "1" "pos_y" "2" "CTSpawn_x" "0.1" "CTSpawn_y" "0.2" "TSpawn_x" "0.3" "TSpawn_y" "0.4" }"#,
        )
        .unwrap();
        let points = build_points(&root);
        let obj = points.as_object().unwrap();
        assert!(!obj.contains_key("pos"));
        assert_eq!(obj["ctSpawn"], json!([0.1, 0.2]));
        assert_eq!(obj["tSpawn"], json!([0.3, 0.4]));
    }

    #[test]
    fn build_sections_defaults_to_one_boundless_section_without_verticalsections() {
        let root = kv1::parse(r#""m" { "pos_x" "0" "pos_y" "0" "scale" "1" }"#).unwrap();
        let sections = build_sections(&root);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0]["name"], json!("default"));
        assert_eq!(sections[0]["altitudeMin"], Value::Null);
        assert_eq!(sections[0]["altitudeMax"], Value::Null);
    }

    #[test]
    fn build_sections_orders_by_descending_altitude() {
        let root = kv1::parse(
            r#""m" { "verticalsections" { "lower" { "AltitudeMax" "-495" "AltitudeMin" "-10000" } "default" { "AltitudeMax" "10000" "AltitudeMin" "-495" } } }"#,
        )
        .unwrap();
        let sections = build_sections(&root);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0]["name"], json!("default"));
        assert_eq!(sections[1]["name"], json!("lower"));
    }

    #[test]
    fn is_safe_ident_rejects_path_traversal() {
        assert!(is_safe_ident("de_nuke"));
        assert!(!is_safe_ident("../etc"));
        assert!(!is_safe_ident("de nuke"));
        assert!(!is_safe_ident(""));
    }
}
