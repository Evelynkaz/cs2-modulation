//! Stage 2 of the inverse solver: sweep standable origins and view angles,
//! keep throws whose grenade comes to rest inside the stage 1 landing zone.
//! Ported from `cs2-smoke-solver/src/Solver/LineupSolver.cs:92-533,902-969`.
//!
//! **Deliberate deviation from the reference**: `Solve`'s `origins` parameter
//! defaults to `FindStandableOrigins(...)` (part A's `origins` module) when
//! `null`. This port always requires `origins` explicitly (part A's helper
//! is called by the caller, not by this function), since part A's module was
//! being written concurrently; wiring the default in is a follow-up once
//! part A lands.

use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use geom::math::V3;
use geom::voxel::VoxelGrid;
use rayon::prelude::*;
use sim::{BASE_GRAVITY, ThrowConstants, ThrowSpec, ThrowType, eye_height, simulate_voxel};

use crate::dotnet_sort::{float_cmp, introsort};
use crate::free_space;
use crate::lineup::{Lineup, normalize_yaw};
use crate::reach;
use crate::zone::zone_lookup;

/// `LineupSolver.cs:102` (`YawSpreadDeg`).
pub const YAW_SPREAD_DEG: f32 = 30.0;
/// `LineupSolver.cs:107` (`StandardPitchFloorDeg`).
pub const STANDARD_PITCH_FLOOR_DEG: f32 = -65.0;
/// `LineupSolver.cs:108` (`SteepPitchFloorDeg`).
pub const STEEP_PITCH_FLOOR_DEG: f32 = -89.0;
/// `LineupSolver.cs:112` (`SteepLobMaxRange`).
pub const STEEP_LOB_MAX_RANGE: f32 = 1500.0;
/// `LineupSolver.cs:117` (`MaxRefineSeeds`).
pub const MAX_REFINE_SEEDS: usize = 8;
/// `LineupSolver.cs:118` (`RefineHalfSpan`).
pub const REFINE_HALF_SPAN: i32 = 2;
/// `LineupSolver.cs:119` (`AllStrengths`).
pub const ALL_STRENGTHS: [f32; 3] = [1.0, 0.5, 0.0];
/// `LineupSolver.cs:125` (`RunYawOffsets`).
pub const RUN_YAW_OFFSETS: [f32; 5] = [0.0, 45.0, -45.0, 90.0, -90.0];
/// `LineupSolver.cs:126` (`NoRunOffset`).
pub const NO_RUN_OFFSET: [f32; 1] = [0.0];
/// `LineupSolver.cs:147` (`FreeSpaceBudgetScale`).
pub const FREE_SPACE_BUDGET_SCALE: f32 = 1.6;
/// `LineupSolver.cs:152` (`VerticalReachMargin`).
pub const VERTICAL_REACH_MARGIN: f32 = 128.0;

/// `LineupSolver.cs:158-159` (`Settled`).
pub fn settled(r: &sim::TrajectoryResult) -> bool {
    !r.lost && r.flight_time < sim::MAX_FLIGHT_SECONDS - 0.01
}

/// `LineupSolver.cs:163` (`MaxRange`).
pub fn max_range(t: ThrowType) -> f32 {
    reach::left_click_bound(t)
}

/// `LineupSolver.cs:168-184` (`FreeCellNear`): the first open cell at or
/// above `point`, or `None` when the column is solid for the whole search.
pub fn free_cell_near(grid: &VoxelGrid, point: V3) -> Option<usize> {
    let (x, y, z) = grid.cell_of(point);
    for dz in 0..=3 {
        if !grid.in_bounds(x, y, z + dz) {
            break;
        }
        let index = grid.index(x, y, z + dz);
        if !grid.is_solid(index) {
            return Some(index);
        }
    }
    None
}

/// `LineupSolver.cs:129-141` (`AllKinds`): every kind a solve asks for -
/// stance x click x run direction.
pub fn all_kinds(types: &[ThrowType], strengths: Option<&[f32]>) -> Vec<(ThrowType, f32, f32)> {
    let mut out = Vec::new();
    for &t in types {
        let runs: &[f32] = if t == ThrowType::RunJumpThrow {
            &RUN_YAW_OFFSETS
        } else {
            &NO_RUN_OFFSET
        };
        for &run in runs {
            for &strength in strengths.unwrap_or(&ALL_STRENGTHS) {
                out.push((t, strength, run));
            }
        }
    }
    out
}

