//! `GET /api/trajectory|lineup-one|slack|smoke`: single-throw physics, computed straight from a
//! throw spec rather than a map-wide solve. Ported in contract from
//! `cs2-smoke-solver/src/Cli/Services/LineupApi.cs:108-331` (trajectory/lineup-one/slack) and
//! `src/Cli/Commands/ServeCommand.cs:541-612` (smoke).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use geom::collider::Collider;
use geom::filter::{self, AttributeMask};
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::CollisionAttribute;
use geom::voxel::VoxelGrid;
use sim::{SmokeParams, ThrowSpec, Trace, eye_height, simulate_exact, smoke_fill};
use solver::{aim_reference, identity, origins, rank};

use crate::AppState;
use crate::registry::MapEntry;
use crate::routes::{api_error, get_entry, if_none_match_hits, parse_finite, unknown_map_error};
use crate::solve::{self, SINGLE_TARGET_DEFAULT_ATTRS};

fn set_physics_cache_headers(headers: &mut HeaderMap, etag: &str) {
    headers.insert(header::ETAG, HeaderValue::from_str(etag).unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800"),
    );
}

fn etag_of(entry: &MapEntry) -> String {
    format!("\"{}\"", entry.etag)
}

/// `ServeCommand.cs:1817-1838` (`ParseBroken`): a comma list from `{glass, doors}`, mapped and
/// Ordinal-sorted.
fn parse_broken(s: Option<&str>) -> Result<Vec<String>, String> {
    let mut groups: Vec<String> = Vec::new();
    let Some(s) = s.filter(|s| !s.is_empty()) else {
        return Ok(groups);
    };
    for token in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let group = match token {
            "glass" => "EntityBreakable",
            "doors" => "EntityDoor",
            _ => return Err("broken must be a comma list drawn from glass, doors".to_string()),
        };
        if !groups.iter().any(|g| g == group) {
            groups.push(group.to_string());
        }
    }
    groups.sort();
    Ok(groups)
}

/// `MeshSetup.cs:52-63`'s grenade-solid predicate, applied to a single attribute (mirrors
/// `target.rs::is_grenade_solid`, duplicated here since that one is private to `solver`).
fn is_grenade_solid(a: &CollisionAttribute) -> bool {
    let any_ci =
        |layers: &[String], name: &str| layers.iter().any(|l| l.eq_ignore_ascii_case(name));
    !any_ci(&a.interact_exclude, "csgo_thrown_grenade")
        && !any_ci(&a.interact_as, "playerclip")
        && !any_ci(&a.interact_as, "npcclip")
        && !any_ci(&a.interact_as, "sky")
}

/// The grenade-solid collider for `broken`'s world state: the entry's cached one when nothing is
/// broken, else a fresh grid excluding those groups (`MapRegistry.cs`'s `entry.ColliderExcluding`,
/// uncached here - broken-state requests are rare enough not to warrant a per-combination cache).
fn grenade_collider_for(entry: &MapEntry, broken: &[String]) -> Result<Arc<UniformGrid>, String> {
    if broken.is_empty() {
        entry.grenade_collider().map_err(|e| e.to_string())
    } else {
        let mask = filter::from_fn(&entry.bundle.mesh, |a| {
            is_grenade_solid(a) && !broken.iter().any(|n| n.eq_ignore_ascii_case(&a.name))
        });
        UniformGrid::build(&entry.bundle.mesh, &mask, None, 128.0)
            .map(Arc::new)
            .map_err(|e| e.to_string())
    }
}

fn settled(r: &sim::TrajectoryResult) -> bool {
    !r.lost && r.flight_time < sim::MAX_FLIGHT_SECONDS - 0.01
}

// ---- response rounding (`LineupApi.cs:125,128,202,293,327`) --------------------------------

/// Rounds and returns `f64`, not `f32`: `serde_json::Value`'s `Number` is always `f64`-backed, so
/// serializing a rounded `f32` still promotes it to the nearest `f64`, which is *not* the decimal
/// we rounded to (e.g. `-1156.8f32` prints as `-1156.800048828125`) - the whole point of rounding
/// here. Rounding in `f64` instead gives the shortest-round-trip decimal we actually want.
fn round1(v: f32) -> f64 {
    ((v as f64) * 10.0).round() / 10.0
}

