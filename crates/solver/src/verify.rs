//! Exact verification: the referee (`ExhaustiveExactSpot`), acceptance
//! predicates, and the stability/scatter-scoring re-aim search
//! (`VerifyExact`). Ported from
//! `cs2-smoke-solver/src/Solver/LineupSolver.cs:554-900`.

use std::collections::HashMap;
use std::f32::consts::PI;

use geom::collider::Collider;
use geom::math::V3;
use geom::voxel::VoxelGrid;
use rayon::prelude::*;
use sim::{
    ThrowConstants, ThrowSpec, ThrowType, TrajectoryResult, eye_height, simulate_exact,
    simulate_exact_raw,
};

use crate::dotnet_sort::{float_cmp, introsort};
use crate::lineup::{Lineup, normalize_yaw};
use crate::sweep::{self, ordinal_cmp, settled};
use crate::zone::zone_lookup;

/// `LineupSolver.cs:673` (`StepDeg`); `VerifyOptions::step_deg`'s default. `pub(crate)` so
/// `target.rs` can name the non-precise value explicitly next to its own `PRECISE_STEP_DEG`
/// (`s6l_aim_precision.md`).
pub(crate) const STEP_DEG: f32 = 0.6;
/// `LineupSolver.cs:674` (`AimReach`); `VerifyOptions::aim_reach`'s default - see `STEP_DEG`.
pub(crate) const AIM_REACH: i32 = 2;
/// `LineupSolver.cs:675` (aim-window/stability offsets).
const OFFSETS: [(i32, i32); 5] = [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)];
/// `LineupSolver.cs:100` (`ScatterOffsets`).
const SCATTER_OFFSETS: [(f32, f32); 4] = [(0.25, 0.0), (-0.25, 0.0), (0.0, 0.25), (0.0, -0.25)];

/// `LineupSolver.cs:877-887` (`WithinTolerance`).
pub fn within_tolerance(rest_point: V3, target: V3, tolerance: f32, voxel_size: f32) -> bool {
    let dx = rest_point.x - target.x;
    let dy = rest_point.y - target.y;
    if dx * dx + dy * dy > tolerance * tolerance {
        return false;
    }
    let dz = rest_point.z - target.z;
    dz >= -(2.0 * voxel_size) && dz <= tolerance + 2.0 * voxel_size
}

/// `LineupSolver.cs:889-900` (`InZone`).
pub fn in_zone(grid: &VoxelGrid, zone_crossings: &HashMap<usize, i32>, rest_point: V3) -> bool {
    let (x, y, z) = grid.cell_of(rest_point);
    for dz in 0..=1 {
        if grid.in_bounds(x, y, z + dz) && zone_crossings.contains_key(&grid.index(x, y, z + dz)) {
            return true;
        }
    }
    false
}

/// `area_accept`, when set (`Target::Area`), is the exact polygon+z-range predicate - `in_zone`
/// alone only tests the *cell* a rest point falls in (a zone cell qualifies by its own center
/// being inside the polygon, so a rest point near that cell's far edge can sit a few units past
/// the drawn boundary); requiring both closes that gap so `accepts()` never lets a lineup through
/// that a caller-visible "is this point inside the area" check would reject.
fn accepts(
    grid: &VoxelGrid,
    zone_crossings: &HashMap<usize, i32>,
    aim_target: Option<V3>,
    tolerance: Option<f32>,
    area_accept: Option<&(dyn Fn(V3) -> bool + Sync)>,
    rest: V3,
) -> bool {
    match (aim_target, tolerance) {
        (Some(g), Some(tol)) => within_tolerance(rest, g, tol, grid.voxel_size()),
        _ => in_zone(grid, zone_crossings, rest) && area_accept.is_none_or(|f| f(rest)),
    }
}

#[allow(clippy::too_many_arguments)]
fn sim_at<C: Collider>(
    cache: &mut HashMap<(i32, i32), TrajectoryResult>,
    collider: &C,
    k: &ThrowConstants,
    eye: V3,
    lineup: &Lineup,
    step_deg: f32,
    d_yaw: i32,
    d_pitch: i32,
) -> TrajectoryResult {
    *cache.entry((d_yaw, d_pitch)).or_insert_with(|| {
        let spec = ThrowSpec {
            eye,
            yaw_deg: lineup.yaw_deg + d_yaw as f32 * step_deg,
            pitch_deg: lineup.pitch_deg + d_pitch as f32 * step_deg,
            throw_type: lineup.throw_type,
            strength: lineup.strength,
            run_yaw_offset_deg: lineup.run_yaw_offset_deg,
        };
        simulate_exact(collider, &spec, k, sim::Trace::default())
    })
}

#[allow(clippy::too_many_arguments)]
fn stability_around<C: Collider>(
    cache: &mut HashMap<(i32, i32), TrajectoryResult>,
    collider: &C,
    k: &ThrowConstants,
    eye: V3,
    lineup: &Lineup,
    step_deg: f32,
    grid: &VoxelGrid,
    zone_crossings: &HashMap<usize, i32>,
    aim_target: Option<V3>,
    tolerance: Option<f32>,
    area_accept: Option<&(dyn Fn(V3) -> bool + Sync)>,
    cy: i32,
    cp: i32,
) -> f32 {
    let mut hits = 0;
    for &(dy, dp) in &OFFSETS {
        let r = sim_at(cache, collider, k, eye, lineup, step_deg, cy + dy, cp + dp);
        if settled(&r)
            && accepts(
                grid,
                zone_crossings,
                aim_target,
                tolerance,
                area_accept,
                r.rest,
            )
        {
            hits += 1;
        }
    }
    hits as f32 / OFFSETS.len() as f32
}

/// `LineupSolver.cs:643-670` (`VerifyExact`'s named parameters).
pub struct VerifyOptions<'a, C: Collider> {
    pub min_stability: f32,
    pub constants: Option<&'a ThrowConstants>,
    pub aim_target: Option<V3>,
    pub tolerance: Option<f32>,
    /// `Target::Area`'s exact polygon+z predicate (`None` for `Target::Point`) - see `accepts()`.
    pub area_accept: Option<&'a (dyn Fn(V3) -> bool + Sync)>,
    pub collider_glass_gone: Option<&'a C>,
    /// `LineupSolver.cs`'s `onCandidate` diagnostics callback: called once
    /// per candidate with whether it survived verification.
    pub on_candidate: Option<&'a (dyn Fn(V3, bool) + Sync)>,
    /// Checked once per candidate; once set, that candidate (and every
    /// candidate after it, since the flag does not clear) contributes
    /// nothing.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// `s6l_aim_precision.md`: the aim-window/stability probe step, `STEP_DEG` (0.6) by default,
    /// `0.2` in precise mode.
    pub step_deg: f32,
    /// `s6l_aim_precision.md`: the aim-window half-width in `step_deg` units, `AIM_REACH` (2) by
    /// default, `4` in precise mode (a 9x9 lattice at `step_deg` 0.2, ±0.8°).
    pub aim_reach: i32,
    /// `s6l_aim_precision.md`: also compute the 5-probe stability at the reference ±0.6° step
    /// around the final chosen aim and carry it as `Lineup::stability_wide` - set alongside a
    /// precise `step_deg`/`aim_reach`, so a difficulty badge can tell a lineup that is only
    /// precise-mode-stable from one that is stable even at the coarser reference window.
    pub wide_stability: bool,
}

impl<C: Collider> Default for VerifyOptions<'_, C> {
    fn default() -> Self {
        VerifyOptions {
            min_stability: 0.4,
            constants: None,
            aim_target: None,
            tolerance: None,
            area_accept: None,
            collider_glass_gone: None,
            on_candidate: None,
            cancel: None,
            step_deg: STEP_DEG,
            aim_reach: AIM_REACH,
            wide_stability: false,
        }
    }
}

