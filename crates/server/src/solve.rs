//! `POST /api/lineup`: validation, the disk cache, and the NDJSON solve stream. Ported in
//! contract from `cs2-smoke-solver/src/Cli/Services/LineupApi.cs` (`ValidateLineupQuery`,
//! `QueryCacheKey`, `RunTargetQuery`, `Ranked`/`Rank`) and
//! `src/Cli/Commands/ServeCommand.cs:1504-1811` (`POST /api/lineup`, `DrainProgress`). The
//! solver is driven through `solver::target::SolveHooks`, whose `on_origin`/`on_candidate`
//! feed this module's `checked`/`verified` batching (`ServeCommand.cs:1694-1707,1780-1811`).

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use geom::math::V3;
use sim::{ThrowConstants, ThrowType};
use solver::rank::{self, RankedLineup};
use solver::target::{
    self, MapData, OriginArea, Phase, SolveHooks, SolveQuery, StandSpotOrigin, Target, TargetArea,
    TargetSolve,
};

use crate::AppState;
use crate::registry::MapEntry;
use crate::routes::api_error;

/// `MeshSetup.cs:18` (`SingleTargetDefaultAttrs`), reused here so the server solves the exact
/// same world the CLI does by default.
pub(crate) const SINGLE_TARGET_DEFAULT_ATTRS: [&str; 3] = ["Default", "default", "EntitySolid"];
const SINGLE_TARGET_DEFAULT_ATTRS_STR: &str = "Default,default,EntitySolid";

pub(crate) const MAX_LINEUP_BODY: usize = 4 * 1024;
const MAX_QUEUED_SOLVES: usize = 16;

/// `s6e_progress_jobs.md`'s per-line and per-solve caps on `checked`/`verified` progress points:
/// past `MAX_STREAM_POINTS` for one solve, further points are simply dropped (the solve itself is
/// untouched) and a single `{"phase":"progress-truncated"}` line tells the client why the stream
/// went quiet on points.
pub(crate) const MAX_POINTS_PER_LINE: usize = 512;
pub(crate) const MAX_STREAM_POINTS: usize = 200_000;
/// How often buffered `checked`/`verified` points are drained into the stream
/// (`ServeCommand.cs`'s own `Task.Delay(100)` poll).
const PROGRESS_DRAIN_INTERVAL: Duration = Duration::from_millis(100);

const MAP_BOUNDS_MARGIN: f32 = 512.0;
const MIN_ORIGIN_REACH: f32 = 16.0;
const MAX_ORIGIN_REACH: f32 = 4000.0;
const MIN_TOLERANCE: f32 = 1.0;
const MAX_TOLERANCE: f32 = 512.0;

// ---- `originArea`/`targetArea` polygons (`s6g_origin_area.md`, `s6g2_target_area.md`) ---------

const MIN_AREA_VERTICES: usize = 3;
const MAX_AREA_VERTICES: usize = 64;
/// The largest bounding box a maxed-out `originReach` circle could already produce (its diameter,
/// `2 * MAX_ORIGIN_REACH`, per axis) - both `originArea` and `targetArea`'s own bounding boxes
/// are capped to the same span so neither can be used to ask for a bigger search region than the
/// reach circle already allowed (`s6g_origin_area.md`: "не дать запросить всю карту в обход
/// ограничений"; `s6g2_target_area.md` reuses the same reasoning and the same number for its own
/// "size ограничен разумно для цели" - there is no separate "target reach" constant to anchor a
/// different cap to, and a landing area is not inherently smaller than a throw area).
const MAX_AREA_SPAN: f32 = 2.0 * MAX_ORIGIN_REACH;
/// Below this (world units²) an `originArea`/`targetArea` polygon is treated as degenerate
/// (collinear or coincident points) rather than a real region.
const MIN_AREA_AREA: f64 = 1.0;

/// Our own cache format/solve-behavior version (`LineupApi.cs:475`'s `QueryVersion`, our own
/// counter): bump whenever the response shape or the solver's behavior changes, so an old cached
/// answer is never replayed as current.
const CACHE_VERSION: u32 = 10;
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
const CACHE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

// ---- the solve JSON envelope, shared with `cs2mod solve --json` -------------------------------

#[derive(Serialize)]
pub struct AimRefJson {
    pub tier: &'static str,
    pub sky: f32,
    #[serde(rename = "edgeDeg")]
    pub edge_deg: Option<f32>,
    #[serde(rename = "reticleDeg")]
    pub reticle_deg: Option<f32>,
    pub band: i32,
    #[serde(rename = "marginDeg")]
    pub margin_deg: Option<f32>,
}

#[derive(Serialize)]
pub struct LineupJson {
    pub id: String,
    pub feet: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub how: String,
    pub strength: f32,
    pub click: &'static str,
    #[serde(rename = "runDeg")]
    pub run_deg: f32,
    pub rest: [f32; 3],
    #[serde(rename = "Bounces")]
    pub bounces: u32,
    #[serde(rename = "flightTime")]
    pub flight_time: f32,
    pub stability: f32,
    pub scatter: f32,
    pub pin: Option<&'static str>,
    #[serde(rename = "wallGap")]
    pub wall_gap: Option<f32>,
    pub exposed: bool,
    pub glass: u32,
    #[serde(rename = "restIfBroken")]
    pub rest_if_broken: Option<[f32; 3]>,
    #[serde(rename = "stateDependent")]
    pub state_dependent: bool,
    #[serde(rename = "humanError")]
    pub human_error: f32,
    #[serde(rename = "aimRef")]
    pub aim_ref: AimRefJson,
    pub console: String,
    /// `s6g2_target_area.md`: whether this lineup's rest point falls inside the query's
    /// `targetArea` - only present for an area-target solve (omitted, not `null`, for a
    /// point-target one, so `cs2mod solve --json`'s existing byte-for-byte output is untouched).
    #[serde(rename = "insideTargetArea", skip_serializing_if = "Option::is_none")]
    pub inside_target_area: Option<bool>,
}

#[derive(Serialize)]
pub struct SolveJson {
    pub target: [f32; 3],
    pub origins: usize,
    #[serde(rename = "emptyReason")]
    pub empty_reason: Option<String>,
    pub coverage: Vec<[i32; 5]>,
    pub lineups: Vec<LineupJson>,
}

pub fn type_name(t: ThrowType) -> &'static str {
    match t {
        ThrowType::Stand => "Stand",
        ThrowType::Crouch => "Crouch",
        ThrowType::JumpThrow => "JumpThrow",
        ThrowType::CrouchJumpThrow => "CrouchJumpThrow",
        ThrowType::RunJumpThrow => "RunJumpThrow",
    }
}