fn round3(v: f32) -> f64 {
    ((v as f64) * 1000.0).round() / 1000.0
}

fn round_points(points: &[[f32; 3]]) -> Vec<[f64; 3]> {
    points
        .iter()
        .map(|p| [round1(p[0]), round1(p[1]), round1(p[2])])
        .collect()
}

// ---- /api/trajectory ------------------------------------------------------------------------

/// `get_trajectory`'s blocking computation result: flight-tick points, bounce contact points, and
/// the sim's own summary.
type TrajectoryCompute = (Vec<[f32; 3]>, Vec<[f32; 3]>, sim::TrajectoryResult);

#[derive(Debug, Deserialize)]
pub struct TrajectoryQuery {
    map: Option<String>,
    x: Option<String>,
    y: Option<String>,
    z: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    pitch: Option<String>,
    yaw: Option<String>,
    strength: Option<String>,
    #[serde(rename = "runDeg")]
    run_deg: Option<String>,
    broken: Option<String>,
}

pub async fn get_trajectory(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TrajectoryQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map.as_deref() else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let Some(throw_type) = q.type_.as_deref().and_then(solve::parse_throw_type_ci) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("unknown throw type '{}'", q.type_.as_deref().unwrap_or("")),
        );
    };
    let (Some(x), Some(y), Some(z), Some(pitch), Some(yaw), Some(strength)) = (
        parse_finite(q.x.as_deref()),
        parse_finite(q.y.as_deref()),
        parse_finite(q.z.as_deref()),
        parse_finite(q.pitch.as_deref()),
        parse_finite(q.yaw.as_deref()),
        parse_finite(q.strength.as_deref()),
    ) else {
        return api_error(StatusCode::BAD_REQUEST, "non-finite throw parameter");
    };
    let run_deg = match q.run_deg.as_deref() {
        None => 0.0,
        Some(s) => match parse_finite(Some(s)) {
            Some(v) => v,
            None => return api_error(StatusCode::BAD_REQUEST, "non-finite throw parameter"),
        },
    };
    let broken = match parse_broken(q.broken.as_deref()) {
        Ok(b) => b,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };

    let etag = etag_of(&entry);
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_physics_cache_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let constants = state.constants;
    let compute = move || -> Result<TrajectoryCompute, String> {
        let collider = grenade_collider_for(&entry, &broken)?;
        let eye = V3::new(x, y, z + eye_height(throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: yaw,
            pitch_deg: pitch,
            throw_type,
            strength,
            run_yaw_offset_deg: run_deg,
        };
        let mut ticks: Vec<(V3, V3)> = Vec::new();
        let mut bounces: Vec<sim::BounceRecord> = Vec::new();
        let result = simulate_exact(
            collider.as_ref(),
            &spec,
            &constants,
            Trace {
                ticks: Some(&mut ticks),
                bounces: Some(&mut bounces),
            },
        );
        let points: Vec<[f32; 3]> = ticks.iter().map(|(p, _)| [p.x, p.y, p.z]).collect();
        let contacts: Vec<[f32; 3]> = bounces
            .iter()
            .map(|b| [b.contact.x, b.contact.y, b.contact.z])
            .collect();
        Ok((points, contacts, result))
    };
    let (points, contacts, result) = match tokio::task::spawn_blocking(compute).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    // `contacts`: the sim's own `BounceRecord.contact` list (viewer bounce marks - `s6f3b_viewer3d.md`
    // - used to draw exact touchdown points instead of guessing them from a Z-local-minimum over
    // `points`).
    let mut resp = Json(json!({
        "points": round_points(&points),
        "bounces": result.bounces,
        "contacts": round_points(&contacts),
        "flightTime": round3(result.flight_time),
        "lost": result.lost,
    }))
    .into_response();
    set_physics_cache_headers(resp.headers_mut(), &etag);
    resp
}