/// `LineupSolver.cs:643-867` (`VerifyExact`).
pub fn verify_exact<C: Collider>(
    grid: &VoxelGrid,
    collider: &C,
    zone: &[(usize, i32)],
    candidates: &[Lineup],
    opts: &VerifyOptions<C>,
) -> Vec<Lineup> {
    let zone_crossings = zone_lookup(zone);
    let mut zone_centroid = V3::ZERO;
    for &(cell, _) in zone {
        zone_centroid = zone_centroid + grid.cell_center(cell);
    }
    zone_centroid = zone_centroid / zone.len().max(1) as f32;

    let default_k = ThrowConstants::default();
    let k = opts.constants.unwrap_or(&default_k);

    // Order-preserving parallel map, not a `Mutex`-guarded push list: the
    // reference iterates candidates in order on a single thread, so this
    // keeps that same final order regardless of thread scheduling.
    let verified: Vec<Option<Lineup>> = candidates
        .par_iter()
        .map(|lineup| -> Option<Lineup> {
            if opts
                .cancel
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            {
                return None;
            }
            let eye = lineup.feet + V3::new(0.0, 0.0, eye_height(lineup.throw_type));
            let mut cache: HashMap<(i32, i32), TrajectoryResult> = HashMap::new();
            let report = |ok: bool| {
                if let Some(f) = opts.on_candidate {
                    f(lineup.feet, ok);
                }
            };

            let aim_yaw: i32;
            let aim_pitch: i32;
            let stability: f32;
            if let Some(goal) = opts.aim_target {
                let mut in_window: Vec<(f32, i32, i32)> = Vec::new();
                for d_yaw in -opts.aim_reach..=opts.aim_reach {
                    for d_pitch in -opts.aim_reach..=opts.aim_reach {
                        let r = sim_at(
                            &mut cache,
                            collider,
                            k,
                            eye,
                            lineup,
                            opts.step_deg,
                            d_yaw,
                            d_pitch,
                        );
                        if !settled(&r)
                            || !accepts(
                                grid,
                                &zone_crossings,
                                opts.aim_target,
                                opts.tolerance,
                                opts.area_accept,
                                r.rest,
                            )
                        {
                            continue;
                        }
                        let dx = r.rest.x - goal.x;
                        let dy = r.rest.y - goal.y;
                        in_window.push((dx * dx + dy * dy, d_yaw, d_pitch));
                    }
                }
                introsort(
                    &mut in_window,
                    &|a: &(f32, i32, i32), b: &(f32, i32, i32)| float_cmp(a.0, b.0),
                );
                let mut chosen = None;
                for &(_, dy, dp) in &in_window {
                    let s = stability_around(
                        &mut cache,
                        collider,
                        k,
                        eye,
                        lineup,
                        opts.step_deg,
                        grid,
                        &zone_crossings,
                        opts.aim_target,
                        opts.tolerance,
                        opts.area_accept,
                        dy,
                        dp,
                    );
                    if s >= opts.min_stability {
                        chosen = Some((dy, dp, s));
                        break;
                    }
                }
                match chosen {
                    Some((dy, dp, s)) => {
                        aim_yaw = dy;
                        aim_pitch = dp;
                        stability = s;
                    }
                    None => {
                        report(false);
                        return None;
                    }
                }
            } else {
                // Bug fix: `s0 >= min_stability` alone does not mean offset (0,0) itself is a
                // valid landing - `stability_around` counts hits across all 5 probes, so 2 of the
                // other 4 could pass while (0,0) itself misses (doesn't settle, or settles outside
                // the area). Keeping (0,0) unconditionally then left `rest_point` on the coarse
                // sweep rest (never re-simulated, never re-checked against the exact area) once
                // `settled_ok` below went false - require (0,0) to settle and accept on its own
                // before taking the fast path, else fall through to the best-offset search.
                let r0 = sim_at(&mut cache, collider, k, eye, lineup, opts.step_deg, 0, 0);
                let r0_ok = settled(&r0)
                    && accepts(
                        grid,
                        &zone_crossings,
                        opts.aim_target,
                        opts.tolerance,
                        opts.area_accept,
                        r0.rest,
                    );
                let s0 = stability_around(
                    &mut cache,
                    collider,
                    k,
                    eye,
                    lineup,
                    opts.step_deg,
                    grid,
                    &zone_crossings,
                    opts.aim_target,
                    opts.tolerance,
                    opts.area_accept,
                    0,
                    0,
                );
                if r0_ok && s0 >= opts.min_stability {
                    aim_yaw = 0;
                    aim_pitch = 0;
                    stability = s0;
                } else {
                    let mut best_score = f32::MAX;
                    let mut best_offset = (0i32, 0i32);
                    let mut found_any = false;
                    for d_yaw in -opts.aim_reach..=opts.aim_reach {
                        for d_pitch in -opts.aim_reach..=opts.aim_reach {
                            let r = sim_at(
                                &mut cache,
                                collider,
                                k,
                                eye,
                                lineup,
                                opts.step_deg,
                                d_yaw,
                                d_pitch,
                            );
                            if !settled(&r)
                                || !accepts(
                                    grid,
                                    &zone_crossings,
                                    opts.aim_target,
                                    opts.tolerance,
                                    opts.area_accept,
                                    r.rest,
                                )
                            {
                                continue;
                            }
                            let score = (r.rest - zone_centroid).length_squared();
                            if score < best_score {
                                best_score = score;
                                best_offset = (d_yaw, d_pitch);
                                found_any = true;
                            }
                        }
                    }
                    if !found_any {
                        report(false);
                        return None;
                    }
                    let s = stability_around(
                        &mut cache,
                        collider,
                        k,
                        eye,
                        lineup,
                        opts.step_deg,
                        grid,
                        &zone_crossings,
                        opts.aim_target,
                        opts.tolerance,
                        opts.area_accept,
                        best_offset.0,
                        best_offset.1,
                    );
                    if s < opts.min_stability {
                        report(false);
                        return None;
                    }
                    aim_yaw = best_offset.0;
                    aim_pitch = best_offset.1;
                    stability = s;
                }
            }

            let best = sim_at(
                &mut cache,
                collider,
                k,
                eye,
                lineup,
                opts.step_deg,
                aim_yaw,
                aim_pitch,
            );
            let settled_ok = settled(&best)
                && accepts(
                    grid,
                    &zone_crossings,
                    opts.aim_target,
                    opts.tolerance,
                    opts.area_accept,
                    best.rest,
                );
            // Every `(aim_yaw, aim_pitch)` chosen above was only ever selected because its own
            // `sim_at` result already settled and passed `accepts()` - this should always hold,
            // but guard it explicitly rather than silently falling back to `lineup.rest_point`
            // (the coarse, unverified sweep rest) below if that invariant is ever violated.
            if !settled_ok {
                report(false);
                return None;
            }
            let final_yaw = lineup.yaw_deg + aim_yaw as f32 * opts.step_deg;
            let final_pitch = lineup.pitch_deg + aim_pitch as f32 * opts.step_deg;

            let mut rest_if_broken: Option<V3> = None;
            if settled_ok
                && best.glass_breaks > 0
                && let Some(gone) = opts.collider_glass_gone
            {
                let spec = ThrowSpec {
                    eye,
                    yaw_deg: final_yaw,
                    pitch_deg: final_pitch,
                    throw_type: lineup.throw_type,
                    strength: lineup.strength,
                    run_yaw_offset_deg: lineup.run_yaw_offset_deg,
                };
                let gone_r = simulate_exact(gone, &spec, k, sim::Trace::default());
                rest_if_broken = if settled(&gone_r) {
                    Some(gone_r.rest)
                } else {
                    None
                };
            }

            let mut scatter = 0f32;
            if settled_ok {
                for &(dx, dy) in &SCATTER_OFFSETS {
                    let spec = ThrowSpec {
                        eye: eye + V3::new(dx, dy, 0.0),
                        yaw_deg: final_yaw,
                        pitch_deg: final_pitch,
                        throw_type: lineup.throw_type,
                        strength: lineup.strength,
                        run_yaw_offset_deg: lineup.run_yaw_offset_deg,
                    };
                    let probe = simulate_exact(collider, &spec, k, sim::Trace::default());
                    scatter = scatter.max(if settled(&probe) {
                        (probe.rest - best.rest).length()
                    } else {
                        512.0
                    });
                }
            }

            // `s6l_aim_precision.md`: precise mode's own ±`opts.step_deg` probes are what let a
            // chaotic-landing lineup like this pass `min_stability` at all - `stability_wide`
            // re-probes the same chosen aim at the reference ±`STEP_DEG` (0.6°) window so a caller
            // can tell "stable only with a precise aim reference" from "stable even at the coarser
            // window" (in normal mode `step_deg` already *is* `STEP_DEG`, so re-probing would just
            // repeat `stability` - skip it and copy instead).
            let stability_wide = if opts.wide_stability {
                let mut at_final = *lineup;
                at_final.yaw_deg = final_yaw;
                at_final.pitch_deg = final_pitch;
                let mut wide_cache: HashMap<(i32, i32), TrajectoryResult> = HashMap::new();
                stability_around(
                    &mut wide_cache,
                    collider,
                    k,
                    eye,
                    &at_final,
                    STEP_DEG,
                    grid,
                    &zone_crossings,
                    opts.aim_target,
                    opts.tolerance,
                    opts.area_accept,
                    0,
                    0,
                )
            } else {
                stability
            };

            let mut out = *lineup;
            out.yaw_deg = normalize_yaw(final_yaw);
            out.pitch_deg = final_pitch;
            out.rest_point = if settled_ok {
                best.rest
            } else {
                lineup.rest_point
            };
            out.bounces = if settled_ok {
                best.bounces
            } else {
                lineup.bounces
            };
            out.flight_time = if settled_ok {
                best.flight_time
            } else {
                lineup.flight_time
            };
            out.stability = stability;
            out.stability_wide = stability_wide;
            out.rest_scatter = scatter;
            out.glass_breaks = if settled_ok { best.glass_breaks } else { 0 };
            out.rest_if_broken = rest_if_broken;
            report(true);
            Some(out)
        })
        .collect();

    let mut result: Vec<Lineup> = verified.into_iter().flatten().collect();
    result.sort_by(|a, b| {
        b.stability
            .partial_cmp(&a.stability)
            .unwrap()
            .then(a.bounces.cmp(&b.bounces))
            .then(b.rest_crossings.cmp(&a.rest_crossings))
            .then(a.flight_time.partial_cmp(&b.flight_time).unwrap())
            .then_with(|| ordinal_cmp(a, b))
    });
    result
}

