//! Grenade flight: the exact triangle-collider integrator
//! (`SimulateExact`/`SimulateExactRaw`) and the coarse voxel model
//! (`Simulate`). Ported from `cs2-smoke-solver/src/Sim/GrenadeTrajectory.cs`.

use geom::collider::Collider;
use geom::math::V3;
use geom::voxel::VoxelGrid;

use crate::throw::{
    BASE_GRAVITY, BROKEN_PANE_REACH, FLOOR_NORMAL_Z, GRENADE_HALF, MAX_FLIGHT_SECONDS,
    MAX_VELOCITY_PER_AXIS, PHYSICS_SUBSTEPS, STOP_EPSILON, TIME_STEP, ThrowConstants, ThrowSpec,
    derive_initial,
};

/// `GrenadeTrajectory.cs:28` (`TrajectoryResult`). `ticks` is the number of
/// ticks integrated, including the tick a rest/lost outcome was decided in
/// (not in the reference record; added so callers/tests can inspect flight
/// length directly). This always agrees with `flight_time`: both count the
/// tick the result was returned in, whether that was a mid-tick rest (the
/// loop's own `tick`/`time` counters have not been bumped yet at that point,
/// so both are reported as `+ 1`/`+ TIME_STEP`) or a full-tick timeout.
///
/// `end` is not in the reference record either: it is the seam other
/// grenade kinds (flash/HE/molotov/decoy, stage 7) hang their detonation
/// timing off (see [`Detonation`]), while `lost` stays exactly the
/// reference's own bool for callers that only care about that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajectoryResult {
    pub rest: V3,
    pub bounces: u32,
    pub flight_time: f32,
    pub lost: bool,
    pub first_touch: Option<V3>,
    pub glass_breaks: u32,
    pub ticks: u32,
    pub end: EndReason,
}

/// Why a [`TrajectoryResult`] ended. Not in the reference (which only has
/// `Lost: bool`): added so `simulate_exact_raw`'s detonation seam can report
/// *why* the flight stopped without overloading `lost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The exact integrator's stop rule fired: slow enough and supported.
    Rest,
    /// The voxel model only: either flew out of the voxelized region
    /// entirely (`Simulate`'s `cx < 0 || ...` branch,
    /// `GrenadeTrajectory.cs:220-224`), or `MaxFlightSeconds` elapsed with no
    /// solid voxel below it (`GrenadeTrajectory.cs:262`, `Lost:
    /// !HasGroundBelow`). The exact integrator, which has no grid bounds and
    /// no such check, never reports this.
    Lost,
    /// `MaxFlightSeconds` elapsed without resting; for the voxel model, only
    /// when there IS a solid voxel below it (otherwise that timeout is
    /// [`EndReason::Lost`] instead, per `GrenadeTrajectory.cs:262`).
    Timeout,
    /// A [`Detonation`] hook fired mid-flight.
    Detonated,
}

/// A grenade-kind-specific detonation rule, threaded through the exact
/// integrator's tick loop. Not part of the reference: this crate only
/// implements smoke grenades (stage 4), which never detonate mid-flight, but
/// flash/HE/molotov/decoy (stage 7) all do, at rules that hook the same two
/// points every kind needs: once per tick (a fuse timer) and on every
/// contact (an impact fuse). Default methods return `None`, so `SmokeRest`
/// costs nothing extra once monomorphised - the compiler sees both hooks
/// unconditionally return `None` and can fold the checks away entirely.
pub trait Detonation {
    /// Called once per tick, after that tick's physics is integrated, with
    /// the resulting position/velocity. `tick` is the number of ticks
    /// completed so far, same as [`TrajectoryResult::ticks`]. `Some(pos)`
    /// short-circuits the flight, resting the grenade at `pos`.
    fn on_tick(&mut self, _tick: u32, _pos: V3, _vel: V3) -> Option<V3> {
        None
    }
    /// Called at every contact (including a glass pass-through), before the
    /// reference's own rest/bounce decision for that contact is applied.
    /// `tick` is the 0-based tick currently running, same as
    /// [`BounceRecord::tick`]. `Some(pos)` short-circuits the flight at
    /// `pos`.
    fn on_contact(
        &mut self,
        _tick: u32,
        _contact: V3,
        _normal: V3,
        _vel_before: V3,
        _vel_after: V3,
    ) -> Option<V3> {
        None
    }
}

/// The default (and, for now, only implemented) detonation rule: none.
/// Smoke grenades always fly until they rest or the flight times out,
/// exactly like the reference.
#[derive(Debug, Clone, Copy, Default)]
pub struct SmokeRest;
impl Detonation for SmokeRest {}

/// `GrenadeTrajectory.cs:31` (`BounceRecord`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BounceRecord {
    pub tick: u32,
    pub contact: V3,
    pub normal: V3,
    pub triangle: u32,
    pub vel_before: V3,
    pub vel_after: V3,
}