// ---- /api/lineup-one -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LineupOneQuery {
    map: Option<String>,
    x: Option<String>,
    y: Option<String>,
    z: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    pitch: Option<String>,
    yaw: Option<String>,
    strength: Option<String>,
    tx: Option<String>,
    ty: Option<String>,
    tz: Option<String>,
    #[serde(rename = "runDeg")]
    run_deg: Option<String>,
    broken: Option<String>,
}

const LINEUP_ONE_STEP_DEG: f32 = 0.6;
const LINEUP_ONE_AIM_REACH: i32 = 2;
const LINEUP_ONE_COVER_RADIUS: f32 = 72.0;

pub async fn get_lineup_one(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LineupOneQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map.as_deref() else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let Some(throw_type) = q.type_.as_deref().and_then(solve::parse_throw_type_ci) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("unknown throw type '{}'", q.type_.as_deref().unwrap_or("")),
        );
    };
    let (
        Some(x),
        Some(y),
        Some(z),
        Some(pitch),
        Some(yaw),
        Some(strength),
        Some(tx),
        Some(ty),
        Some(tz),
    ) = (
        parse_finite(q.x.as_deref()),
        parse_finite(q.y.as_deref()),
        parse_finite(q.z.as_deref()),
        parse_finite(q.pitch.as_deref()),
        parse_finite(q.yaw.as_deref()),
        parse_finite(q.strength.as_deref()),
        parse_finite(q.tx.as_deref()),
        parse_finite(q.ty.as_deref()),
        parse_finite(q.tz.as_deref()),
    )
    else {
        return api_error(StatusCode::BAD_REQUEST, "non-finite lineup parameter");
    };
    let run_deg = match q.run_deg.as_deref() {
        None => 0.0,
        Some(s) => match parse_finite(Some(s)) {
            Some(v) => v,
            None => return api_error(StatusCode::BAD_REQUEST, "non-finite lineup parameter"),
        },
    };
    let broken = match parse_broken(q.broken.as_deref()) {
        Ok(b) => b,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };

    let etag = etag_of(&entry);
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_physics_cache_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let constants = state.constants;
    let compute = move || -> Result<serde_json::Value, String> {
        let collider = grenade_collider_for(&entry, &broken)?;
        let player_collider = entry.player_collider().map_err(|e| e.to_string())?;

        let feet = V3::new(x, y, z);
        let target = V3::new(tx, ty, tz);
        let eye = feet + V3::new(0.0, 0.0, eye_height(throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: yaw,
            pitch_deg: pitch,
            throw_type,
            strength,
            run_yaw_offset_deg: run_deg,
        };
        let mut ticks: Vec<(V3, V3)> = Vec::new();
        let result = simulate_exact(
            collider.as_ref(),
            &spec,
            &constants,
            Trace {
                ticks: Some(&mut ticks),
                bounces: None,
            },
        );

        // `LineupApi.cs:158-167`: one-tick (0.25u) foot-shift scatter.
        let mut scatter = 0f32;
        if settled(&result) {
            for (dx, dy) in [(0.25, 0.0), (-0.25, 0.0), (0.0, 0.25), (0.0, -0.25)] {
                let probe_spec = ThrowSpec {
                    eye: eye + V3::new(dx, dy, 0.0),
                    ..spec
                };
                let probe =
                    simulate_exact(collider.as_ref(), &probe_spec, &constants, Trace::default());
                let d = if settled(&probe) {
                    (probe.rest - result.rest).length()
                } else {
                    512.0
                };
                scatter = scatter.max(d);
            }
        }

        // `LineupApi.cs:169-192`: a 5x5 aim window at 0.6 deg steps, hit if it still lands within
        // 72u of `target`.
        let mut hits = 0;
        let mut total = 0;
        for d_yaw in -LINEUP_ONE_AIM_REACH..=LINEUP_ONE_AIM_REACH {
            for d_pitch in -LINEUP_ONE_AIM_REACH..=LINEUP_ONE_AIM_REACH {
                total += 1;
                let probe_spec = ThrowSpec {
                    eye,
                    yaw_deg: yaw + d_yaw as f32 * LINEUP_ONE_STEP_DEG,
                    pitch_deg: pitch + d_pitch as f32 * LINEUP_ONE_STEP_DEG,
                    throw_type,
                    strength,
                    run_yaw_offset_deg: run_deg,
                };
                let probe =
                    simulate_exact(collider.as_ref(), &probe_spec, &constants, Trace::default());
                if settled(&probe) {
                    let dx = probe.rest.x - target.x;
                    let dy = probe.rest.y - target.y;
                    if (dx * dx + dy * dy).sqrt() <= LINEUP_ONE_COVER_RADIUS {
                        hits += 1;
                    }
                }
            }
        }
        let stability = if total == 0 {
            0.0
        } else {
            hits as f32 / total as f32
        };

        let aim = aim_reference::analyze(collider.as_ref(), feet, throw_type, pitch, yaw);
        let (pin, wall_gap) = origins::position_stance(player_collider.as_ref(), feet);
        let id = identity::id(throw_type, strength, run_deg, feet, yaw, pitch);
        let points: Vec<[f32; 3]> = ticks.iter().map(|(p, _)| [p.x, p.y, p.z]).collect();

        let lineup = json!({
            "id": id,
            "feet": [feet.x, feet.y, feet.z],
            "yaw": yaw,
            "pitch": pitch,
            "type": solve::type_name(throw_type),
            "how": rank::describe(throw_type, strength, run_deg),
            "strength": strength,
            "click": rank::click_name(strength),
            "runDeg": run_deg,
            "rest": [result.rest.x, result.rest.y, result.rest.z],
            "Bounces": result.bounces,
            "flightTime": result.flight_time,
            "stability": stability,
            "scatter": scatter,
            "pin": match pin { 2 => Some("corner"), 1 => Some("wall"), _ => None },
            "wallGap": wall_gap,
            "aimRef": {
                "tier": aim.tier(),
                "sky": aim.sky_fraction,
                "edgeDeg": aim.nearest_silhouette_deg.is_finite().then_some(aim.nearest_silhouette_deg),
                "reticleDeg": aim.nearest_reticle_deg.is_finite().then_some(aim.nearest_reticle_deg),
                "band": aim.band(),
                "marginDeg": aim.margin_deg(),
            },
            "console": rank::setpos_command(feet, pitch, yaw),
            "lost": result.lost,
        });
        Ok(json!({ "points": round_points(&points), "lineup": lineup }))
    };
    let body = match tokio::task::spawn_blocking(compute).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut resp = Json(body).into_response();
    set_physics_cache_headers(resp.headers_mut(), &etag);
    resp
}