/// `LineupApi.cs:559-694`: the solve's JSON envelope, shared by `cs2mod solve --json` and
/// `POST /api/lineup`'s cached `result` line - moved here from `cmd_solver.rs::json_payload` so
/// the two can never drift apart. `target_area`: the query's `targetArea`, when it had one
/// (`s6g2_target_area.md`) - `None` for a point-target solve, so the CLI's own call site (which
/// never has one) keeps its existing byte-for-byte `--json` output.
pub fn json_payload(
    solve: &TargetSolve,
    ranked_list: &[RankedLineup],
    target_area: Option<&target::TargetArea>,
) -> SolveJson {
    let verified_at: std::collections::HashSet<(i32, i32)> = solve
        .lineups
        .iter()
        .map(|l| {
            (
                l.feet.x.round_ties_even() as i32,
                l.feet.y.round_ties_even() as i32,
            )
        })
        .collect();
    SolveJson {
        target: [solve.target.x, solve.target.y, solve.target.z],
        origins: solve.origins,
        empty_reason: solve.empty_reason.clone(),
        coverage: solve
            .coverage
            .iter()
            .map(|c| {
                let verified_here = verified_at.contains(&(c[0], c[1])) as i32;
                [c[0], c[1], c[2], verified_here, c[3]]
            })
            .collect(),
        lineups: ranked_list
            .iter()
            .take(400)
            .map(|rl| {
                let l = &rl.lineup;
                LineupJson {
                    id: rl.id.clone(),
                    feet: [l.feet.x, l.feet.y, l.feet.z],
                    yaw: l.yaw_deg,
                    pitch: l.pitch_deg,
                    type_: type_name(l.throw_type),
                    how: rl.describe.clone(),
                    strength: l.strength,
                    click: rl.click,
                    run_deg: l.run_yaw_offset_deg,
                    rest: [l.rest_point.x, l.rest_point.y, l.rest_point.z],
                    bounces: l.bounces,
                    flight_time: l.flight_time,
                    stability: l.stability,
                    scatter: l.rest_scatter,
                    pin: match rl.pin {
                        2 => Some("corner"),
                        1 => Some("wall"),
                        _ => None,
                    },
                    wall_gap: rl.wall_gap,
                    exposed: l.direct_los,
                    glass: l.glass_breaks,
                    rest_if_broken: l.rest_if_broken.map(|v| [v.x, v.y, v.z]),
                    state_dependent: rank::state_dependent(l),
                    human_error: rl.human_error,
                    aim_ref: AimRefJson {
                        tier: rl.aim_ref.tier(),
                        sky: rl.aim_ref.sky_fraction,
                        edge_deg: rl
                            .aim_ref
                            .nearest_silhouette_deg
                            .is_finite()
                            .then_some(rl.aim_ref.nearest_silhouette_deg),
                        reticle_deg: rl
                            .aim_ref
                            .nearest_reticle_deg
                            .is_finite()
                            .then_some(rl.aim_ref.nearest_reticle_deg),
                        band: rl.aim_ref.band(),
                        margin_deg: rl.aim_ref.margin_deg(),
                    },
                    console: rl.console.clone(),
                    inside_target_area: target_area.map(|a| {
                        target::point_in_area_polygon(&a.polygon, l.rest_point.x, l.rest_point.y)
                            && a.z_min.is_none_or(|lo| l.rest_point.z >= lo)
                            && a.z_max.is_none_or(|hi| l.rest_point.z <= hi)
                    }),
                }
            })
            .collect(),
    }
}

// ---- throw type parsing (shared with physics.rs) -----------------------------------------------

/// `Enum.TryParse<ThrowType>(..., ignoreCase: true)`.
pub(crate) fn parse_throw_type_ci(s: &str) -> Option<ThrowType> {
    Some(match () {
        _ if s.eq_ignore_ascii_case("Stand") => ThrowType::Stand,
        _ if s.eq_ignore_ascii_case("Crouch") => ThrowType::Crouch,
        _ if s.eq_ignore_ascii_case("JumpThrow") => ThrowType::JumpThrow,
        _ if s.eq_ignore_ascii_case("CrouchJumpThrow") => ThrowType::CrouchJumpThrow,
        _ if s.eq_ignore_ascii_case("RunJumpThrow") => ThrowType::RunJumpThrow,
        _ => return None,
    })
}

// ---- constants -----------------------------------------------------------------------------

/// `crates/cli/src/constants.rs::resolve_constants(None)`'s auto-load half, duplicated here
/// (that function also prints and takes a CLI `--constants` override the server has no flag
/// for): `data/throw-constants.json` relative to the cwd if it exists, else `sim`'s built-in
/// defaults - the same resolution `cs2mod solve` uses without `--constants`. Read once at server
/// startup (`AppState::constants`) rather than per request; a broken file fails the whole startup
/// with a readable message instead of silently falling back (previously every request re-read the
/// file and swallowed a parse error into the built-in defaults).
pub fn load_constants() -> Result<ThrowConstants, String> {
    let default = Path::new("data/throw-constants.json");
    if default.is_file() {
        ThrowConstants::load_or_default(Some(default))
            .map_err(|e| format!("failed to parse {}: {e}", default.display()))
    } else {
        Ok(ThrowConstants::default())
    }
}

// ---- validation (`LineupApi.cs:344-433`) -------------------------------------------------------

fn as_f32_finite(v: &Value) -> Option<f32> {
    let f = v.as_f64()?;
    let f = f as f32;
    f.is_finite().then_some(f)
}