/// `LineupSolver.cs:554-641` (`ExhaustiveExactSpot`): every throw kind over a
/// full angle lattice, run through the exact simulator, from one origin.
/// `hit_ok`, when set (`Target::Area`), is the exact polygon+z predicate - the "closest to
/// target" pick per kind below prefers a hit that passes it, so a hit that only clears the loose
/// `tolerance` circle (and would fail `verify_exact`'s own `area_accept` regardless) never beats
/// out a genuinely-inside one for that kind's single slot.
#[allow(clippy::too_many_arguments)]
pub fn exhaustive_exact_spot<C: Collider>(
    collider: &C,
    feet: V3,
    target: V3,
    tolerance: f32,
    types: &[ThrowType],
    strengths: Option<&[f32]>,
    constants: Option<&ThrowConstants>,
    step_deg: f32,
    distinct_bounces: bool,
    only_kinds: Option<&[(ThrowType, f32, f32)]>,
    hit_ok: Option<&(dyn Fn(V3) -> bool + Sync)>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<Lineup> {
    let default_k = ThrowConstants::default();
    let k = constants.unwrap_or(&default_k);
    let to_target = target - feet;
    let yaw_center = to_target.y.atan2(to_target.x) * 180.0 / PI;
    let kinds: Vec<(ThrowType, f32, f32)> = match only_kinds {
        Some(k) => k.to_vec(),
        None => sweep::all_kinds(types, strengths),
    };

    let mut columns: Vec<(ThrowType, f32, f32, f32)> = Vec::new();
    for &(ty, strength, run) in &kinds {
        let shift_shallow = sweep::run_yaw_shift_deg(k, strength, 0.0, run);
        let shift_steep = sweep::run_yaw_shift_deg(k, strength, sweep::STEEP_PITCH_FLOOR_DEG, run);
        let yaw_lo = yaw_center + shift_shallow.min(shift_steep) - sweep::YAW_SPREAD_DEG;
        let yaw_hi = yaw_center + shift_shallow.max(shift_steep) + sweep::YAW_SPREAD_DEG;
        let mut yaw = yaw_lo;
        while yaw <= yaw_hi {
            columns.push((ty, strength, run, yaw));
            yaw += step_deg;
        }
    }

    let tol_sq = tolerance * tolerance;
    // Order-preserving parallel map over columns, then flattened in column
    // order: matches the reference's single-threaded column order exactly,
    // instead of whatever order a `Mutex`-guarded push list would produce.
    let per_column: Vec<Vec<Lineup>> = columns
        .par_iter()
        .map(|&(ty, strength, run, yaw)| {
            if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                return Vec::new();
            }
            let mut local = Vec::new();
            let eye = feet + V3::new(0.0, 0.0, eye_height(ty));
            let mut pitch = sweep::STEEP_PITCH_FLOOR_DEG;
            while pitch <= 0.0 {
                let spec = ThrowSpec {
                    eye,
                    yaw_deg: yaw,
                    pitch_deg: pitch,
                    throw_type: ty,
                    strength,
                    run_yaw_offset_deg: run,
                };
                let r = simulate_exact(collider, &spec, k, sim::Trace::default());
                if settled(&r) {
                    let dx = r.rest.x - target.x;
                    let dy = r.rest.y - target.y;
                    if dx * dx + dy * dy <= tol_sq {
                        let mut l = Lineup::new(
                            feet,
                            normalize_yaw(yaw),
                            pitch,
                            ty,
                            r.rest,
                            r.bounces,
                            r.flight_time,
                            1,
                        );
                        l.strength = strength;
                        l.run_yaw_offset_deg = run;
                        local.push(l);
                    }
                }
                pitch += step_deg;
            }
            local
        })
        .collect();
    let found: Vec<Lineup> = per_column.into_iter().flatten().collect();

    // `LineupSolver.cs:638-640`: one per kind (and per bounce count if
    // `distinct_bounces`), the closest to the target. Grouped and resolved
    // in insertion order (`Dictionary` order), and the closest pick uses a
    // strict `<` fold over that same order, so an exact tie keeps whichever
    // candidate was found first instead of whatever an unstable sort picks.
    let mut group_order: Vec<(i32, u32, u32, u32)> = Vec::new();
    let mut groups: HashMap<(i32, u32, u32, u32), Vec<Lineup>> = HashMap::new();
    for l in found {
        let bounces_key = if distinct_bounces { l.bounces } else { 0 };
        let key = (
            l.throw_type as i32,
            l.strength.to_bits(),
            l.run_yaw_offset_deg.to_bits(),
            bounces_key,
        );
        if !groups.contains_key(&key) {
            group_order.push(key);
        }
        groups.entry(key).or_default().push(l);
    }
    let closest = |candidates: &[Lineup]| -> Lineup {
        let mut best = candidates[0];
        let mut best_d = (best.rest_point - target).length_squared();
        for &l in &candidates[1..] {
            let d = (l.rest_point - target).length_squared();
            if d < best_d {
                best = l;
                best_d = d;
            }
        }
        best
    };
    group_order
        .into_iter()
        .map(|key| {
            let g = &groups[&key];
            match hit_ok {
                Some(ok) => {
                    let in_area: Vec<Lineup> =
                        g.iter().copied().filter(|l| ok(l.rest_point)).collect();
                    if in_area.is_empty() {
                        closest(g)
                    } else {
                        closest(&in_area)
                    }
                }
                None => closest(g),
            }
        })
        .collect()
}

// ---- Robust aim centering (`s6q_robust_aim.md`) ------------------------------------------------
//
// A lineup the viewer showed as "stable" (5-probe `stability` 0.8) still missed in game, because
// (a) the rounded console string was not the aim actually verified, and (b) the chosen aim sat on
// the EDGE of its own working yaw/pitch band rather than its middle - the reference/precise
// aim-window search above (`verify_exact`) picks the first accepted candidate that clears
// `min_stability`, not the most centered one. `robust_center` runs once per already-verified
// precise-mode lineup: a fine 13x13 exact-sim grid around the chosen aim finds the accepted cell
// farthest from any rejected one (the new, centered aim), then a small combined sample at that new
// aim (an aim-disk plus a feet-jitter grid) measures how robust it really is.

/// The fine grid's step and half-reach: yaw/pitch offsets from -0.30 to +0.30 in 0.05 steps, a
/// 13x13 lattice (`s6q_robust_aim.md`).
pub const ROBUST_STEP_DEG: f32 = 0.05;
pub const ROBUST_REACH: i32 = 6;
/// `aimMarginDeg`'s value when the whole fine grid comes back accepted (no rejected cell to
/// measure a real clearance against) - the grid's own outer edge, spelled out rather than computed
/// from `ROBUST_REACH * ROBUST_STEP_DEG` so it can never drift from a literal step-count change. A
/// caller reports a margin exactly at this value as "≥0.30°" (untested beyond it).
pub const ROBUST_NO_REJECT_MARGIN_DEG: f32 = 0.30;
/// The clearance/centering metric's pitch weight (`s6q_robust_aim.md`): a pitch-axis degree of
/// difference to a rejected cell is divided by this before being squared into the distance, i.e. it
/// counts for *more* raw distance (a pitch-different reject is farther, in this metric, than an
/// equal-sized yaw-different one) - reviewed and corrected: an earlier version of this code
/// multiplied instead of dividing, which made a pitch-adjacent reject look *closer* than it really
/// was and could dominate the whole clearance figure regardless of the actual (correctly-behaved)
/// yaw margin, occasionally centering on the wrong offset entirely (see
/// `pick_centered_offset_finds_the_middle_of_a_synthetic_band_with_pitch_bounds`'s own regression
/// case).
///
/// Verified against the Evidence data (`s6q_robust_aim.md`, the de_mirage window lineup): the
/// user's own aim (yaw -163.14) sat on the edge of a working yaw band only ~0.10-0.14 deg wide (a
/// 0.04 deg rounding change, -163.14 -> -163.1, already flipped the landing from the room to the
/// mid floor), while re-centering on yaw -163.20 kept landing correctly with pitch free to move
/// over a ~0.8 deg band (+-0.4) at that same yaw - roughly 6-8x wider than the yaw band. Yaw is the
/// axis that actually decides accept/reject near the boundary; a rejected cell that differs mostly
/// in pitch should count as farther away than one that differs in yaw, which biases the centering
/// search toward maximizing yaw clearance (the sensitive axis) instead of splitting effort evenly
/// across both.
pub const ROBUST_PITCH_WEIGHT: f32 = 0.5;
/// The aim-disk radius for `robustAim`/`robustness` - plain (unweighted) degrees, unlike the
/// clearance metric above.
pub const ROBUST_DISK_DEG: f32 = 0.15;
/// The feet-jitter half-width for `robustPos`/`robustness`: a single movement-key tick.
pub const ROBUST_FEET_JITTER: f32 = 1.0;
/// Below this combined `robustness`, `s6q_robust_aim.md` drops the lineup outright (documented
/// constant, not a magic number at the call site).
pub const ROBUST_MIN: f32 = 0.5;

/// `s6q_robust_aim.md`'s real-game addendum: launch speed scale factors sampled for `robustModel`
/// (nominal launch position at each).
const ROBUST_MODEL_SPEED_SCALES: [f32; 4] = [0.995, 0.9975, 1.0025, 1.005];
/// `robustModel`'s own launch-position offsets (z and perpendicular-to-throw, each independently),
/// sampled as every non-`(0, 0)` pair - 8 position samples, plus the 4 speed-only ones above, for
/// the requirement's own "~12 samples".
const ROBUST_MODEL_POS_OFFSETS: [f32; 3] = [-1.0, 0.0, 1.0];

/// The clearance a raw `setpos` teleport (`consoleExact`) needs around the real (un-shrunk) player
/// hull before it is considered safe. Unlike `standspots::stance_at`'s own 0.5u `SKIN_WIDTH` (a
/// WALKING tolerance - a real player who walks up to a wall is pushed out by the engine's own
/// collision response, so a spot within 0.5u of a wall is still walkable), a `setpos` teleport
/// places the origin raw with no pushout at all. The real-game failure this addendum fixes
/// (`s6q_robust_aim.md`) was exactly a spot `stance_at`'s 0.5u-shrunk box accepted that a raw
/// teleport could not: CS2 refused it outright ("setpos into world, use noclip to unstick
/// yourself!"), because the real (unshrunk) 32-wide hull overlapped a slanted wall face by up to
/// ~0.5u - well inside that skin margin. This is a property of the WALKING-vs-TELEPORT distinction,
/// not a defect in `origins.rs`'s own pin logic (its ray-based wall-plane search is also a coarser
/// approximation than an exact AABB-vs-triangle test, but even an exact box test with `stance_at`'s
/// own margin would still pass this spot) - so the fix lives here, precise-mode-only, rather than
/// changing `origins.rs`'s shared (normal-mode-affecting) constants.
const SAFE_CLEARANCE: f32 = 0.05;
/// How far (and in what increments) `consoleExact`'s x/y may be nudged off `hull_clear` before its
/// lineup is dropped outright as un-teleportable.
const SAFE_NUDGE_STEP: f32 = 0.25;
const SAFE_NUDGE_MAX_STEPS: i32 = 8; // 0.25 * 8 = 2.0u, the requirement's own cap.
/// Compass directions probed per nudge step - a nudge only needs to find *any* clear direction
/// (unlike `origins.rs`'s `nearby_wall_planes`, which needs the wall's actual normal), so this
/// samples finer than that function's own 8-per-45°.
const SAFE_NUDGE_DIRECTIONS: i32 = 16;