// ---- /api/slack -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SlackQuery {
    map: Option<String>,
    x: Option<String>,
    y: Option<String>,
    z: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    pitch: Option<String>,
    yaw: Option<String>,
    strength: Option<String>,
    tx: Option<String>,
    ty: Option<String>,
    tz: Option<String>,
    within: Option<String>,
    #[serde(rename = "runDeg")]
    run_deg: Option<String>,
    broken: Option<String>,
}

const SLACK_DIRECTIONS: i32 = 12;
const SLACK_MAX_PROBE: f32 = 64.0;

pub async fn get_slack(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SlackQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map.as_deref() else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let Some(throw_type) = q.type_.as_deref().and_then(solve::parse_throw_type_ci) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("unknown throw type '{}'", q.type_.as_deref().unwrap_or("")),
        );
    };
    let (
        Some(x),
        Some(y),
        Some(z),
        Some(pitch),
        Some(yaw),
        Some(strength),
        Some(tx),
        Some(ty),
        Some(tz),
        Some(within),
    ) = (
        parse_finite(q.x.as_deref()),
        parse_finite(q.y.as_deref()),
        parse_finite(q.z.as_deref()),
        parse_finite(q.pitch.as_deref()),
        parse_finite(q.yaw.as_deref()),
        parse_finite(q.strength.as_deref()),
        parse_finite(q.tx.as_deref()),
        parse_finite(q.ty.as_deref()),
        parse_finite(q.tz.as_deref()),
        parse_finite(q.within.as_deref()),
    )
    else {
        return api_error(StatusCode::BAD_REQUEST, "non-finite slack parameter");
    };
    if !(1.0..=512.0).contains(&within) {
        return api_error(StatusCode::BAD_REQUEST, "within must be between 1 and 512");
    }
    let run_deg = match q.run_deg.as_deref() {
        None => 0.0,
        Some(s) => match parse_finite(Some(s)) {
            Some(v) => v,
            None => return api_error(StatusCode::BAD_REQUEST, "non-finite slack parameter"),
        },
    };
    let broken = match parse_broken(q.broken.as_deref()) {
        Ok(b) => b,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };

    let etag = etag_of(&entry);
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_physics_cache_headers(resp.headers_mut(), &etag);
        return resp;
    }
    let constants = state.constants;
    let compute = move || -> Result<Vec<[f64; 2]>, String> {
        let collider = grenade_collider_for(&entry, &broken)?;
        let player_collider = entry.player_collider().map_err(|e| e.to_string())?;
        let feet = V3::new(x, y, z);
        let target = V3::new(tx, ty, tz);

        let lands_from = |candidate_feet: V3| -> bool {
            let eye = candidate_feet + V3::new(0.0, 0.0, eye_height(throw_type));
            let spec = ThrowSpec {
                eye,
                yaw_deg: yaw,
                pitch_deg: pitch,
                throw_type,
                strength,
                run_yaw_offset_deg: run_deg,
            };
            let result = simulate_exact(collider.as_ref(), &spec, &constants, Trace::default());
            if !settled(&result) {
                return false;
            }
            let dx = result.rest.x - target.x;
            let dy = result.rest.y - target.y;
            (dx * dx + dy * dy).sqrt() <= within
        };
        // `LineupApi.cs:265-286`: probes at waist height, sliding to `xy` (blocked -> no spot
        // there), then dropping onto standable floor beneath it.
        let waist = feet + V3::new(0.0, 0.0, 36.0);
        let feet_at = |xy: (f32, f32)| -> Option<V3> {
            let offset_waist = V3::new(xy.0, xy.1, waist.z);
            if player_collider
                .as_ref()
                .first_hit_ray(waist, offset_waist)
                .is_some()
            {
                return None;
            }
            let hit = player_collider
                .as_ref()
                .first_hit_ray(offset_waist, offset_waist - V3::new(0.0, 0.0, 72.0))?;
            (hit.normal.z >= sim::FLOOR_NORMAL_Z)
                .then(|| V3::new(xy.0, xy.1, offset_waist.z - 72.0 * hit.t))
        };

        let feet_xy = (feet.x, feet.y);
        let centered = lands_from(feet);
        let mut dirs: Vec<[f64; 2]> = Vec::with_capacity(SLACK_DIRECTIONS as usize);
        for i in 0..SLACK_DIRECTIONS {
            let ang = i as f32 * 2.0 * std::f32::consts::PI / SLACK_DIRECTIONS as f32;
            let dir = (ang.cos(), ang.sin());
            let ok = |r: f32| -> bool {
                feet_at((feet_xy.0 + dir.0 * r, feet_xy.1 + dir.1 * r)).is_some_and(lands_from)
            };
            let mut radius = 0.0f32;
            if centered {
                if ok(SLACK_MAX_PROBE) {
                    radius = SLACK_MAX_PROBE;
                } else {
                    let (mut lo, mut hi) = (0.0f32, SLACK_MAX_PROBE);
                    for _ in 0..6 {
                        let mid = (lo + hi) / 2.0;
                        if ok(mid) {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    radius = lo;
                }
            }
            dirs.push([
                (ang * 180.0 / std::f32::consts::PI).round() as f64,
                round1(radius),
            ]);
        }
        Ok(dirs)
    };
    let dirs = match tokio::task::spawn_blocking(compute).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut resp = Json(json!({ "within": within.round() as f64, "dirs": dirs })).into_response();
    set_physics_cache_headers(resp.headers_mut(), &etag);
    resp
}

// ---- /api/smoke -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SmokeQuery {
    map: Option<String>,
    x: Option<String>,
    y: Option<String>,
    z: Option<String>,
    full: Option<String>,
}