/// `LineupApi.cs:344-433` (`ValidateLineupQuery`).
pub fn validate_lineup_query(query: &Value, mesh: &geom::mesh::CollisionMesh) -> Option<String> {
    if !query.is_object() {
        return Some("body must be a JSON object".to_string());
    }
    let (mesh_min, mesh_max) = mesh.bounds().unwrap_or(([0.0; 3], [0.0; 3]));
    // `s6g2_target_area.md`: `targetArea` replaces `target` outright (an area has no single point
    // to validate here) - the two are mutually exclusive, checked before either's own validation
    // runs so a request with both gets one clear error instead of whichever happened to run first.
    if query.get("targetArea").is_some() {
        if query.get("target").is_some() {
            return Some("target and targetArea are mutually exclusive".to_string());
        }
        if let Some(err) = validate_target_area(query, mesh_min, mesh_max) {
            return Some(err);
        }
    } else {
        let target_arr = match query.get("target").and_then(Value::as_array) {
            Some(a) if (2..=3).contains(&a.len()) => a,
            _ => return Some("target must be [x,y] or [x,y,z]".to_string()),
        };
        for el in target_arr {
            if as_f32_finite(el).is_none() {
                return Some("target coordinates must be finite numbers".to_string());
            }
        }
        let tx = as_f32_finite(&target_arr[0]).unwrap();
        let ty = as_f32_finite(&target_arr[1]).unwrap();
        if tx < mesh_min[0] - MAP_BOUNDS_MARGIN
            || tx > mesh_max[0] + MAP_BOUNDS_MARGIN
            || ty < mesh_min[1] - MAP_BOUNDS_MARGIN
            || ty > mesh_max[1] + MAP_BOUNDS_MARGIN
        {
            return Some("target is outside the map bounds".to_string());
        }
        if target_arr.len() == 3 {
            let tz = as_f32_finite(&target_arr[2]).unwrap();
            if tz < mesh_min[2] - MAP_BOUNDS_MARGIN || tz > mesh_max[2] + MAP_BOUNDS_MARGIN {
                return Some(format!(
                    "target z is outside the map bounds ({} to {} allowed)",
                    mesh_min[2] - MAP_BOUNDS_MARGIN,
                    mesh_max[2] + MAP_BOUNDS_MARGIN
                ));
            }
        }
        // `targetZMin`/`targetZMax` only mean anything alongside `targetArea`, but
        // `validate_target_area` still validates them on their own in this branch (its own
        // `None` early-return path) so a typo'd request fails fast here too.
        if let Some(err) = validate_target_area(query, mesh_min, mesh_max) {
            return Some(err);
        }
    }
    if let Some(origin) = query.get("origin") {
        let ok = origin
            .as_array()
            .is_some_and(|a| a.len() >= 2 && a.iter().all(|e| as_f32_finite(e).is_some()));
        if !ok {
            return Some("origin must be [x,y] with finite numbers".to_string());
        }
    }
    if let Some(err) = validate_origin_area(query, mesh_min, mesh_max) {
        return Some(err);
    }
    if let Some(scope) = query.get("scope") {
        let lower = scope.as_str().map(|s| s.to_ascii_lowercase());
        match lower.as_deref() {
            Some("spawns") | Some("exact") => {}
            _ => return Some("scope must be \"spawns\" or \"exact\"".to_string()),
        }
        if lower.as_deref() == Some("exact") && query.get("origin").is_none() {
            return Some("scope \"exact\" needs an origin".to_string());
        }
    }
    // `s6j_pin_filter.md`: restricts the search/result to wall/corner-pinned stand spots.
    if let Some(pin) = query.get("originPin")
        && !pin.is_null()
    {
        let lower = pin.as_str().map(|s| s.to_ascii_lowercase());
        match lower.as_deref() {
            Some("corner") | Some("wall") => {}
            _ => return Some("originPin must be \"corner\" or \"wall\"".to_string()),
        }
    }
    if let Some(reach) = query.get("originReach")
        && !as_f32_finite(reach).is_some_and(|v| (MIN_ORIGIN_REACH..=MAX_ORIGIN_REACH).contains(&v))
    {
        return Some(format!(
            "originReach must be between {MIN_ORIGIN_REACH} and {MAX_ORIGIN_REACH}"
        ));
    }
    if let Some(tol) = query.get("tolerance")
        && !as_f32_finite(tol).is_some_and(|v| (MIN_TOLERANCE..=MAX_TOLERANCE).contains(&v))
    {
        return Some(format!(
            "tolerance must be between {MIN_TOLERANCE} and {MAX_TOLERANCE}"
        ));
    }
    if let Some(stab) = query.get("minStability")
        && !as_f32_finite(stab).is_some_and(|v| (0.05..=1.0).contains(&v))
    {
        return Some("minStability must be between 0.05 and 1".to_string());
    }
    if let Some(fine) = query.get("fineScan")
        && !fine.is_boolean()
    {
        return Some("fineScan must be a boolean".to_string());
    }
    if let Some(types) = query.get("types") {
        let ok = types.as_array().is_some_and(|a| {
            (1..=5).contains(&a.len())
                && a.iter()
                    .all(|e| e.as_str().is_some_and(|s| parse_throw_type_ci(s).is_some()))
        });
        if !ok {
            return Some("types must be a non-empty array of throw type names".to_string());
        }
    }
    if let Some(strengths) = query.get("strengths") {
        let ok = strengths.as_array().is_some_and(|a| {
            (1..=3).contains(&a.len())
                && a.iter()
                    .all(|e| e.as_f64().is_some_and(|v| v == 0.0 || v == 0.5 || v == 1.0))
        });
        if !ok {
            return Some("strengths must be a non-empty array drawn from 0, 0.5, 1".to_string());
        }
    }
    if let Some(broken) = query.get("broken") {
        let ok = broken.as_array().is_some_and(|a| {
            a.len() <= 2
                && a.iter()
                    .all(|e| matches!(e.as_str(), Some("glass") | Some("doors")))
        });
        if !ok {
            return Some("broken must be an array drawn from \"glass\", \"doors\"".to_string());
        }
    }
    None
}

/// Shared polygon-only validation for `originArea` and `targetArea` (`s6g_origin_area.md`,
/// `s6g2_target_area.md`): 3..64 finite vertices inside the map bounds, a bounding box no larger
/// than `MAX_AREA_SPAN` per axis, and a non-degenerate (non-zero-area) shape. `field_name` names
/// the JSON field in error text. Returns the parsed polygon on success.
fn validate_area_polygon(
    field_name: &str,
    arr: &[Value],
    mesh_min: [f32; 3],
    mesh_max: [f32; 3],
) -> Result<Vec<[f32; 2]>, String> {
    if !(MIN_AREA_VERTICES..=MAX_AREA_VERTICES).contains(&arr.len()) {
        return Err(format!(
            "{field_name} must have between {MIN_AREA_VERTICES} and {MAX_AREA_VERTICES} vertices"
        ));
    }
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(arr.len());
    for v in arr {
        let ok = v
            .as_array()
            .is_some_and(|p| p.len() == 2 && p.iter().all(|e| as_f32_finite(e).is_some()));
        if !ok {
            return Err(format!(
                "{field_name} vertices must be [x,y] with finite numbers"
            ));
        }
        let p = v.as_array().unwrap();
        let (x, y) = (as_f32_finite(&p[0]).unwrap(), as_f32_finite(&p[1]).unwrap());
        if x < mesh_min[0] - MAP_BOUNDS_MARGIN
            || x > mesh_max[0] + MAP_BOUNDS_MARGIN
            || y < mesh_min[1] - MAP_BOUNDS_MARGIN
            || y > mesh_max[1] + MAP_BOUNDS_MARGIN
        {
            return Err(format!("{field_name} vertex is outside the map bounds"));
        }
        pts.push([x, y]);
    }
    let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in &pts {
        min_x = min_x.min(p[0]);
        max_x = max_x.max(p[0]);
        min_y = min_y.min(p[1]);
        max_y = max_y.max(p[1]);
    }
    if max_x - min_x > MAX_AREA_SPAN || max_y - min_y > MAX_AREA_SPAN {
        return Err(format!(
            "{field_name}'s bounding box cannot exceed {MAX_AREA_SPAN} units per axis"
        ));
    }
    // A signed (shoelace) area cancels out on a self-intersecting polygon with equal-area lobes
    // (a bowtie) even though it plainly encloses real space under the even-odd rule the solver
    // itself uses - test non-degeneracy directly instead: find the vertex farthest from the
    // first one, then the largest triangle (p0, far, p_i) over every vertex. That triangle can
    // only be near-zero if every vertex is (near-)collinear with p0/far, which is the actual
    // degenerate case (duplicate points, a segment, a sliver).
    let p0 = (pts[0][0] as f64, pts[0][1] as f64);
    let mut far = p0;
    let mut far_dist2 = 0.0f64;
    for p in &pts {
        let (px, py) = (p[0] as f64, p[1] as f64);
        let d2 = (px - p0.0).powi(2) + (py - p0.1).powi(2);
        if d2 > far_dist2 {
            far_dist2 = d2;
            far = (px, py);
        }
    }
    if far_dist2 < 1e-6 {
        return Err(format!("{field_name} must enclose a non-zero area"));
    }
    let (fx, fy) = (far.0 - p0.0, far.1 - p0.1);
    let mut max_cross = 0.0f64;
    for p in &pts {
        let (px, py) = (p[0] as f64 - p0.0, p[1] as f64 - p0.1);
        let cross = (fx * py - fy * px).abs();
        if cross > max_cross {
            max_cross = cross;
        }
    }
    if max_cross / 2.0 < MIN_AREA_AREA {
        return Err(format!("{field_name} must enclose a non-zero area"));
    }
    Ok(pts)
}

