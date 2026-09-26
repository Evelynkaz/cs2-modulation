//! Target solve orchestration: resolves a click/target into a landing zone,
//! builds the region's colliders, gathers origins, sweeps, verifies,
//! escalates and computes the referee/miss explanation. Ported from
//! `cs2-smoke-solver/src/Cli/Services/TargetSolver.cs` (whole file).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use geom::bvh::Bvh;
use geom::collider::Collider;
use geom::filter::{AttributeMask, all_mask, from_fn, grenade_mask, player_mask};
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::{CollisionAttribute, CollisionMesh};
use geom::voxel::VoxelGrid;
use rayon::prelude::*;
use sim::{ThrowConstants, ThrowSpec, ThrowType, eye_height, forward_from_angles, simulate_voxel};

use crate::lineup::Lineup;
use crate::standspots::float_lerp;
use crate::{nav_ground, origins, sweep, verify, zone};

/// `TargetSolver.cs:15` (`DefenderEyeHeight`).
const DEFENDER_EYE_HEIGHT: f32 = 55.0;
/// `TargetSolver.cs:18` (`SpawnDrop`).
const SPAWN_DROP: f32 = 512.0;
/// `TargetSolver.cs:30` (`SpawnSnap`).
const SPAWN_SNAP: f32 = 32.0;
/// `TargetSolver.cs:33` (`ClickKindRadius`).
const CLICK_KIND_RADIUS: f32 = 24.0;
/// `TargetSolver.cs:630` (`TargetWallClearance`).
const TARGET_WALL_CLEARANCE: f32 = 8.0;
/// `TargetSolver.cs:631` (`TargetFloorDrop`).
const TARGET_FLOOR_DROP: f32 = 96.0;
/// `TargetSolver.cs:142` (`voxelSize`), hardcoded independently of the
/// caller's own `--voxel` option.
const VOXEL_SIZE: f32 = 16.0;
/// `s6l_aim_precision.md`: `q.precise_aim`'s aim-window step, in place of
/// `verify::VerifyOptions`'s default (`STEP_DEG` 0.6°).
const PRECISE_STEP_DEG: f32 = 0.2;
/// `s6l_aim_precision.md`: `q.precise_aim`'s aim-window half-width, in place of
/// `verify::VerifyOptions`'s default (`AIM_REACH` 2) - a 9x9 lattice, ±0.8°.
const PRECISE_AIM_REACH: i32 = 4;

/// `Target::Area`'s per-`(origin, throw kind)` sweep bucket depth - see `keep_per_bucket` on
/// `sweep::SweepOptions` and the round-based verify loop below. 32, not a smaller depth: a
/// bucket's own candidates are ordered by bounces then flight time (`sweep::insert_ranked`'s
/// `better`), which puts the near-edge, marginal landings first - those are exactly the ones
/// most likely to fail exact verification (a voxel rest inside an edge-admitted column, or on a
/// ledge, can slide back outside once re-simulated exactly). Measured on real Mirage (P1 vs A1z,
/// review G2 round 3): missing buckets went 15 at depth 4, 6 at 8, 1 at 16, 0 at 26 and 32, with
/// server CPU time flat across all of them (A1z ~102-103s, Narrow ~1080s at every depth) - the
/// extra candidates a bucket already generated cost nothing extra to verify, so there is no
/// real trade-off to shallowing this back down.
///
/// 256, not 32 (review G2 round 4): one cluster of false voxel hits can fill a whole bucket.
/// For the point target (-1300,-1072) inside A1z, bucket (-144,-100) RunJumpThrow/1/0 kept 32
/// candidates that were all 5-bounce voxel rests on the z=-128 platform lip whose exact
/// re-simulation lands elsewhere; the 33rd passed. With `fineScan` the first passing candidates
/// of other buckets sat at ranks 33, 51, 64 and 125. At 256 every resolvable bucket resolves
/// (A1z 266 lineups at normal and fine scan); with the binary-search `insert_ranked` the Narrow
/// sweep costs about the same as depth 32 did (143 s -> 146 s).
const AREA_BUCKET_CANDIDATES: usize = 256;

/// `s6q_robust_aim.md`: caps how many of `verify_exact`'s own best-ranked precise-mode survivors
/// get the expensive (~169+25 exact sims each) robust-aim-centering pass - anything past this rank
/// is left untouched rather than analyzed, so the pass's own cost stays bounded regardless of how
/// many candidates a solve verifies.
const ROBUST_MAX_LINEUPS: usize = 600;

const ALL_TYPES: [ThrowType; 5] = [
    ThrowType::Stand,
    ThrowType::Crouch,
    ThrowType::JumpThrow,
    ThrowType::CrouchJumpThrow,
    ThrowType::RunJumpThrow,
];

/// A precomputed stand spot, taken literally. `StandSpotOrigin` in the
/// reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StandSpotOrigin {
    pub feet: V3,
    pub crouched: bool,
}

/// Everything the target solver needs about one map, loaded once and reused
/// across solves.
pub struct MapData {
    pub mesh: CollisionMesh,
    /// Each nav area's corner ring.
    pub nav_areas: Vec<Vec<V3>>,
    pub stand_spots: Option<Vec<StandSpotOrigin>>,
    /// Every T+CT spawn on the map (`MapRegistry.cs:SpawnPoints`), 2v2 ones
    /// excluded. `solve_for_target` itself only reads the per-query
    /// `SolveQuery::spawn_fronts`/`spawn_points` (a caller derives those
    /// from this list, e.g. `cs2mod solve`'s `--spawns`); this field is
    /// reserved for a stage-6 server that wants "every spawn" without
    /// re-parsing `entities.json` per request.
    pub spawns: Vec<V3>,
    /// The vision/world-state attribute restriction (`--attrs`); `None`
    /// means every triangle counts as solid ground for the target grid
    /// (`VoxelGrid.Build`'s own `null` default, `VoxelGrid.cs:95,116`).
    pub attribute_filter: Option<AttributeMask>,
}

/// A polygon in the XY plane (plus an optional Z range) restricting where a thrower may stand,
/// instead of `origin_click`/`origin_reach`'s circle (`s6g_origin_area.md`). Mutually exclusive
/// with `origin_click` - the caller (`crates/server/src/solve.rs`'s validation) rejects a query
/// that sets both.
#[derive(Debug, Clone)]
pub struct OriginArea {
    /// At least 3 vertices, in order; winding does not matter - `point_in_area_polygon` uses the
    /// even-odd rule, which is winding-independent.
    pub polygon: Vec<[f32; 2]>,
    pub z_min: Option<f32>,
    pub z_max: Option<f32>,
}

impl OriginArea {
    /// The polygon's own AABB, used in place of `origin_click`'s reach box for the region bounds
    /// and the nav-origin scan box below (`s6g_origin_area.md`: "рамка многоугольника вместо
    /// круга").
    fn bounds_xy(&self) -> ([f32; 2], [f32; 2]) {
        polygon_bounds_xy(&self.polygon)
    }
}

/// A polygon in the XY plane (plus an optional Z range) a lineup's landing spot must fall inside
/// to count, instead of `Target::Point`'s point-and-tolerance circle (`s6g2_target_area.md`).
/// Same shape and polygon rules as `OriginArea` (even-odd, boundary inside) - a different type
/// because the two are unrelated concepts (where a thrower may stand vs. where the grenade must
/// land) validated and consumed independently.
#[derive(Debug, Clone)]
pub struct TargetArea {
    pub polygon: Vec<[f32; 2]>,
    pub z_min: Option<f32>,
    pub z_max: Option<f32>,
}

impl TargetArea {
    fn bounds_xy(&self) -> ([f32; 2], [f32; 2]) {
        polygon_bounds_xy(&self.polygon)
    }
}

/// The solve's target: an exact point with a landing tolerance (as before `s6g2_target_area.md`),
/// or an area the grenade just needs to come to rest anywhere inside (new). Kept as an enum
/// rather than another bag of `Option` fields on `SolveQuery` because stage 7 adds a third kind
/// (a target point in mid-air, for flashes/HE) - see this type's own call sites in
/// `solve_for_target` for what each variant means at every point the old single `V3` target was
/// read (region bounds, origin-click fallback, zone, aim/verify, ranking, exhaustive search).
pub enum Target {
    Point {
        pos: V3,
        has_z: bool,
        tolerance: f32,
    },
    Area(TargetArea),
}

fn polygon_bounds_xy(polygon: &[[f32; 2]]) -> ([f32; 2], [f32; 2]) {
    let mut min = [f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];
    for p in polygon {
        min[0] = min[0].min(p[0]);
        min[1] = min[1].min(p[1]);
        max[0] = max[0].max(p[0]);
        max[1] = max[1].max(p[1]);
    }
    (min, max)
}

/// Whether `(a, b)` - one polygon edge - passes within `eps` of `(x, y)`, used by
/// `point_in_area_polygon` to give boundary points a definite answer instead of leaving them to
/// float-rounding luck in the ray-cast below.
fn point_on_segment(a: [f32; 2], b: [f32; 2], x: f32, y: f32, eps: f32) -> bool {
    let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
    let len = (ex * ex + ey * ey).sqrt();
    if len < 1e-6 {
        return (x - a[0]).abs() <= eps && (y - a[1]).abs() <= eps;
    }
    let (px, py) = (x - a[0], y - a[1]);
    let cross = (ex * py - ey * px) / len; // signed perpendicular distance
    if cross.abs() > eps {
        return false;
    }
    let t = (px * ex + py * ey) / (len * len);
    (-eps / len..=1.0 + eps / len).contains(&t)
}

/// Even-odd point-in-polygon test with `s6g_origin_area.md`'s explicit boundary rule ("точки на
/// границе — внутри"): every edge is checked for an exact hit first, so a self-intersecting
/// polygon still classifies its own edges as inside regardless of how the ray-cast below would
/// otherwise count crossings there. Shared by `OriginArea` (where a thrower may stand) and
/// `TargetArea` (where the grenade must land, `s6g2_target_area.md`) - same rule, both places.
pub fn point_in_area_polygon(polygon: &[[f32; 2]], x: f32, y: f32) -> bool {
    const EPS: f32 = 0.01;
    let n = polygon.len();
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        if point_on_segment(a, b, x, y, EPS) {
            return true;
        }
    }
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

/// Whether `(x, y)` is within `eps` of any edge of `polygon` - used to admit an `area_target_zone`
/// column whose cell center falls just outside the polygon but still overlaps it.
fn near_polygon_edge(polygon: &[[f32; 2]], x: f32, y: f32, eps: f32) -> bool {
    let n = polygon.len();
    for i in 0..n {
        if point_on_segment(polygon[i], polygon[(i + 1) % n], x, y, eps) {
            return true;
        }
    }
    false
}

/// Whether `z` falls inside an optional `[z_min, z_max]` height range - both ends open by
/// default. Shared by `OriginArea` and `TargetArea`.
fn z_in_range(z_min: Option<f32>, z_max: Option<f32>, z: f32) -> bool {
    z_min.is_none_or(|lo| z >= lo) && z_max.is_none_or(|hi| z <= hi)
}

/// Whether a zone cell centered at `cz` should be admitted under an optional `[z_min, z_max]`
/// filter - not by testing `cz` itself, but by testing the *range of rest heights `verify::in_zone`
/// actually treats as matching this cell* for any overlap with the requested range at all.
/// `in_zone` accepts a rest point whose own cell either equals this one or sits directly below it
/// (its own "check the cell above a rest point's cell too" rule), so a cell registered at `cz`
/// really covers rest heights `[cz - 1.5*voxel_size, cz + 0.5*voxel_size)` - testing `cz` alone
/// against a tight height range could wrongly reject a cell that a real rest point in its lower
/// half would still land in and pass.
fn z_range_admits_cell(z_min: Option<f32>, z_max: Option<f32>, cz: f32, voxel_size: f32) -> bool {
    z_min.is_none_or(|lo| cz + 0.5 * voxel_size >= lo)
        && z_max.is_none_or(|hi| cz - 1.5 * voxel_size <= hi)
}