/// Accepts `full=1`, `full=true`, `full=on` (case-insensitive) as truthy - a plain `bool` field
/// makes axum reject any other spelling (e.g. `?full=yes`) with its own plain-text rejection
/// instead of our `{"error":...}` shape.
fn parse_full(s: Option<&str>) -> bool {
    s.is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "on"))
}

/// `ServeCommand.cs:199` (`MinPlausibleBloomCells`).
const MIN_PLAUSIBLE_BLOOM_CELLS: usize = 48;
const SMOKE_VOXEL: f32 = 16.0;
const SMOKE_BOUNDS_MARGIN: f32 = 512.0;

pub async fn get_smoke(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SmokeQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(map) = q.map.as_deref() else {
        return unknown_map_error();
    };
    let entry = match get_entry(&state, map) {
        Ok(e) => e,
        Err(r) => return *r,
    };
    let (Some(x), Some(y), Some(z)) = (
        parse_finite(q.x.as_deref()),
        parse_finite(q.y.as_deref()),
        parse_finite(q.z.as_deref()),
    ) else {
        return api_error(StatusCode::BAD_REQUEST, "non-finite coordinate");
    };
    let at = V3::new(x, y, z);
    let (mesh_min, mesh_max) = entry.bundle.mesh.bounds().unwrap_or(([0.0; 3], [0.0; 3]));
    if at.x < mesh_min[0] - SMOKE_BOUNDS_MARGIN
        || at.x > mesh_max[0] + SMOKE_BOUNDS_MARGIN
        || at.y < mesh_min[1] - SMOKE_BOUNDS_MARGIN
        || at.y > mesh_max[1] + SMOKE_BOUNDS_MARGIN
    {
        return api_error(StatusCode::BAD_REQUEST, "point is outside the map bounds");
    }

    let etag = etag_of(&entry);
    if if_none_match_hits(&headers, &etag) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        set_physics_cache_headers(resp.headers_mut(), &etag);
        return resp;
    }

    let full = parse_full(q.full.as_deref());
    let compute = move || -> Result<serde_json::Value, (StatusCode, String)> {
        let p = if full {
            SmokeParams::FULL_REACH
        } else {
            SmokeParams::COVERAGE
        };
        let mask: AttributeMask =
            filter::names_mask(&entry.bundle.mesh, &SINGLE_TARGET_DEFAULT_ATTRS);
        let pad = p.max_radius + 4.0 * SMOKE_VOXEL;
        let half = V3::new(pad, pad, pad);
        let grid = VoxelGrid::build(
            &entry.bundle.mesh,
            &mask,
            SMOKE_VOXEL,
            Aabb {
                min: at - half,
                max: at + half,
            },
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let (gx, gy, gz) = grid.cell_of(at);
        if !grid.in_bounds(gx, gy, gz) {
            return Err((
                StatusCode::BAD_REQUEST,
                "point is outside the map bounds".to_string(),
            ));
        }

        let mut smoke = smoke_fill(&grid, at, &p).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "point is outside the map bounds".to_string(),
            )
        })?;
        // `ServeCommand.cs:581-588`: a target sitting inside geometry starts in a sealed pocket;
        // lift the start until the bloom is a smoke's worth, up to the coverage radius.
        let mut lift = SMOKE_VOXEL;
        while smoke.cells.len() < MIN_PLAUSIBLE_BLOOM_CELLS
            && lift <= SmokeParams::COVERAGE.max_radius
        {
            if let Ok(lifted) = smoke_fill(&grid, at + V3::new(0.0, 0.0, lift), &p)
                && lifted.cells.len() > smoke.cells.len()
            {
                smoke = lifted;
            }
            lift += SMOKE_VOXEL;
        }

        let mut cells: Vec<f32> = Vec::with_capacity(smoke.cells.len() * 3);
        for &c in &smoke.cells {
            let center = grid.cell_center(c as usize);
            cells.push(center.x);
            cells.push(center.y);
            cells.push(center.z);
        }
        Ok(json!({
            "voxel": grid.voxel_size(),
            "radius": p.max_radius,
            "fullRadius": SmokeParams::FULL_REACH.max_radius,
            "cells": cells,
        }))
    };
    let body = match tokio::task::spawn_blocking(compute).await {
        Ok(Ok(v)) => v,
        Ok(Err((status, e))) => return api_error(status, e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut resp = Json(body).into_response();
    set_physics_cache_headers(resp.headers_mut(), &etag);
    resp
}