/// `s6g_origin_area.md`: `originArea` is a closed XY polygon, plus optional finite `zMin`/`zMax`
/// with `zMin <= zMax`; mutually exclusive with `origin`/`originReach`/`scope: "spawns"`.
fn validate_origin_area(query: &Value, mesh_min: [f32; 3], mesh_max: [f32; 3]) -> Option<String> {
    let Some(area) = query.get("originArea") else {
        // `zMin`/`zMax` only mean anything alongside `originArea`, but are still validated on
        // their own here so a typo'd request fails fast instead of silently doing nothing.
        for key in ["zMin", "zMax"] {
            if let Some(v) = query.get(key)
                && as_f32_finite(v).is_none()
            {
                return Some(format!("{key} must be a finite number"));
            }
        }
        if let (Some(lo), Some(hi)) = (
            query.get("zMin").and_then(as_f32_finite),
            query.get("zMax").and_then(as_f32_finite),
        ) && lo > hi
        {
            return Some("zMin must not be greater than zMax".to_string());
        }
        return None;
    };
    if query.get("origin").is_some() || query.get("originReach").is_some() {
        return Some("origin/originReach and originArea are mutually exclusive".to_string());
    }
    if query
        .get("scope")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("spawns"))
    {
        return Some("scope \"spawns\" and originArea are mutually exclusive".to_string());
    }
    let Some(arr) = area.as_array() else {
        return Some("originArea must be an array of [x,y] points".to_string());
    };
    if let Err(e) = validate_area_polygon("originArea", arr, mesh_min, mesh_max) {
        return Some(e);
    }
    for key in ["zMin", "zMax"] {
        if let Some(v) = query.get(key)
            && as_f32_finite(v).is_none()
        {
            return Some(format!("{key} must be a finite number"));
        }
    }
    if let (Some(lo), Some(hi)) = (
        query.get("zMin").and_then(as_f32_finite),
        query.get("zMax").and_then(as_f32_finite),
    ) && lo > hi
    {
        return Some("zMin must not be greater than zMax".to_string());
    }
    None
}

/// `s6g2_target_area.md`: `targetArea` is a closed XY polygon, plus optional finite
/// `targetZMin`/`targetZMax` with `targetZMin <= targetZMax`; mutually exclusive with `target`
/// (the caller skips `target`'s own mandatory-presence check when this field is present, and
/// rejects both being present at once).
fn validate_target_area(query: &Value, mesh_min: [f32; 3], mesh_max: [f32; 3]) -> Option<String> {
    let Some(area) = query.get("targetArea") else {
        for key in ["targetZMin", "targetZMax"] {
            if let Some(v) = query.get(key)
                && as_f32_finite(v).is_none()
            {
                return Some(format!("{key} must be a finite number"));
            }
        }
        if let (Some(lo), Some(hi)) = (
            query.get("targetZMin").and_then(as_f32_finite),
            query.get("targetZMax").and_then(as_f32_finite),
        ) && lo > hi
        {
            return Some("targetZMin must not be greater than targetZMax".to_string());
        }
        return None;
    };
    let Some(arr) = area.as_array() else {
        return Some("targetArea must be an array of [x,y] points".to_string());
    };
    if let Err(e) = validate_area_polygon("targetArea", arr, mesh_min, mesh_max) {
        return Some(e);
    }
    for key in ["targetZMin", "targetZMax"] {
        if let Some(v) = query.get(key)
            && as_f32_finite(v).is_none()
        {
            return Some(format!("{key} must be a finite number"));
        }
    }
    if let (Some(lo), Some(hi)) = (
        query.get("targetZMin").and_then(as_f32_finite),
        query.get("targetZMax").and_then(as_f32_finite),
    ) && lo > hi
    {
        return Some("targetZMin must not be greater than targetZMax".to_string());
    }
    None
}

/// `LineupApi.cs:439-445` (`BrokenGroups`): csv/array tokens from `{glass, doors}` to the mesh's
/// own attribute group names, deduped and Ordinal-sorted.
pub(crate) fn broken_groups_from_query(query: &Value) -> Vec<String> {
    let Some(arr) = query.get("broken").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut groups: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(|s| {
            if s == "glass" {
                "EntityBreakable".to_string()
            } else {
                "EntityDoor".to_string()
            }
        })
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

// ---- cache key (`LineupApi.cs:447-486`) --------------------------------------------------------

pub fn query_cache_key(
    map: &str,
    mesh_version: &str,
    constants: &ThrowConstants,
    query: &Value,
    attrs: &str,
    stand_spots: &str,
) -> String {
    let target = query
        .get("target")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tx = format!(
        "{:.1}",
        target.first().and_then(Value::as_f64).unwrap_or(0.0)
    );
    let ty = format!(
        "{:.1}",
        target.get(1).and_then(Value::as_f64).unwrap_or(0.0)
    );
    let tz = if target.len() > 2 {
        format!("{:.1}", target[2].as_f64().unwrap_or(0.0))
    } else {
        "none".to_string()
    };
    let origin = query
        .get("origin")
        .and_then(Value::as_array)
        .map(|a| {
            // The Z the thrower stands at picks which floor a query resolves against
            // (`SolveQuery::origin_z`); folding it into the key stops two requests that agree on
            // X/Y but differ on Z from colliding on one cache entry.
            let oz = if a.len() > 2 {
                format!("{:.1}", a[2].as_f64().unwrap_or(0.0))
            } else {
                "none".to_string()
            };
            format!(
                "{:.1},{:.1},{oz}",
                a.first().and_then(Value::as_f64).unwrap_or(0.0),
                a.get(1).and_then(Value::as_f64).unwrap_or(0.0)
            )
        })
        .unwrap_or_else(|| "all".to_string());
    let reach = query
        .get("originReach")
        .and_then(Value::as_f64)
        .unwrap_or(-1.0);
    let tol = query
        .get("tolerance")
        .and_then(Value::as_f64)
        .unwrap_or(80.0);
    let stab = query
        .get("minStability")
        .and_then(Value::as_f64)
        .unwrap_or(0.4);
    let fine = query
        .get("fineScan")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let types_key = query
        .get("types")
        .and_then(Value::as_array)
        .map(|a| {
            let mut v: Vec<String> = a
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_ascii_lowercase())
                .collect();
            v.sort();
            v.join(",")
        })
        .unwrap_or_else(|| "all".to_string());
    let strengths_key = query
        .get("strengths")
        .and_then(Value::as_array)
        .map(|a| {
            let mut v: Vec<String> = a
                .iter()
                .filter_map(Value::as_f64)
                .map(|f| format!("{f:.1}"))
                .collect();
            v.sort();
            v.join(",")
        })
        .unwrap_or_else(|| "all".to_string());
    let broken = broken_groups_from_query(query);
    let broken_key = if broken.is_empty() {
        "none".to_string()
    } else {
        broken.join(",")
    };
    let scope_key = query
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".to_string());
    // `s6j_pin_filter.md`: `originPin` picks a different origin set, same idiom as `scope_key`.
    let pin_key = query
        .get("originPin")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s == "corner" || s == "wall")
        .unwrap_or_else(|| "none".to_string());
    // `s6g_origin_area.md`: the polygon's own vertices (in order - a self-intersecting ring is a
    // different region than its reordered self, per the even-odd rule) at fixed precision, plus
    // its optional Z range; "none" when no `originArea` was given, same idiom as `origin` above.
    let origin_area_key = query
        .get("originArea")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    let p = v.as_array().cloned().unwrap_or_default();
                    format!(
                        "{:.1},{:.1}",
                        p.first().and_then(Value::as_f64).unwrap_or(0.0),
                        p.get(1).and_then(Value::as_f64).unwrap_or(0.0)
                    )
                })
                .collect::<Vec<_>>()
                .join(";")
        })
        .unwrap_or_else(|| "none".to_string());
    let zmin_key = query
        .get("zMin")
        .and_then(Value::as_f64)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "none".to_string());
    let zmax_key = query
        .get("zMax")
        .and_then(Value::as_f64)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "none".to_string());
    // `s6g2_target_area.md`: same idiom as `origin_area_key` above, for `targetArea` - keeps an
    // area-target query from colliding with the point-target `(tx,ty,tz)` segment above, which
    // otherwise stays `"0.0,0.0,none"` (the empty-array default) for every area-target request.
    let target_area_key = query
        .get("targetArea")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| {
                    let p = v.as_array().cloned().unwrap_or_default();
                    format!(
                        "{:.1},{:.1}",
                        p.first().and_then(Value::as_f64).unwrap_or(0.0),
                        p.get(1).and_then(Value::as_f64).unwrap_or(0.0)
                    )
                })
                .collect::<Vec<_>>()
                .join(";")
        })
        .unwrap_or_else(|| "none".to_string());
    let target_zmin_key = query
        .get("targetZMin")
        .and_then(Value::as_f64)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "none".to_string());
    let target_zmax_key = query
        .get("targetZMax")
        .and_then(Value::as_f64)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "none".to_string());
    let constants_json = serde_json::to_string(constants).unwrap_or_default();
    // `LineupApi.cs:483` buckets `reach`/`tolerance` to whole units and `minStability` to two
    // decimals; we deliberately format `reach`/`tolerance` to one decimal and `minStability` to
    // three instead, because the coarser reference buckets let two distinct requests (observed
    // live: `tolerance:80` and `tolerance:80.4`) collide on one cache file and answer from the
    // wrong query.
    let seed = format!(
        "v{CACHE_VERSION}|{map}|{mesh_version}|{constants_json}|{tx},{ty},{tz}|{origin}|{reach:.1}|{tol:.1}|{stab:.3}|{}|{types_key}|{strengths_key}|{broken_key}|{scope_key}|{pin_key}|{origin_area_key}|{zmin_key}|{zmax_key}|{target_area_key}|{target_zmin_key}|{target_zmax_key}|{attrs}|{stand_spots}",
        i32::from(fine)
    );
    let digest = Sha256::digest(seed.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..20].to_string()
}