/// Optional per-tick/per-bounce recording for the exact integrator, used by
/// the viewer's trajectory display and diagnostics (`trace`/`tickTrace`/
/// `bounceTrace` in `SimulateExactRaw`, `GrenadeTrajectory.cs:421`).
#[derive(Default)]
pub struct Trace<'a> {
    pub ticks: Option<&'a mut Vec<(V3, V3)>>,
    pub bounces: Option<&'a mut Vec<BounceRecord>>,
}

/// `System.Numerics.Vector3.Lerp` (`System.Private.CoreLib`,
/// `src/libraries/System.Private.CoreLib/src/System/Numerics/Vector3.cs`):
/// mathematically `(value1 * (1.0f - amount)) + (value2 * amount)`, NOT `a +
/// (b - a) * t` (the two differ in rounding) - but on hardware with FMA
/// (all modern x64/ARM64; `System.Runtime.Intrinsics.X86.Fma.IsSupported`),
/// .NET 10's JIT lowers this to a single fused multiply-add per component,
/// `fma(a, 1-t, b*t)`, which rounds ONCE instead of twice and so differs
/// from the naive two-multiply-one-add sequence in the last bit. Verified
/// empirically against the real .NET 10 `Vector3.Lerp` over 2,000,000
/// randomized cases (0 mismatches; `lerptest` in
/// `D:\porject\modulator-work\scratch\review_s4`). On a non-FMA CPU the
/// reference itself would compute the unfused result instead - this port
/// follows the FMA behavior, matching every machine this was validated on.
fn lerp(a: V3, b: V3, t: f32) -> V3 {
    let s = 1.0 - t;
    V3::new(
        a.x.mul_add(s, b.x * t),
        a.y.mul_add(s, b.y * t),
        a.z.mul_add(s, b.z * t),
    )
}

/// `GrenadeTrajectory.cs:566-569` (`ClampVelocity`, `sv_maxvelocity`).
fn clamp_velocity(v: V3) -> V3 {
    V3::new(
        v.x.clamp(-MAX_VELOCITY_PER_AXIS, MAX_VELOCITY_PER_AXIS),
        v.y.clamp(-MAX_VELOCITY_PER_AXIS, MAX_VELOCITY_PER_AXIS),
        v.z.clamp(-MAX_VELOCITY_PER_AXIS, MAX_VELOCITY_PER_AXIS),
    )
}

/// `GrenadeTrajectory.cs:571-574` (`SnapStopEpsilon`, `STOP_EPSILON` in the
/// SDK): snaps near-zero reflected components to exactly zero BEFORE the
/// elasticity multiply.
fn snap_stop_epsilon(v: V3) -> V3 {
    V3::new(
        if v.x.abs() < STOP_EPSILON { 0.0 } else { v.x },
        if v.y.abs() < STOP_EPSILON { 0.0 } else { v.y },
        if v.z.abs() < STOP_EPSILON { 0.0 } else { v.z },
    )
}

/// `GrenadeTrajectory.cs:170-171` (`FloorImpactDamp`): floor impacts faster
/// than `DampGateSpeed` and steeper than the floor/wall split additionally
/// scale by `1.5 - u`; wall impacts never damp.
fn floor_impact_damp(speed: f32, u: f32, is_floor: bool, k: &ThrowConstants) -> f32 {
    if speed > k.damp_gate_speed && u > 0.5 && is_floor {
        1.5 - u
    } else {
        1.0
    }
}

/// `GrenadeTrajectory.cs:178-196` (`Bounce`). `gate_speed` is the speed the
/// floor-damp gate is judged on (the whole tick's worth of gravity), which
/// may differ from `w`'s own speed for a first-half-step contact.
pub fn bounce(w: V3, normal: V3, k: &ThrowConstants, gate_speed: f32) -> V3 {
    let speed = w.length();
    // `w - 2f * Vector3.Dot(w, normal) * normal`: the scalar `2f * dot` is
    // computed first, then multiplied into the normal.
    let reflected = snap_stop_epsilon(w - normal * (2.0 * w.dot(normal)));
    let u = if speed > 1e-6 {
        w.dot(normal).abs() / speed
    } else {
        0.0
    };
    let damp = floor_impact_damp(gate_speed, u, normal.z > FLOOR_NORMAL_Z, k);
    reflected * (k.elasticity * damp)
}