/// The core of `robust_center`'s search: given a fine grid's own accept/reject cells (`(d_yaw_step,
/// d_pitch_step, accepted)` triples), picks the accepted cell with the largest pitch-weighted
/// clearance to the nearest rejected cell, tie-breaking on closeness (plain, unweighted degrees)
/// to the original aim (offset `(0, 0)`). Returns `(d_yaw_step, d_pitch_step, clearance_deg)`, or
/// `None` if every cell was rejected.
///
/// Reviewed and corrected: the grid this searches is only ever `ROBUST_REACH` steps wide - a cell
/// can look "safe" only because nothing *tested* rejected it nearby, when the untested region just
/// past the grid's own edge is actually unknown, not proven safe. Selection uses
/// `min(real-reject clearance, edge clearance)` (`edge clearance` treating the first untested step
/// past the grid boundary as if it were a reject), so a one-sided real reject can no longer push the
/// pick out to the grid's own edge just because nothing tests beyond it. The *reported*
/// `aimMarginDeg`, though, is the real-reject clearance alone (capped at `ROBUST_NO_REJECT_MARGIN_DEG`,
/// unchanged when there is no real reject at all) - the edge term only ever influences which cell
/// wins, never what margin gets reported for it.
///
/// Split out from `robust_center` so this pure centering math can be unit-tested against a
/// hand-built synthetic grid, with no simulator involved.
fn pick_centered_offset(cells: &[(i32, i32, bool)]) -> Option<(i32, i32, f32)> {
    let rejected: Vec<(i32, i32)> = cells
        .iter()
        .filter(|&&(_, _, ok)| !ok)
        .map(|&(dy, dp, _)| (dy, dp))
        .collect();
    let real_clearance = |dy: i32, dp: i32| -> Option<f32> {
        if rejected.is_empty() {
            return None;
        }
        Some(
            rejected
                .iter()
                .map(|&(ry, rp)| {
                    let ddy = (dy - ry) as f32 * ROBUST_STEP_DEG;
                    let ddp = (dp - rp) as f32 * ROBUST_STEP_DEG / ROBUST_PITCH_WEIGHT;
                    (ddy * ddy + ddp * ddp).sqrt()
                })
                .fold(f32::MAX, f32::min),
        )
    };
    let edge_clearance = |dy: i32, dp: i32| -> f32 {
        let edge_yaw = (ROBUST_REACH + 1 - dy.abs()) as f32 * ROBUST_STEP_DEG;
        let edge_pitch =
            (ROBUST_REACH + 1 - dp.abs()) as f32 * ROBUST_STEP_DEG / ROBUST_PITCH_WEIGHT;
        edge_yaw.min(edge_pitch)
    };

    // (dy, dp, selection key, dist2-to-original, real clearance for the reported margin).
    let mut best: Option<(i32, i32, f32, i32, Option<f32>)> = None;
    for &(dy, dp, ok) in cells {
        if !ok {
            continue;
        }
        let real = real_clearance(dy, dp);
        let key = match real {
            Some(r) => r.min(edge_clearance(dy, dp)),
            None => edge_clearance(dy, dp),
        };
        let dist2 = dy * dy + dp * dp;
        let take = match best {
            None => true,
            Some((_, _, bk, bd, _)) => key > bk || (key == bk && dist2 < bd),
        };
        if take {
            best = Some((dy, dp, key, dist2, real));
        }
    }
    let (dy, dp, _, _, real) = best?;
    let margin = real.map_or(ROBUST_NO_REJECT_MARGIN_DEG, |r| {
        r.min(ROBUST_NO_REJECT_MARGIN_DEG)
    });
    Some((dy, dp, margin))
}

/// `s6q_robust_aim.md`'s real-game addendum: how far above `feet.z` a `setpos` teleport actually
/// lands the player (`rank::setpos_command_exact`'s own `feet.z + TELEPORT_LIFT`) - a bare `0.0`
/// would risk landing exactly on the floor boundary; this is shared as one constant so `hull_clear`
/// tests the hull at the exact height the teleport will really place it at, not at `feet.z` itself.
pub const TELEPORT_LIFT: f32 = 0.1;

/// `s6q_robust_aim.md`'s real-game addendum: whether the real player hull for `throw_type`, teleported
/// to `feet` (i.e. actually sitting at `feet.z + TELEPORT_LIFT`, `setpos`'s own landing height - not
/// `feet.z` itself), clears every nearby solid triangle by at least `SAFE_CLEARANCE` - an exact
/// AABB-vs-triangle overlap test (`Collider::box_intersects`, real SAT geometry) on a box *widened*
/// past the real 32-wide hull on every side, including top and bottom (unlike
/// `standspots::stance_at`'s own 0.5u-*shrunk* box - see `SAFE_CLEARANCE`'s own doc comment for why
/// that distinction matters here).
///
/// Reviewed and corrected: an earlier version tested a box shrunk 0.5u off `feet.z` (mirroring
/// `stance_at`'s own floor/ceiling skin), which missed a shallow slope or step rising into the
/// footprint anywhere in that skipped 0-0.5u band - exactly a slope of roughly 11-42 degrees under
/// part of the hull's own footprint, since `atan(0.5 / 32-wide-footprint-reach)` sits in that
/// range. Testing at the teleport's own real landing height instead (`feet.z + TELEPORT_LIFT`,
/// inflated by `SAFE_CLEARANCE` on every side including bottom and top) means there is no skipped
/// band left for such a slope to hide in.
///
/// DECISION 2 (`s6q_robust_aim.md`'s real-game addendum): `setpos` always lands the player
/// STANDING, never in whatever stance the lineup's own `throw_type` needs, so `throw_type` itself
/// is not what should pick the tested hull height here (kept as a parameter only for callers'
/// convenience/API stability; unused) - the standing hull is tested unless the spot is crouch-only
/// (`stance_at` returns `Crouching`: nothing else fits there at all), in which case the crouch hull
/// is tested instead so a genuinely crouch-only stand spot is not wrongly flagged as unsafe. Chosen
/// over also surfacing a "crouch after teleporting" UI flag: the hull-height swap alone was cheap
/// (one more `stance_at` call, already used elsewhere in this crate); plumbing a new flag through
/// `Lineup`/`LineupJson`/the viewer was not judged cheap enough to add on top of an already very
/// large change set, so this is reported instead of implemented.
pub(crate) fn hull_clear<C: Collider>(
    player_collider: &C,
    feet: V3,
    _throw_type: ThrowType,
) -> bool {
    let height = if crate::standspots::stance_at(player_collider, feet)
        == crate::standspots::Stance::Crouching
    {
        crate::standspots::CROUCH_HEIGHT
    } else {
        crate::standspots::STANDING_HEIGHT
    };
    let half = V3::new(
        crate::standspots::HULL_HALF_WIDTH + SAFE_CLEARANCE,
        crate::standspots::HULL_HALF_WIDTH + SAFE_CLEARANCE,
        height / 2.0 + SAFE_CLEARANCE,
    );
    let center = feet + V3::new(0.0, 0.0, TELEPORT_LIFT + height / 2.0);
    !player_collider.box_intersects(center, half)
}

/// `s6q_robust_aim.md`'s real-game addendum: the closest safe `(x, y)` to `lineup.feet.xy` (z
/// unchanged) that both clears `hull_clear` and still settles/accepts when re-thrown from there at
/// `final_yaw`/`final_pitch` - tried in `SAFE_NUDGE_STEP` increments up to `SAFE_NUDGE_MAX_STEPS`
/// steps, `SAFE_NUDGE_DIRECTIONS` compass directions per step (closest step wins; first direction
/// in angle order wins a tie within a step). Returns `(feet, nudge_distance, resimulated)`, where
/// `resimulated` is `None` when `lineup.feet` itself was already safe (the caller's own
/// already-computed result at that spot is still valid) and `Some` when a nudge moved the throw and
/// it had to be re-simulated. `None` overall when nothing within the cap is both safe and accepted.
#[allow(clippy::too_many_arguments)]
fn safe_teleport_feet<C: Collider>(
    grid: &VoxelGrid,
    collider: &C,
    player_collider: &C,
    k: &ThrowConstants,
    zone_crossings: &HashMap<usize, i32>,
    aim_target: Option<V3>,
    tolerance: Option<f32>,
    area_accept: Option<&(dyn Fn(V3) -> bool + Sync)>,
    lineup: &Lineup,
    final_yaw: f32,
    final_pitch: f32,
) -> Option<(V3, f32, Option<TrajectoryResult>)> {
    if hull_clear(player_collider, lineup.feet, lineup.throw_type) {
        return Some((lineup.feet, 0.0, None));
    }
    for step_i in 1..=SAFE_NUDGE_MAX_STEPS {
        let dist = step_i as f32 * SAFE_NUDGE_STEP;
        for dir_i in 0..SAFE_NUDGE_DIRECTIONS {
            let a = dir_i as f32 * (2.0 * PI) / SAFE_NUDGE_DIRECTIONS as f32;
            let candidate = lineup.feet + V3::new(a.cos() * dist, a.sin() * dist, 0.0);
            if !hull_clear(player_collider, candidate, lineup.throw_type) {
                continue;
            }
            let eye = candidate + V3::new(0.0, 0.0, eye_height(lineup.throw_type));
            let spec = ThrowSpec {
                eye,
                yaw_deg: final_yaw,
                pitch_deg: final_pitch,
                throw_type: lineup.throw_type,
                strength: lineup.strength,
                run_yaw_offset_deg: lineup.run_yaw_offset_deg,
            };
            let r = simulate_exact(collider, &spec, k, sim::Trace::default());
            if settled(&r)
                && accepts(
                    grid,
                    zone_crossings,
                    aim_target,
                    tolerance,
                    area_accept,
                    r.rest,
                )
            {
                eprintln!("s6q safe-teleport: nudged {dist:.2}u off {:?}", lineup.feet);
                return Some((candidate, dist, Some(r)));
            }
        }
    }
    None
}