/// The landing zone for `Target::Area` (`s6g2_target_area.md`): open cells whose center is
/// inside the polygon (and, if given, within `z_min..z_max`) with solid ground directly beneath
/// them, plus the cell directly above each of those ("плюс клетка над ними (запас на радиус
/// гранаты)"). Justification for that margin: `sim::trajectory`'s rest point usually settles
/// within a couple of units of the floor it lands on (e.g. `trajectory.rs`'s own
/// `drop_rests_on_flat_plane_at_2_plus_backoff` test - `rest.z` around 2.0 on a flat floor at
/// z=0), but on a stair/ledge edge can land a full voxel higher (its own
/// `floor_impact_damp_gates_on_speed_angle_and_surface` test bounds a rest z up to 32u/2 cells
/// above a floor edge) - without this margin a lineup resting exactly on such a boundary would be
/// wrongly rejected. Deterministic order: a fixed nested loop (y outer, x inner, z innermost)
/// over the polygon's own cell range, the same "no incidental hash-map order" contract
/// `point_target_zone`'s own doc comment already requires of the point-target zone.
fn area_target_zone(grid: &VoxelGrid, area: &TargetArea) -> Vec<(usize, i32)> {
    let (lo, hi) = area.bounds_xy();
    // Padded by one cell on every side: a column whose *center* sits just outside the polygon's
    // own bbox can still be within `EDGE_ADMIT_DIST` of an edge below (up to ~0.71 of a cell).
    let (x0i, y0i, _) = grid.cell_of(V3::new(
        lo[0] - grid.voxel_size(),
        lo[1] - grid.voxel_size(),
        0.0,
    ));
    let (x1i, y1i, _) = grid.cell_of(V3::new(
        hi[0] + grid.voxel_size(),
        hi[1] + grid.voxel_size(),
        0.0,
    ));
    let x0 = x0i.max(0);
    let y0 = y0i.max(0);
    let x1 = x1i.min(grid.nx - 1);
    let y1 = y1i.min(grid.ny - 1);
    // A column whose center falls just short of the polygon still has real overlap with it -
    // 0.7072 (~1/sqrt(2)) of a cell is half a cell's own diagonal, so a center up to that close to
    // an edge always has some part of its cell genuinely inside. Exact per-lineup membership is
    // still enforced later (`verify::VerifyOptions::area_accept`, the strict post-filter in
    // `solve_for_target`) - this only widens which *columns* get their z-cells considered at all.
    const EDGE_ADMIT_DIST: f32 = VOXEL_SIZE * 0.7072;

    let mut seen: HashSet<usize> = HashSet::new();
    let mut zone: Vec<(usize, i32)> = Vec::new();
    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            let center = grid.cell_center_xyz(x, y, 0);
            let admitted = point_in_area_polygon(&area.polygon, center.x, center.y)
                || near_polygon_edge(&area.polygon, center.x, center.y, EDGE_ADMIT_DIST);
            if admitted {
                for z in 1..grid.nz {
                    let index = grid.index(x, y, z);
                    if !grid.is_solid(index) && grid.is_solid(grid.index(x, y, z - 1)) {
                        let cz = grid.cell_center(index).z;
                        if z_range_admits_cell(area.z_min, area.z_max, cz, grid.voxel_size())
                            && seen.insert(index)
                        {
                            zone.push((index, 1));
                        }
                        if z + 1 < grid.nz {
                            let above = grid.index(x, y, z + 1);
                            let above_z = grid.cell_center(above).z;
                            if !grid.is_solid(above)
                                && z_range_admits_cell(
                                    area.z_min,
                                    area.z_max,
                                    above_z,
                                    grid.voxel_size(),
                                )
                                && seen.insert(above)
                            {
                                zone.push((above, 1));
                            }
                        }
                    }
                }
            }
            x += 1;
        }
        y += 1;
    }
    zone
}

/// The single point downstream aim-reference/exhaustive-search/tie-break code needs for
/// `Target::Area` (`s6g2_target_area.md`: "представительная точка: центроид клеток зоны,
/// притянутый к ближайшей клетке зоны"): the zone's raw mean can land outside the zone entirely
/// on a concave or ring-shaped area (e.g. an L-shape's own notch), so it is pulled onto whichever
/// actual zone cell sits closest to it instead of used as-is. `None` only when the zone is empty.
fn zone_representative_point(grid: &VoxelGrid, zone: &[(usize, i32)]) -> Option<V3> {
    let (first, rest) = zone.split_first()?;
    let mut mean = grid.cell_center(first.0);
    for &(cell, _) in rest {
        mean = mean + grid.cell_center(cell);
    }
    mean = mean / zone.len() as f32;
    let mut best = grid.cell_center(first.0);
    let mut best_d = (best - mean).length_squared();
    for &(cell, _) in rest {
        let c = grid.cell_center(cell);
        let d = (c - mean).length_squared();
        if d < best_d {
            best_d = d;
            best = c;
        }
    }
    Some(best)
}

/// `TargetSolver.SolveForTarget`'s named parameters (`TargetSolver.cs:80-132`).
pub struct SolveQuery {
    /// `s6g2_target_area.md`: `Target::Point` (as before) or `Target::Area`.
    pub target: Target,
    pub origin_click: Option<[f32; 2]>,
    pub origin_z: Option<f32>,
    pub origin_reach: f32,
    /// `s6g_origin_area.md`: an alternative to `origin_click`/`origin_reach`'s circle. The two
    /// are mutually exclusive by contract (validated by the caller, not here).
    pub origin_area: Option<OriginArea>,
    /// `0.4` in the reference.
    pub min_stability: f32,
    pub fine_scan: bool,
    pub types: Option<Vec<ThrowType>>,
    pub strengths: Option<Vec<f32>>,
    pub broken_groups: Vec<String>,
    pub spawn_fronts: Vec<V3>,
    pub spawn_points: Vec<V3>,
    pub spawn_scope_radius: f32,
    pub spawns_only: bool,
    pub exact_origin: bool,
    pub referee: bool,
    /// `s6j_pin_filter.md`: 0 = any (default), 1 = wall or corner, 2 = corner only - matches
    /// [`origins::position_pin`]'s own 0/1/2 scale. Skipped for an exact origin click
    /// (`exact_origin && origin_click.is_some()`), which is an explicit user spot.
    pub origin_pin_min: u8,
    /// `s6l_aim_precision.md`: opt-in precision mode - narrows `verify::VerifyOptions`'s aim-window
    /// search/probe step from `STEP_DEG`/`AIM_REACH` (0.6°, ±2 steps) to 0.2°/±4 steps (±0.8°),
    /// and turns on `stability_wide` (the same 5-probe stability re-measured at the reference
    /// 0.6° window). `false` (the default) must reproduce the pre-existing behavior byte-for-byte.
    pub precise_aim: bool,
}

impl Default for SolveQuery {
    fn default() -> Self {
        SolveQuery {
            target: Target::Point {
                pos: V3::ZERO,
                has_z: false,
                tolerance: 80.0,
            },
            origin_click: None,
            origin_z: None,
            origin_reach: 300.0,
            origin_area: None,
            min_stability: 0.4,
            fine_scan: false,
            types: None,
            strengths: None,
            broken_groups: Vec::new(),
            spawn_fronts: Vec::new(),
            spawn_points: Vec::new(),
            spawn_scope_radius: 0.0,
            spawns_only: false,
            exact_origin: false,
            referee: false,
            origin_pin_min: 0,
            precise_aim: false,
        }
    }
}

/// `TargetSolver.SolveForTarget`'s progress/diagnostics callbacks
/// (`TargetSolver.cs:80-100`'s `onPhase`/`onOrigin`/`onCandidate`
/// parameters), grouped so a caller wanting only phase progress does not
/// have to name the other two `None`s inline at every call site.
pub struct SolveHooks<'a> {
    pub progress: &'a dyn Fn(Phase, usize),
    /// `sweep::SweepOptions::on_origin`: once per swept origin, with the
    /// number of throws it landed in the zone.
    pub on_origin: Option<&'a (dyn Fn(V3, usize) + Sync)>,
    /// `verify::VerifyOptions::on_candidate`: once per verified candidate,
    /// with whether it survived verification.
    pub on_candidate: Option<&'a (dyn Fn(V3, bool) + Sync)>,
}

/// `onPhase` phase names (`TargetSolver.cs`'s string literals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prepare,
    Colliders,
    Origins,
    PinnedOrigins,
    AfterPins,
    Sweep,
    Verify,
    Exhaustive,
    Escalate,
    EscalateFine,
    Sightline,
    Pins,
}

/// `TargetSolver.cs`'s `TargetSolve` record.
pub struct TargetSolve {
    pub target: V3,
    pub origins: usize,
    /// `[x, y, raw option count, pin class]` per evaluated origin.
    pub coverage: Vec<[i32; 4]>,
    pub lineups: Vec<Lineup>,
    pub empty_reason: Option<String>,
    pub referee: Option<Vec<Lineup>>,
    pub referee_notes: Option<Vec<String>>,
    /// Kept for ranking (`AimReference::analyze`, `origins::position_pin`),
    /// like the reference keeps `Collider`/`PlayerCollider` on the record.
    pub collider: UniformGrid,
    pub player_collider: UniformGrid,
    pub collider_glass_gone: Option<UniformGrid>,
}

fn xy(v: V3) -> [f32; 2] {
    [v.x, v.y]
}

/// A partial solve abandoned because `cancel` fired mid-flight: discarded
/// results, not a "nothing found" answer (`s5c_target.md`'s progress/cancel
/// contract - "cancellation checked per origin in the sweep and per
/// candidate in verify (partial results discarded)").
fn cancelled_solve(
    target: V3,
    origins: usize,
    collider: UniformGrid,
    player_collider: UniformGrid,
    collider_glass_gone: Option<UniformGrid>,
) -> TargetSolve {
    TargetSolve {
        target,
        origins,
        coverage: Vec::new(),
        lineups: Vec::new(),
        empty_reason: Some("cancelled".to_string()),
        referee: None,
        referee_notes: None,
        collider,
        player_collider,
        collider_glass_gone,
    }
}

fn distance2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

/// `TargetSolver.cs:24-27` (`DropToFloor`).
fn drop_to_floor(collider: &dyn Collider, p: V3) -> V3 {
    match origins::floor_under_hull(collider, p, sim::STAND_EYE_HEIGHT, SPAWN_DROP) {
        Some(z) => V3::new(p.x, p.y, z),
        None => p,
    }
}

/// `TargetSolver.cs:38-56` (`NearestStandSpot`).
fn nearest_stand_spot(
    spots: Option<&[StandSpotOrigin]>,
    at: [f32; 2],
    radius: f32,
) -> Option<StandSpotOrigin> {
    let spots = spots?;
    if spots.is_empty() {
        return None;
    }
    let mut best: Option<StandSpotOrigin> = None;
    let mut best_d = radius;
    for &s in spots {
        let d = distance2(xy(s.feet), at);
        if d < best_d {
            best_d = d;
            best = Some(s);
        }
    }
    best
}

/// `TargetSolver.cs:63-78` (`NearestStandSpotZ`).
fn nearest_stand_spot_z(spots: Option<&[StandSpotOrigin]>, at: [f32; 2]) -> Option<f32> {
    let spots = spots?;
    let mut lowest: Option<f32> = None;
    for s in spots {
        if distance2(xy(s.feet), at) < 24.0 && lowest.is_none_or(|z| s.feet.z < z) {
            lowest = Some(s.feet.z);
        }
    }
    lowest
}

/// `MeshSetup.cs:52-63`/`CollisionMesh.cs:52-63`'s grenade-solid predicate,
/// applied to a single attribute (so it can be combined with the excluded-
/// groups check below without a second, redundant `AttributeMask` pass).
fn is_grenade_solid(a: &CollisionAttribute) -> bool {
    let any_ci =
        |layers: &[String], name: &str| layers.iter().any(|l| l.eq_ignore_ascii_case(name));
    !any_ci(&a.interact_exclude, "csgo_thrown_grenade")
        && !any_ci(&a.interact_as, "playerclip")
        && !any_ci(&a.interact_as, "npcclip")
        && !any_ci(&a.interact_as, "sky")
}

/// `MeshSetup.cs:27-33` (`BuildGrenadeCollider`/`BuildGrenadeColliderExcluding`).
fn grenade_mask_excluding(mesh: &CollisionMesh, excluded: &[String]) -> AttributeMask {
    from_fn(mesh, |a| {
        is_grenade_solid(a) && !excluded.iter().any(|n| n.eq_ignore_ascii_case(&a.name))
    })
}

/// `LineupApi.cs:544-549` (`RunTargetQuery`'s pre-`SolveForTarget` broken-
/// groups adjustment): `attributeFilter = a => baseFilter(a) && !excluded[a]`,
/// applied here (inside the port of `SolveForTarget` itself, since the
/// broken groups are per-query while the base filter is per-map) to every
/// place the reference's own `attributeFilter` parameter feeds: the target
/// probe/region voxel grid and the sightline raycaster.
///
/// `AttributeMask` only exposes a per-index `is_solid`, not iteration, so
/// this recovers the index via `from_fn`'s documented, `Vec`-order
/// (`mesh.attributes.iter().map(f).collect()`) call sequence.
fn effective_attribute_mask(
    mesh: &CollisionMesh,
    base: Option<&AttributeMask>,
    broken_groups: &[String],
) -> AttributeMask {
    if broken_groups.is_empty() {
        return base.cloned().unwrap_or_else(|| all_mask(mesh));
    }
    let index = Cell::new(0u16);
    from_fn(mesh, |a| {
        let i = index.get();
        index.set(i + 1);
        let base_solid = base.is_none_or(|m| m.is_solid(i));
        base_solid
            && !broken_groups
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&a.name))
    })
}