/// `GrenadeTrajectory.cs:300-345` (`EdgeTip`). Only reachable when
/// `k.edge_tipping` is set (reference default is `false`; see
/// `ThrowConstants::edge_tipping`'s doc), ported anyway per spec.
fn edge_tip<C: Collider>(c: &C, position: V3, heading: V3) -> Option<V3> {
    const REACH: f32 = GRENADE_HALF + 1.0;
    const EDGE_NEUTRAL_BAND: f32 = 0.75;
    let held = |dx: f32, dy: f32| -> bool {
        let from = V3::new(position.x + dx, position.y + dy, position.z);
        c.first_hit_ray(from, from + V3::new(0.0, 0.0, -REACH))
            .is_some_and(|h| h.normal.z > FLOOR_NORMAL_Z)
    };
    if held(0.0, 0.0) {
        return None;
    }
    let offsets: [(f32, f32); 8] = std::array::from_fn(|i| {
        let a = i as f32 * std::f32::consts::PI / 4.0;
        (3.0 * a.cos(), 3.0 * a.sin())
    });
    let mut supported = (0.0f32, 0.0f32);
    let mut count = 0;
    for &(ox, oy) in &offsets {
        if held(ox, oy) {
            supported.0 += ox;
            supported.1 += oy;
            count += 1;
        }
    }
    let dir = if count > 0 && (supported.0 * supported.0 + supported.1 * supported.1) > 1e-6 {
        let len = (supported.0 * supported.0 + supported.1 * supported.1).sqrt();
        let dir = (-supported.0 / len, -supported.1 / len);
        if held(-dir.0 * EDGE_NEUTRAL_BAND, -dir.1 * EDGE_NEUTRAL_BAND) {
            return None;
        }
        dir
    } else {
        let h = (heading.x, heading.y);
        let len_sq = h.0 * h.0 + h.1 * h.1;
        if len_sq < 1e-6 {
            return None;
        }
        let len = len_sq.sqrt();
        (h.0 / len, h.1 / len)
    };
    Some(V3::new(dir.0, dir.1, 0.0))
}

/// The centroid of triangle `t`, matching the reference's `Centroid = (A + B
/// + C) / 3f` exactly (a single division by 3, not a multiply by `1/3`).
fn centroid<C: Collider>(c: &C, t: u32) -> V3 {
    let [a, b, cc] = c.triangle(t);
    (a + b + cc) / 3.0
}

/// Builds the `ignore` predicate for panes already broken this flight
/// (`GrenadeTrajectory.cs:477`), or `None` if nothing has broken yet.
fn glass_ignore<'a, C: Collider>(c: &'a C, broken: &'a [V3]) -> Option<impl Fn(u32) -> bool + 'a> {
    if broken.is_empty() {
        None
    } else {
        Some(move |t: u32| {
            c.is_breakable(t)
                && broken.iter().any(|&b| {
                    (b - centroid(c, t)).length_squared() < BROKEN_PANE_REACH * BROKEN_PANE_REACH
                })
        })
    }
}

/// `GrenadeTrajectory.cs:392-396` (`SimulateExact`), with `Detonation` = the
/// zero-cost default [`SmokeRest`] (never detonates). See
/// [`simulate_exact_with`] for other grenade kinds.
pub fn simulate_exact<C: Collider>(
    c: &C,
    spec: &ThrowSpec,
    k: &ThrowConstants,
    trace: Trace,
) -> TrajectoryResult {
    simulate_exact_with(c, spec, k, trace, &mut SmokeRest)
}

/// `GrenadeTrajectory.cs:392-396` (`SimulateExact`), threaded through a
/// caller-supplied [`Detonation`] rule.
pub fn simulate_exact_with<C: Collider, D: Detonation>(
    c: &C,
    spec: &ThrowSpec,
    k: &ThrowConstants,
    trace: Trace,
    det: &mut D,
) -> TrajectoryResult {
    let (position, velocity) = derive_initial(spec, k);
    simulate_exact_raw_with(c, position, velocity, k, trace, det)
}

/// `GrenadeTrajectory.cs:421-564` (`SimulateExactRaw`), with `Detonation` =
/// the zero-cost default [`SmokeRest`] (never detonates). See
/// [`simulate_exact_raw_with`] for other grenade kinds.
pub fn simulate_exact_raw<C: Collider>(
    c: &C,
    pos0: V3,
    vel0: V3,
    k: &ThrowConstants,
    trace: Trace,
) -> TrajectoryResult {
    simulate_exact_raw_with(c, pos0, vel0, k, trace, &mut SmokeRest)
}