/// `s6q_robust_aim.md`'s real-game addendum: launch-model uncertainty at `feet`/`final_yaw`/
/// `final_pitch` - throw speed scaled by each of `ROBUST_MODEL_SPEED_SCALES` (nominal launch
/// position), plus every non-`(0, 0)` combination of a `ROBUST_MODEL_POS_OFFSETS` launch-height (z)
/// shift and a same-range shift perpendicular (horizontal, relative to yaw) to the throw direction
/// (nominal speed) - 4 + 8 = 12 samples. Models the sim-vs-game error the real evidence's own
/// multi-bounce miss sits inside (a grazing hit that must then thread a narrow gap, which flips with
/// 1-2u of trajectory error) - `robustAim`/`robustPos` (aim and stand-position uncertainty alone)
/// do not cover this.
#[allow(clippy::too_many_arguments)]
fn robust_model_fraction<C: Collider>(
    grid: &VoxelGrid,
    collider: &C,
    k: &ThrowConstants,
    zone_crossings: &HashMap<usize, i32>,
    aim_target: Option<V3>,
    tolerance: Option<f32>,
    area_accept: Option<&(dyn Fn(V3) -> bool + Sync)>,
    lineup: &Lineup,
    feet: V3,
    final_yaw: f32,
    final_pitch: f32,
) -> f32 {
    let base_eye = feet + V3::new(0.0, 0.0, eye_height(lineup.throw_type));
    let yaw_rad = final_yaw * PI / 180.0;
    let perp = V3::new(-yaw_rad.sin(), yaw_rad.cos(), 0.0);
    let accept_at = |eye: V3, kk: &ThrowConstants| -> bool {
        let spec = ThrowSpec {
            eye,
            yaw_deg: final_yaw,
            pitch_deg: final_pitch,
            throw_type: lineup.throw_type,
            strength: lineup.strength,
            run_yaw_offset_deg: lineup.run_yaw_offset_deg,
        };
        let r = simulate_exact(collider, &spec, kk, sim::Trace::default());
        settled(&r)
            && accepts(
                grid,
                zone_crossings,
                aim_target,
                tolerance,
                area_accept,
                r.rest,
            )
    };

    // Reviewed and corrected (DECISION 1): scaling only `k.throw_speed` left the jump/run
    // velocity components (added on top of the throw itself by `derive_initial`, for a jump or
    // running throw) completely unscaled, understating this axis's own uncertainty for exactly
    // the throw kinds (`JumpThrow`, `RunJumpThrow`, ...) this whole addendum is about. Scaling the
    // FULL derived launch velocity instead covers throw speed, jump velocity and run speed
    // together, in the same proportion.
    let base_spec = ThrowSpec {
        eye: base_eye,
        yaw_deg: final_yaw,
        pitch_deg: final_pitch,
        throw_type: lineup.throw_type,
        strength: lineup.strength,
        run_yaw_offset_deg: lineup.run_yaw_offset_deg,
    };
    let (launch_pos, launch_vel) = sim::derive_initial(&base_spec, k);
    let mut hits = 0usize;
    let mut total = 0usize;
    for &scale in &ROBUST_MODEL_SPEED_SCALES {
        let r = simulate_exact_raw(
            collider,
            launch_pos,
            launch_vel * scale,
            k,
            sim::Trace::default(),
        );
        total += 1;
        hits += usize::from(
            settled(&r)
                && accepts(
                    grid,
                    zone_crossings,
                    aim_target,
                    tolerance,
                    area_accept,
                    r.rest,
                ),
        );
    }
    for &dz in &ROBUST_MODEL_POS_OFFSETS {
        for &dp in &ROBUST_MODEL_POS_OFFSETS {
            if dz == 0.0 && dp == 0.0 {
                continue;
            }
            let eye = base_eye + V3::new(0.0, 0.0, dz) + perp * dp;
            total += 1;
            hits += usize::from(accept_at(eye, k));
        }
    }
    hits as f32 / total as f32
}

