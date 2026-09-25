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
use sim::{ThrowConstants, ThrowSpec, ThrowType, TrajectoryResult, eye_height, simulate_exact};

use crate::dotnet_sort::{float_cmp, introsort};
use crate::lineup::{Lineup, normalize_yaw};
use crate::sweep::{self, ordinal_cmp, settled};
use crate::zone::zone_lookup;

/// `LineupSolver.cs:673` (`StepDeg`).
const STEP_DEG: f32 = 0.6;
/// `LineupSolver.cs:674` (`AimReach`).
const AIM_REACH: i32 = 2;
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
    d_yaw: i32,
    d_pitch: i32,
) -> TrajectoryResult {
    *cache.entry((d_yaw, d_pitch)).or_insert_with(|| {
        let spec = ThrowSpec {
            eye,
            yaw_deg: lineup.yaw_deg + d_yaw as f32 * STEP_DEG,
            pitch_deg: lineup.pitch_deg + d_pitch as f32 * STEP_DEG,
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
        let r = sim_at(cache, collider, k, eye, lineup, cy + dy, cp + dp);
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
                for d_yaw in -AIM_REACH..=AIM_REACH {
                    for d_pitch in -AIM_REACH..=AIM_REACH {
                        let r = sim_at(&mut cache, collider, k, eye, lineup, d_yaw, d_pitch);
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
                let r0 = sim_at(&mut cache, collider, k, eye, lineup, 0, 0);
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
                    for d_yaw in -AIM_REACH..=AIM_REACH {
                        for d_pitch in -AIM_REACH..=AIM_REACH {
                            let r = sim_at(&mut cache, collider, k, eye, lineup, d_yaw, d_pitch);
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

            let best = sim_at(&mut cache, collider, k, eye, lineup, aim_yaw, aim_pitch);
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
            let final_yaw = lineup.yaw_deg + aim_yaw as f32 * STEP_DEG;
            let final_pitch = lineup.pitch_deg + aim_pitch as f32 * STEP_DEG;

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
}