/// The cache key's stand-spots segment: `none` when the map has no precomputed stand spots (the
/// solve falls back to nav-mesh origins), else the point count and step - so building
/// `standspots.json` for a map invalidates its previously-cached nav-origin answers instead of
/// serving them for the next 30 days regardless.
fn stand_spots_cache_key(entry: &MapEntry) -> String {
    match &entry.bundle.stand_spots {
        extract::mapdata::StandSpotsState::Loaded(cached) => {
            format!("{}@{:.2}", cached.spots.len(), cached.step)
        }
        _ => "none".to_string(),
    }
}

// ---- turning a validated query into a `SolveQuery` (`LineupApi.cs:497-554`) --------------------

fn stand_spots_of(entry: &MapEntry) -> Option<Vec<StandSpotOrigin>> {
    match &entry.bundle.stand_spots {
        extract::mapdata::StandSpotsState::Loaded(cached) => Some(
            cached
                .spots
                .iter()
                .map(|s| StandSpotOrigin {
                    feet: V3::new(s.feet[0], s.feet[1], s.feet[2]),
                    crouched: s.stance == "Crouching",
                })
                .collect(),
        ),
        _ => None,
    }
}

/// `LineupApi.cs:497-554` (`RunTargetQuery`'s query decoding), minus the broken-groups
/// attribute-filter adjustment (`solve_for_target` already folds `broken_groups` into its own
/// attribute mask, see `target.rs::effective_attribute_mask`). Returns the query plus the origin
/// click, kept separately for `rank::ranked`.
fn build_solve_query(
    query: &Value,
    spawn_fronts: Vec<V3>,
    spawn_points: Vec<V3>,
) -> (SolveQuery, Option<[f32; 2]>) {
    // `s6g2_target_area.md`: `targetArea` replaces `target` outright, validated mutually
    // exclusive with it already (`validate_target_area`/`validate_lineup_query`).
    let target = match query.get("targetArea").and_then(Value::as_array) {
        Some(arr) => {
            let polygon: Vec<[f32; 2]> = arr
                .iter()
                .map(|v| {
                    let p = v.as_array().expect("validated");
                    [as_f32_finite(&p[0]).unwrap(), as_f32_finite(&p[1]).unwrap()]
                })
                .collect();
            Target::Area(TargetArea {
                polygon,
                z_min: query.get("targetZMin").and_then(as_f32_finite),
                z_max: query.get("targetZMax").and_then(as_f32_finite),
            })
        }
        None => {
            let target_arr = query
                .get("target")
                .and_then(Value::as_array)
                .expect("validated");
            let tx = as_f32_finite(&target_arr[0]).unwrap();
            let ty = as_f32_finite(&target_arr[1]).unwrap();
            let has_z = target_arr.len() > 2;
            let tz = if has_z {
                as_f32_finite(&target_arr[2]).unwrap()
            } else {
                0.0
            };
            let tolerance = query
                .get("tolerance")
                .and_then(Value::as_f64)
                .map(|v| v as f32)
                .unwrap_or(80.0);
            Target::Point {
                pos: V3::new(tx, ty, tz),
                has_z,
                tolerance,
            }
        }
    };

    let scope = query
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let origin_arr = query.get("origin").and_then(Value::as_array);
    let has_origin = origin_arr.is_some();
    let origin_click =
        origin_arr.map(|a| [as_f32_finite(&a[0]).unwrap(), as_f32_finite(&a[1]).unwrap()]);
    let origin_z = origin_arr.and_then(|a| (a.len() > 2).then(|| as_f32_finite(&a[2]).unwrap()));
    let origin_reach = if has_origin {
        query
            .get("originReach")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or(300.0)
    } else {
        3100.0
    };
    let min_stability = query
        .get("minStability")
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .unwrap_or(0.4);
    let fine_scan = query
        .get("fineScan")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let types = query.get("types").and_then(Value::as_array).map(|a| {
        let mut seen = std::collections::HashSet::new();
        a.iter()
            .filter_map(|e| e.as_str().and_then(parse_throw_type_ci))
            .filter(|t| seen.insert(*t))
            .collect::<Vec<_>>()
    });
    let strengths = query.get("strengths").and_then(Value::as_array).map(|a| {
        let mut seen: Vec<f32> = Vec::new();
        a.iter()
            .filter_map(Value::as_f64)
            .map(|v| v as f32)
            .filter(|v| {
                if seen.contains(v) {
                    false
                } else {
                    seen.push(*v);
                    true
                }
            })
            .collect::<Vec<_>>()
    });
    let broken_groups = broken_groups_from_query(query);
    // `s6g_origin_area.md`: an alternative to `origin`/`originReach`, validated mutually
    // exclusive with them already (`validate_origin_area`).
    let origin_area = query
        .get("originArea")
        .and_then(Value::as_array)
        .map(|arr| {
            let polygon: Vec<[f32; 2]> = arr
                .iter()
                .map(|v| {
                    let p = v.as_array().expect("validated");
                    [as_f32_finite(&p[0]).unwrap(), as_f32_finite(&p[1]).unwrap()]
                })
                .collect();
            OriginArea {
                polygon,
                z_min: query.get("zMin").and_then(as_f32_finite),
                z_max: query.get("zMax").and_then(as_f32_finite),
            }
        });
    // `s6j_pin_filter.md`: validated to "corner"/"wall"/absent-or-null already.
    let origin_pin_min: u8 = match query.get("originPin").and_then(Value::as_str) {
        Some(s) if s.eq_ignore_ascii_case("corner") => 2,
        Some(s) if s.eq_ignore_ascii_case("wall") => 1,
        _ => 0,
    };

    let q = SolveQuery {
        target,
        origin_click,
        origin_z,
        origin_reach,
        origin_area,
        min_stability,
        fine_scan,
        types,
        strengths,
        broken_groups,
        spawn_fronts,
        spawn_points,
        spawn_scope_radius: 0.0,
        spawns_only: scope == "spawns",
        exact_origin: scope == "exact",
        referee: false,
        origin_pin_min,
    };
    (q, origin_click)
}