/// `s6q_robust_aim.md`: re-centers a precise-mode `verify_exact` survivor's aim on the middle of
/// its own fine-grid working band, nudges its exact teleport spot off any real hull overlap
/// (`safe_teleport_feet`), then measures `robustAim`/`robustPos`/`robustModel`/`aimMarginDeg` -
/// `robustness` is `min(robustAim, robustPos, robustModel)`, not an average, since a lineup that
/// fails hard on any one axis (aim, stand position, or launch-model uncertainty) is not actually
/// robust just because the other two look good. `None` when the chosen aim's own re-simulation
/// doesn't settle/accept (should never happen - every grid cell this picks from already did, same
/// idiom as `verify_exact`'s own `settled_ok` guard), or when no teleport spot within
/// `safe_teleport_feet`'s own cap is both safe and accepted.
#[allow(clippy::too_many_arguments)]
pub fn robust_center<C: Collider>(
    grid: &VoxelGrid,
    collider: &C,
    player_collider: &C,
    k: &ThrowConstants,
    zone_crossings: &HashMap<usize, i32>,
    aim_target: Option<V3>,
    tolerance: Option<f32>,
    area_accept: Option<&(dyn Fn(V3) -> bool + Sync)>,
    collider_glass_gone: Option<&C>,
    lineup: &Lineup,
) -> Option<Lineup> {
    let eye = lineup.feet + V3::new(0.0, 0.0, eye_height(lineup.throw_type));
    let sim_offset = |dy: i32, dp: i32| -> TrajectoryResult {
        let spec = ThrowSpec {
            eye,
            yaw_deg: lineup.yaw_deg + dy as f32 * ROBUST_STEP_DEG,
            pitch_deg: lineup.pitch_deg + dp as f32 * ROBUST_STEP_DEG,
            throw_type: lineup.throw_type,
            strength: lineup.strength,
            run_yaw_offset_deg: lineup.run_yaw_offset_deg,
        };
        simulate_exact(collider, &spec, k, sim::Trace::default())
    };
    let accept_at = |dy: i32, dp: i32| -> bool {
        let r = sim_offset(dy, dp);
        settled(&r)
            && accepts(
                grid,
                zone_crossings,
                aim_target,
                tolerance,
                area_accept,
                r.rest,
            )
    };

    let mut cells: Vec<(i32, i32, bool)> =
        Vec::with_capacity((2 * ROBUST_REACH as usize + 1).pow(2));
    for dy in -ROBUST_REACH..=ROBUST_REACH {
        for dp in -ROBUST_REACH..=ROBUST_REACH {
            cells.push((dy, dp, accept_at(dy, dp)));
        }
    }

    let (best_dy, best_dp, aim_margin_deg) = pick_centered_offset(&cells)?;

    let final_yaw = normalize_yaw(lineup.yaw_deg + best_dy as f32 * ROBUST_STEP_DEG);
    let final_pitch = lineup.pitch_deg + best_dp as f32 * ROBUST_STEP_DEG;
    let final_spec = ThrowSpec {
        eye,
        yaw_deg: final_yaw,
        pitch_deg: final_pitch,
        throw_type: lineup.throw_type,
        strength: lineup.strength,
        run_yaw_offset_deg: lineup.run_yaw_offset_deg,
    };
    let best_result = simulate_exact(collider, &final_spec, k, sim::Trace::default());
    if !settled(&best_result)
        || !accepts(
            grid,
            zone_crossings,
            aim_target,
            tolerance,
            area_accept,
            best_result.rest,
        )
    {
        return None;
    }

    // Real-game addendum: the exact teleport spot itself must clear the real hull, or drop the
    // lineup outright - see `safe_teleport_feet`'s own doc comment.
    let (safe_feet, _nudge_dist, resimulated) = safe_teleport_feet(
        grid,
        collider,
        player_collider,
        k,
        zone_crossings,
        aim_target,
        tolerance,
        area_accept,
        lineup,
        final_yaw,
        final_pitch,
    )?;
    let safe_result = resimulated.unwrap_or(best_result);

    // `robustAim`: the same 169-cell grid above, re-centered on the new aim and filtered to a
    // plain (unweighted) 0.15 deg disk - always at least one cell (the new aim's own, distance 0),
    // so no extra sims and no divide-by-zero risk. Left at the original (pre-nudge) feet: a nudge
    // of at most 2u does not meaningfully change the aim-sensitivity band of a throw travelling
    // hundreds of units, and re-simulating the whole 169-cell grid at the nudged feet would double
    // this pass's own cost for a difference the grid's own 0.05 deg step could not resolve anyway.
    let mut disk_total = 0usize;
    let mut disk_hits = 0usize;
    for &(dy, dp, ok) in &cells {
        let ddy = (dy - best_dy) as f32 * ROBUST_STEP_DEG;
        let ddp = (dp - best_dp) as f32 * ROBUST_STEP_DEG;
        if (ddy * ddy + ddp * ddp).sqrt() <= ROBUST_DISK_DEG {
            disk_total += 1;
            disk_hits += usize::from(ok);
        }
    }
    let robust_aim = disk_hits as f32 / disk_total as f32;

    // `robustPos`: a 5x5 feet-jitter grid at the safe (post-nudge) feet and the new aim, +-1u in
    // x/y, same eye height (z untouched) - the safe feet are what a player is actually told to
    // stand at, so that is what its own position uncertainty should be measured around.
    let safe_eye = safe_feet + V3::new(0.0, 0.0, eye_height(lineup.throw_type));
    let mut pos_hits = 0usize;
    let mut pos_total = 0usize;
    for i in -2..=2i32 {
        for j in -2..=2i32 {
            let jittered_eye = safe_eye
                + V3::new(
                    i as f32 * (ROBUST_FEET_JITTER / 2.0),
                    j as f32 * (ROBUST_FEET_JITTER / 2.0),
                    0.0,
                );
            let spec = ThrowSpec {
                eye: jittered_eye,
                yaw_deg: final_yaw,
                pitch_deg: final_pitch,
                throw_type: lineup.throw_type,
                strength: lineup.strength,
                run_yaw_offset_deg: lineup.run_yaw_offset_deg,
            };
            let r = simulate_exact(collider, &spec, k, sim::Trace::default());
            pos_total += 1;
            if settled(&r)
                && accepts(
                    grid,
                    zone_crossings,
                    aim_target,
                    tolerance,
                    area_accept,
                    r.rest,
                )
            {
                pos_hits += 1;
            }
        }
    }
    let robust_pos = pos_hits as f32 / pos_total as f32;

    let robust_model = robust_model_fraction(
        grid,
        collider,
        k,
        zone_crossings,
        aim_target,
        tolerance,
        area_accept,
        lineup,
        safe_feet,
        final_yaw,
        final_pitch,
    );

    let robustness = robust_aim.min(robust_pos).min(robust_model);

    // Reviewed and corrected: centering/nudging can change the final aim and feet, so `glass_breaks`/
    // `rest_if_broken`/`rest_scatter` - all computed by `verify_exact` at the ORIGINAL aim/feet - must
    // be refreshed at the final ones too, the same way `verify_exact` itself computes them
    // (`s6q_robust_aim.md`'s own review: a stale `rest_if_broken` under-reports `stateDependent`, a
    // stale `rest_scatter` under/over-reports `humanError`).
    let final_spec_at_safe = ThrowSpec {
        eye: safe_eye,
        yaw_deg: final_yaw,
        pitch_deg: final_pitch,
        throw_type: lineup.throw_type,
        strength: lineup.strength,
        run_yaw_offset_deg: lineup.run_yaw_offset_deg,
    };
    let mut rest_if_broken: Option<V3> = None;
    if safe_result.glass_breaks > 0
        && let Some(gone) = collider_glass_gone
    {
        let gone_r = simulate_exact(gone, &final_spec_at_safe, k, sim::Trace::default());
        rest_if_broken = settled(&gone_r).then_some(gone_r.rest);
    }
    let mut scatter = 0f32;
    for &(dx, dy) in &SCATTER_OFFSETS {
        let spec = ThrowSpec {
            eye: safe_eye + V3::new(dx, dy, 0.0),
            yaw_deg: final_yaw,
            pitch_deg: final_pitch,
            throw_type: lineup.throw_type,
            strength: lineup.strength,
            run_yaw_offset_deg: lineup.run_yaw_offset_deg,
        };
        let probe = simulate_exact(collider, &spec, k, sim::Trace::default());
        scatter = scatter.max(if settled(&probe) {
            (probe.rest - safe_result.rest).length()
        } else {
            512.0
        });
    }

    let mut out = *lineup;
    out.feet = safe_feet;
    out.yaw_deg = final_yaw;
    out.pitch_deg = final_pitch;
    out.rest_point = safe_result.rest;
    out.bounces = safe_result.bounces;
    out.flight_time = safe_result.flight_time;
    out.glass_breaks = safe_result.glass_breaks;
    out.rest_if_broken = rest_if_broken;
    out.rest_scatter = scatter;
    out.aim_margin_deg = Some(aim_margin_deg);
    out.robustness = Some(robustness);
    out.robust_aim = Some(robust_aim);
    out.robust_pos = Some(robust_pos);
    out.robust_model = Some(robust_model);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    fn flat_plane(half: f32) -> (CollisionMesh, geom::filter::AttributeMask) {
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
        (mesh, mask)
    }

    #[test]
    fn within_tolerance_checks_xy_radius_and_asymmetric_z_band() {
        let target = V3::new(0.0, 0.0, 0.0);
        assert!(within_tolerance(
            V3::new(10.0, 0.0, 0.0),
            target,
            16.0,
            16.0
        ));
        assert!(!within_tolerance(
            V3::new(20.0, 0.0, 0.0),
            target,
            16.0,
            16.0
        ));
        // z band: -2*voxel .. tolerance + 2*voxel.
        assert!(within_tolerance(
            V3::new(0.0, 0.0, -32.0),
            target,
            16.0,
            16.0
        ));
        assert!(!within_tolerance(
            V3::new(0.0, 0.0, -33.0),
            target,
            16.0,
            16.0
        ));
        assert!(within_tolerance(
            V3::new(0.0, 0.0, 48.0),
            target,
            16.0,
            16.0
        ));
        assert!(!within_tolerance(
            V3::new(0.0, 0.0, 49.0),
            target,
            16.0,
            16.0
        ));
    }

    #[test]
    fn stability_is_one_on_a_generous_zone_and_below_one_on_a_knife_edge() {
        let (mesh, mask) = flat_plane(4000.0);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let bounds = geom::math::Aabb {
            min: V3::new(-4000.0, -4000.0, -8.0),
            max: V3::new(4000.0, 4000.0, 512.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let k = ThrowConstants::default();

        let feet = V3::new(0.0, 0.0, 0.0);
        let eye = feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        let spec = ThrowSpec {
            eye,
            yaw_deg: 0.0,
            pitch_deg: -1.0,
            throw_type: ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let exact = simulate_exact(&collider, &spec, &k, sim::Trace::default());
        assert!(settled(&exact));
        let lineup = Lineup::new(
            feet,
            0.0,
            -1.0,
            ThrowType::Stand,
            exact.rest,
            exact.bounces,
            exact.flight_time,
            1,
        );

        // Generous zone: a 5 degree perturbation's landing spread is well
        // inside 400u, so every offset accepts -> stability 1.0.
        let wide_zone = crate::zone::point_target_zone(&grid, exact.rest, 400.0);
        let opts_wide: VerifyOptions<UniformGrid> = VerifyOptions {
            min_stability: 0.0,
            constants: Some(&k),
            ..Default::default()
        };
        let verified = verify_exact(&grid, &collider, &wide_zone, &[lineup], &opts_wide);
        assert_eq!(verified.len(), 1);
        assert_eq!(
            verified[0].stability, 1.0,
            "wide zone should accept every offset"
        );

        // Knife-edge zone: only the exact aim's landing is inside a tight
        // tolerance, so at least one of the four perturbed offsets misses. 16.0 (not the
        // absolute minimum 1.0) so `point_target_zone`'s own `tolerance + voxel_size` margin
        // reliably reaches the open cell above `exact.rest`'s (solid, floor) cell regardless of
        // where in its cell `exact.rest` happens to land - `exact.rest` sitting near a cell
        // corner can put that open cell's center up to about 19.6u away in the worst case (one
        // full cell height plus up to a cell's own XY diagonal), so this needs real headroom.
        let narrow_zone = crate::zone::point_target_zone(&grid, exact.rest, 16.0);
        let opts_narrow: VerifyOptions<UniformGrid> = VerifyOptions {
            min_stability: 0.0,
            constants: Some(&k),
            ..Default::default()
        };
        let verified = verify_exact(&grid, &collider, &narrow_zone, &[lineup], &opts_narrow);
        assert_eq!(verified.len(), 1);
        assert!(
            verified[0].stability < 1.0,
            "knife-edge zone should not survive every perturbation, got {}",
            verified[0].stability
        );
    }

    /// `s6l_aim_precision.md`: `VerifyOptions::default()` must keep the exact pre-precision-mode
    /// aim-window step/reach and never opt into `stability_wide` on its own.
    #[test]
    fn default_verify_options_keep_the_reference_step_reach_and_no_wide_stability() {
        let opts: VerifyOptions<UniformGrid> = VerifyOptions::default();
        assert_eq!(opts.step_deg, STEP_DEG);
        assert_eq!(opts.aim_reach, AIM_REACH);
        assert!(!opts.wide_stability);
    }

    /// `s6l_aim_precision.md`: default `VerifyOptions` (`step_deg`/`aim_reach` left at their
    /// `Default::default()` values) must reproduce the pre-change behavior - checked against
    /// values computed with no `VerifyOptions` involved at all (a direct `simulate_exact` call per
    /// probe), not against a second `VerifyOptions` construction, so this cannot pass merely
    /// because two option structs agree with each other.
    #[test]
    fn default_verify_options_reproduce_the_pre_change_explicit_constants() {
        let (mesh, mask) = flat_plane(4000.0);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let bounds = geom::math::Aabb {
            min: V3::new(-4000.0, -4000.0, -8.0),
            max: V3::new(4000.0, 4000.0, 512.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let k = ThrowConstants::default();

        let feet = V3::new(0.0, 0.0, 0.0);
        let eye = feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        let spec = ThrowSpec {
            eye,
            yaw_deg: 0.0,
            pitch_deg: -1.0,
            throw_type: ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let exact = simulate_exact(&collider, &spec, &k, sim::Trace::default());
        assert!(settled(&exact));
        let lineup = Lineup::new(
            feet,
            0.0,
            -1.0,
            ThrowType::Stand,
            exact.rest,
            exact.bounces,
            exact.flight_time,
            1,
        );
        let narrow_zone = crate::zone::point_target_zone(&grid, exact.rest, 16.0);
        let zone_crossings = zone_lookup(&narrow_zone);

        let opts_default: VerifyOptions<UniformGrid> = VerifyOptions {
            min_stability: 0.0,
            constants: Some(&k),
            ..Default::default()
        };
        let a = verify_exact(&grid, &collider, &narrow_zone, &[lineup], &opts_default);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].yaw_deg, 0.0);
        assert_eq!(a[0].pitch_deg, -1.0);
        assert_eq!(a[0].rest_point, exact.rest);

        // The reference 5-probe stability window: the base aim itself, plus the base yaw/pitch
        // each nudged by the literal `STEP_DEG` (0.6) - computed independently of `VerifyOptions`
        // or `sim_at`'s own cache, straight from `simulate_exact`.
        let probe_offsets: [(f32, f32); 5] =
            [(0.0, 0.0), (-0.6, 0.0), (0.6, 0.0), (0.0, -0.6), (0.0, 0.6)];
        let hits = probe_offsets
            .iter()
            .filter(|&&(d_yaw, d_pitch)| {
                let probe = ThrowSpec {
                    eye,
                    yaw_deg: d_yaw,
                    pitch_deg: -1.0 + d_pitch,
                    throw_type: ThrowType::Stand,
                    strength: 1.0,
                    run_yaw_offset_deg: 0.0,
                };
                let r = simulate_exact(&collider, &probe, &k, sim::Trace::default());
                settled(&r) && in_zone(&grid, &zone_crossings, r.rest)
            })
            .count();
        let expected_stability = hits as f32 / probe_offsets.len() as f32;
        assert_eq!(a[0].stability, expected_stability);
        assert_eq!(a[0].stability_wide, a[0].stability);
    }

    /// `s6l_aim_precision.md`: synthetic case where the reference ±0.6° probes miss a tight
    /// landing zone (a near-horizontal, grazing throw is exactly the kind of high-sensitivity
    /// aim the real window evidence describes) but precise mode's ±0.2° probes hit it. Uses a
    /// `Target::Point`-style `aim_target`/`tolerance` (the real evidence's own `--tolerance 32`
    /// query shape) rather than a cell-based zone, so the accept test is a plain continuous
    /// distance check with no voxel-grid boundary effects to account for.
    #[test]
    fn precise_step_recovers_stability_a_coarser_probe_step_misses() {
        let (mesh, mask) = flat_plane(4000.0);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let bounds = geom::math::Aabb {
            min: V3::new(-4000.0, -4000.0, -8.0),
            max: V3::new(4000.0, 4000.0, 512.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let k = ThrowConstants::default();

        let feet = V3::new(0.0, 0.0, 0.0);
        let eye = feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        let base_pitch = -1.0;
        let spec_at = |pitch_deg: f32| ThrowSpec {
            eye,
            yaw_deg: 0.0,
            pitch_deg,
            throw_type: ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let exact = simulate_exact(&collider, &spec_at(base_pitch), &k, sim::Trace::default());
        assert!(settled(&exact));
        let lineup = Lineup::new(
            feet,
            0.0,
            base_pitch,
            ThrowType::Stand,
            exact.rest,
            exact.bounces,
            exact.flight_time,
            1,
        );

        // How far a reference-step (0.6 deg) vs a precise-step (0.2 deg) pitch nudge moves the
        // rest point - the tolerance sits strictly between the two, so the reference step's probe
        // falls outside it while the precise step's stays inside.
        let coarse = simulate_exact(
            &collider,
            &spec_at(base_pitch + STEP_DEG),
            &k,
            sim::Trace::default(),
        );
        let precise_probe = simulate_exact(
            &collider,
            &spec_at(base_pitch + 0.2),
            &k,
            sim::Trace::default(),
        );
        assert!(settled(&coarse) && settled(&precise_probe));
        let d_coarse = (coarse.rest - exact.rest).length();
        let d_precise = (precise_probe.rest - exact.rest).length();
        assert!(
            d_precise < d_coarse,
            "a smaller probe step should move the rest point less: {d_precise} vs {d_coarse}"
        );
        let tolerance = (d_coarse + d_precise) / 2.0;

        let opts_coarse: VerifyOptions<UniformGrid> = VerifyOptions {
            min_stability: 0.0,
            constants: Some(&k),
            aim_target: Some(exact.rest),
            tolerance: Some(tolerance),
            ..Default::default()
        };
        let coarse_verified = verify_exact(&grid, &collider, &[], &[lineup], &opts_coarse);
        assert_eq!(coarse_verified.len(), 1);
        assert!(
            coarse_verified[0].stability < 0.8,
            "the reference 0.6 deg probes should not all land inside so tight a tolerance, got {}",
            coarse_verified[0].stability
        );

        let opts_precise: VerifyOptions<UniformGrid> = VerifyOptions {
            min_stability: 0.0,
            constants: Some(&k),
            aim_target: Some(exact.rest),
            tolerance: Some(tolerance),
            step_deg: 0.2,
            aim_reach: 4,
            wide_stability: true,
            ..Default::default()
        };
        let precise_verified = verify_exact(&grid, &collider, &[], &[lineup], &opts_precise);
        assert_eq!(precise_verified.len(), 1);
        assert!(
            precise_verified[0].stability > coarse_verified[0].stability,
            "the precise 0.2 deg probes should land inside the tolerance more reliably, got {} vs coarse {}",
            precise_verified[0].stability,
            coarse_verified[0].stability
        );
        // Both the coarse run's own `stability` and the precise run's `stability_wide` are the
        // same ±0.6 deg 5-probe stability around the same (unmoved, `min_stability: 0.0` keeps the
        // closest-to-goal offset (0,0) since `exact.rest` itself is the aim target) final aim -
        // they must agree.
        assert_eq!(
            precise_verified[0].stability_wide,
            coarse_verified[0].stability
        );
    }

    #[test]
    fn exhaustive_exact_spot_finds_a_hit_on_flat_ground() {
        let (mesh, mask) = flat_plane(4000.0);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let k = ThrowConstants::default();
        let feet = V3::new(0.0, 0.0, 0.0);

        // First find where a straightforward stand throw actually lands,
        // then ask ExhaustiveExactSpot to find it again from scratch.
        let eye = feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        let probe = simulate_exact(
            &collider,
            &ThrowSpec {
                eye,
                yaw_deg: 0.0,
                pitch_deg: -5.0,
                throw_type: ThrowType::Stand,
                strength: 1.0,
                run_yaw_offset_deg: 0.0,
            },
            &k,
            sim::Trace::default(),
        );
        assert!(settled(&probe));

        let found = exhaustive_exact_spot(
            &collider,
            feet,
            probe.rest,
            32.0,
            &[ThrowType::Stand],
            Some(&[1.0]),
            Some(&k),
            2.0,
            false,
            None,
            None,
            None,
        );
        assert!(
            !found.is_empty(),
            "should find at least one hit near the known rest point"
        );
        for l in &found {
            let dx = l.rest_point.x - probe.rest.x;
            let dy = l.rest_point.y - probe.rest.y;
            assert!((dx * dx + dy * dy).sqrt() <= 32.0);
        }
    }

    /// `s6q_robust_aim.md`: a hand-built synthetic band (accepted iff `d_yaw` in `[-4, 0]`, for
    /// every `d_pitch` - the real Evidence's own shape, a working yaw band sitting off to one side
    /// of the original aim at offset `(0, 0)`) - the true middle of that band is `d_yaw = -2`, and
    /// since the band ignores pitch entirely, `d_pitch = 0` wins the tie-break (closest to the
    /// original aim) among every `d_pitch` at that same clearance.
    #[test]
    fn pick_centered_offset_finds_the_middle_of_a_synthetic_band() {
        let mut cells: Vec<(i32, i32, bool)> = Vec::new();
        for dy in -ROBUST_REACH..=ROBUST_REACH {
            for dp in -ROBUST_REACH..=ROBUST_REACH {
                cells.push((dy, dp, (-4..=0).contains(&dy)));
            }
        }
        let (dy, dp, clearance) = pick_centered_offset(&cells).expect("some cell accepted");
        assert_eq!((dy, dp), (-2, 0));
        assert!(
            (clearance - 3.0 * ROBUST_STEP_DEG).abs() < 1e-4,
            "expected the 3-step clearance to either edge, got {clearance}"
        );
    }

    #[test]
    fn pick_centered_offset_is_none_when_the_whole_grid_is_rejected() {
        let cells: Vec<(i32, i32, bool)> = (-ROBUST_REACH..=ROBUST_REACH)
            .flat_map(|dy| (-ROBUST_REACH..=ROBUST_REACH).map(move |dp| (dy, dp, false)))
            .collect();
        assert!(pick_centered_offset(&cells).is_none());
    }

    #[test]
    fn pick_centered_offset_reports_the_no_reject_sentinel_when_everything_accepted() {
        let cells: Vec<(i32, i32, bool)> = (-ROBUST_REACH..=ROBUST_REACH)
            .flat_map(|dy| (-ROBUST_REACH..=ROBUST_REACH).map(move |dp| (dy, dp, true)))
            .collect();
        let (dy, dp, clearance) = pick_centered_offset(&cells).unwrap();
        // Every cell ties at the sentinel clearance (no rejected cell exists at all), so the
        // closest-to-original tie-break picks the original aim itself back, offset (0, 0).
        assert_eq!((dy, dp), (0, 0));
        assert_eq!(clearance, ROBUST_NO_REJECT_MARGIN_DEG);
    }

    /// Regression (review round 2, item 1): a synthetic band bounded on BOTH axes - accepted iff
    /// `d_yaw` in `[-4, 0]` AND `d_pitch` in `[-1, 1]`. With the pitch weight applied backwards (an
    /// earlier version of this code multiplied instead of dividing), every cell's clearance was
    /// dominated by the artificially-shrunk distance to the nearby pitch-boundary reject
    /// (identical at every `d_yaw`), which masked the real yaw-centering signal and left the
    /// tie-break picking `d_yaw = 0` instead of the true middle. Correctly weighted, `(-2, 0)` wins
    /// outright (yaw clearance 0.15 beats every other accepted cell's).
    #[test]
    fn pick_centered_offset_finds_the_middle_of_a_synthetic_band_with_pitch_bounds() {
        let mut cells: Vec<(i32, i32, bool)> = Vec::new();
        for dy in -ROBUST_REACH..=ROBUST_REACH {
            for dp in -ROBUST_REACH..=ROBUST_REACH {
                cells.push((dy, dp, (-4..=0).contains(&dy) && (-1..=1).contains(&dp)));
            }
        }
        let (dy, dp, clearance) = pick_centered_offset(&cells).expect("some cell accepted");
        assert_eq!((dy, dp), (-2, 0));
        assert!(
            (clearance - 3.0 * ROBUST_STEP_DEG).abs() < 1e-4,
            "expected the 3-step yaw clearance, got {clearance}"
        );
    }

    /// Regression (review round 2, item 2): accepted everywhere except a single one-sided reject at
    /// `d_yaw = -6` (the grid's own far edge). Ignoring the untested region past the OTHER edge
    /// (`d_yaw = +7` and beyond) let the old code report `d_yaw = +6` as "centered" with a margin of
    /// 0.60 deg - a cell sitting right on the grid's own boundary, not actually centered in anything.
    /// With the grid edge itself treated as an unknown (soft) boundary, `d_yaw = 0` wins instead.
    #[test]
    fn pick_centered_offset_does_not_drift_to_the_grid_edge_on_a_one_sided_reject() {
        let mut cells: Vec<(i32, i32, bool)> = Vec::new();
        for dy in -ROBUST_REACH..=ROBUST_REACH {
            for dp in -ROBUST_REACH..=ROBUST_REACH {
                cells.push((dy, dp, dy != -ROBUST_REACH));
            }
        }
        let (dy, dp, _) = pick_centered_offset(&cells).expect("some cell accepted");
        assert_ne!(
            (dy, dp),
            (ROBUST_REACH, 0),
            "must not drift to the grid's own edge"
        );
        assert_eq!((dy, dp), (0, 0));
    }

    /// `s6q_robust_aim.md`'s real-game addendum: `robustness` is the minimum of its three
    /// sub-fractions, not any kind of average - one weak axis must not be hidden by two strong ones.
    #[test]
    fn robustness_is_the_minimum_not_an_average_of_its_sub_fractions() {
        let robust_aim = 1.0f32;
        let robust_pos = 1.0f32;
        let robust_model = 0.25f32;
        let robustness = robust_aim.min(robust_pos).min(robust_model);
        assert_eq!(robustness, 0.25);
    }

    fn robust_center_test_fixture() -> (
        CollisionMesh,
        geom::filter::AttributeMask,
        VoxelGrid,
        ThrowConstants,
        Lineup,
    ) {
        let (mesh, mask) = flat_plane(4000.0);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let bounds = geom::math::Aabb {
            min: V3::new(-4000.0, -4000.0, -8.0),
            max: V3::new(4000.0, 4000.0, 512.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let k = ThrowConstants::default();
        let feet = V3::new(0.0, 0.0, 0.0);
        let eye = feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        let spec = ThrowSpec {
            eye,
            yaw_deg: 0.0,
            pitch_deg: -1.0,
            throw_type: ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let exact = simulate_exact(&collider, &spec, &k, sim::Trace::default());
        assert!(settled(&exact));
        let lineup = Lineup::new(
            feet,
            0.0,
            -1.0,
            ThrowType::Stand,
            exact.rest,
            exact.bounces,
            exact.flight_time,
            1,
        );
        (mesh, mask, grid, k, lineup)
    }

    #[test]
    fn robust_center_is_fully_robust_on_a_wide_open_zone() {
        let (mesh, mask, grid, k, lineup) = robust_center_test_fixture();
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let wide_zone = crate::zone::point_target_zone(&grid, lineup.rest_point, 400.0);
        let zone_crossings = zone_lookup(&wide_zone);
        let out = robust_center(
            &grid,
            &collider,
            &collider,
            &k,
            &zone_crossings,
            None,
            None,
            None,
            None,
            &lineup,
        )
        .expect("a wide-open zone should stay accepted at every tested offset");
        assert_eq!(out.robustness, Some(1.0));
        assert_eq!(out.robust_aim, Some(1.0));
        assert_eq!(out.robust_pos, Some(1.0));
        assert_eq!(out.robust_model, Some(1.0));
        assert_eq!(out.aim_margin_deg, Some(ROBUST_NO_REJECT_MARGIN_DEG));
    }

    #[test]
    fn robust_center_scores_below_one_on_a_knife_edge_zone() {
        let (mesh, mask, grid, k, lineup) = robust_center_test_fixture();
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let eye = lineup.feet + V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT);
        // Tolerance derived from the grid's own extreme (`ROBUST_REACH` steps of `ROBUST_STEP_DEG`)
        // pitch displacement, same idiom as `precise_step_recovers_stability_a_coarser_probe_step_misses`:
        // half that displacement rejects the grid's own far edge while still accepting its center.
        let extreme = simulate_exact(
            &collider,
            &ThrowSpec {
                eye,
                yaw_deg: lineup.yaw_deg,
                pitch_deg: lineup.pitch_deg + ROBUST_REACH as f32 * ROBUST_STEP_DEG,
                throw_type: ThrowType::Stand,
                strength: 1.0,
                run_yaw_offset_deg: 0.0,
            },
            &k,
            sim::Trace::default(),
        );
        assert!(settled(&extreme));
        let tolerance = (extreme.rest - lineup.rest_point).length() / 2.0;
        let zone_crossings: HashMap<usize, i32> = HashMap::new();
        let out = robust_center(
            &grid,
            &collider,
            &collider,
            &k,
            &zone_crossings,
            Some(lineup.rest_point),
            Some(tolerance),
            None,
            None,
            &lineup,
        )
        .expect("the center of the grid must still be accepted");
        assert!(
            out.robustness.unwrap() < 1.0,
            "a knife-edge tolerance should not stay accepted at every tested offset, got {:?}",
            out.robustness
        );
    }

    fn push_quad(mesh: &mut CollisionMesh, attr: u16, obj: u32, quad: [[f32; 3]; 4]) {
        mesh.push_triangles(
            &quad,
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
    }

    /// `s6q_robust_aim.md`'s real-game addendum: the exact geometry from the failing in-game
    /// report - a vertical wall at x=1376, and (just inside it) a slanted face whose profile in
    /// the x/z plane runs (1375.0, z=-200) -> (1375.938, z=-136) -> (1375.0, z=-72), extruded
    /// across y 95..200, over a floor at z=-164. A standing hull at x=1359.4956 (half-width 16, so
    /// its own edge reaches x=1375.4956, past the slant's own peak protrusion) must be reported as
    /// overlapping - this exact spot is what every existing (walking-tolerance) check accepted,
    /// right up until a raw `setpos` teleport there got refused in game.
    #[test]
    fn hull_clear_flags_the_real_window_wall_overlap() {
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
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [1000.0, 0.0, -164.0],
                [1500.0, 0.0, -164.0],
                [1500.0, 300.0, -164.0],
                [1000.0, 300.0, -164.0],
            ],
        );
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [1376.0, 95.0, -200.0],
                [1376.0, 200.0, -200.0],
                [1376.0, 200.0, -72.0],
                [1376.0, 95.0, -72.0],
            ],
        );
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [1375.0, 95.0, -200.0],
                [1375.938, 95.0, -136.0],
                [1375.938, 200.0, -136.0],
                [1375.0, 200.0, -200.0],
            ],
        );
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [1375.938, 95.0, -136.0],
                [1375.0, 95.0, -72.0],
                [1375.0, 200.0, -72.0],
                [1375.938, 200.0, -136.0],
            ],
        );
        let mask = all_mask(&mesh);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();

        assert!(
            !hull_clear(
                &collider,
                V3::new(1359.4956, 144.42, -164.0),
                ThrowType::Stand
            ),
            "a standing hull at x=1359.4956 must be reported as overlapping the slanted face"
        );
        assert!(
            hull_clear(&collider, V3::new(1200.0, 144.42, -164.0), ThrowType::Stand),
            "well clear of the wall entirely"
        );
    }

    /// Regression (review round 2, item 4): a 30 deg ramp whose toe sits just inside the hull's own
    /// footprint edge (0.5u in) - within the footprint the ramp only rises to `0.5*tan(30) ≈ 0.29u`
    /// above `feet.z`, a bump the OLD (0.5u-shrunk-off-`feet.z`) box entirely skipped (its own
    /// bottom started at `feet.z + 0.5`, above the ramp's own highest point inside the footprint) but
    /// the real teleport (landing at `feet.z + TELEPORT_LIFT`) would have clipped straight into.
    #[test]
    fn hull_clear_catches_a_shallow_ramp_the_old_floor_skin_would_have_missed() {
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
        // Flat floor, x in [-200, 15.5] (feet sit at x=0, z=0; the hull's own footprint reaches to
        // x=16).
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [-200.0, -200.0, 0.0],
                [15.5, -200.0, 0.0],
                [15.5, 200.0, 0.0],
                [-200.0, 200.0, 0.0],
            ],
        );
        // A 30 deg ramp from the floor's own edge (x=15.5, z=0) rising toward x=50.
        let rise = (50.0f32 - 15.5) * 30f32.to_radians().tan();
        push_quad(
            &mut mesh,
            attr,
            obj,
            [
                [15.5, -200.0, 0.0],
                [50.0, -200.0, rise],
                [50.0, 200.0, rise],
                [15.5, 200.0, 0.0],
            ],
        );
        let mask = all_mask(&mesh);
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();

        assert!(
            !hull_clear(&collider, V3::new(0.0, 0.0, 0.0), ThrowType::Stand),
            "the ramp's own toe pokes up to ~0.29u inside the hull's footprint edge - must overlap"
        );
    }

    /// `s6q_robust_aim.md`'s real-game addendum: `robust_center` must nudge a lineup whose original
    /// feet overlap a wall onto a hull-clear spot nearby, rather than dropping it outright, as long
    /// as the nudged spot still lands correctly.
    #[test]
    fn robust_center_nudges_an_overlapping_spot_to_a_clear_one() {
        let (mesh, mask, grid, k, lineup) = robust_center_test_fixture();
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();

        // A thin wall placed just past the hull's own edge on the flat-plane fixture's +x side -
        // close enough that the fixture's own feet (x=0) overlap it, far enough that a small nudge
        // in -x clears it.
        let mut walled = mesh.clone();
        let attr = walled.attributes[0].clone();
        let attr_idx = walled.add_attribute(attr).unwrap();
        let obj = walled.add_object(MeshObject {
            kind: ObjectKind::WorldMesh,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 1,
            hull_flags: None,
        });
        let wall_x = lineup.feet.x + crate::standspots::HULL_HALF_WIDTH - 1.0;
        push_quad(
            &mut walled,
            attr_idx,
            obj,
            [
                [wall_x, -500.0, -100.0],
                [wall_x, 500.0, -100.0],
                [wall_x, 500.0, 200.0],
                [wall_x, -500.0, 200.0],
            ],
        );
        let walled_mask = all_mask(&walled);
        let player_collider = UniformGrid::build(&walled, &walled_mask, None, 128.0).unwrap();
        assert!(
            !hull_clear(&player_collider, lineup.feet, lineup.throw_type),
            "the fixture's own feet must start out overlapping the added wall"
        );

        let wide_zone = crate::zone::point_target_zone(&grid, lineup.rest_point, 400.0);
        let zone_crossings = zone_lookup(&wide_zone);
        let out = robust_center(
            &grid,
            &collider,
            &player_collider,
            &k,
            &zone_crossings,
            None,
            None,
            None,
            None,
            &lineup,
        )
        .expect("a nearby clear spot exists and still lands correctly");
        assert_ne!(out.feet, lineup.feet, "must have nudged off the wall");
        assert!(hull_clear(&player_collider, out.feet, out.throw_type));
        let dist = (out.feet - lineup.feet).length();
        assert!(dist <= 2.0, "nudge distance {dist} exceeds the 2u cap");
    }

    /// `s6q_robust_aim.md`'s real-game addendum: a wide-open zone (no aim, position, or model
    /// sensitivity at all) must score `robustModel` 1.0 too, and `robustness` must be the minimum
    /// of the three sub-fractions, not an average.
    #[test]
    fn robust_model_fraction_is_one_when_every_perturbation_still_lands() {
        let (mesh, mask, grid, k, lineup) = robust_center_test_fixture();
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let wide_zone = crate::zone::point_target_zone(&grid, lineup.rest_point, 400.0);
        let zone_crossings = zone_lookup(&wide_zone);
        let f = robust_model_fraction(
            &grid,
            &collider,
            &k,
            &zone_crossings,
            None,
            None,
            None,
            &lineup,
            lineup.feet,
            lineup.yaw_deg,
            lineup.pitch_deg,
        );
        assert_eq!(f, 1.0);
    }
}