/// `GrenadeTrajectory.cs:421-564` (`SimulateExactRaw`), threaded through a
/// caller-supplied [`Detonation`] rule: `det.on_tick` is polled once per
/// tick and `det.on_contact` at every contact (including glass pass-through
/// and a same-tick second contact), either short-circuiting the flight with
/// [`EndReason::Detonated`]. Monomorphised per `D`, so `simulate_exact_raw`'s
/// `SmokeRest` instantiation compiles to exactly the same code as before
/// this seam existed (the golden-fixture tests are the proof: bit-identical
/// results).
pub fn simulate_exact_raw_with<C: Collider, D: Detonation>(
    c: &C,
    pos0: V3,
    vel0: V3,
    k: &ThrowConstants,
    mut trace: Trace,
    det: &mut D,
) -> TrajectoryResult {
    let half = V3::new(GRENADE_HALF, GRENADE_HALF, GRENADE_HALF);
    let gravity_step = BASE_GRAVITY * k.gravity_scale * TIME_STEP;
    let mut position = pos0;
    let mut velocity = vel0;
    let mut bounces: u32 = 0;
    let mut time: f32 = 0.0;
    let mut tick: u32 = 0;
    let mut first_touch: Option<V3> = None;
    let mut broken: Vec<V3> = Vec::new();
    let mut glass_breaks: u32 = 0;

    while time < MAX_FLIGHT_SECONDS {
        let step_dt = TIME_STEP / PHYSICS_SUBSTEPS as f32;
        let step_g = gravity_step / PHYSICS_SUBSTEPS as f32;
        for step in 0..PHYSICS_SUBSTEPS {
            velocity = clamp_velocity(velocity);
            let vz_old = velocity.z;
            velocity.z -= step_g;
            let mv = V3::new(velocity.x, velocity.y, (vz_old + velocity.z) * 0.5) * step_dt;
            let next = position + mv;

            let hit = {
                let ignore_fn = glass_ignore(c, &broken);
                let ignore: Option<&dyn Fn(u32) -> bool> =
                    ignore_fn.as_ref().map(|f| f as &dyn Fn(u32) -> bool);
                c.first_hit_hull(position, next, half, -2.0, ignore)
            };
            let hit = match hit {
                None => {
                    position = next;
                    continue;
                }
                Some(h) => h,
            };
            let contact = lerp(position, next, (hit.t - 1e-3).max(0.0));
            position = contact;
            if first_touch.is_none() {
                first_touch = Some(contact);
            }
            bounces += 1;

            if k.glass_pass_factor > 0.0 && c.is_breakable(hit.triangle) {
                broken.push(contact);
                glass_breaks += 1;
                velocity = velocity * k.glass_pass_factor;
                let vel_before = velocity / k.glass_pass_factor;
                if let Some(bt) = trace.bounces.as_deref_mut() {
                    // `GrenadeTrajectory.cs:480`: the reference reconstructs
                    // the pre-scale velocity as `velocity / GlassPassFactor`
                    // from the ALREADY-scaled `velocity`, not from the
                    // original value it just overwrote - match that exactly
                    // (a multiply then divide by the same float is not
                    // always a no-op).
                    bt.push(BounceRecord {
                        tick,
                        contact,
                        normal: hit.normal,
                        triangle: hit.triangle,
                        vel_before,
                        vel_after: velocity,
                    });
                }
                if let Some(at) = det.on_contact(tick, contact, hit.normal, vel_before, velocity) {
                    return TrajectoryResult {
                        rest: at,
                        bounces,
                        flight_time: time + TIME_STEP,
                        lost: false,
                        first_touch,
                        glass_breaks,
                        ticks: tick + 1,
                        end: EndReason::Detonated,
                    };
                }
                let through = position + velocity * ((1.0 - hit.t) * step_dt);
                let ignore_fn2 = glass_ignore(c, &broken);
                let ignore2: Option<&dyn Fn(u32) -> bool> =
                    ignore_fn2.as_ref().map(|f| f as &dyn Fn(u32) -> bool);
                position = match c.first_hit_hull(position, through, half, -2.0, ignore2) {
                    Some(behind) => lerp(position, through, (behind.t - 1e-3).max(0.0)),
                    None => through,
                };
                continue;
            }

            let w = velocity;
            let gate_speed = V3::new(
                w.x,
                w.y,
                w.z - step_g * (PHYSICS_SUBSTEPS - 1 - step) as f32,
            )
            .length();
            let v_after = bounce(w, hit.normal, k, gate_speed);
            if let Some(bt) = trace.bounces.as_deref_mut() {
                bt.push(BounceRecord {
                    tick,
                    contact,
                    normal: hit.normal,
                    triangle: hit.triangle,
                    vel_before: w,
                    vel_after: v_after,
                });
            }
            if let Some(at) = det.on_contact(tick, contact, hit.normal, w, v_after) {
                return TrajectoryResult {
                    rest: at,
                    bounces,
                    flight_time: time + TIME_STEP,
                    lost: false,
                    first_touch,
                    glass_breaks,
                    ticks: tick + 1,
                    end: EndReason::Detonated,
                };
            }

            let supported = hit.normal.z > FLOOR_NORMAL_Z
                || c.first_hit_hull(
                    position,
                    position + V3::new(0.0, 0.0, -2.0),
                    half,
                    FLOOR_NORMAL_Z,
                    None,
                )
                .is_some();
            if v_after.length() < k.stop_speed && supported {
                if !k.edge_tipping {
                    return TrajectoryResult {
                        rest: position,
                        bounces,
                        flight_time: time + TIME_STEP,
                        lost: false,
                        first_touch,
                        glass_breaks,
                        ticks: tick + 1,
                        end: EndReason::Rest,
                    };
                }
                match edge_tip(c, position, w) {
                    None => {
                        return TrajectoryResult {
                            rest: position,
                            bounces,
                            flight_time: time + TIME_STEP,
                            lost: false,
                            first_touch,
                            glass_breaks,
                            ticks: tick + 1,
                            end: EndReason::Rest,
                        };
                    }
                    Some(tip) => {
                        velocity = tip * (k.stop_speed * 0.3);
                        velocity.z -= step_g * (1.0 - hit.t);
                        position = position + velocity * ((1.0 - hit.t) * step_dt);
                        continue;
                    }
                }
            }

            velocity = v_after;
            let mut remainder = 1.0 - hit.t;
            let mut next2 = position + v_after * (remainder * step_dt);
            let mut sub = 1;
            loop {
                let ignore_fn3 = glass_ignore(c, &broken);
                let ignore3: Option<&dyn Fn(u32) -> bool> =
                    ignore_fn3.as_ref().map(|f| f as &dyn Fn(u32) -> bool);
                let hit2 = match c.first_hit_hull(position, next2, half, -2.0, ignore3) {
                    None => {
                        position = next2;
                        break;
                    }
                    Some(h) => h,
                };
                position = lerp(position, next2, (hit2.t - 1e-3).max(0.0));
                remainder *= 1.0 - hit2.t;
                if sub >= k.bounces_per_tick || remainder <= 1e-4 {
                    break;
                }
                bounces += 1;
                let w2 = velocity;
                let gate2 = V3::new(
                    w2.x,
                    w2.y,
                    w2.z - step_g * (PHYSICS_SUBSTEPS - 1 - step) as f32,
                )
                .length();
                velocity = bounce(w2, hit2.normal, k, gate2);
                if let Some(bt) = trace.bounces.as_deref_mut() {
                    bt.push(BounceRecord {
                        tick,
                        contact: position,
                        normal: hit2.normal,
                        triangle: hit2.triangle,
                        vel_before: w2,
                        vel_after: velocity,
                    });
                }
                if let Some(at) = det.on_contact(tick, position, hit2.normal, w2, velocity) {
                    return TrajectoryResult {
                        rest: at,
                        bounces,
                        flight_time: time + TIME_STEP,
                        lost: false,
                        first_touch,
                        glass_breaks,
                        ticks: tick + 1,
                        end: EndReason::Detonated,
                    };
                }
                next2 = position + velocity * (remainder * step_dt);
                sub += 1;
            }
        }
        time += TIME_STEP;
        tick += 1;
        if let Some(t) = trace.ticks.as_deref_mut() {
            t.push((position, velocity));
        }
        if let Some(at) = det.on_tick(tick, position, velocity) {
            return TrajectoryResult {
                rest: at,
                bounces,
                flight_time: time,
                lost: false,
                first_touch,
                glass_breaks,
                ticks: tick,
                end: EndReason::Detonated,
            };
        }
    }
    TrajectoryResult {
        rest: position,
        bounces,
        flight_time: time,
        lost: true,
        first_touch,
        glass_breaks,
        ticks: tick,
        end: EndReason::Timeout,
    }
}