/// `LineupSolver.cs:907-916` (`RunYawShiftDeg`).
pub fn run_yaw_shift_deg(
    k: &ThrowConstants,
    strength: f32,
    pitch_deg: f32,
    run_offset_deg: f32,
) -> f32 {
    if run_offset_deg == 0.0 {
        return 0.0;
    }
    let horizontal = k.throw_speed * k.speed_scale(strength) * (pitch_deg * PI / 180.0).cos();
    let offset = run_offset_deg * PI / 180.0;
    -(k.run_speed * offset.sin()).atan2(horizontal + k.run_speed * offset.cos()) * 180.0 / PI
}

/// `LineupSolver.cs:918-956` (`Better`): a total order (ordinal fallback), so
/// a bucket's winner never depends on which thread got there first.
pub fn better(a: &Lineup, b: &Lineup, target: Option<V3>) -> bool {
    if a.bounces != b.bounces {
        return a.bounces < b.bounces;
    }
    if let Some(t) = target {
        let da = (a.rest_point - t).length_squared();
        let db = (b.rest_point - t).length_squared();
        if (da - db).abs() > 1.0 {
            return da < db;
        }
    }
    if a.rest_crossings != b.rest_crossings {
        return a.rest_crossings > b.rest_crossings;
    }
    if a.flight_time != b.flight_time {
        return a.flight_time < b.flight_time;
    }
    ordinal_less(a, b)
}

fn ordinal_less(a: &Lineup, b: &Lineup) -> bool {
    let ta = a.throw_type as i32;
    let tb = b.throw_type as i32;
    if ta != tb {
        return ta < tb;
    }
    for (fa, fb) in [
        (a.feet.x, b.feet.x),
        (a.feet.y, b.feet.y),
        (a.feet.z, b.feet.z),
        (a.yaw_deg, b.yaw_deg),
        (a.pitch_deg, b.pitch_deg),
        (a.strength, b.strength),
        (a.run_yaw_offset_deg, b.run_yaw_offset_deg),
    ] {
        if fa != fb {
            return fa < fb;
        }
    }
    false
}