/// `TargetSolver.cs:676-759` (`SettleTarget`).
pub fn settle_target<C: Collider>(collider: &C, grid: &VoxelGrid, target: V3) -> V3 {
    let mut p = target;
    let probe = p + V3::new(0.0, 0.0, 2.0);
    for _pass in 0..2 {
        let mut walls: Vec<((f32, f32), f32)> = Vec::new();
        for i in 0..8 {
            let a = i as f32 * std::f32::consts::PI / 4.0;
            let dir = V3::new(a.cos(), a.sin(), 0.0);
            let from = V3::new(p.x, p.y, probe.z + 6.0) - dir;
            let span = TARGET_WALL_CLEARANCE + 1.0;
            let Some(wall) = collider.first_hit_ray(from, from + dir * span) else {
                continue;
            };
            if wall.normal.z.abs() >= 0.35 {
                continue;
            }
            let n_len = (wall.normal.x * wall.normal.x + wall.normal.y * wall.normal.y).sqrt();
            if n_len < 0.8 {
                continue;
            }
            let mut n = (wall.normal.x / n_len, wall.normal.y / n_len);
            if n.0 * dir.x + n.1 * dir.y > 0.0 {
                n = (-n.0, -n.1);
            }
            let hit_point = from + dir * (wall.t * span);
            if walls
                .iter()
                .any(|w: &((f32, f32), f32)| w.0.0 * n.0 + w.0.1 * n.1 > 0.9)
            {
                continue;
            }
            let d = n.0 * hit_point.x + n.1 * hit_point.y;
            walls.push((n, d));
        }
        for (n, d) in &walls {
            let gap = n.0 * p.x + n.1 * p.y - d;
            if gap < TARGET_WALL_CLEARANCE {
                p = p + V3::new(n.0, n.1, 0.0) * (TARGET_WALL_CLEARANCE - gap);
            }
        }
    }
    // `TargetSolver.cs:738-742`: `cz` itself is never clamped (only the
    // `InBounds` check and the descent start `k` are) - clamping `cz` before
    // computing `lowest` would shift the search window on a target whose
    // probe cell already sits outside `[1, Nz-1]`.
    let (cx, cy, cz) = grid.cell_of(p + V3::new(0.0, 0.0, 1.0));
    if grid.in_bounds(cx, cy, cz.clamp(1, grid.nz - 1)) {
        let lowest = (cz - (TARGET_FLOOR_DROP / grid.voxel_size()).ceil() as i32).max(1);
        let mut k = (cz + 1).clamp(1, grid.nz - 1);
        while k >= lowest {
            if !grid.is_solid(grid.index(cx, cy, k)) && grid.is_solid(grid.index(cx, cy, k - 1)) {
                let boundary = grid.cell_center_xyz(cx, cy, k).z - grid.voxel_size() / 2.0;
                let top = V3::new(p.x, p.y, boundary + 2.0);
                let bottom = V3::new(p.x, p.y, boundary - grid.voxel_size() - 2.0);
                if let Some(floor) = collider.first_hit_ray(top, bottom)
                    && floor.normal.z >= sim::FLOOR_NORMAL_Z
                {
                    p.z = float_lerp(top.z, bottom.z, floor.t);
                    return p;
                }
            }
            k -= 1;
        }
    }
    p
}

/// `TargetSolver.cs:774-806` (`SnapTargetToGround`).
pub fn snap_target_to_ground(
    grid: &VoxelGrid,
    x: i32,
    y: i32,
    expected_z: Option<f32>,
) -> Option<V3> {
    if x < 0 || x >= grid.nx || y < 0 || y >= grid.ny {
        return None;
    }
    let mut best: Option<f32> = None;
    for z in 1..=(grid.nz - 2) {
        if grid.is_solid(grid.index(x, y, z)) || !grid.is_solid(grid.index(x, y, z - 1)) {
            continue;
        }
        let surface = grid.cell_center_xyz(x, y, z).z - grid.voxel_size() / 2.0;
        match best {
            None => best = Some(surface),
            Some(b) => {
                if let Some(want) = expected_z
                    && (surface - want).abs() < (b - want).abs()
                {
                    best = Some(surface);
                }
            }
        }
    }
    let z0 = best?;
    let column = grid.cell_center_xyz(x, y, 0);
    Some(V3::new(column.x, column.y, z0))
}

/// `TargetSolver.cs:811-819` (`AimReferencePoint`).
pub fn aim_reference_point<C: Collider>(
    collider: &C,
    feet: V3,
    t: ThrowType,
    pitch_deg: f32,
    yaw_deg: f32,
) -> V3 {
    let eye = feet + V3::new(0.0, 0.0, eye_height(t));
    let dir = forward_from_angles(pitch_deg, yaw_deg);
    let far = eye + dir * 1200.0;
    let dist = match collider.first_hit_ray(eye, far) {
        Some(h) => (h.t * 1200.0 - 24.0).max(60.0),
        None => 1200.0,
    };
    eye + dir * dist
}

type PrunedMap = HashMap<(ThrowType, u32, u32), String>;
type OnPruned<'a> = dyn Fn(V3, ThrowType, f32, f32, &str) + Sync + 'a;
type FeetKey = (u32, u32, u32, ThrowType, u32, u32);

fn kind_of(l: &Lineup) -> (ThrowType, u32, u32) {
    (
        l.throw_type,
        l.strength.to_bits(),
        l.run_yaw_offset_deg.to_bits(),
    )
}

fn key3(v: V3) -> (i32, i32, i32) {
    (
        v.x.round_ties_even() as i32,
        v.y.round_ties_even() as i32,
        v.z.round_ties_even() as i32,
    )
}

/// `TargetSolver.cs:496-511` (`DropStandingAtCrouchOnly`): a stand spot only
/// the crouched hull fits keeps its crouched lineups and drops the rest -
/// the sim released a standing/run-jump throw from an eye height 18u above
/// where the grenade would really leave the hand.
fn drop_standing_at_crouch_only(
    crouch_only: &HashSet<(i32, i32, i32)>,
    ls: Vec<Lineup>,
) -> Vec<Lineup> {
    if crouch_only.is_empty() {
        ls
    } else {
        ls.into_iter()
            .filter(|l| {
                !crouch_only.contains(&key3(l.feet))
                    || matches!(l.throw_type, ThrowType::Crouch | ThrowType::CrouchJumpThrow)
            })
            .collect()
    }
}

/// Bug fix: verifies `sweep::solve_buckets`' ranked buckets in rounds - round `r` batches every
/// still-unresolved bucket's `r`-th candidate (if it has one) into a single `verify_exact` call, a
/// bucket resolves once one of its own candidates survives verification (identified by `FeetKey`,
/// since `verify_exact` may nudge a survivor's own yaw/pitch within its aim window), and a bucket
/// whose *every* candidate fails verification simply contributes nothing - same as it always did
/// with one candidate, just no longer giving up after only trying the single best one. The
/// combined result is sorted exactly the way a single `verify_exact` call already sorts its own
/// output (`stability` desc, `bounces` asc, `rest_crossings` desc, `flight_time` asc, ordinal).
fn verify_buckets_in_rounds<C: Collider>(
    grid: &VoxelGrid,
    collider: &C,
    zone: &[(usize, i32)],
    buckets: &[Vec<Lineup>],
    verify_opts: &verify::VerifyOptions<C>,
) -> Vec<Lineup> {
    fn feet_key(l: &Lineup) -> FeetKey {
        (
            l.feet.x.to_bits(),
            l.feet.y.to_bits(),
            l.feet.z.to_bits(),
            l.throw_type,
            l.strength.to_bits(),
            l.run_yaw_offset_deg.to_bits(),
        )
    }
    let max_depth = buckets.iter().map(Vec::len).max().unwrap_or(0);
    let mut resolved = vec![false; buckets.len()];
    let mut out: Vec<Lineup> = Vec::new();
    for r in 0..max_depth {
        if verify_opts
            .cancel
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            break;
        }
        let batch: Vec<Lineup> = buckets
            .iter()
            .zip(resolved.iter())
            .filter(|&(_, &done)| !done)
            .filter_map(|(bucket, _)| bucket.get(r).copied())
            .collect();
        if batch.is_empty() {
            continue;
        }
        let batch_keys: HashSet<FeetKey> = batch.iter().map(feet_key).collect();
        let newly = verify::verify_exact(grid, collider, zone, &batch, verify_opts);
        let mut by_key: HashMap<FeetKey, Lineup> = HashMap::new();
        for l in newly {
            let key = feet_key(&l);
            if batch_keys.contains(&key) {
                by_key.entry(key).or_insert(l);
            }
        }
        for (bucket, done) in buckets.iter().zip(resolved.iter_mut()) {
            if *done {
                continue;
            }
            let Some(cand) = bucket.get(r) else { continue };
            if let Some(l) = by_key.remove(&feet_key(cand)) {
                out.push(l);
                *done = true;
            }
        }
    }
    out.sort_by(|a, b| {
        b.stability
            .partial_cmp(&a.stability)
            .unwrap()
            .then(a.bounces.cmp(&b.bounces))
            .then(b.rest_crossings.cmp(&a.rest_crossings))
            .then(a.flight_time.partial_cmp(&b.flight_time).unwrap())
            .then_with(|| sweep::ordinal_cmp(a, b))
    });
    out
}

/// `TargetSolver.cs:669-674` (`NearestAim`).
fn nearest_aim(candidates: &[&Lineup], referee: &Lineup) -> String {
    fn yaw_delta(a: f32, b: f32) -> f32 {
        (((a - b) % 360.0 + 540.0) % 360.0 - 180.0).abs()
    }
    let mut best: Option<(f32, f32)> = None;
    let mut best_sum = f32::MAX;
    for &c in candidates {
        let yaw = yaw_delta(c.yaw_deg, referee.yaw_deg);
        let pitch = (c.pitch_deg - referee.pitch_deg).abs();
        if yaw + pitch < best_sum {
            best_sum = yaw + pitch;
            best = Some((yaw, pitch));
        }
    }
    let (y, p) = best.unwrap_or((0.0, 0.0));
    format!("{y:.1} yaw / {p:.1} pitch away")
}