/// `MapRegistry.cs:299-317` (`SpawnFronts`)/(`SpawnPoints`): one representative point per side,
/// and every spawn, straight from the map's raw (un-grounded) entity positions - matching
/// `cmd_solver.rs::solve`'s own spawn handling, not `MapEntry::grounded_spawns` (which floor-drops
/// for `/api/spawns`' display only).
fn spawn_fronts_and_points(entry: &MapEntry) -> (Vec<V3>, Vec<V3>) {
    let t = &entry.bundle.spawns.t;
    let ct = &entry.bundle.spawns.ct;
    let mut fronts = Vec::new();
    if !t.is_empty() {
        fronts.push(t[t.len() / 2]);
    }
    if !ct.is_empty() {
        fronts.push(ct[ct.len() / 2]);
    }
    let mut points = t.clone();
    points.extend(ct.iter().copied());
    (fronts, points)
}

// ---- phase names (`s6d_solve_api.md`'s table) --------------------------------------------------

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Prepare => "prepare",
        Phase::Colliders => "colliders",
        Phase::Origins => "origins",
        Phase::PinnedOrigins => "pinned-origins",
        Phase::AfterPins => "after-pins",
        Phase::Sweep => "sweep",
        Phase::Verify => "verify",
        Phase::Exhaustive => "exhaustive",
        Phase::Escalate => "escalate",
        Phase::EscalateFine => "escalate-fine",
        Phase::Sightline => "sightline",
        Phase::Pins => "pins",
    }
}

// ---- the NDJSON body stream ---------------------------------------------------------------------

/// Wraps a `tokio::sync::mpsc::Receiver` as a `futures_core::Stream`, so `Body::from_stream` can
/// drive it; a disconnect (the client leaving) drops the `Body`, which drops this and the
/// `Receiver` inside it, which is what turns a subsequent `Sender::send` on the writer side into
/// an error - the signal `run_lineup_stream` uses to set `cancel`.
pub(crate) struct RxStream(pub(crate) tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>);

impl futures_core::Stream for RxStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

pub(crate) type LineSender = tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>;

pub(crate) async fn send_line(tx: &LineSender, mut line: String) -> Result<(), ()> {
    line.push('\n');
    tx.send(Ok(Bytes::from(line))).await.map_err(|_| ())
}

/// One `SolveHooks` callback firing, tagged so the drain loop below can batch consecutive
/// same-kind events into one line without losing their order relative to phase changes
/// (`ServeCommand.cs`'s `DrainProgress`).
enum SolveEvent {
    Phase(Phase, usize),
    /// `on_origin`: feet (rounded) and the number of throws that landed in the zone.
    Origin(i64, i64, i64, i64),
    /// `on_candidate`: feet (rounded) and whether it survived verification (0/1).
    Candidate(i64, i64, i64, i64),
}

fn round_i64(v: f32) -> i64 {
    v.round_ties_even() as i64
}

/// `{"checked":[[x,y,z,hits], ...]}` / `{"verified":[[x,y,z,ok], ...]}`.
fn points_line(kind: &str, points: &[[i64; 4]]) -> String {
    let mut s = format!("{{\"{kind}\":[");
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("[{},{},{},{}]", p[0], p[1], p[2], p[3]));
    }
    s.push_str("]}");
    s
}