/// `ordinal_less` as a total-order `Ordering`, for `.then_with(...)` tails
/// on the final result sorts (here and in `verify`).
pub(crate) fn ordinal_cmp(a: &Lineup, b: &Lineup) -> std::cmp::Ordering {
    if ordinal_less(a, b) {
        std::cmp::Ordering::Less
    } else if ordinal_less(b, a) {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

/// Sweep configuration; defaults match the reference's named-parameter
/// defaults (`LineupSolver.cs:192-251`).
pub struct SweepOptions<'a> {
    pub yaw_step_deg: f32,
    pub pitch_step_deg: f32,
    pub dedupe_bucket_size: f32,
    pub strengths: Option<&'a [f32]>,
    pub constants: Option<&'a ThrowConstants>,
    pub target: Option<V3>,
    pub extra_fronts: &'a [V3],
    pub max_refine_seeds: usize,
    pub keep_every_kind: bool,
    pub keep_every_kind_at: Option<&'a (dyn Fn(V3) -> bool + Sync)>,
    pub own_bucket_at: Option<&'a (dyn Fn(V3) -> bool + Sync)>,
    pub measured_weak_click_reach: bool,
    /// How many ranked candidates `solve_buckets` keeps per `(origin bucket, throw kind)` -
    /// `solve()` itself only ever uses the first (best) of each, so `1` (the default) reproduces
    /// its exact old single-slot behavior. `Target::Area` solves ask for more (`target.rs`), so a
    /// bucket whose single best candidate fails exact verification still has runners-up to fall
    /// back to, instead of that `(origin, kind)` combination silently landing nothing.
    pub keep_per_bucket: usize,
    /// `false` (the default) keeps every per-origin range/window check measured to the zone's
    /// own centroid, exactly as before. `Target::Area` sets this (`target.rs`) instead: a point
    /// well inside a spread-out area can sit outside a window built from centroid-direction ±
    /// `YAW_SPREAD_DEG`, or get pruned by a straight-line-to-centroid range check, even though a
    /// different corner of the same area is genuinely reachable - see the per-origin
    /// `widen_to_zone_extent` block below for what widens and how.
    pub widen_to_zone_extent: bool,
    /// `LineupSolver.cs`'s `onPruned` diagnostics callback: called whenever
    /// a `(feet, type, run)` combination is skipped before any simulation
    /// runs, with the reason. Never affects the solved result.
    #[allow(clippy::type_complexity)]
    pub on_pruned: Option<&'a (dyn Fn(V3, ThrowType, f32, f32, &str) + Sync)>,
    /// `LineupSolver.cs`'s `coverage` diagnostics map: the raw option count
    /// per evaluated origin, keyed by `(round(feet.X), round(feet.Y))`
    /// (`LineupSolver.cs:205,298,527`). Includes a `0` entry for an origin
    /// dropped by the free-space pre-filter before any angle was tried.
    pub coverage: Option<&'a Mutex<HashMap<(i32, i32), i32>>>,
    /// `LineupSolver.cs`'s `onOrigin` diagnostics callback: called once per
    /// origin after it has been swept, with the number of throws it landed
    /// in the zone.
    pub on_origin: Option<&'a (dyn Fn(V3, usize) + Sync)>,
    /// Checked once per origin; once set, that origin (and every origin
    /// after it, since the flag does not clear) contributes nothing.
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for SweepOptions<'_> {
    fn default() -> Self {
        SweepOptions {
            yaw_step_deg: 2.0,
            pitch_step_deg: 2.0,
            dedupe_bucket_size: 64.0,
            strengths: None,
            constants: None,
            target: None,
            extra_fronts: &[],
            max_refine_seeds: MAX_REFINE_SEEDS,
            keep_every_kind: false,
            keep_every_kind_at: None,
            own_bucket_at: None,
            measured_weak_click_reach: false,
            keep_per_bucket: 1,
            widen_to_zone_extent: false,
            on_pruned: None,
            coverage: None,
            on_origin: None,
            cancel: None,
        }
    }
}

/// `LineupSolver.cs:186-533` (`Solve`): the single best candidate per `(origin, throw kind)`
/// bucket (`opts.keep_per_bucket` is ignored here - always exactly one), same as ever.
pub fn solve(
    grid: &VoxelGrid,
    zone: &[(usize, i32)],
    types: &[ThrowType],
    origins: &[V3],
    opts: &SweepOptions,
) -> Vec<Lineup> {
    let mut result: Vec<Lineup> = solve_buckets(grid, zone, types, origins, opts)
        .into_iter()
        .filter_map(|bucket| bucket.into_iter().next())
        .collect();
    result.sort_by(|a, b| {
        a.bounces
            .cmp(&b.bounces)
            .then(b.rest_crossings.cmp(&a.rest_crossings))
            .then(a.flight_time.partial_cmp(&b.flight_time).unwrap())
            .then_with(|| ordinal_cmp(a, b))
    });
    result
}

/// Ranked insert of `lineup` into `list` (best-first, per `better`), capped at `cap` entries - an
/// exact tie with an already-present entry is dropped (first-seen keeps its spot, same as
/// `solve()`'s old single-slot fold), and a lineup worse than every entry already at `cap` is
/// dropped too.
fn insert_ranked(list: &mut Vec<Lineup>, lineup: Lineup, target: Option<V3>, cap: usize) {
    if cap == 0 {
        return;
    }
    // `list` is kept sorted best-first by `better`, so a full list only admits a candidate that
    // beats its last entry, the insert position is a binary search, and an exact tie can only sit
    // at that position - one comparison replaces the two full passes this used to make per hit.
    if list.len() >= cap && !better(&lineup, list.last().unwrap(), target) {
        return;
    }
    let pos = list.partition_point(|existing| better(existing, &lineup, target));
    if pos < list.len() && !better(&lineup, &list[pos], target) {
        return;
    }
    list.insert(pos, lineup);
    list.truncate(cap);
}

/// Same sweep as `solve()`, but keeps up to `opts.keep_per_bucket` ranked candidates per
/// `(origin, throw kind)` bucket instead of collapsing straight to one - `Target::Area`'s own
/// round-based verify loop (`target.rs`) needs the runners-up a bucket's best candidate can fail
/// exact verification while a later one in the same bucket would have passed.
pub fn solve_buckets(
    grid: &VoxelGrid,
    zone: &[(usize, i32)],
    types: &[ThrowType],
    origins: &[V3],
    opts: &SweepOptions,
) -> Vec<Vec<Lineup>> {
    if zone.is_empty() {
        return Vec::new();
    }
    let lookup = zone_lookup(zone);
    let mut zone_centroid = V3::ZERO;
    for &(cell, _) in zone {
        zone_centroid = zone_centroid + grid.cell_center(cell);
    }
    zone_centroid = zone_centroid / zone.len() as f32;
    let mut zone_radius = 0f32;
    for &(cell, _) in zone {
        zone_radius = zone_radius.max((grid.cell_center(cell) - zone_centroid).length());
    }
    // Only collected when `widen_to_zone_extent` actually needs them, so a `Target::Point` solve
    // (or a referee/escalate pass, neither of which set the flag) pays nothing extra here.
    let zone_cells: Vec<V3> = if opts.widen_to_zone_extent {
        zone.iter()
            .map(|&(cell, _)| grid.cell_center(cell))
            .collect()
    } else {
        Vec::new()
    };

    let default_k = ThrowConstants::default();
    let k = opts.constants.unwrap_or(&default_k);

    let reach_budget = max_range(ThrowType::RunJumpThrow) * FREE_SPACE_BUDGET_SCALE;
    let reach_field = free_space::build(grid, zone.iter().map(|&(cell, _)| cell), reach_budget);

    let mut reachable: Vec<usize> = Vec::with_capacity(origins.len());
    let mut path_distance: Vec<f32> = vec![0.0; origins.len()];
    // Pre-sweep drops (no free-space path to the zone at all): collected in
    // `origins`' own order and applied - deterministically, from one thread -
    // before any swept origin's result below. `LineupSolver.cs:290-300`
    // writes its `ConcurrentDictionary` entry for this straight from the
    // (parallel) loop body; ours defers every diagnostic write to the
    // sequential fold at the end instead, so two origins that ever round to
    // the same coverage key can never race each other.
    let mut prefilter_misses: Vec<(V3, String)> = Vec::new();
    for (i, &feet) in origins.iter().enumerate() {
        let release = feet + V3::new(0.0, 0.0, sim::CROUCH_EYE_HEIGHT);
        match reach_field
            .distance_from(release)
            .or_else(|| reach_field.distance_from(feet))
        {
            Some(pd) => {
                path_distance[i] = pd;
                reachable.push(i);
            }
            None => {
                prefilter_misses.push((
                    feet,
                    format!(
                        "no free-space path from the origin to the zone within {reach_budget:.0}u"
                    ),
                ));
            }
        }
    }

    let mut order_key: Vec<f32> = reachable.iter().map(|&i| path_distance[i]).collect();
    for &front in opts.extra_fronts {
        let Some(seed) = free_cell_near(grid, front + V3::new(0.0, 0.0, sim::CROUCH_EYE_HEIGHT))
        else {
            continue;
        };
        let field = free_space::build(grid, [seed], reach_budget);
        for (slot, &idx) in reachable.iter().enumerate() {
            let feet = origins[idx];
            let release = feet + V3::new(0.0, 0.0, sim::CROUCH_EYE_HEIGHT);
            if let Some(d) = field
                .distance_from(release)
                .or_else(|| field.distance_from(feet))
                && d < order_key[slot]
            {
                order_key[slot] = d;
            }
        }
    }
    let mut order: Vec<usize> = (0..reachable.len()).collect();
    introsort(&mut order, &|&a: &usize, &b: &usize| {
        float_cmp(order_key[a], order_key[b])
    });
    let reachable: Vec<usize> = order.into_iter().map(|s| reachable[s]).collect();

    // Each origin's throws are evaluated on one thread; everything that
    // would otherwise be a diagnostic side effect from that thread (a
    // `coverage`/`onPruned` write) is instead accumulated locally and
    // returned, so `par_iter().map().collect()`'s order-preserving merge is
    // the ONLY thing that decides the final order - not which thread
    // happened to finish, or land on a colliding key, first.
    type Hit = ((i32, i32, i32), Lineup);
    type PrunedEvent = (ThrowType, f32, f32, String);
    struct OriginOutcome {
        coverage_key: (i32, i32),
        coverage_count: i32,
        pruned: Vec<PrunedEvent>,
        hits: Vec<Hit>,
    }
    let per_origin: Vec<Option<OriginOutcome>> = reachable
        .par_iter()
        .map(|&oi| {
            let feet = origins[oi];
            if opts.cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed)) {
                // No result at all for a cancelled origin, same as the
                // reference's own early-out: no coverage entry, no hits.
                return None;
            }
            let to_zone = zone_centroid - feet;
            let distance = (to_zone.x * to_zone.x + to_zone.y * to_zone.y).sqrt();
            let yaw_center = to_zone.y.atan2(to_zone.x) * 180.0 / PI;
            // `widen_to_zone_extent`: the range/rise/window checks below use the zone's own
            // extent from this origin (`range_test_distance`/`zone_z_for_rise`/`yaw_rel_lo`/
            // `yaw_rel_hi`) instead of its centroid alone - `(distance, to_zone.z, 0.0, 0.0)`
            // when unset reproduces the original centroid-only values exactly (0.0 added to a
            // yaw bound is a no-op), so `Target::Point` (which never sets the flag) is untouched.
            let (range_test_distance, zone_z_for_rise, yaw_rel_lo, yaw_rel_hi): (f32, f32, f32, f32) =
                if opts.widen_to_zone_extent {
                    let mut near = f32::MAX;
                    let mut low_z = f32::MAX;
                    let mut rel_lo = f32::MAX;
                    let mut rel_hi = f32::MIN;
                    for &cell in &zone_cells {
                        let dx = cell.x - feet.x;
                        let dy = cell.y - feet.y;
                        near = near.min((dx * dx + dy * dy).sqrt());
                        low_z = low_z.min(cell.z);
                        let cell_yaw = dy.atan2(dx) * 180.0 / PI;
                        let rel = normalize_yaw(cell_yaw - yaw_center);
                        rel_lo = rel_lo.min(rel);
                        rel_hi = rel_hi.max(rel);
                    }
                    if rel_hi - rel_lo > 300.0 {
                        rel_lo = -150.0;
                        rel_hi = 150.0;
                    }
                    (near, low_z - feet.z, rel_lo, rel_hi)
                } else {
                    (distance, to_zone.z, 0.0, 0.0)
                };
            let path_distance_i = path_distance[oi];
            let mut hits: Vec<Hit> = Vec::new();
            let mut pruned: Vec<PrunedEvent> = Vec::new();

            let mut evaluate = |eye: V3,
                                yaw: f32,
                                pitch: f32,
                                ty: ThrowType,
                                strength: f32,
                                run_offset: f32|
             -> f32 {
                let spec = ThrowSpec {
                    eye,
                    yaw_deg: yaw,
                    pitch_deg: pitch,
                    throw_type: ty,
                    strength,
                    run_yaw_offset_deg: run_offset,
                };
                let result = simulate_voxel(grid, &spec, k);
                if !settled(&result) {
                    return f32::MAX;
                }
                let (cx, cy, cz) = grid.cell_of(result.rest);
                if !grid.in_bounds(cx, cy, cz) {
                    return (result.rest - zone_centroid).length_squared();
                }
                let index = grid.index(cx, cy, cz);
                let Some(&crossings) = lookup.get(&index) else {
                    return (result.rest - zone_centroid).length_squared();
                };
                let mut lineup = Lineup::new(
                    feet,
                    normalize_yaw(yaw),
                    pitch,
                    ty,
                    result.rest,
                    result.bounces,
                    result.flight_time,
                    crossings,
                );
                lineup.strength = strength;
                lineup.run_yaw_offset_deg = run_offset;
                let kind =
                    if opts.keep_every_kind || opts.keep_every_kind_at.is_some_and(|f| f(feet)) {
                        (ty as i32) * 1000
                            + (strength * 10.0).round_ties_even() as i32 * 10
                            + (run_offset / 45.0).round_ties_even() as i32
                            + 2
                    } else {
                        0
                    };
                let key = if opts.own_bucket_at.is_some_and(|f| f(feet)) {
                    (
                        (feet.x * 4.0).round_ties_even() as i32,
                        (feet.y * 4.0).round_ties_even() as i32,
                        kind,
                    )
                } else {
                    (
                        (feet.x / opts.dedupe_bucket_size).floor() as i32,
                        (feet.y / opts.dedupe_bucket_size).floor() as i32,
                        kind,
                    )
                };
                hits.push((key, lineup));
                0.0
            };

            for &ty in types {
                let eye = feet + V3::new(0.0, 0.0, eye_height(ty));
                let zone_rise = zone_z_for_rise - eye_height(ty);
                let runs: &[f32] = if ty == ThrowType::RunJumpThrow {
                    &RUN_YAW_OFFSETS
                } else {
                    &NO_RUN_OFFSET
                };
                for &run_offset in runs {
                    for &strength in opts.strengths.unwrap_or(&ALL_STRENGTHS) {
                        let speed_factor = k.speed_scale(strength);
                        let max_range_val = if opts.measured_weak_click_reach {
                            reach::bound(k, ty, strength) + (-zone_rise).max(0.0)
                        } else {
                            max_range(ty) * speed_factor * speed_factor
                        };
                        if range_test_distance > max_range_val {
                            if opts.on_pruned.is_some() {
                                pruned.push((
                                    ty,
                                    strength,
                                    run_offset,
                                    format!(
                                        "straight-line distance {distance:.0}u over max range {max_range_val:.0}u"
                                    ),
                                ));
                            }
                            continue;
                        }
                        if path_distance_i > max_range_val * FREE_SPACE_BUDGET_SCALE {
                            if opts.on_pruned.is_some() {
                                pruned.push((
                                    ty,
                                    strength,
                                    run_offset,
                                    format!(
                                        "free-space path {path_distance_i:.0}u over budget {:.0}u (straight line {distance:.0}u)",
                                        max_range_val * FREE_SPACE_BUDGET_SCALE
                                    ),
                                ));
                            }
                            continue;
                        }
                        let pitch_floor =
                            if range_test_distance <= STEEP_LOB_MAX_RANGE * (max_range_val / max_range(ty)) {
                                STEEP_PITCH_FLOOR_DEG
                            } else {
                                STANDARD_PITCH_FLOOR_DEG
                            };
                        if zone_rise > 0.0 {
                            let launch_speed = k.throw_speed * speed_factor
                                + if matches!(
                                    ty,
                                    ThrowType::JumpThrow
                                        | ThrowType::CrouchJumpThrow
                                        | ThrowType::RunJumpThrow
                                ) {
                                    k.jump_velocity
                                } else {
                                    0.0
                                }
                                + if ty == ThrowType::RunJumpThrow {
                                    k.run_speed
                                } else {
                                    0.0
                                };
                            let gravity = BASE_GRAVITY * k.gravity_scale;
                            let apex = launch_speed * launch_speed / (2.0 * gravity);
                            let reach_at_distance = apex
                                - gravity * range_test_distance * range_test_distance
                                    / (2.0 * launch_speed * launch_speed);
                            if zone_rise > reach_at_distance + VERTICAL_REACH_MARGIN {
                                if opts.on_pruned.is_some() {
                                    pruned.push((
                                        ty,
                                        strength,
                                        run_offset,
                                        format!(
                                            "zone rises {zone_rise:.0}u, vertical reach at {distance:.0}u is {reach_at_distance:.0}u"
                                        ),
                                    ));
                                }
                                continue;
                            }
                        }
                        let reach_val = zone_radius
                            + distance * opts.yaw_step_deg.max(opts.pitch_step_deg) * PI / 180.0;
                        let reach_sq = reach_val * reach_val;
                        let mut near_misses: Vec<(f32, f32, f32)> = Vec::new();
                        let shift_shallow = run_yaw_shift_deg(k, strength, 0.0, run_offset);
                        let shift_steep = run_yaw_shift_deg(k, strength, pitch_floor, run_offset);
                        let yaw_lo = yaw_center + shift_shallow.min(shift_steep) - YAW_SPREAD_DEG + yaw_rel_lo;
                        let yaw_hi = yaw_center + shift_shallow.max(shift_steep) + YAW_SPREAD_DEG + yaw_rel_hi;

                        let mut yaw = yaw_lo;
                        while yaw <= yaw_hi {
                            let mut pitch = STANDARD_PITCH_FLOOR_DEG;
                            while pitch <= 0.0 {
                                let miss_sq = evaluate(eye, yaw, pitch, ty, strength, run_offset);
                                if miss_sq > 0.0 && miss_sq <= reach_sq {
                                    near_misses.push((yaw, pitch, miss_sq));
                                }
                                pitch += opts.pitch_step_deg;
                            }
                            let mut pitch = STANDARD_PITCH_FLOOR_DEG - opts.pitch_step_deg;
                            while pitch >= pitch_floor {
                                let miss_sq = evaluate(eye, yaw, pitch, ty, strength, run_offset);
                                if miss_sq > 0.0 && miss_sq <= reach_sq {
                                    near_misses.push((yaw, pitch, miss_sq));
                                }
                                pitch -= opts.pitch_step_deg;
                            }
                            yaw += opts.yaw_step_deg;
                        }
                        introsort(
                            &mut near_misses,
                            &|a: &(f32, f32, f32), b: &(f32, f32, f32)| float_cmp(a.2, b.2),
                        );
                        for &(seed_yaw, seed_pitch, _) in
                            near_misses.iter().take(opts.max_refine_seeds)
                        {
                            for i in -REFINE_HALF_SPAN..=REFINE_HALF_SPAN {
                                for j in -REFINE_HALF_SPAN..=REFINE_HALF_SPAN {
                                    if i == 0 && j == 0 {
                                        continue;
                                    }
                                    evaluate(
                                        eye,
                                        seed_yaw
                                            + i as f32 * opts.yaw_step_deg
                                                / (2 * REFINE_HALF_SPAN) as f32,
                                        seed_pitch
                                            + j as f32 * opts.pitch_step_deg
                                                / (2 * REFINE_HALF_SPAN) as f32,
                                        ty,
                                        strength,
                                        run_offset,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // `LineupSolver.cs:527-528`: per-origin option count, including
            // zeroes; returned rather than written here (see the type's own
            // doc comment above), so it lands in `coverage` from the
            // sequential fold below instead of racing another origin's
            // write on the same key.
            //
            // `on_origin` is diagnostic only (nothing reads it back to
            // influence the solved result), so unlike `coverage`/`on_pruned`
            // it is called right here, from the parallel body, rather than
            // deferred to the sequential fold: that is what makes points
            // stream out while the sweep is still running instead of arriving
            // in one burst after `collect()`. Worker completion order is
            // acceptable for a diagnostic stream; the reference does the same
            // (`LineupSolver.cs:529`, inside `Parallel.ForEach`).
            if let Some(f) = opts.on_origin {
                f(feet, hits.len());
            }
            Some(OriginOutcome {
                coverage_key: (feet.x.round_ties_even() as i32, feet.y.round_ties_even() as i32),
                coverage_count: hits.len() as i32,
                pruned,
                hits,
            })
        })
        .collect();

    // Sequential, deterministic application of every diagnostic side effect -
    // pre-sweep misses first (`origins`' own order), then each swept
    // origin's own coverage/pruned/on_origin calls (`reachable`'s order,
    // preserved by `par_iter().map().collect()`) - immediately followed by
    // the fold over `hits` that decides the solved result. Mirrors a single
    // worker's `ConcurrentDictionary.AddOrUpdate` exactly, since `better` is
    // a total order and origins are folded in this one fixed sequence
    // regardless of which thread computed which origin's hits. Kept in
    // insertion order, like the reference's own `Dictionary` (updating an
    // existing key's value does not move it): the two colliding candidates
    // this sees are picked by `better`'s total order either way, but when
    // the *final* sort below still ties (equal bounces/crossings/flight_time
    // and, on quantized rest points, equal ordinal fields too), a stable
    // sort over this same pre-sort order reproduces the reference's own
    // `Dictionary.Values` enumeration instead of Rust `HashMap`'s
    // unspecified, per-process-randomized one.
    if let Some(coverage) = opts.coverage {
        let mut map = coverage.lock().unwrap();
        for (feet, _) in &prefilter_misses {
            map.insert(
                (
                    feet.x.round_ties_even() as i32,
                    feet.y.round_ties_even() as i32,
                ),
                0,
            );
        }
    }
    if let Some(f) = opts.on_pruned {
        for (feet, reason) in &prefilter_misses {
            f(*feet, ThrowType::Stand, f32::NAN, 0.0, reason);
        }
    }

    let cap = opts.keep_per_bucket.max(1);
    let mut best_order: Vec<(i32, i32, i32)> = Vec::new();
    let mut best: HashMap<(i32, i32, i32), Vec<Lineup>> = HashMap::new();
    for (&oi, outcome) in reachable.iter().zip(per_origin.iter()) {
        let Some(outcome) = outcome else { continue };
        let feet = origins[oi];
        if let Some(coverage) = opts.coverage {
            coverage
                .lock()
                .unwrap()
                .insert(outcome.coverage_key, outcome.coverage_count);
        }
        if let Some(f) = opts.on_pruned {
            for (ty, strength, run_offset, reason) in &outcome.pruned {
                f(feet, *ty, *strength, *run_offset, reason);
            }
        }
        for &(key, lineup) in &outcome.hits {
            let list = best.entry(key).or_insert_with(|| {
                best_order.push(key);
                Vec::new()
            });
            insert_ranked(list, lineup, opts.target, cap);
        }
    }

    best_order
        .into_iter()
        .map(|key| best.remove(&key).unwrap_or_default())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    fn flat_plane_grid(half: f32) -> VoxelGrid {
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
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-half, -half, -8.0),
            max: V3::new(half, half, 512.0),
        };
        VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap()
    }

    #[test]
    fn better_orders_by_bounces_then_target_distance_then_crossings_then_time_then_ordinal() {
        let base = Lineup::new(V3::ZERO, 0.0, -10.0, ThrowType::Stand, V3::ZERO, 1, 2.0, 1);
        let mut fewer_bounces = base;
        fewer_bounces.bounces = 0;
        assert!(better(&fewer_bounces, &base, None));
        assert!(!better(&base, &fewer_bounces, None));

        let mut close = base;
        close.rest_point = V3::new(1.0, 0.0, 0.0);
        let mut far = base;
        far.rest_point = V3::new(100.0, 0.0, 0.0);
        let target = V3::ZERO;
        assert!(better(&close, &far, Some(target)));

        let mut more_crossings = base;
        more_crossings.rest_crossings = 5;
        assert!(better(&more_crossings, &base, None));

        let mut faster = base;
        faster.flight_time = 1.0;
        assert!(better(&faster, &base, None));

        // Full tie except feet.x: ordinal fallback is deterministic.
        let mut a = base;
        a.feet.x = 1.0;
        let mut b = base;
        b.feet.x = 2.0;
        assert!(better(&a, &b, None));
        assert!(!better(&b, &a, None));
    }

    #[test]
    fn run_yaw_shift_is_zero_for_w_and_nonzero_for_strafes() {
        let k = ThrowConstants::default();
        assert_eq!(run_yaw_shift_deg(&k, 1.0, 0.0, 0.0), 0.0);
        assert_ne!(run_yaw_shift_deg(&k, 1.0, 0.0, 45.0), 0.0);
        assert_ne!(run_yaw_shift_deg(&k, 1.0, 0.0, -45.0), 0.0);
    }

    #[test]
    fn sweep_on_open_plane_finds_stand_and_jump_lineups_within_tolerance() {
        let grid = flat_plane_grid(2000.0);
        let target = V3::new(1000.0, 0.0, 2.0001886);
        let mut zone_crossings = Vec::new();
        let (cx, cy, cz) = grid.cell_of(target);
        for dz in 0..=1 {
            let idx = grid.index(cx, cy, cz + dz);
            zone_crossings.push((idx, 1));
        }
        let origins = [V3::new(0.0, 0.0, 0.0)];
        let types = [ThrowType::Stand, ThrowType::JumpThrow];
        let opts = SweepOptions::default();
        let result = solve(&grid, &zone_crossings, &types, &origins, &opts);
        assert!(!result.is_empty(), "expected at least one candidate lineup");
        for l in &result {
            let (dx, dy) = (l.rest_point.x - target.x, l.rest_point.y - target.y);
            let miss = (dx * dx + dy * dy).sqrt();
            assert!(miss < 100.0, "lineup {l:?} missed target by {miss}u");
        }
        assert!(result.iter().any(|l| l.throw_type == ThrowType::Stand));
    }

    #[test]
    fn insert_ranked_keeps_best_first_drops_exact_ties_and_truncates() {
        let base = Lineup::new(V3::ZERO, 0.0, -10.0, ThrowType::Stand, V3::ZERO, 1, 2.0, 1);
        let with_time = |t: f32| {
            let mut l = base;
            l.flight_time = t;
            l
        };
        let times = |list: &[Lineup]| list.iter().map(|l| l.flight_time).collect::<Vec<_>>();
        let mut list = Vec::new();
        for t in [3.0, 1.0, 2.0, 4.0] {
            insert_ranked(&mut list, with_time(t), None, 3);
        }
        assert_eq!(
            times(&list),
            vec![1.0, 2.0, 3.0],
            "best-first, the worst truncated away"
        );
        insert_ranked(&mut list, with_time(2.0), None, 3);
        assert_eq!(
            times(&list),
            vec![1.0, 2.0, 3.0],
            "an exact tie is not inserted twice"
        );
        insert_ranked(&mut list, with_time(5.0), None, 3);
        assert_eq!(
            times(&list),
            vec![1.0, 2.0, 3.0],
            "a full list rejects a worse candidate"
        );
        insert_ranked(&mut list, with_time(0.5), None, 3);
        assert_eq!(
            times(&list),
            vec![0.5, 1.0, 2.0],
            "a new best evicts the worst"
        );
    }

    /// A wide landing area seen from close by spans far more than the ±30° yaw window around its
    /// centroid; with `widen_to_zone_extent` the sweep must reach its far flanks too.
    #[test]
    fn widen_to_zone_extent_reaches_the_flanks_of_a_wide_zone() {
        let grid = flat_plane_grid(2000.0);
        let mut zone = Vec::new();
        // A strip about 950u out (full-strength stand throws land around there), 2000u long:
        // seen from the origin it spans about ±46°.
        for ix in 0..=4 {
            for iy in 0..=125 {
                let p = V3::new(900.0 + 16.0 * ix as f32, -1000.0 + 16.0 * iy as f32, 2.0);
                // The floor cell and the open cell above it, as in the open-plane test above.
                let (cx, cy, cz) = grid.cell_of(p);
                zone.push((grid.index(cx, cy, cz), 1));
                zone.push((grid.index(cx, cy, cz + 1), 1));
            }
        }
        let origins = [V3::new(0.0, 0.0, 0.0)];
        let types = [ThrowType::Stand];
        let strengths = [1.0];
        let max_off_axis = |widen: bool| {
            let opts = SweepOptions {
                strengths: Some(&strengths),
                keep_per_bucket: 100_000,
                widen_to_zone_extent: widen,
                ..SweepOptions::default()
            };
            solve_buckets(&grid, &zone, &types, &origins, &opts)
                .iter()
                .flatten()
                .map(|l| l.rest_point.y.atan2(l.rest_point.x).to_degrees().abs())
                .fold(0.0f32, f32::max)
        };
        let (on, off) = (max_off_axis(true), max_off_axis(false));
        assert!(on > 40.0, "widened window must reach the zone's flanks");
        assert!(off <= 40.0, "the centroid window stays within about ±30°");
    }
}