/// `GrenadeTrajectory.cs:580-610` (`FindContact`), against a `VoxelGrid`.
fn find_contact_voxel(grid: &VoxelGrid, free: V3, solid: V3) -> (V3, i32) {
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..8 {
        let mid = (lo + hi) / 2.0;
        let p = lerp(free, solid, mid);
        let (x, y, z) = grid.cell_of(p);
        if grid.in_bounds(x, y, z) && grid.is_solid(grid.index(x, y, z)) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let contact = lerp(free, solid, lo);
    let (fx, fy, _) = grid.cell_of(contact);
    let (sx, sy, _) = grid.cell_of(lerp(free, solid, hi));
    if sx != fx {
        (contact, 0)
    } else if sy != fy {
        (contact, 1)
    } else {
        (contact, 2)
    }
}

/// `GrenadeTrajectory.cs:612-616` (`HasGroundBelow`), against a `VoxelGrid`.
fn has_ground_below_voxel(grid: &VoxelGrid, p: V3) -> bool {
    let (x, y, z) = grid.cell_of(p);
    grid.in_bounds(x, y, z - 1) && grid.is_solid(grid.index(x, y, z - 1))
}

/// `GrenadeTrajectory.cs:202-263` (`Simulate`): the coarse stage-1 voxel
/// model, one physics step per tick (no sub-stepping).
pub fn simulate_voxel(grid: &VoxelGrid, spec: &ThrowSpec, k: &ThrowConstants) -> TrajectoryResult {
    let (mut position, mut velocity) = derive_initial(spec, k);
    let gravity_step = BASE_GRAVITY * k.gravity_scale * TIME_STEP;
    let mut bounces: u32 = 0;
    let mut time: f32 = 0.0;
    let mut tick: u32 = 0;
    let mut first_touch: Option<V3> = None;

    while time < MAX_FLIGHT_SECONDS {
        let vz_old = velocity.z;
        velocity.z -= gravity_step;
        let next =
            position + V3::new(velocity.x, velocity.y, (vz_old + velocity.z) * 0.5) * TIME_STEP;

        let (cx, cy, cz) = grid.cell_of(next);
        if cx < 0 || cx >= grid.nx || cy < 0 || cy >= grid.ny || cz < 0 {
            return TrajectoryResult {
                rest: next,
                bounces,
                flight_time: time,
                lost: true,
                first_touch,
                glass_breaks: 0,
                ticks: tick,
                end: EndReason::Lost,
            };
        }
        if cz >= grid.nz {
            position = next;
            time += TIME_STEP;
            tick += 1;
            continue;
        }
        if grid.is_solid(grid.index(cx, cy, cz)) {
            let (contact, axis) = find_contact_voxel(grid, position, next);
            let pre_impact = velocity;
            position = contact;
            if first_touch.is_none() {
                first_touch = Some(contact);
            }
            bounces += 1;
            velocity = match axis {
                0 => V3::new(-velocity.x, velocity.y, velocity.z),
                1 => V3::new(velocity.x, -velocity.y, velocity.z),
                _ => V3::new(velocity.x, velocity.y, -velocity.z),
            };
            let speed = pre_impact.length();
            let u = if speed > 1e-6 {
                pre_impact.z.abs() / speed
            } else {
                0.0
            };
            velocity = velocity * (k.elasticity * floor_impact_damp(speed, u, axis == 2, k));
            if axis == 2
                && velocity.length() < k.stop_speed
                && has_ground_below_voxel(grid, position)
            {
                return TrajectoryResult {
                    rest: position,
                    bounces,
                    flight_time: time,
                    lost: false,
                    first_touch,
                    glass_breaks: 0,
                    ticks: tick,
                    end: EndReason::Rest,
                };
            }
        } else {
            position = next;
        }
        time += TIME_STEP;
        tick += 1;
    }
    let lost = !has_ground_below_voxel(grid, position);
    TrajectoryResult {
        rest: position,
        bounces,
        flight_time: time,
        lost,
        first_touch,
        glass_breaks: 0,
        ticks: tick,
        end: if lost {
            EndReason::Lost
        } else {
            EndReason::Timeout
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    fn flat_plane_mesh(half: f32, z: f32, breakable: bool) -> CollisionMesh {
        let mut mesh = CollisionMesh::new();
        let attr = mesh
            .add_attribute(CollisionAttribute {
                name: if breakable {
                    "EntityBreakable".to_string()
                } else {
                    "Default".to_string()
                },
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
        mesh
    }

    fn plane_grid(z: f32) -> UniformGrid {
        let mesh = flat_plane_mesh(2000.0, z, false);
        let mask = all_mask(&mesh);
        UniformGrid::build(&mesh, &mask, None, 128.0).unwrap()
    }

    #[test]
    fn floor_impact_damp_gates_on_speed_angle_and_surface() {
        let k = ThrowConstants::default();
        // Below the gate speed: never damps, even steep and floor.
        assert_eq!(floor_impact_damp(689.0, 0.9, true, &k), 1.0);
        // Above the gate, shallow angle (u <= 0.5): no damp.
        assert_eq!(floor_impact_damp(700.0, 0.5, true, &k), 1.0);
        // Above the gate, steep, floor: damped by 1.5 - u.
        let u = 0.9f32;
        assert_eq!(floor_impact_damp(700.0, u, true, &k), 1.5 - u);
        // Above the gate, steep, but a WALL: never damps.
        assert_eq!(floor_impact_damp(700.0, u, false, &k), 1.0);
    }

    #[test]
    fn bounce_reflects_and_snaps_epsilon() {
        let k = ThrowConstants::default();
        let w = V3::new(0.0, 0.0, -300.0);
        let normal = V3::new(0.0, 0.0, 1.0);
        let v = bounce(w, normal, &k, w.length());
        // Straight-down onto a floor: reflects to straight up, scaled by
        // elasticity alone (u = 1.0 > 0.5, but speed 300 < gate 690).
        assert!((v.x).abs() < 1e-6);
        assert!((v.y).abs() < 1e-6);
        assert!((v.z - 300.0 * k.elasticity).abs() < 1e-3);
    }

    #[test]
    fn bounce_applies_floor_damp_gate_above_690() {
        let k = ThrowConstants::default();
        // Straight-down onto a floor at 800 u/s: speed 800 > DampGateSpeed
        // 690 and u = 1.0 > 0.5, so the floor-angle damp (1.5 - u = 0.5)
        // multiplies elasticity on top of the plain reflection. Confirmed
        // exact via a standalone Rust call to this crate's `bounce` (no
        // floating rounding ambiguity: every intermediate value here is
        // exactly representable).
        let w = V3::new(0.0, 0.0, -800.0);
        let normal = V3::new(0.0, 0.0, 1.0);
        let v = bounce(w, normal, &k, w.length());
        assert_eq!(v, V3::new(0.0, 0.0, 180.0));
    }

    #[test]
    fn drop_rests_on_flat_plane_at_2_plus_backoff() {
        let grid = plane_grid(0.0);
        let k = ThrowConstants::default();
        // Drop from directly above with a small lateral speed so it does not
        // sit exactly on a grid-cell seam.
        let pos = V3::new(1.0, 1.0, 500.0);
        let vel = V3::new(5.0, 0.0, 0.0);
        let result = simulate_exact_raw(&grid, pos, vel, &k, Trace::default());
        assert!(!result.lost, "should settle, not fly off forever");
        // GrenadeTrajectory.cs:270-274's "2.03 above the floor" doc comment
        // is a measured LIVE-CAPTURE number (server telemetry with real
        // engine rounding/backoff noise), not this exact synthetic scenario;
        // the sim itself rests the hull centre at GrenadeRadius (2.0) plus
        // whatever ContactBackoff/Lerp remainder the last bounce happened to
        // leave, which for this exact drop (verified against the compiled
        // reference via `cs_sim` on the same flat-plane geometry) is
        // 2.0001886, not 2.03.
        assert_eq!(result.rest.to_array(), [14.323946, 1.0, 2.0001886]);
    }

    /// `GrenadeTrajectory.cs:438-448`: a contact in the first half of a tick
    /// leaves at elasticity times the HALF-STEP velocity, then takes the
    /// second half-step's gravity on top; a contact in the second half
    /// leaves at elasticity times the (already fuller) half-step velocity
    /// with no more gravity left in the tick. Two straight-down drops onto
    /// the same flat plane, released from heights that land the very first
    /// contact just before vs. just after that tick's sub-step boundary
    /// (found by scanning `simulate_exact_raw`'s own bounce trace, then
    /// cross-checked against the compiled reference via `cs_sim` on the
    /// same plane/pos/vel - both agree bit-exactly on the values below).
    #[test]
    fn two_substep_bounce_timing_differs_across_the_half_tick_boundary() {
        let grid = plane_grid(0.0);
        let k = ThrowConstants::default();
        let vel = V3::new(0.0, 0.0, -500.0);

        let mut first_half_bounces = Vec::new();
        let first_half = simulate_exact_raw(
            &grid,
            V3::new(0.0, 0.0, 5.90),
            vel,
            &k,
            Trace {
                ticks: None,
                bounces: Some(&mut first_half_bounces),
            },
        );
        let mut second_half_bounces = Vec::new();
        let second_half = simulate_exact_raw(
            &grid,
            V3::new(0.0, 0.0, 5.92),
            vel,
            &k,
            Trace {
                ticks: None,
                bounces: Some(&mut second_half_bounces),
            },
        );

        // Both land their first contact in tick 0, but on opposite sides of
        // its sub-step boundary: the pre-bounce (gravity-integrated) speed
        // and the post-bounce exit speed both differ by exactly
        // `stepG * elasticity` = 2.5 * 0.45 = 1.125 u/s, the extra half-step
        // of gravity the second-half contact carried into its own bounce.
        assert_eq!(first_half_bounces[0].vel_before.z, -502.5);
        assert_eq!(first_half_bounces[0].vel_after.z, 226.125);
        assert_eq!(second_half_bounces[0].vel_before.z, -505.0);
        assert_eq!(second_half_bounces[0].vel_after.z, 227.25);

        // Verified against the compiled reference via `cs_sim` (raw pos/vel)
        // on this exact plane: full trajectories still settle, at slightly
        // different final rest heights and flight times, since which half
        // the first bounce landed in changes every tick after it.
        assert_eq!(first_half.rest.to_array(), [0.0, 0.0, 2.000171]);
        assert_eq!(first_half.bounces, 5);
        assert_eq!(first_half.flight_time, 2.5);
        assert_eq!(second_half.rest.to_array(), [0.0, 0.0, 2.0001976]);
        assert_eq!(second_half.bounces, 5);
        assert_eq!(second_half.flight_time, 2.578125);
    }

    #[test]
    fn glass_pass_through_scales_speed_and_ignores_broken_pane_nearby() {
        // A breakable window at z=0 above a solid floor at z=-500: thrown
        // straight up through the window, it must break it once, fall back
        // through the same (now-ignored) hole, and rest on the floor.
        let mut mesh = flat_plane_mesh(20.0, 0.0, true);
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
            source_index: 1,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[
                [-1000.0, -1000.0, -200.0],
                [1000.0, -1000.0, -200.0],
                [1000.0, 1000.0, -200.0],
                [-1000.0, 1000.0, -200.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let k = ThrowConstants::default();
        // 300 u/s upward with 320 u/s^2 net gravity peaks ~140u above the
        // window, comfortably clearing it before falling back through the
        // same (ignored) hole onto the floor.
        let pos = V3::new(0.0, 0.0, -10.0);
        let vel = V3::new(0.0, 0.0, 300.0);
        let result = simulate_exact_raw(&grid, pos, vel, &k, Trace::default());
        // Verified against the compiled reference via `cs_sim` on this exact
        // geometry/pos/vel.
        assert_eq!(result.glass_breaks, 1);
        assert!(!result.lost, "should fall back through the hole and land");
        assert_eq!(
            result.rest.to_array(),
            [0.0, 0.0, -197.9997],
            "rest = {:?}",
            result.rest
        );
        assert_eq!(result.bounces, 5);
        assert_eq!(result.flight_time, 3.359375);
    }

    #[test]
    fn voxel_simulate_lost_when_flying_past_grid_bounds() {
        use geom::math::Aabb;
        use geom::voxel::VoxelGrid;
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-64.0, -64.0, -64.0),
            max: V3::new(64.0, 64.0, 64.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let spec = ThrowSpec {
            eye: V3::new(0.0, 0.0, 0.0),
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            throw_type: crate::throw::ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let k = ThrowConstants::default();
        let result = simulate_voxel(&grid, &spec, &k);
        assert!(result.lost);
    }

    #[test]
    fn voxel_simulate_lands_on_a_floor() {
        use geom::math::Aabb;
        use geom::voxel::VoxelGrid;
        let mesh = flat_plane_mesh(2000.0, 0.0, false);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-64.0, -64.0, -8.0),
            max: V3::new(64.0, 64.0, 512.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let spec = ThrowSpec {
            eye: V3::new(0.0, 0.0, 200.0),
            yaw_deg: 0.0,
            pitch_deg: 90.0,
            throw_type: crate::throw::ThrowType::Stand,
            strength: 0.0,
            run_yaw_offset_deg: 0.0,
        };
        let k = ThrowConstants::default();
        let result = simulate_voxel(&grid, &spec, &k);
        assert!(!result.lost, "should land on the floor, not fly off");
        assert!(result.bounces >= 1);
        assert!(result.rest.z > 0.0 && result.rest.z < 32.0);
    }

    /// A minimal fuse-timer `Detonation` firing on `on_tick`, plus
    /// `SmokeRest`'s own zero-effect default methods, to prove the seam
    /// short-circuits the flight and reports `EndReason::Detonated` without
    /// touching the plain `simulate_exact_raw` path (proven bit-identical by
    /// every other test/golden-fixture in this crate).
    struct FuseAfterTicks(u32);
    impl Detonation for FuseAfterTicks {
        fn on_tick(&mut self, tick: u32, pos: V3, _vel: V3) -> Option<V3> {
            (tick >= self.0).then_some(pos)
        }
    }

    #[test]
    fn detonation_on_tick_short_circuits_the_flight() {
        let grid = plane_grid(-10_000.0); // far below: never actually lands
        let k = ThrowConstants::default();
        let mut fuse = FuseAfterTicks(3);
        let result = simulate_exact_raw_with(
            &grid,
            V3::new(0.0, 0.0, 500.0),
            V3::new(0.0, 0.0, 0.0),
            &k,
            Trace::default(),
            &mut fuse,
        );
        assert_eq!(result.end, EndReason::Detonated);
        assert_eq!(result.ticks, 3);
        assert_eq!(result.flight_time, 3.0 * TIME_STEP);
        assert!(!result.lost);
    }

    #[test]
    fn smoke_rest_never_detonates() {
        let grid = plane_grid(0.0);
        let k = ThrowConstants::default();
        let mut rest = SmokeRest;
        let result = simulate_exact_raw_with(
            &grid,
            V3::new(1.0, 1.0, 500.0),
            V3::new(5.0, 0.0, 0.0),
            &k,
            Trace::default(),
            &mut rest,
        );
        assert_eq!(result.end, EndReason::Rest);
    }
}