/// `TargetSolver.cs:641-667` (`ExplainMisses`).
#[allow(clippy::too_many_arguments)]
fn explain_misses(
    grid: &VoxelGrid,
    zone: &HashMap<usize, i32>,
    candidates: &[Lineup],
    verified: &[Lineup],
    referee: &[Lineup],
    target: V3,
    k: &ThrowConstants,
    pruned: Option<&HashMap<(ThrowType, u32, u32), String>>,
) -> Vec<String> {
    let found: HashSet<_> = verified.iter().map(kind_of).collect();
    let mut candidate_kinds: HashMap<(ThrowType, u32, u32), Vec<&Lineup>> = HashMap::new();
    for c in candidates {
        candidate_kinds.entry(kind_of(c)).or_default().push(c);
    }

    let mut order: Vec<(ThrowType, u32, u32)> = Vec::new();
    let mut groups: HashMap<(ThrowType, u32, u32), Vec<&Lineup>> = HashMap::new();
    for r in referee {
        let kd = kind_of(r);
        if found.contains(&kd) {
            continue;
        }
        if !groups.contains_key(&kd) {
            order.push(kd);
        }
        groups.entry(kd).or_default().push(r);
    }

    let mut notes = Vec::new();
    for kd in order {
        let g = &groups[&kd];
        let mut best = g[0];
        let mut best_d = (best.rest_point - target).length_squared();
        for &l in &g[1..] {
            let d = (l.rest_point - target).length_squared();
            if d < best_d {
                best = l;
                best_d = d;
            }
        }
        let r = best;
        let eye = r.feet + V3::new(0.0, 0.0, eye_height(r.throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: r.yaw_deg,
            pitch_deg: r.pitch_deg,
            throw_type: r.throw_type,
            strength: r.strength,
            run_yaw_offset_deg: r.run_yaw_offset_deg,
        };
        let voxel = simulate_voxel(grid, &spec, k);
        let (cx, cy, cz) = grid.cell_of(voxel.rest);
        let in_zone = grid.in_bounds(cx, cy, cz) && zone.contains_key(&grid.index(cx, cy, cz));
        let (dx, dy) = (voxel.rest.x - target.x, voxel.rest.y - target.y);
        let voxel_miss = (dx * dx + dy * dy).sqrt();
        let kind_str = format!(
            "{:?}/{}{}",
            r.throw_type,
            r.strength,
            if r.run_yaw_offset_deg != 0.0 {
                format!("@{:.0}", r.run_yaw_offset_deg)
            } else {
                String::new()
            }
        );
        let stage = if let Some(same) = candidate_kinds.get(&kd) {
            format!(
                "{} candidate(s) of this kind failed verification, nearest aim {}",
                same.len(),
                nearest_aim(same, r)
            )
        } else if let Some(why) = pruned.and_then(|m| m.get(&kd)) {
            format!("pruned before the sweep: {why}")
        } else {
            "no candidate of this kind left the sweep".to_string()
        };
        let voxel_desc = if voxel.lost {
            "lost".to_string()
        } else if in_zone {
            "lands in zone".to_string()
        } else {
            format!(
                "misses by {voxel_miss:.0}u ({} bounces, rest z {:.0} vs {:.0})",
                voxel.bounces, voxel.rest.z, r.rest_point.z
            )
        };
        notes.push(format!(
            "{kind_str}: referee aim yaw {:.1} pitch {:.1} bounces {} stability {:.2}; voxel sim at that aim {voxel_desc}; {stage}",
            r.yaw_deg, r.pitch_deg, r.bounces, r.stability
        ));
    }
    notes
}

/// `TargetSolver.cs:80-626` (`SolveForTarget`).
pub fn solve_for_target(
    map: &MapData,
    q: &SolveQuery,
    k: &ThrowConstants,
    hooks: &SolveHooks<'_>,
    cancel: &AtomicBool,
) -> TargetSolve {
    let progress = hooks.progress;
    let has_origin = q.origin_click.is_some();
    // A cheap XY anchor for the origin-click fallback and the region bounds below, available
    // before any grid exists: the click itself for `Target::Point` (its Z isn't resolved yet, but
    // neither use below ever needs it), the polygon's own bbox center for `Target::Area`.
    let target_anchor_xy = match &q.target {
        Target::Point { pos, .. } => [pos.x, pos.y],
        Target::Area(area) => {
            let (lo, hi) = area.bounds_xy();
            [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0]
        }
    };
    let origin_click = q.origin_click.unwrap_or(target_anchor_xy);

    progress(Phase::Prepare, 0);

    let (mesh_min_a, mesh_max_a) = map
        .mesh
        .bounds()
        .unwrap_or(([0.0, 0.0, 0.0], [0.0, 0.0, 0.0]));
    let mesh_min = V3::from_array(mesh_min_a);
    let mesh_max = V3::from_array(mesh_max_a);

    // `LineupApi.cs:544-549`: the query's broken groups fold into the
    // world/vision filter too, not just the grenade collider below - used
    // for the target probe/region grid here and the sightline raycaster
    // later.
    let attr_mask =
        effective_attribute_mask(&map.mesh, map.attribute_filter.as_ref(), &q.broken_groups);

    // Resolve `Target::Point`'s Z (`TargetSolver.cs:144-169`); `Target::Area` has nothing to
    // resolve here - its own optional `z_min`/`z_max` already describe its full vertical extent,
    // and there is no single point to nav-ground-snap or settle.
    let mut point_target: V3 = match &q.target {
        Target::Point { pos, .. } => *pos,
        Target::Area(_) => V3::ZERO,
    };
    if let Target::Point { has_z, .. } = &q.target {
        let nav_z = if !has_z {
            nav_ground::nav_ground_z_nearby(&map.nav_areas, point_target.x, point_target.y)
        } else {
            None
        };
        if let Some(z0) = nav_z {
            point_target.z = z0;
        } else if !has_z {
            let probe_min = V3::new(point_target.x - 200.0, point_target.y - 200.0, mesh_min.z);
            let probe_max = V3::new(point_target.x + 200.0, point_target.y + 200.0, mesh_max.z);
            let probe_grid = VoxelGrid::build(
                &map.mesh,
                &attr_mask,
                VOXEL_SIZE,
                Aabb {
                    min: probe_min,
                    max: probe_max,
                },
            )
            .expect("finite mesh vertices");
            let (tx, ty, _) =
                probe_grid.cell_of(V3::new(point_target.x, point_target.y, mesh_min.z + 100.0));
            let anchor = nav_ground::nav_ground_z_within(
                &map.nav_areas,
                point_target.x,
                point_target.y,
                f32::MAX,
            );
            point_target = snap_target_to_ground(&probe_grid, tx, ty, anchor).unwrap_or(V3::new(
                point_target.x,
                point_target.y,
                0.0,
            ));
        }
    }

    // Region min/max (`TargetSolver.cs:171-181`), widened to the origin area's own bounding box
    // instead of the reach circle's when one is given (`s6g_origin_area.md`: "рамка
    // многоугольника вместо круга (с теми же +500 и обрезкой по мешу)"), and likewise to the
    // target area's own bounding box instead of a single point's (`s6g2_target_area.md`).
    let (reach_min_xy, reach_max_xy) = match &q.origin_area {
        Some(area) => area.bounds_xy(),
        None => (
            [
                origin_click[0] - q.origin_reach,
                origin_click[1] - q.origin_reach,
            ],
            [
                origin_click[0] + q.origin_reach,
                origin_click[1] + q.origin_reach,
            ],
        ),
    };
    let (target_min_xy, target_max_xy) = match &q.target {
        Target::Point { .. } => (
            [point_target.x, point_target.y],
            [point_target.x, point_target.y],
        ),
        Target::Area(area) => area.bounds_xy(),
    };
    let min = V3::new(
        (target_min_xy[0].min(reach_min_xy[0]) - 500.0).max(mesh_min.x),
        (target_min_xy[1].min(reach_min_xy[1]) - 500.0).max(mesh_min.y),
        mesh_min.z,
    );
    // The Z bound stays computed the same way as without an area, but a `z_max` above the
    // default 900u-over-target window must still be reachable, or a rooftop-only area would have
    // no grid left to find its own candidates in (`s6g_origin_area.md`: "по Z — как сейчас, но с
    // учётом диапазона, если задан"). A target area with no `z_max` of its own has no single
    // point to anchor the same "+900" window on, so it falls back to the mesh's own ceiling
    // instead (`s6g2_target_area.md`'s region is "как сейчас" plus the area's bbox - the simplest
    // faithful reading when there is no z hint at all is the same map-wide fallback `origin_area`
    // already uses without an origin click).
    let mut z_cap = match &q.target {
        Target::Point { .. } => point_target.z + 900.0,
        Target::Area(area) => area.z_max.map_or(mesh_max.z + 64.0, |z| z + 900.0),
    };
    if let Some(zmax) = q.origin_area.as_ref().and_then(|a| a.z_max) {
        z_cap = z_cap.max(zmax + VOXEL_SIZE);
    }
    let max = V3::new(
        (target_max_xy[0].max(reach_max_xy[0]) + 500.0).min(mesh_max.x),
        (target_max_xy[1].max(reach_max_xy[1]) + 500.0).min(mesh_max.y),
        // A target far below the map used to invert this region (`max.z` below `min.z`),
        // which sent `VoxelGrid::build` a negative cell count and aborted the process - clamp
        // to at least one voxel above `min.z` so the grid always stays non-degenerate here too.
        ((mesh_max.z + 64.0).min(z_cap)).max(min.z + VOXEL_SIZE),
    );
    let region = Aabb { min, max };
    let grid =
        VoxelGrid::build(&map.mesh, &attr_mask, VOXEL_SIZE, region).expect("finite mesh vertices");
    progress(Phase::Colliders, 0);

    let collider = if !q.broken_groups.is_empty() {
        let mask = grenade_mask_excluding(&map.mesh, &q.broken_groups);
        UniformGrid::build(&map.mesh, &mask, Some(region), 128.0).expect("finite mesh vertices")
    } else {
        let mask = grenade_mask(&map.mesh);
        UniformGrid::build(&map.mesh, &mask, Some(region), 128.0).expect("finite mesh vertices")
    };
    let has_glass = map
        .mesh
        .attributes
        .iter()
        .any(|a| a.name == "EntityBreakable");
    let glass_already_gone = q.broken_groups.iter().any(|g| g == "EntityBreakable");
    let collider_glass_gone = if has_glass && !glass_already_gone {
        let mut excluded = q.broken_groups.clone();
        excluded.push("EntityBreakable".to_string());
        let mask = grenade_mask_excluding(&map.mesh, &excluded);
        Some(
            UniformGrid::build(&map.mesh, &mask, Some(region), 128.0)
                .expect("finite mesh vertices"),
        )
    } else {
        None
    };

    // Settle `Target::Point` (wall-push + floor-snap) only when an explicit Z was given -
    // `Target::Area` has nothing to settle, the drawn polygon is taken literally.
    if let Target::Point { has_z: true, .. } = &q.target {
        point_target = settle_target(&collider, &grid, point_target);
    }

    // The zone: cells a lineup's rest point must land in to count.
    let zone_crossings = match &q.target {
        Target::Point { tolerance, .. } => zone::point_target_zone(&grid, point_target, *tolerance),
        Target::Area(area) => area_target_zone(&grid, area),
    };

    // The single point downstream aim-reference/exhaustive-search/tie-break code still needs
    // (`s6g2_target_area.md`'s "представительная точка"): the settled point for `Target::Point`,
    // or the area zone's own centroid pulled onto its nearest actual cell for `Target::Area`
    // (`zone_representative_point`). Falls back to the polygon's own bbox center when the zone
    // came back empty, purely so the "no reachable landing cells" message below has *a* point to
    // report - nothing downstream reads it in that case.
    let target =
        match &q.target {
            Target::Point { .. } => point_target,
            Target::Area(_) => zone_representative_point(&grid, &zone_crossings)
                .unwrap_or(V3::new(target_anchor_xy[0], target_anchor_xy[1], min.z)),
        };

    let player_mask_val = player_mask(&map.mesh);
    let player_collider = UniformGrid::build(&map.mesh, &player_mask_val, Some(region), 128.0)
        .expect("finite mesh vertices");
    progress(Phase::Origins, 0);

    if zone_crossings.is_empty() {
        let why = format!(
            "target ({:.0},{:.0},{:.0}) has no reachable landing cells - it resolved inside solid geometry, or the tolerance is too small",
            target.x, target.y, target.z
        );
        // `TargetSolver.cs:249` also `Console.Error.WriteLine`s this; left
        // to the caller here instead (`TargetSolve::empty_reason` already
        // carries it) so a CLI/server surfacing it once doesn't print it
        // twice.
        return TargetSolve {
            target,
            origins: 0,
            coverage: Vec::new(),
            lineups: Vec::new(),
            empty_reason: Some(why),
            referee: None,
            referee_notes: None,
            collider,
            player_collider,
            collider_glass_gone,
        };
    }

    // `origin_area` replaces the reach-circle test with polygon containment (plus an optional Z
    // range) everywhere a candidate origin is accepted or rejected below
    // (`s6g_origin_area.md`: "кандидаты ... только внутри многоугольника").
    let in_reach = |p: [f32; 2]| -> bool {
        match &q.origin_area {
            Some(area) => point_in_area_polygon(&area.polygon, p[0], p[1]),
            None => distance2(p, origin_click) <= q.origin_reach,
        }
    };
    let in_area_z = |z: f32| -> bool {
        q.origin_area
            .as_ref()
            .is_none_or(|a| z_in_range(a.z_min, a.z_max, z))
    };

    let mut origins_list: Vec<V3> = match &map.stand_spots {
        Some(spots) if !spots.is_empty() => spots
            .iter()
            .filter(|s| {
                in_reach(xy(s.feet))
                    && s.feet.z >= min.z
                    && s.feet.z <= max.z
                    && in_area_z(s.feet.z)
            })
            .map(|s| s.feet)
            .collect(),
        _ => {
            let cider: &dyn Collider = &player_collider;
            origins::origins_from_nav_areas(
                &grid,
                &map.nav_areas,
                V3::new(reach_min_xy[0], reach_min_xy[1], mesh_min.z),
                V3::new(reach_max_xy[0], reach_max_xy[1], max.z),
                24.0,
                Some(cider),
            )
            .into_iter()
            .filter(|o| in_reach(xy(*o)) && in_area_z(o.z))
            .collect()
        }
    };

    let mut crouch_only_extras: Vec<V3> = Vec::new();

    if q.spawns_only && !q.spawn_points.is_empty() {
        origins_list = Vec::new();
        for &raw in &q.spawn_points {
            if raw.z < min.z - SPAWN_DROP || raw.z > max.z {
                continue;
            }
            let dropped = drop_to_floor(&player_collider, raw);
            let exact = origins::exact_origin_only(
                &grid,
                Some(&player_collider),
                dropped,
                Some(&mut crouch_only_extras),
            );
            if !exact.is_empty() {
                origins_list.extend(exact);
                continue;
            }
            if let Some(spot) =
                nearest_stand_spot(map.stand_spots.as_deref(), xy(dropped), SPAWN_SNAP)
            {
                origins_list.push(spot.feet);
                if spot.crouched {
                    crouch_only_extras.push(spot.feet);
                }
            }
        }
    } else if q.spawn_scope_radius > 0.0 && !q.spawn_points.is_empty() {
        origins_list.retain(|o| {
            q.spawn_points
                .iter()
                .any(|p| distance2(xy(*o), xy(*p)) <= q.spawn_scope_radius)
        });
    }

    // `s6j_pin_filter.md`: pinned origins are only added below when stand spots exist and
    // `!q.spawns_only` - without this, "spawns + corner only" would filter every spawn-derived
    // origin down to nothing before a wall/corner variant of any of them ever existed.
    if q.origin_pin_min > 0 && q.spawns_only {
        origins::add_pinned_origins_to(
            &grid,
            &player_collider,
            &mut origins_list,
            Some(&mut crouch_only_extras),
        );
    }

    if q.exact_origin && has_origin {
        let exact_z = q
            .origin_z
            .or_else(|| nearest_stand_spot_z(map.stand_spots.as_deref(), origin_click))
            .or_else(|| nav_ground::nav_ground_z(&map.nav_areas, origin_click[0], origin_click[1]))
            .unwrap_or(target.z);
        origins_list = origins::exact_origin_only(
            &grid,
            Some(&player_collider),
            V3::new(origin_click[0], origin_click[1], exact_z),
            Some(&mut crouch_only_extras),
        );
    }

    let mut pinned_origins: HashSet<(i32, i32)> = HashSet::new();
    if !(q.exact_origin && has_origin)
        && map.stand_spots.as_ref().is_some_and(|s| !s.is_empty())
        && !q.spawns_only
    {
        let before = origins_list.len();
        progress(Phase::PinnedOrigins, origins_list.len());
        origins::add_pinned_origins_to(
            &grid,
            &player_collider,
            &mut origins_list,
            Some(&mut crouch_only_extras),
        );
        // Unlike the reach circle (unfiltered here too, matching the reference), an `origin_area`
        // polygon is a hard promise to the caller - every resulting throw point must lie inside
        // it (`s6g_origin_area.md`'s own check 2: "все раскидки (в) внутри многоугольника") - a
        // wall/corner pin can walk an origin a few units past the polygon boundary, so the newly
        // added ones get re-checked here.
        if q.origin_area.is_some() {
            let mut new_pins = origins_list.split_off(before);
            new_pins.retain(|o| in_reach(xy(*o)) && in_area_z(o.z));
            origins_list.extend(new_pins);
        }
        progress(Phase::AfterPins, origins_list.len());
        for &o in &origins_list[before..] {
            pinned_origins.insert((
                (o.x * 4.0).round_ties_even() as i32,
                (o.y * 4.0).round_ties_even() as i32,
            ));
        }
    }

    if has_origin && !q.exact_origin {
        let click_z = q
            .origin_z
            .or_else(|| nearest_stand_spot_z(map.stand_spots.as_deref(), origin_click))
            .or_else(|| nav_ground::nav_ground_z(&map.nav_areas, origin_click[0], origin_click[1]))
            .unwrap_or(target.z);
        origins_list.extend(origins::exact_origin_with_pins(
            &grid,
            Some(&player_collider),
            V3::new(origin_click[0], origin_click[1], click_z),
            Some(&mut crouch_only_extras),
        ));
    }

    // `s6j_pin_filter.md`: restrict the final origin list to wall/corner-pinned spots. Skipped for
    // an exact origin click - that is an explicit user spot, not a search the filter should prune.
    // Uses `origins::position_pin` with the same `player_collider` the ranking pin (`rank.rs`)
    // re-derives from `l.feet`, so every lineup this solve returns satisfies the filter.
    let mut pin_filter_emptied = false;
    if q.origin_pin_min > 0 && !(q.exact_origin && has_origin) {
        let pin_min = i32::from(q.origin_pin_min);
        let before_filter = origins_list.len();
        origins_list = origins_list
            .par_iter()
            .filter(|&&o| origins::position_pin(&player_collider, o) >= pin_min)
            .copied()
            .collect();
        pin_filter_emptied = before_filter > 0 && origins_list.is_empty();
    }

    let deep_spot = q.exact_origin && has_origin;
    let (yaw_step, pitch_step) = if deep_spot {
        (0.25, 0.25)
    } else if has_origin {
        if q.fine_scan { (0.5, 0.5) } else { (1.0, 1.0) }
    } else if q.fine_scan {
        (2.0, 2.0)
    } else {
        (3.0, 4.0)
    };
    let min_stability = if deep_spot {
        q.min_stability.min(0.05)
    } else {
        q.min_stability
    };

    let types_list: Vec<ThrowType> = q.types.clone().unwrap_or_else(|| ALL_TYPES.to_vec());

    let coverage_map: Mutex<HashMap<(i32, i32), i32>> = Mutex::new(HashMap::new());
    let pruned: Option<Mutex<PrunedMap>> = if q.referee {
        Some(Mutex::new(HashMap::new()))
    } else {
        None
    };
    // `TargetSolver.cs:429-445`'s `onPruned` wrapper: a concrete
    // `(type, strength, run)` is recorded as-is; the free-space pre-filter's
    // NaN-strength "whole kind" sentinel (`LineupSolver.cs:298`, fired
    // before any origin's own type/strength/run loop runs) fans out over
    // every kind the query actually asked for instead.
    let on_pruned_closure = |_feet: V3, ty: ThrowType, strength: f32, run: f32, why: &str| {
        let Some(p) = &pruned else { return };
        let mut map = p.lock().unwrap();
        if strength.is_nan() {
            for &t in &types_list {
                let runs: &[f32] = if t == ThrowType::RunJumpThrow {
                    &sweep::RUN_YAW_OFFSETS
                } else {
                    &sweep::NO_RUN_OFFSET
                };
                for &r in runs {
                    for &s in q.strengths.as_deref().unwrap_or(&sweep::ALL_STRENGTHS) {
                        map.insert((t, s.to_bits(), r.to_bits()), why.to_string());
                    }
                }
            }
        } else {
            map.insert((ty, strength.to_bits(), run.to_bits()), why.to_string());
        }
    };
    let on_pruned_ref: Option<&OnPruned> = Some(&on_pruned_closure);

    let click_kind_at: Option<Box<dyn Fn(V3) -> bool + Sync>> = if has_origin && !deep_spot {
        Some(Box::new(move |feet: V3| {
            distance2(xy(feet), origin_click) <= CLICK_KIND_RADIUS
        }))
    } else {
        None
    };
    let own_bucket_at: Option<Box<dyn Fn(V3) -> bool + Sync>> = if !pinned_origins.is_empty() {
        let pinned = pinned_origins.clone();
        Some(Box::new(move |feet: V3| {
            pinned.contains(&(
                (feet.x * 4.0).round_ties_even() as i32,
                (feet.y * 4.0).round_ties_even() as i32,
            ))
        }))
    } else {
        None
    };
    progress(Phase::Sweep, origins_list.len());
    let sweep_opts = sweep::SweepOptions {
        yaw_step_deg: yaw_step,
        pitch_step_deg: pitch_step,
        dedupe_bucket_size: if has_origin { 8.0 } else { 64.0 },
        strengths: q.strengths.as_deref(),
        constants: Some(k),
        // `sweep::better`'s own distance-to-target tie-break (`LineupSolver.cs:918-956`) is
        // exactly the kind of "distance to center" preference `s6g2_target_area.md` rules out for
        // `Target::Area` ("любая точка внутри одинаково годна") - passing `None` here drops
        // straight to the next tie-break (bounces already compared first, then rest_crossings,
        // flight_time, ordinal), none of which favor any particular spot inside the area.
        target: match &q.target {
            Target::Point { .. } => Some(target),
            Target::Area(_) => None,
        },
        extra_fronts: if has_origin { &[] } else { &q.spawn_fronts },
        max_refine_seeds: if deep_spot {
            usize::MAX
        } else {
            sweep::MAX_REFINE_SEEDS
        },
        keep_every_kind: deep_spot,
        keep_every_kind_at: click_kind_at.as_deref(),
        own_bucket_at: own_bucket_at.as_deref(),
        measured_weak_click_reach: has_origin,
        // Bug fix: a single candidate per (origin, throw kind) bucket meant a bucket that would
        // have yielded a lineup for a point sitting inside the area could come up empty for the
        // area itself, if its one candidate happened to fail exact verification while a
        // runner-up in the same bucket would have passed. `Target::Area` keeps up to
        // `AREA_BUCKET_CANDIDATES` ranked candidates per bucket below and verifies them in
        // rounds; `Target::Point` keeps its original single slot.
        keep_per_bucket: match &q.target {
            Target::Point { .. } => 1,
            Target::Area(_) => AREA_BUCKET_CANDIDATES,
        },
        // Review G2 round 3, item 2: a per-origin range/window check measured only to the zone's
        // centroid can wrongly prune or narrow away a point that is genuinely reachable via a
        // different part of a spread-out area. `Target::Point`'s own single point *is* its
        // centroid, so leaving this `false` there is exact, not an approximation.
        widen_to_zone_extent: matches!(q.target, Target::Area(_)),
        on_pruned: on_pruned_ref,
        coverage: Some(&coverage_map),
        on_origin: hooks.on_origin,
        cancel: Some(cancel),
    };
    // Bug fix: `Target::Area` keeps `AREA_BUCKET_CANDIDATES` ranked candidates per bucket
    // (`sweep_opts.keep_per_bucket` above) instead of collapsing straight to one - the round-based
    // verify loop below tries each bucket's runners-up in turn, so a bucket whose best candidate
    // fails exact verification isn't automatically empty if a later one in the same bucket would
    // have passed.
    let area_buckets: Option<Vec<Vec<Lineup>>> = match &q.target {
        Target::Point { .. } => None,
        Target::Area(_) => Some(sweep::solve_buckets(
            &grid,
            &zone_crossings,
            &types_list,
            &origins_list,
            &sweep_opts,
        )),
    };
    let candidates: Vec<Lineup> = match &area_buckets {
        Some(buckets) => buckets.iter().filter_map(|b| b.first().copied()).collect(),
        None => sweep::solve(
            &grid,
            &zone_crossings,
            &types_list,
            &origins_list,
            &sweep_opts,
        ),
    };
    let candidate_count: usize = match &area_buckets {
        Some(buckets) => buckets.iter().map(Vec::len).sum(),
        None => candidates.len(),
    };
    progress(Phase::Verify, candidate_count);
    if cancel.load(Ordering::Relaxed) {
        return cancelled_solve(
            target,
            origins_list.len(),
            collider,
            player_collider,
            collider_glass_gone,
        );
    }

    // `Target::Point` keeps verify's own continuous point+tolerance accept test
    // (`verify::within_tolerance`); `Target::Area` passes neither, which routes verify's
    // `accepts()` to its cell-based `in_zone` fallback instead - the *same* zone built above, so
    // "landed anywhere inside the area" is accepted with no distance test at all. Together with
    // `sweep_opts.target` above (also `None` for `Target::Area`, dropping `sweep::better`'s own
    // distance tie-break) this is what actually satisfies `s6g2_target_area.md`'s ranking
    // requirement - "distance to center must not penalize inside the area": `in_zone` is a hard
    // yes/no, and with no distance tie-break left in either the dedup or the accept step, nothing
    // between the sweep and the final list ever scores how far inside a hit landed;
    // `rank::cmp_lineups` itself never sorted by distance to the target to begin with, only by
    // aim/positioning quality and, with an origin click, distance from *that*.
    let (verify_aim_target, verify_tolerance) = match &q.target {
        Target::Point { tolerance, .. } => (Some(target), Some(*tolerance)),
        Target::Area(_) => (None, None),
    };
    // `exhaustive_exact_spot`'s own accept test is a flat XY circle around `target`, no zone
    // lookup at all - exactly the query for `Target::Point`, but for `Target::Area` only ever a
    // coarse pre-filter: every lineup it produces is re-checked by `verify_exact` (the real
    // `in_zone` test) before it can survive, so this only needs to be wide enough not to
    // prematurely drop a genuine hit - the distance from the representative point to the
    // farthest polygon vertex, plus a voxel of slack, comfortably covers the whole area
    // regardless of its shape.
    let exhaustive_tolerance = match &q.target {
        Target::Point { tolerance, .. } => *tolerance,
        Target::Area(area) => {
            let mut r: f32 = 0.0;
            for p in &area.polygon {
                let dx = p[0] - target.x;
                let dy = p[1] - target.y;
                r = r.max((dx * dx + dy * dy).sqrt());
            }
            r + VOXEL_SIZE
        }
    };
    // The exact polygon+z predicate `verify_exact` combines with its own cell-based `in_zone`
    // test (`verify.rs`'s `accepts()`) - closes the cell-boundary gap `in_zone` alone leaves (a
    // zone cell qualifies by its *center* being inside the polygon, so a rest point near that
    // cell's far edge could otherwise sit a few units past the drawn boundary).
    let area_predicate: Option<Box<dyn Fn(V3) -> bool + Sync + '_>> = match &q.target {
        Target::Point { .. } => None,
        Target::Area(area) => Some(Box::new(move |p: V3| {
            point_in_area_polygon(&area.polygon, p.x, p.y)
                && z_in_range(area.z_min, area.z_max, p.z)
        })),
    };
    let verify_opts = verify::VerifyOptions {
        min_stability,
        constants: Some(k),
        aim_target: verify_aim_target,
        tolerance: verify_tolerance,
        area_accept: area_predicate.as_deref(),
        collider_glass_gone: collider_glass_gone.as_ref(),
        on_candidate: hooks.on_candidate,
        cancel: Some(cancel),
        step_deg: if q.precise_aim {
            PRECISE_STEP_DEG
        } else {
            verify::STEP_DEG
        },
        aim_reach: if q.precise_aim {
            PRECISE_AIM_REACH
        } else {
            verify::AIM_REACH
        },
        wide_stability: q.precise_aim,
    };
    let mut verified = match &area_buckets {
        Some(buckets) => {
            verify_buckets_in_rounds(&grid, &collider, &zone_crossings, buckets, &verify_opts)
        }
        None => verify::verify_exact(&grid, &collider, &zone_crossings, &candidates, &verify_opts),
    };
    if cancel.load(Ordering::Relaxed) {
        return cancelled_solve(
            target,
            origins_list.len(),
            collider,
            player_collider,
            collider_glass_gone,
        );
    }

    let mut referee_lineups: Option<Vec<Lineup>> = None;
    let mut lattice_flown = false;
    if deep_spot && !origins_list.is_empty() && (q.referee || verified.is_empty()) {
        lattice_flown = true;
        progress(Phase::Exhaustive, origins_list.len());
        let mut lattice: Vec<Lineup> = Vec::new();
        for &o in &origins_list {
            lattice.extend(verify::exhaustive_exact_spot(
                &collider,
                o,
                target,
                exhaustive_tolerance,
                &types_list,
                q.strengths.as_deref(),
                Some(k),
                1.0,
                q.referee,
                None,
                area_predicate.as_deref(),
                Some(cancel),
            ));
        }
        if cancel.load(Ordering::Relaxed) {
            return cancelled_solve(
                target,
                origins_list.len(),
                collider,
                player_collider,
                collider_glass_gone,
            );
        }
        if q.referee {
            progress(Phase::Verify, lattice.len());
            referee_lineups = Some(verify::verify_exact(
                &grid,
                &collider,
                &zone_crossings,
                &lattice,
                &verify_opts,
            ));
            if cancel.load(Ordering::Relaxed) {
                return cancelled_solve(
                    target,
                    origins_list.len(),
                    collider,
                    player_collider,
                    collider_glass_gone,
                );
            }
        }
        if verified.is_empty() {
            let rescued: Vec<Lineup> = if q.referee {
                let mut order: Vec<FeetKey> = Vec::new();
                let mut groups: HashMap<FeetKey, Vec<Lineup>> = HashMap::new();
                for l in &lattice {
                    let key = (
                        l.feet.x.to_bits(),
                        l.feet.y.to_bits(),
                        l.feet.z.to_bits(),
                        l.throw_type,
                        l.strength.to_bits(),
                        l.run_yaw_offset_deg.to_bits(),
                    );
                    if !groups.contains_key(&key) {
                        order.push(key);
                    }
                    groups.entry(key).or_default().push(*l);
                }
                order
                    .into_iter()
                    .map(|key| {
                        let g = &groups[&key];
                        let mut best = g[0];
                        let mut best_d = (best.rest_point - target).length_squared();
                        for &l in &g[1..] {
                            let d = (l.rest_point - target).length_squared();
                            if d < best_d {
                                best = l;
                                best_d = d;
                            }
                        }
                        best
                    })
                    .collect()
            } else {
                lattice.clone()
            };
            progress(Phase::Verify, rescued.len());
            verified =
                verify::verify_exact(&grid, &collider, &zone_crossings, &rescued, &verify_opts);
            if cancel.load(Ordering::Relaxed) {
                return cancelled_solve(
                    target,
                    origins_list.len(),
                    collider,
                    player_collider,
                    collider_glass_gone,
                );
            }
        }
    }

    let mut crouch_only: HashSet<(i32, i32, i32)> =
        crouch_only_extras.iter().map(|&v| key3(v)).collect();
    if let Some(spots) = &map.stand_spots {
        crouch_only.extend(spots.iter().filter(|s| s.crouched).map(|s| key3(s.feet)));
    }
    verified = drop_standing_at_crouch_only(&crouch_only, verified);

    if deep_spot && !origins_list.is_empty() && !lattice_flown {
        let have: HashSet<(ThrowType, u32, u32)> = verified.iter().map(kind_of).collect();
        let missing: Vec<(ThrowType, f32, f32)> =
            sweep::all_kinds(&types_list, q.strengths.as_deref())
                .into_iter()
                .filter(|&(t, s, r)| !have.contains(&(t, s.to_bits(), r.to_bits())))
                .collect();
        if !missing.is_empty() {
            progress(Phase::Escalate, missing.len());
            let escalated: Vec<Lineup> = origins_list
                .iter()
                .flat_map(|&o| {
                    verify::exhaustive_exact_spot(
                        &collider,
                        o,
                        target,
                        exhaustive_tolerance,
                        &types_list,
                        q.strengths.as_deref(),
                        Some(k),
                        2.0,
                        false,
                        Some(&missing),
                        area_predicate.as_deref(),
                        Some(cancel),
                    )
                })
                .collect();
            if !escalated.is_empty() {
                progress(Phase::Verify, escalated.len());
                let newly = drop_standing_at_crouch_only(
                    &crouch_only,
                    verify::verify_exact(
                        &grid,
                        &collider,
                        &zone_crossings,
                        &escalated,
                        &verify_opts,
                    ),
                );
                verified.extend(newly);
            }
            if cancel.load(Ordering::Relaxed) {
                return cancelled_solve(
                    target,
                    origins_list.len(),
                    collider,
                    player_collider,
                    collider_glass_gone,
                );
            }
            let mut seen: HashSet<(ThrowType, u32, u32)> = HashSet::new();
            let mut still_order: Vec<(ThrowType, u32, u32)> = Vec::new();
            for l in &escalated {
                let kd = kind_of(l);
                if seen.insert(kd) {
                    still_order.push(kd);
                }
            }
            let still_missing: Vec<(ThrowType, f32, f32)> = still_order
                .into_iter()
                .filter(|kd| !verified.iter().any(|l| kind_of(l) == *kd))
                .map(|(t, sb, rb)| (t, f32::from_bits(sb), f32::from_bits(rb)))
                .collect();
            if !still_missing.is_empty() {
                progress(Phase::EscalateFine, still_missing.len());
                let fine: Vec<Lineup> = origins_list
                    .iter()
                    .flat_map(|&o| {
                        verify::exhaustive_exact_spot(
                            &collider,
                            o,
                            target,
                            exhaustive_tolerance,
                            &types_list,
                            q.strengths.as_deref(),
                            Some(k),
                            1.0,
                            false,
                            Some(&still_missing),
                            area_predicate.as_deref(),
                            Some(cancel),
                        )
                    })
                    .collect();
                if !fine.is_empty() {
                    progress(Phase::Verify, fine.len());
                    let newly = drop_standing_at_crouch_only(
                        &crouch_only,
                        verify::verify_exact(
                            &grid,
                            &collider,
                            &zone_crossings,
                            &fine,
                            &verify_opts,
                        ),
                    );
                    verified.extend(newly);
                }
                if cancel.load(Ordering::Relaxed) {
                    return cancelled_solve(
                        target,
                        origins_list.len(),
                        collider,
                        player_collider,
                        collider_glass_gone,
                    );
                }
            }
        }
    }

    // `s6q_robust_aim.md`: precise mode's own post-verify pass - re-centers each of the best
    // `ROBUST_MAX_LINEUPS` survivors' aim on the middle of its own fine-grid working band, then
    // drops anything whose combined robustness comes back under `verify::ROBUST_MIN`. Anything past
    // the cap is left untouched - its own `robustness` stays `None`, which `rank::cmp_lineups`'s
    // robustness-bucket key already ranks behind every measured lineup - since the expensive
    // per-lineup grid is too costly to run over an unbounded result set. Runs before the
    // `Target::Area` strict re-check and the sightline pass below so both see the re-centered rest
    // point, not the pre-centering one.
    if q.precise_aim {
        let robust_zone = zone::zone_lookup(&zone_crossings);
        let cap = ROBUST_MAX_LINEUPS.min(verified.len());
        let (to_center, tail) = verified.split_at(cap);
        let mut centered: Vec<Lineup> = to_center
            .par_iter()
            .filter_map(|l| {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let out = verify::robust_center(
                    &grid,
                    &collider,
                    &player_collider,
                    k,
                    &robust_zone,
                    verify_aim_target,
                    verify_tolerance,
                    area_predicate.as_deref(),
                    collider_glass_gone.as_ref(),
                    l,
                )?;
                (out.robustness.unwrap_or(0.0) >= verify::ROBUST_MIN).then_some(out)
            })
            .collect();
        // Diagnostic only (stderr, never the JSON payload): how many of the analyzed lineups the
        // robustness/safe-teleport filter above actually dropped - silently discarding lineups is
        // exactly the kind of thing worth being able to see happen.
        eprintln!(
            "s6q robust-aim: {} of {} analyzed lineups survived (dropped {})",
            centered.len(),
            to_center.len(),
            to_center.len() - centered.len()
        );
        centered.extend_from_slice(tail);
        verified = centered;
        if cancel.load(Ordering::Relaxed) {
            return cancelled_solve(
                target,
                origins_list.len(),
                collider,
                player_collider,
                collider_glass_gone,
            );
        }
    }

    // `Target::Area`'s own hard promise ("где угодно внутри области" - `s6g2_target_area.md`):
    // `verify_exact`'s `in_zone` accepts by *cell* (a rest point anywhere in a zone cell, even a
    // corner a few units past the drawn edge, since the cell qualified by its *center* being
    // inside the polygon in `area_target_zone`) - re-check every survivor against the exact
    // continuous polygon/z-range here so a lineup can never leak past the boundary the user
    // actually drew, mirroring how `origin_area`'s own pinned origins get the same re-check.
    if let Target::Area(area) = &q.target {
        let strictly_inside = |l: &Lineup| {
            point_in_area_polygon(&area.polygon, l.rest_point.x, l.rest_point.y)
                && z_in_range(area.z_min, area.z_max, l.rest_point.z)
        };
        verified.retain(strictly_inside);
        if let Some(rl) = referee_lineups.as_mut() {
            rl.retain(strictly_inside);
        }
    }

    let mut referee_notes: Option<Vec<String>> = None;
    if let Some(rl) = referee_lineups.as_mut() {
        *rl = drop_standing_at_crouch_only(&crouch_only, std::mem::take(rl));
        let pruned_map = pruned.as_ref().map(|p| p.lock().unwrap());
        referee_notes = Some(explain_misses(
            &grid,
            &zone::zone_lookup(&zone_crossings),
            &candidates,
            &verified,
            rl,
            target,
            k,
            pruned_map.as_deref(),
        ));
    }

    progress(Phase::Sightline, verified.len());
    let raycaster = Bvh::build(&map.mesh, &attr_mask, Some(region)).expect("finite mesh vertices");
    for l in verified.iter_mut() {
        let eye = l.feet + V3::new(0.0, 0.0, eye_height(l.throw_type));
        let landing_eye = l.rest_point + V3::new(0.0, 0.0, DEFENDER_EYE_HEIGHT);
        l.direct_los = !raycaster.blocked(eye, landing_eye);
    }

    progress(Phase::Pins, origins_list.len());
    let mut origin_pins: HashMap<(i32, i32), i32> = HashMap::new();
    for &o in &origins_list {
        let key = (o.x.round_ties_even() as i32, o.y.round_ties_even() as i32);
        origin_pins
            .entry(key)
            .or_insert_with(|| origins::position_pin(&player_collider, o));
    }

    let empty_reason = if !verified.is_empty() {
        None
    } else if pin_filter_emptied {
        Some("no corner/wall stand spots in the chosen area".to_string())
    } else if origins_list.is_empty() {
        Some("no stand spots in range of that throw position - try a wider search, or a spot on the ground".to_string())
    } else {
        Some(format!(
            "none of the {} stand spots in range can land a smoke there",
            origins_list.len()
        ))
    };

    let mut coverage: Vec<[i32; 4]> = coverage_map
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|(xy_key, count)| {
            let pin = origin_pins.get(&xy_key).copied().unwrap_or(0);
            [xy_key.0, xy_key.1, count, pin]
        })
        .collect();
    // Deterministic (not just a `HashMap`'s process-random order); the
    // reference's own `ConcurrentDictionary` order is likewise unspecified,
    // and nothing downstream depends on this array's order.
    coverage.sort_by_key(|c| (c[0], c[1]));

    TargetSolve {
        target,
        origins: origins_list.len(),
        coverage,
        lineups: verified,
        empty_reason,
        referee: referee_lineups,
        referee_notes,
        collider,
        player_collider,
        collider_glass_gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::mesh::{MeshObject, ObjectKind, SurfaceProperty};
    use std::sync::atomic::AtomicBool;

    fn flat_plane(half: f32) -> CollisionMesh {
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

    fn add_quad(mesh: &mut CollisionMesh, quad: [[f32; 3]; 4], source_index: u32) {
        let attr = mesh.attributes[0].clone();
        let idx = mesh.add_attribute(attr).unwrap();
        let obj = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldMesh,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index,
            hull_flags: None,
        });
        mesh.push_triangles(
            &quad,
            &[[0, 1, 2], [0, 2, 3]],
            idx,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
    }

    #[test]
    fn settle_target_pushes_off_a_wall_and_drops_to_the_floor() {
        // A flat floor at z=0, plus a vertical wall at x=50 crossing it.
        let mut mesh = flat_plane(400.0);
        add_quad(
            &mut mesh,
            [
                [50.0, -300.0, -50.0],
                [50.0, 300.0, -50.0],
                [50.0, 300.0, 150.0],
                [50.0, -300.0, 150.0],
            ],
            1,
        );
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-400.0, -400.0, -8.0),
            max: V3::new(400.0, 400.0, 256.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();

        // 2u from the wall (well inside `TargetWallClearance` = 8u), floating
        // just above the floor.
        let settled = settle_target(&collider, &grid, V3::new(48.0, 0.0, 5.0));
        let gap_from_wall = (50.0 - settled.x).abs();
        assert!(
            gap_from_wall >= 7.0,
            "expected the target pushed at least ~8u off the wall, got gap {gap_from_wall}"
        );
        assert!(
            settled.z.abs() < 8.0,
            "expected the target dropped onto the z=0 floor, got z={}",
            settled.z
        );
    }

    #[test]
    fn drop_standing_at_crouch_only_keeps_crouch_kinds_and_drops_others() {
        let crouch_spot = V3::new(10.0, 20.0, 0.0);
        let open_spot = V3::new(500.0, 0.0, 0.0);
        let mut crouch_only = HashSet::new();
        crouch_only.insert(key3(crouch_spot));

        let stand_at_crouch_spot = Lineup::new(
            crouch_spot,
            0.0,
            -10.0,
            ThrowType::Stand,
            V3::ZERO,
            0,
            1.0,
            1,
        );
        let crouch_at_crouch_spot = Lineup::new(
            crouch_spot,
            0.0,
            -10.0,
            ThrowType::Crouch,
            V3::ZERO,
            0,
            1.0,
            1,
        );
        let crouch_jump_at_crouch_spot = Lineup::new(
            crouch_spot,
            0.0,
            -10.0,
            ThrowType::CrouchJumpThrow,
            V3::ZERO,
            0,
            1.0,
            1,
        );
        let stand_at_open_spot =
            Lineup::new(open_spot, 0.0, -10.0, ThrowType::Stand, V3::ZERO, 0, 1.0, 1);

        let kept = drop_standing_at_crouch_only(
            &crouch_only,
            vec![
                stand_at_crouch_spot,
                crouch_at_crouch_spot,
                crouch_jump_at_crouch_spot,
                stand_at_open_spot,
            ],
        );
        assert_eq!(kept.len(), 3, "kept {kept:?}");
        assert!(
            !kept
                .iter()
                .any(|l| l.feet == crouch_spot && l.throw_type == ThrowType::Stand)
        );
        assert!(
            kept.iter()
                .any(|l| l.feet == open_spot && l.throw_type == ThrowType::Stand)
        );

        // Empty `crouch_only` is a no-op (the reference's own fast path).
        let unfiltered = drop_standing_at_crouch_only(&HashSet::new(), vec![stand_at_open_spot]);
        assert_eq!(unfiltered.len(), 1);
    }

    #[test]
    fn snap_target_to_ground_picks_nearest_to_expected_when_stacked() {
        let mut mesh = flat_plane(300.0);
        // A second floor ~80u up, over the same column. Offset off the
        // -32/-16/0/16/32... cell-boundary lattice `VoxelGrid::build` snaps
        // its origin to, so neither floor straddles a cell boundary
        // (ambiguous which of the two touching cells is "solid" - a known
        // voxel-quantization edge case, not what this test is about).
        let attr = mesh.attributes[0].clone();
        let idx = mesh.add_attribute(attr).unwrap();
        let obj = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldMesh,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 1,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[
                [-300.0, -300.0, 84.0],
                [300.0, -300.0, 84.0],
                [300.0, 300.0, 84.0],
                [-300.0, 300.0, 84.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            idx,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-300.0, -300.0, -8.0),
            max: V3::new(300.0, 300.0, 256.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let (x, y, _) = grid.cell_of(V3::new(4.0, 0.0, 0.0));
        // The ground floor sits exactly on a cell boundary (z=0); accept the
        // full voxel of quantization slop the grid can introduce there.
        let low = snap_target_to_ground(&grid, x, y, Some(5.0)).unwrap();
        assert!((low.z - 0.0).abs() <= 16.0, "low.z = {}", low.z);
        let high = snap_target_to_ground(&grid, x, y, Some(75.0)).unwrap();
        assert!((high.z - 84.0).abs() <= 16.0, "high.z = {}", high.z);
    }

    fn solid_block(half: f32, z_lo: f32, z_hi: f32, step: f32) -> CollisionMesh {
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
        // Planes spaced closer than the 16u voxel cell size so every voxel
        // layer between `z_lo` and `z_hi` has a crossing triangle and reads
        // solid - an effectively solid interior for the voxel grid, without
        // needing a closed/watertight volume.
        let mut z = z_lo;
        while z <= z_hi {
            mesh.push_triangles(
                &[
                    [-half, -half, z],
                    [half, -half, z],
                    [half, half, z],
                    [-half, half, z],
                ],
                &[[0, 1, 2], [0, 2, 3]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
            z += step;
        }
        mesh
    }

    #[test]
    fn no_reachable_landing_cells_reports_empty_reason() {
        let mesh = solid_block(400.0, -64.0, 64.0, 8.0);
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: None,
            spawns: vec![],
            attribute_filter: None,
        };
        let q = SolveQuery {
            // Buried in the solid block's interior: no open cell is ever
            // within tolerance.
            target: Target::Point {
                pos: V3::new(0.0, 0.0, 0.0),
                has_z: true,
                tolerance: 1.0,
            },
            ..Default::default()
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        assert!(solve.lineups.is_empty());
        assert!(solve.empty_reason.is_some());
    }

    /// A target far below the mesh used to invert the region's z range (`max.z < min.z`),
    /// which sent `VoxelGrid::build` a negative cell count and aborted the whole process. It
    /// must now come back as a normal empty result instead.
    #[test]
    fn target_far_below_mesh_does_not_panic() {
        let mesh = flat_plane(400.0);
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: None,
            spawns: vec![],
            attribute_filter: None,
        };
        let q = SolveQuery {
            target: Target::Point {
                pos: V3::new(0.0, 0.0, -1400.0),
                has_z: true,
                tolerance: 80.0,
            },
            ..Default::default()
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        assert!(solve.lineups.is_empty());
        assert!(solve.empty_reason.is_some());
    }

    // ---- `s6g_origin_area.md`: `origin_area` ---------------------------------------------------

    #[test]
    fn point_in_area_polygon_inside_outside_and_on_the_boundary() {
        let square = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(point_in_area_polygon(&square, 5.0, 5.0), "center");
        assert!(!point_in_area_polygon(&square, 20.0, 5.0), "outside");
        // On an edge, exactly and to within float slop - both must read "inside"
        // (`s6g_origin_area.md`: "точки на границе — внутри").
        assert!(point_in_area_polygon(&square, 5.0, 0.0), "edge midpoint");
        assert!(
            point_in_area_polygon(&square, 5.0, 0.001),
            "edge, tiny slop"
        );
        assert!(point_in_area_polygon(&square, 0.0, 0.0), "vertex");
        // The plain ray-cast below (`yi > y`, strict `<`) reads the top/right edges and the far
        // corner as outside on its own - only `point_on_segment`'s explicit check makes these
        // "inside" too, so these three actually exercise it (unlike the bottom edge/near-corner
        // cases above, which the ray-cast alone already gets right).
        assert!(point_in_area_polygon(&square, 5.0, 10.0), "top edge");
        assert!(point_in_area_polygon(&square, 10.0, 5.0), "right edge");
        assert!(point_in_area_polygon(&square, 10.0, 10.0), "far corner");
    }

    #[test]
    fn point_in_area_polygon_concave_notch_is_outside() {
        // An "L": a wide bottom bar (y in 0..4, x in 0..10) plus a narrower upright on the right
        // (y in 4..10, x in 6..10) - the notch at x in 0..6, y in 4..10 is not part of the shape.
        let l_shape = [
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [6.0, 10.0],
            [6.0, 4.0],
            [0.0, 4.0],
        ];
        assert!(point_in_area_polygon(&l_shape, 2.0, 2.0), "bottom bar");
        assert!(point_in_area_polygon(&l_shape, 8.0, 8.0), "upright");
        assert!(!point_in_area_polygon(&l_shape, 2.0, 8.0), "in the notch");
    }

    #[test]
    fn point_in_area_polygon_self_intersecting_bowtie_uses_even_odd_rule() {
        // A bowtie: edges (0,0)-(10,10) and (0,10)-(10,0) cross at (5,5), so the even-odd rule
        // fills only the top and bottom triangles, leaving the left/right "wings" empty.
        let bowtie = [[0.0, 0.0], [10.0, 10.0], [0.0, 10.0], [10.0, 0.0]];
        assert!(point_in_area_polygon(&bowtie, 5.0, 8.0), "top triangle");
        assert!(
            !point_in_area_polygon(&bowtie, 1.0, 8.0),
            "left wing, cancelled out"
        );
        assert!(point_in_area_polygon(&bowtie, 5.0, 2.0), "bottom triangle");
        assert!(
            !point_in_area_polygon(&bowtie, 1.0, 2.0),
            "left wing, cancelled out"
        );
    }

    #[test]
    fn z_in_range_respects_an_open_or_closed_range() {
        assert!(z_in_range(None, None, -1000.0));
        assert!(z_in_range(None, None, 1000.0));

        assert!(!z_in_range(Some(0.0), Some(100.0), -1.0));
        assert!(z_in_range(Some(0.0), Some(100.0), 0.0));
        assert!(z_in_range(Some(0.0), Some(100.0), 50.0));
        assert!(z_in_range(Some(0.0), Some(100.0), 100.0));
        assert!(!z_in_range(Some(0.0), Some(100.0), 100.1));
    }

    /// Full `solve_for_target` wiring: an `origin_area` polygon far outside the default
    /// `origin_reach` (300u) still reaches its own stand spots (`s6g_origin_area.md`: "регион
    /// строится по рамке" - the region is built from the polygon's own bounding box, not the
    /// reach circle), and only the spot actually inside the polygon is kept.
    #[test]
    fn solve_for_target_origin_area_limits_candidates_to_the_polygon() {
        let mesh = flat_plane(2000.0);
        let inside_spot = V3::new(1000.0, 1000.0, 0.0);
        let outside_spot = V3::new(-1000.0, -1000.0, 0.0);
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: Some(vec![
                StandSpotOrigin {
                    feet: inside_spot,
                    crouched: false,
                },
                StandSpotOrigin {
                    feet: outside_spot,
                    crouched: false,
                },
            ]),
            spawns: vec![],
            attribute_filter: None,
        };
        let q = SolveQuery {
            target: Target::Point {
                pos: V3::new(0.0, 0.0, 0.0),
                has_z: true,
                tolerance: 80.0,
            },
            origin_area: Some(OriginArea {
                polygon: vec![
                    [900.0, 900.0],
                    [1100.0, 900.0],
                    [1100.0, 1100.0],
                    [900.0, 1100.0],
                ],
                z_min: None,
                z_max: None,
            }),
            ..Default::default()
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        // `solve.origins` alone only reflects `origins_list` membership, which the stand-spot
        // branch fills straight from `in_reach`/`in_area_z` regardless of the region's own XY
        // bounds - it would stay 1 even if the region were still built from the old reach circle.
        // What actually depends on the region is `sweep::solve`'s free-space prefilter (built
        // over the region's `VoxelGrid`): an origin outside that grid never reaches `on_origin`
        // at all. Recording every swept origin here and requiring `inside_spot` among them is
        // what actually exercises "регион строится по рамке" - confirmed by temporarily reverting
        // `reach_min_xy`/`reach_max_xy` to the old circle formula, which makes this assert fail
        // (region only reaches -500..500ish, well short of 900..1100), then restoring the fix.
        let swept: Mutex<Vec<V3>> = Mutex::new(Vec::new());
        let on_origin = |feet: V3, _hits: usize| {
            swept.lock().unwrap().push(feet);
        };
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: Some(&on_origin),
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        assert_eq!(
            solve.origins, 1,
            "expected only the stand spot inside the polygon"
        );
        let swept = swept.into_inner().unwrap();
        assert!(
            swept
                .iter()
                .any(|&f| (f - inside_spot).length_squared() < 1.0),
            "expected the polygon's own stand spot to reach the sweep (region built from the \
             polygon's bounding box), got {swept:?}"
        );
    }

    // ---- `s6g2_target_area.md`: `Target::Area` -------------------------------------------------

    #[test]
    fn area_target_zone_includes_open_floor_cells_and_the_margin_cell_above() {
        let mesh = flat_plane(400.0);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-400.0, -400.0, -16.0),
            max: V3::new(400.0, 400.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, VOXEL_SIZE, bounds).unwrap();
        let area = TargetArea {
            polygon: vec![[-50.0, -50.0], [50.0, -50.0], [50.0, 50.0], [-50.0, 50.0]],
            z_min: None,
            z_max: None,
        };
        let zone = area_target_zone(&grid, &area);
        assert!(!zone.is_empty());
        // A cell whose center sits outside the polygon can still be admitted if it is close
        // enough to an edge to genuinely overlap it (item 5's fix) - exact per-lineup membership
        // is enforced later, not at the zone-column level.
        let mut any_edge_admitted = false;
        for &(index, _) in &zone {
            assert!(!grid.is_solid(index), "zone cell must be open");
            let c = grid.cell_center(index);
            let inside = point_in_area_polygon(&area.polygon, c.x, c.y);
            let near_edge = near_polygon_edge(&area.polygon, c.x, c.y, VOXEL_SIZE * 0.7072);
            assert!(
                inside || near_edge,
                "zone cell center {c:?} must be inside the polygon or within a cell's diagonal of an edge"
            );
            any_edge_admitted |= !inside && near_edge;
        }
        assert!(
            any_edge_admitted,
            "expected at least one edge-admitted cell at this polygon/grid alignment"
        );
        // A cell 20u outside the polygon's own bbox must never appear, however close to the
        // floor.
        assert!(
            zone.iter()
                .all(|&(i, _)| grid.cell_center(i).x <= 66.0 && grid.cell_center(i).x >= -66.0),
            "zone must not spill past the polygon's own bounding box"
        );
        // The margin cell above the floor (`s6g2_target_area.md`: "плюс клетка над ними") means
        // at least one XY column has two stacked zone cells, not just the floor cell alone.
        let mut column_counts: HashMap<(i32, i32), usize> = HashMap::new();
        for &(index, _) in &zone {
            let (cx, cy, _) = grid.cell_of(grid.cell_center(index));
            *column_counts.entry((cx, cy)).or_insert(0) += 1;
        }
        assert!(
            column_counts.values().any(|&n| n >= 2),
            "expected at least one column with the floor cell plus its margin cell above, got {column_counts:?}"
        );
        // Item 8's test gap: the grid's bounds put several open cells above the floor in every
        // column (up to z=128), so a column with more than 2 zone cells would mean the "solid
        // directly beneath" rule stopped gating which open cells qualify - swapping that rule out
        // for `true` must fail this, not just silently keep passing.
        assert!(
            column_counts.values().all(|&n| n <= 2),
            "no column should have more than the floor cell plus its one margin cell above, got {column_counts:?}"
        );
    }

    #[test]
    fn area_target_zone_covers_a_step_between_two_floor_heights() {
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
        // Low floor: x in [-200,0], z=4 (off the grid's own global 16u cell lattice - see
        // `snap_target_to_ground_picks_nearest_to_expected_when_stacked`'s comment: a floor
        // exactly on a lattice multiple of the voxel size is a solid/open quantization edge
        // case, not what this test is about).
        mesh.push_triangles(
            &[
                [-200.0, -200.0, 4.0],
                [0.0, -200.0, 4.0],
                [0.0, 200.0, 4.0],
                [-200.0, 200.0, 4.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        // The step: a second floor at x in [0,200], z=44 (also off-lattice).
        add_quad(
            &mut mesh,
            [
                [0.0, -200.0, 44.0],
                [200.0, -200.0, 44.0],
                [200.0, 200.0, 44.0],
                [0.0, 200.0, 44.0],
            ],
            1,
        );
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-200.0, -200.0, -16.0),
            max: V3::new(200.0, 200.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, VOXEL_SIZE, bounds).unwrap();
        let area = TargetArea {
            polygon: vec![
                [-150.0, -150.0],
                [150.0, -150.0],
                [150.0, 150.0],
                [-150.0, 150.0],
            ],
            z_min: None,
            z_max: None,
        };
        let zone = area_target_zone(&grid, &area);
        assert!(!zone.is_empty());
        let low_present = zone.iter().any(|&(i, _)| {
            let c = grid.cell_center(i);
            c.x < 0.0 && (c.z - 4.0).abs() < 24.0
        });
        let high_present = zone.iter().any(|&(i, _)| {
            let c = grid.cell_center(i);
            c.x > 0.0 && (c.z - 44.0).abs() < 24.0
        });
        assert!(low_present, "expected zone cells near the low floor");
        assert!(
            high_present,
            "expected zone cells near the stepped-up floor"
        );
    }

    #[test]
    fn area_target_zone_z_range_picks_one_of_two_stacked_floors() {
        // Two floors over the same column, both off the grid's own global 16u cell lattice (see
        // `snap_target_to_ground_picks_nearest_to_expected_when_stacked`'s comment on why a
        // floor exactly on a lattice multiple is a quantization edge case, not what this test is
        // about) - a known-good pattern already used there.
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
                [-300.0, -300.0, 4.0],
                [300.0, -300.0, 4.0],
                [300.0, 300.0, 4.0],
                [-300.0, 300.0, 4.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        add_quad(
            &mut mesh,
            [
                [-300.0, -300.0, 84.0],
                [300.0, -300.0, 84.0],
                [300.0, 300.0, 84.0],
                [-300.0, 300.0, 84.0],
            ],
            1,
        );
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-300.0, -300.0, -8.0),
            max: V3::new(300.0, 300.0, 256.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, VOXEL_SIZE, bounds).unwrap();
        let polygon = vec![[-50.0, -50.0], [50.0, -50.0], [50.0, 50.0], [-50.0, 50.0]];

        // No z range: both floors' cells appear.
        let area_all = TargetArea {
            polygon: polygon.clone(),
            z_min: None,
            z_max: None,
        };
        let zone_all = area_target_zone(&grid, &area_all);
        let has_low = zone_all
            .iter()
            .any(|&(i, _)| (grid.cell_center(i).z - 4.0).abs() < 24.0);
        let has_high = zone_all
            .iter()
            .any(|&(i, _)| (grid.cell_center(i).z - 84.0).abs() < 24.0);
        assert!(
            has_low && has_high,
            "expected cells at both floor heights without a z range"
        );

        // `z_min` above the ground floor: only the upper floor's cells should survive.
        let area_upper = TargetArea {
            polygon,
            z_min: Some(50.0),
            z_max: None,
        };
        let zone_upper = area_target_zone(&grid, &area_upper);
        assert!(!zone_upper.is_empty());
        assert!(
            zone_upper
                .iter()
                .all(|&(i, _)| grid.cell_center(i).z >= 50.0),
            "z_min must exclude the ground floor"
        );
        assert!(
            zone_upper
                .iter()
                .any(|&(i, _)| (grid.cell_center(i).z - 84.0).abs() < 24.0),
            "expected the upper floor to still be present"
        );
    }

    #[test]
    fn zone_representative_point_pulls_the_mean_onto_an_actual_zone_cell() {
        let mesh = flat_plane(400.0);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-400.0, -400.0, -16.0),
            max: V3::new(400.0, 400.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, VOXEL_SIZE, bounds).unwrap();
        // An L-shaped area: the raw mean of its own footprint falls in the missing corner
        // (notch), which is not part of the zone at all.
        let l_shape = TargetArea {
            polygon: vec![
                [0.0, 0.0],
                [100.0, 0.0],
                [100.0, 100.0],
                [60.0, 100.0],
                [60.0, 40.0],
                [0.0, 40.0],
            ],
            z_min: None,
            z_max: None,
        };
        let zone = area_target_zone(&grid, &l_shape);
        assert!(!zone.is_empty());
        let rep = zone_representative_point(&grid, &zone).expect("non-empty zone");
        // The representative point must itself be a real zone cell's center.
        assert!(
            zone.iter().any(|&(i, _)| grid.cell_center(i) == rep),
            "representative point must be an actual zone cell, got {rep:?}"
        );
        assert!(
            point_in_area_polygon(&l_shape.polygon, rep.x, rep.y),
            "representative point must be inside the polygon, got {rep:?}"
        );
    }

    #[test]
    fn zone_representative_point_is_none_for_an_empty_zone() {
        assert!(
            zone_representative_point(
                &{
                    let mesh = flat_plane(10.0);
                    let mask = all_mask(&mesh);
                    VoxelGrid::build(
                        &mesh,
                        &mask,
                        VOXEL_SIZE,
                        Aabb {
                            min: V3::new(-10.0, -10.0, -16.0),
                            max: V3::new(10.0, 10.0, 32.0),
                        },
                    )
                    .unwrap()
                },
                &[]
            )
            .is_none()
        );
    }

    /// `s6g2_target_area.md`'s ranking requirement: any position inside the area must not be
    /// penalized relative to any other. `Target::Area` routes `verify`'s accept test to
    /// `in_zone` (cell membership, a hard yes/no) instead of a distance-scored circle - both a
    /// zone cell right next to the area's own mean and one deliberately far from it accept
    /// identically.
    #[test]
    fn in_zone_accepts_positions_near_and_far_from_the_zone_mean_alike() {
        let mesh = flat_plane(400.0);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-400.0, -400.0, -16.0),
            max: V3::new(400.0, 400.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, VOXEL_SIZE, bounds).unwrap();
        let area = TargetArea {
            polygon: vec![
                [-200.0, -200.0],
                [200.0, -200.0],
                [200.0, 200.0],
                [-200.0, 200.0],
            ],
            z_min: None,
            z_max: None,
        };
        let zone = area_target_zone(&grid, &area);
        assert!(
            zone.len() > 4,
            "expected a sizeable zone to pick far-apart cells from"
        );
        let lookup = zone::zone_lookup(&zone);
        // `zone_representative_point` already picks the zone cell nearest the raw mean - that is
        // "near" here; "far" is whichever zone cell sits farthest from that same mean.
        let mean = zone_representative_point(&grid, &zone).unwrap();
        let near = mean;
        let mut far = mean;
        let mut far_d = 0.0f32;
        for &(index, _) in &zone {
            let c = grid.cell_center(index);
            let d = (c - mean).length_squared();
            if d > far_d {
                far_d = d;
                far = c;
            }
        }
        assert!(verify::in_zone(&grid, &lookup, near));
        assert!(
            verify::in_zone(&grid, &lookup, far),
            "a cell far from the area's own mean must accept just as readily as one near it"
        );
    }

    /// Full `solve_for_target` wiring with `Target::Area`: every returned lineup's rest point
    /// must land inside the drawn polygon.
    #[test]
    fn solve_for_target_area_finds_lineups_whose_rest_point_lands_inside_the_polygon() {
        let mesh = flat_plane(2000.0);
        let stand_spot = V3::new(0.0, 0.0, 0.0);
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: Some(vec![StandSpotOrigin {
                feet: stand_spot,
                crouched: false,
            }]),
            spawns: vec![],
            attribute_filter: None,
        };
        let area = TargetArea {
            polygon: vec![
                [200.0, 200.0],
                [600.0, 200.0],
                [600.0, 600.0],
                [200.0, 600.0],
            ],
            z_min: None,
            z_max: None,
        };
        let q = SolveQuery {
            target: Target::Area(area.clone()),
            origin_click: Some([0.0, 0.0]),
            origin_reach: 50.0,
            ..Default::default()
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        assert!(
            !solve.lineups.is_empty(),
            "expected at least one lineup landing inside the area, empty_reason={:?}",
            solve.empty_reason
        );
        for l in &solve.lineups {
            assert!(
                point_in_area_polygon(&area.polygon, l.rest_point.x, l.rest_point.y),
                "lineup rest point {:?} must be inside the polygon",
                l.rest_point
            );
        }
    }

    // ---- `s6j_pin_filter.md`: `origin_pin_min` --------------------------------------------------

    /// A flat floor plus two perpendicular walls meeting near (50,50) - an open spot, a spot
    /// pressed against one wall, and a spot wedged into the corner they form.
    fn corner_and_open_floor() -> (CollisionMesh, V3, V3, V3) {
        let mut mesh = flat_plane(2000.0);
        add_quad(
            &mut mesh,
            [
                [50.0, -300.0, 0.0],
                [50.0, 300.0, 0.0],
                [50.0, 300.0, 60.0],
                [50.0, -300.0, 60.0],
            ],
            1,
        );
        add_quad(
            &mut mesh,
            [
                [-300.0, 50.0, 0.0],
                [300.0, 50.0, 0.0],
                [300.0, 50.0, 60.0],
                [-300.0, 50.0, 60.0],
            ],
            2,
        );
        let open_spot = V3::new(-100.0, -100.0, 0.0);
        let wall_spot = V3::new(34.0, -100.0, 0.0);
        let corner_spot = V3::new(34.0, 34.0, 0.0);
        (mesh, open_spot, wall_spot, corner_spot)
    }

    /// `origin_pin_min` keeps only origins whose `origins::position_pin` (the same
    /// `player_collider` and function `rank.rs` re-derives each lineup's own pin from) meets the
    /// requested class: 0 keeps every stand spot, 1 keeps wall-or-corner, 2 keeps corner only.
    #[test]
    fn origin_pin_min_filters_origins_by_class() {
        let (mesh, open_spot, wall_spot, corner_spot) = corner_and_open_floor();
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: Some(vec![
                StandSpotOrigin {
                    feet: open_spot,
                    crouched: false,
                },
                StandSpotOrigin {
                    feet: wall_spot,
                    crouched: false,
                },
                StandSpotOrigin {
                    feet: corner_spot,
                    crouched: false,
                },
            ]),
            spawns: vec![],
            attribute_filter: None,
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let make_target = || Target::Point {
            pos: V3::new(0.0, 0.0, 0.0),
            has_z: true,
            tolerance: 400.0,
        };

        let solve0 = solve_for_target(
            &map,
            &SolveQuery {
                target: make_target(),
                origin_pin_min: 0,
                ..Default::default()
            },
            &k,
            &hooks,
            &cancel,
        );
        assert_eq!(solve0.origins, 3, "pin_min 0 keeps every spot");

        let solve1 = solve_for_target(
            &map,
            &SolveQuery {
                target: make_target(),
                origin_pin_min: 1,
                ..Default::default()
            },
            &k,
            &hooks,
            &cancel,
        );
        assert_eq!(solve1.origins, 2, "pin_min 1 keeps wall+corner");

        let solve2 = solve_for_target(
            &map,
            &SolveQuery {
                target: make_target(),
                origin_pin_min: 2,
                ..Default::default()
            },
            &k,
            &hooks,
            &cancel,
        );
        assert_eq!(solve2.origins, 1, "pin_min 2 keeps corner only");
        assert!(
            !solve2.lineups.is_empty(),
            "expected the corner spot to still solve a lineup, empty_reason={:?}",
            solve2.empty_reason
        );
        for l in &solve2.lineups {
            assert!(
                origins::position_pin(&solve2.player_collider, l.feet) >= 2,
                "lineup at {:?} must come from a corner-pinned origin",
                l.feet
            );
        }
    }

    /// `q.spawns_only` skips the general pinned-origins block (it only runs for stand-spot
    /// origins) - without a dedicated pass, "spawns + corner only" would filter every spawn-derived
    /// origin down to nothing.
    #[test]
    fn origin_pin_min_with_spawns_only_adds_pins_before_filtering() {
        let (mesh, _open_spot, _wall_spot, _corner_spot) = corner_and_open_floor();
        // Near the corner but not yet touching either wall (gap ~14u): pin 0 on its own - only
        // `add_pinned_origins_to`'s own corner proposal near it is pin 2.
        let spawn = V3::new(20.0, 20.0, 0.0);
        let map = MapData {
            mesh,
            nav_areas: vec![],
            stand_spots: None,
            spawns: vec![spawn],
            attribute_filter: None,
        };
        let q = SolveQuery {
            target: Target::Point {
                pos: V3::new(0.0, 0.0, 0.0),
                has_z: true,
                tolerance: 400.0,
            },
            spawn_points: vec![spawn],
            spawns_only: true,
            origin_pin_min: 2,
            ..Default::default()
        };
        let k = ThrowConstants::default();
        let cancel = AtomicBool::new(false);
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        assert!(
            solve.origins > 0,
            "expected add_pinned_origins_to's own corner variant of the spawn to survive the filter, empty_reason={:?}",
            solve.empty_reason
        );
    }
}