async fn flush_batch(
    tx: &LineSender,
    cancel: &AtomicBool,
    kind: &str,
    batch: &mut Vec<[i64; 4]>,
    max_points_per_line: usize,
) {
    for chunk in batch.chunks(max_points_per_line) {
        if send_line(tx, points_line(kind, chunk)).await.is_err() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    batch.clear();
}

/// Drains every `SolveEvent` currently queued, sending a `phase` line per phase event and
/// batching consecutive `Origin`/`Candidate` runs into `checked`/`verified` lines
/// (`ServeCommand.cs:1780-1811`). Called on a ~100ms tick and once more after the solve finishes.
async fn drain_events(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SolveEvent>,
    tx: &LineSender,
    cancel: &AtomicBool,
    truncated: &AtomicBool,
    truncated_sent: &mut bool,
    max_points_per_line: usize,
) {
    let mut batch_kind: Option<&'static str> = None;
    let mut batch: Vec<[i64; 4]> = Vec::new();
    while let Ok(ev) = event_rx.try_recv() {
        match ev {
            SolveEvent::Phase(phase, count) => {
                if let Some(k) = batch_kind.take() {
                    flush_batch(tx, cancel, k, &mut batch, max_points_per_line).await;
                }
                let line = format!("{{\"phase\":\"{}\",\"count\":{count}}}", phase_name(phase));
                if send_line(tx, line).await.is_err() {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            SolveEvent::Origin(x, y, z, hits) => {
                if batch_kind != Some("checked") {
                    if let Some(k) = batch_kind.take() {
                        flush_batch(tx, cancel, k, &mut batch, max_points_per_line).await;
                    }
                    batch_kind = Some("checked");
                }
                batch.push([x, y, z, hits]);
            }
            SolveEvent::Candidate(x, y, z, ok) => {
                if batch_kind != Some("verified") {
                    if let Some(k) = batch_kind.take() {
                        flush_batch(tx, cancel, k, &mut batch, max_points_per_line).await;
                    }
                    batch_kind = Some("verified");
                }
                batch.push([x, y, z, ok]);
            }
        }
    }
    if let Some(k) = batch_kind.take() {
        flush_batch(tx, cancel, k, &mut batch, max_points_per_line).await;
    }
    if truncated.load(Ordering::Relaxed) && !*truncated_sent {
        *truncated_sent = true;
        if send_line(tx, "{\"phase\":\"progress-truncated\"}".to_string())
            .await
            .is_err()
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Reads `<cache>/<key>.json` if present.
async fn read_cache(cache_dir: &Path, key: &str) -> Option<String> {
    tokio::fs::read_to_string(cache_dir.join(format!("{key}.json")))
        .await
        .ok()
}

/// Atomic write: a pid-named temp file, then rename (`config::save_at`'s own idiom).
fn write_cache_atomic(cache_dir: &Path, key: &str, json: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let path = cache_dir.join(format!("{key}.json"));
    let tmp = cache_dir.join(format!("{key}.json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// At server startup: drops cached solves older than 30 days, then - if what is left still adds
/// up to more than 1 GiB - evicts the oldest until it does not (`s6d_solve_api.md`'s cache
/// cleanup, our own budget; `MapRegistry.cs:209-248,431-495` do the reference's own version of
/// this, on a different schedule).
pub fn prune_cache(cache_dir: &Path) {
    let Ok(read) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(".json.tmp-") {
            // Orphaned by a crash between the write and the rename (`write_cache_atomic`); never
            // valid to read and never counted against the budget, so it would otherwise sit there
            // forever.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().unwrap_or(now);
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > CACHE_MAX_AGE)
        {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        files.push((path, modified, meta.len()));
    }
    let mut total: u64 = files.iter().map(|(_, _, len)| len).sum();
    if total <= CACHE_BUDGET_BYTES {
        return;
    }
    files.sort_by_key(|(_, modified, _)| *modified);
    for (path, _, len) in files {
        if total <= CACHE_BUDGET_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// Runs the solve, forwarding phase progress and the final `result`/`error` line to `tx`. Caches
/// the answer only when it finished uncancelled. `cancel` is shared with the caller, which sets
/// it when a write to `tx` fails (the client left).
#[allow(clippy::too_many_arguments)]
async fn run_solve_and_stream(
    map_data: MapData,
    query: SolveQuery,
    origin_click: Option<[f32; 2]>,
    constants: ThrowConstants,
    cache_dir: PathBuf,
    cache_key: String,
    tx: LineSender,
    cancel: Arc<AtomicBool>,
    max_stream_points: usize,
    max_points_per_line: usize,
) {
    // `query` moves into `spawn_blocking`'s closure below, so this is captured up front for
    // `json_payload`'s own use once the solve is done (`s6g2_target_area.md`'s `insideTargetArea`).
    let target_area_for_json: Option<TargetArea> = match &query.target {
        Target::Area(area) => Some(area.clone()),
        Target::Point { .. } => None,
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SolveEvent>();
    let total_points = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let truncated_for_drain = truncated.clone();
    let cancel_for_blocking = cancel.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let progress_tx = event_tx.clone();
        let progress = move |phase: Phase, count: usize| {
            let _ = progress_tx.send(SolveEvent::Phase(phase, count));
        };
        let origin_tx = event_tx.clone();
        let total_points_o = total_points.clone();
        let truncated_o = truncated.clone();
        let on_origin = move |feet: V3, hits: usize| {
            if truncated_o.load(Ordering::Relaxed) {
                return;
            }
            if total_points_o.fetch_add(1, Ordering::Relaxed) >= max_stream_points {
                truncated_o.store(true, Ordering::Relaxed);
                return;
            }
            let _ = origin_tx.send(SolveEvent::Origin(
                round_i64(feet.x),
                round_i64(feet.y),
                round_i64(feet.z),
                hits as i64,
            ));
        };
        let candidate_tx = event_tx.clone();
        let total_points_c = total_points.clone();
        let truncated_c = truncated.clone();
        let on_candidate = move |feet: V3, ok: bool| {
            if truncated_c.load(Ordering::Relaxed) {
                return;
            }
            if total_points_c.fetch_add(1, Ordering::Relaxed) >= max_stream_points {
                truncated_c.store(true, Ordering::Relaxed);
                return;
            }
            let _ = candidate_tx.send(SolveEvent::Candidate(
                round_i64(feet.x),
                round_i64(feet.y),
                round_i64(feet.z),
                i64::from(ok),
            ));
        };
        let hooks = SolveHooks {
            progress: &progress,
            on_origin: Some(&on_origin),
            on_candidate: Some(&on_candidate),
        };
        target::solve_for_target(&map_data, &query, &constants, &hooks, &cancel_for_blocking)
    });

    let mut handle = handle;
    let mut handle_done = false;
    let mut solve_result = None;
    let mut ticker = tokio::time::interval(PROGRESS_DRAIN_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut truncated_sent = false;
    while !handle_done {
        tokio::select! {
            res = &mut handle, if !handle_done => {
                solve_result = Some(res);
                handle_done = true;
            }
            _ = ticker.tick() => {
                // A window with no `checked`/`verified` lines (e.g. after truncation, or a long
                // exhaustive-branch stretch) would otherwise leave a disconnect unnoticed until
                // the next line write fails - which may be minutes away. Poll `tx` directly on
                // every tick so cancellation is armed no later than one drain interval after the
                // client leaves (`ServeCommand.cs:1700-1712`'s `context.RequestAborted`).
                if tx.is_closed() {
                    cancel.store(true, Ordering::Relaxed);
                }
                drain_events(&mut event_rx, &tx, &cancel, &truncated_for_drain, &mut truncated_sent, max_points_per_line).await;
            }
        }
    }
    drain_events(
        &mut event_rx,
        &tx,
        &cancel,
        &truncated_for_drain,
        &mut truncated_sent,
        max_points_per_line,
    )
    .await;
    let solve_result = solve_result.expect("handle awaited exactly once above");

    match solve_result {
        Ok(solve) => {
            if solve.empty_reason.as_deref() == Some("cancelled") {
                return;
            }
            let ranked = rank::ranked(&solve, origin_click);
            let payload = json_payload(&solve, &ranked, target_area_for_json.as_ref());
            let Ok(json_text) = serde_json::to_string(&payload) else {
                let _ = send_line(
                    &tx,
                    "{\"error\":\"solver failure - check server log\"}".to_string(),
                )
                .await;
                return;
            };
            if let Err(e) = write_cache_atomic(&cache_dir, &cache_key, &json_text) {
                eprintln!("lineup cache write failed: {e}");
            }
            let _ = send_line(&tx, format!("{{\"result\":{json_text}}}")).await;
        }
        Err(e) => {
            eprintln!("lineup solve failed: {e}");
            let _ = send_line(
                &tx,
                "{\"error\":\"solver failure - check server log\"}".to_string(),
            )
            .await;
        }
    }
}

pub(crate) fn ndjson_response(body: Body) -> Response {
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    resp
}

/// `POST /api/lineup`'s handler body, called from `routes.rs` once the request has already
/// passed the content-type/body-size gate. `entry`/`body_json` are already resolved/parsed.
pub(crate) async fn post_lineup(
    state: Arc<AppState>,
    entry: Arc<MapEntry>,
    body_json: Value,
) -> Response {
    if entry.bundle.nav_areas.is_empty() {
        return api_error(StatusCode::NOT_FOUND, "map has no nav data (see /api/maps)");
    }
    let grenade = body_json
        .get("grenade")
        .and_then(Value::as_str)
        .unwrap_or("smoke");
    if grenade != "smoke" {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "only smoke is implemented so far (stage 7 adds flash, HE, molotov and decoy)",
        );
    }
    if let Some(err) = validate_lineup_query(&body_json, &entry.bundle.mesh) {
        return api_error(StatusCode::BAD_REQUEST, err);
    }

    let constants = state.constants;
    let cache_dir = state.solve_cache_dir();
    let cache_key = query_cache_key(
        &entry.map,
        &entry.etag,
        &constants,
        &body_json,
        SINGLE_TARGET_DEFAULT_ATTRS_STR,
        &stand_spots_cache_key(&entry),
    );

    if let Some(cached) = read_cache(&cache_dir, &cache_key).await {
        return ndjson_response(Body::from(format!("{{\"result\":{cached}}}\n")));
    }

    let ahead = state.solve_queue.fetch_add(1, Ordering::Relaxed);
    if ahead >= MAX_QUEUED_SOLVES {
        state.solve_queue.fetch_sub(1, Ordering::Relaxed);
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many solves queued - try again in a moment",
        );
    }

    let attribute_filter = Some(geom::filter::names_mask(
        &entry.bundle.mesh,
        &SINGLE_TARGET_DEFAULT_ATTRS,
    ));
    let (spawn_fronts, spawn_points) = spawn_fronts_and_points(&entry);
    let (solve_query, origin_click) = build_solve_query(&body_json, spawn_fronts, spawn_points);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let body = Body::from_stream(RxStream(rx));
    let response = ndjson_response(body);

    tokio::spawn(async move {
        if send_line(&tx, format!("{{\"phase\":\"queued\",\"count\":{ahead}}}"))
            .await
            .is_err()
        {
            // The client left before we even started waiting for a permit - don't hold a queue
            // slot for a solve nobody will read.
            state.solve_queue.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        let Ok(_permit) = state.solve_semaphore.acquire().await else {
            state.solve_queue.fetch_sub(1, Ordering::Relaxed);
            return;
        };
        state.solve_queue.fetch_sub(1, Ordering::Relaxed);

        if tx.is_closed() {
            // The client left while queued; the permit is ours now, but starting a solve that
            // would die on its first written line just wastes one of the two concurrent slots.
            return;
        }

        // A double-submit (two tabs, a re-click) may have solved and cached while this request
        // waited for a permit (`ServeCommand.cs:1680-1684`).
        if let Some(cached) = read_cache(&cache_dir, &cache_key).await {
            let _ = send_line(&tx, format!("{{\"result\":{cached}}}")).await;
            return;
        }

        let map_data = MapData {
            mesh: entry.bundle.mesh.clone(),
            nav_areas: entry.bundle.nav_areas.clone(),
            stand_spots: stand_spots_of(&entry),
            spawns: entry
                .bundle
                .spawns
                .t
                .iter()
                .chain(&entry.bundle.spawns.ct)
                .copied()
                .collect(),
            attribute_filter: attribute_filter.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        run_solve_and_stream(
            map_data,
            solve_query,
            origin_click,
            constants,
            cache_dir,
            cache_key,
            tx,
            cancel,
            state.max_stream_points,
            state.max_points_per_line,
        )
        .await;
    });

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::mesh::CollisionMesh;
    use serde_json::json;

    #[test]
    fn validate_rejects_out_of_range_tolerance() {
        let mesh = CollisionMesh::new();
        let q = json!({ "target": [0.0, 0.0], "tolerance": 0.5 });
        assert!(
            validate_lineup_query(&q, &mesh)
                .unwrap()
                .contains("tolerance must be between")
        );
    }

    #[test]
    fn validate_accepts_a_minimal_query() {
        let mesh = CollisionMesh::new();
        let q = json!({ "target": [0.0, 0.0] });
        assert_eq!(validate_lineup_query(&q, &mesh), None);
    }

    #[test]
    fn validate_rejects_target_far_below_mesh_z() {
        let mut mesh = CollisionMesh::new();
        mesh.vertices = vec![[-100.0, -100.0, -448.0], [100.0, 100.0, 1024.0]];
        let q = json!({ "target": [0.0, 0.0, -1400.0] });
        assert!(
            validate_lineup_query(&q, &mesh)
                .unwrap()
                .contains("target z is outside the map bounds")
        );
    }

    /// A self-intersecting bowtie with equal-area lobes has zero *signed* (shoelace) area even
    /// though it plainly encloses real space under the even-odd rule - the non-degeneracy check
    /// must not reject it (a bare shoelace check used to).
    #[test]
    fn validate_origin_area_accepts_a_self_intersecting_bowtie_with_equal_lobes() {
        let q = json!({
            "originArea": [[-1300.0, -1100.0], [-1000.0, -800.0], [-1300.0, -800.0], [-1000.0, -1100.0]]
        });
        let mesh_min = [-2000.0, -2000.0, -2000.0];
        let mesh_max = [2000.0, 2000.0, 2000.0];
        assert_eq!(validate_origin_area(&q, mesh_min, mesh_max), None);
    }

    #[test]
    fn validate_accepts_target_just_inside_the_widened_z_range() {
        let mut mesh = CollisionMesh::new();
        mesh.vertices = vec![[-100.0, -100.0, -448.0], [100.0, 100.0, 1024.0]];
        let q = json!({ "target": [0.0, 0.0, -900.0] });
        assert_eq!(validate_lineup_query(&q, &mesh), None);
    }

    #[test]
    fn cache_key_differs_by_tolerance() {
        let mesh = CollisionMesh::new();
        let constants = ThrowConstants::default();
        let a = query_cache_key(
            "de_test",
            "abc",
            &constants,
            &json!({ "target": [0.0, 0.0], "tolerance": 80.0 }),
            "attrs",
            "none",
        );
        let b = query_cache_key(
            "de_test",
            "abc",
            &constants,
            &json!({ "target": [0.0, 0.0], "tolerance": 90.0 }),
            "attrs",
            "none",
        );
        assert_ne!(a, b);
        let _ = mesh;
    }

    #[test]
    fn cache_key_differs_by_origin_z() {
        let constants = ThrowConstants::default();
        let a = query_cache_key(
            "de_mirage",
            "abc",
            &constants,
            &json!({ "target": [-1227.0, -1072.0, -168.0], "origin": [-1227.0, -1072.0, -168.0] }),
            "attrs",
            "none",
        );
        let b = query_cache_key(
            "de_mirage",
            "abc",
            &constants,
            &json!({ "target": [-1227.0, -1072.0, -168.0], "origin": [-1227.0, -1072.0, -40.0] }),
            "attrs",
            "none",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_differs_by_stand_spots() {
        let constants = ThrowConstants::default();
        let q = json!({ "target": [0.0, 0.0] });
        let a = query_cache_key("de_test", "abc", &constants, &q, "attrs", "none");
        let b = query_cache_key("de_test", "abc", &constants, &q, "attrs", "512@24.00");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_differs_by_origin_pin() {
        let constants = ThrowConstants::default();
        let none = query_cache_key(
            "de_test",
            "abc",
            &constants,
            &json!({ "target": [0.0, 0.0] }),
            "attrs",
            "none",
        );
        let wall = query_cache_key(
            "de_test",
            "abc",
            &constants,
            &json!({ "target": [0.0, 0.0], "originPin": "wall" }),
            "attrs",
            "none",
        );
        let corner = query_cache_key(
            "de_test",
            "abc",
            &constants,
            &json!({ "target": [0.0, 0.0], "originPin": "corner" }),
            "attrs",
            "none",
        );
        assert_ne!(none, wall);
        assert_ne!(none, corner);
        assert_ne!(wall, corner);
    }
}
