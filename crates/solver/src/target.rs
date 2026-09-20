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

/// `TargetSolver.SolveForTarget`'s named parameters (`TargetSolver.cs:80-132`).
pub struct SolveQuery {
    pub target: V3,
    pub has_target_z: bool,
    pub origin_click: Option<[f32; 2]>,
    pub origin_z: Option<f32>,
    pub origin_reach: f32,
    pub tolerance: f32,
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
}

impl Default for SolveQuery {
    fn default() -> Self {
        SolveQuery {
            target: V3::ZERO,
            has_target_z: false,
            origin_click: None,
            origin_z: None,
            origin_reach: 300.0,
            tolerance: 80.0,
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
    let mut target = q.target;
    let has_origin = q.origin_click.is_some();
    let origin_click = q.origin_click.unwrap_or([target.x, target.y]);

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

    // Resolve target Z (`TargetSolver.cs:144-169`).
    let nav_z = if !q.has_target_z {
        nav_ground::nav_ground_z_nearby(&map.nav_areas, target.x, target.y)
    } else {
        None
    };
    if let Some(z0) = nav_z {
        target.z = z0;
    } else if !q.has_target_z {
        let probe_min = V3::new(target.x - 200.0, target.y - 200.0, mesh_min.z);
        let probe_max = V3::new(target.x + 200.0, target.y + 200.0, mesh_max.z);
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
        let (tx, ty, _) = probe_grid.cell_of(V3::new(target.x, target.y, mesh_min.z + 100.0));
        let anchor = nav_ground::nav_ground_z_within(&map.nav_areas, target.x, target.y, f32::MAX);
        target = snap_target_to_ground(&probe_grid, tx, ty, anchor)
            .unwrap_or(V3::new(target.x, target.y, 0.0));
    }

    // Region min/max (`TargetSolver.cs:171-181`).
    let min = V3::new(
        (target.x.min(origin_click[0] - q.origin_reach) - 500.0).max(mesh_min.x),
        (target.y.min(origin_click[1] - q.origin_reach) - 500.0).max(mesh_min.y),
        mesh_min.z,
    );
    let max = V3::new(
        (target.x.max(origin_click[0] + q.origin_reach) + 500.0).min(mesh_max.x),
        (target.y.max(origin_click[1] + q.origin_reach) + 500.0).min(mesh_max.y),
        (mesh_max.z + 64.0).min(target.z + 900.0),
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

    if q.has_target_z {
        target = settle_target(&collider, &grid, target);
    }

    let zone_crossings = zone::point_target_zone(&grid, target, q.tolerance);

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

    let mut origins_list: Vec<V3> = match &map.stand_spots {
        Some(spots) if !spots.is_empty() => spots
            .iter()
            .filter(|s| {
                distance2(xy(s.feet), origin_click) <= q.origin_reach
                    && s.feet.z >= min.z
                    && s.feet.z <= max.z
            })
            .map(|s| s.feet)
            .collect(),
        _ => {
            let cider: &dyn Collider = &player_collider;
            origins::origins_from_nav_areas(
                &grid,
                &map.nav_areas,
                V3::new(
                    origin_click[0] - q.origin_reach,
                    origin_click[1] - q.origin_reach,
                    mesh_min.z,
                ),
                V3::new(
                    origin_click[0] + q.origin_reach,
                    origin_click[1] + q.origin_reach,
                    max.z,
                ),
                24.0,
                Some(cider),
            )
            .into_iter()
            .filter(|o| distance2(xy(*o), origin_click) <= q.origin_reach)
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
        target: Some(target),
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
        on_pruned: on_pruned_ref,
        coverage: Some(&coverage_map),
        on_origin: hooks.on_origin,
        cancel: Some(cancel),
    };
    let candidates = sweep::solve(
        &grid,
        &zone_crossings,
        &types_list,
        &origins_list,
        &sweep_opts,
    );
    progress(Phase::Verify, candidates.len());
    if cancel.load(Ordering::Relaxed) {
        return cancelled_solve(
            target,
            origins_list.len(),
            collider,
            player_collider,
            collider_glass_gone,
        );
    }

    let verify_opts = verify::VerifyOptions {
        min_stability,
        constants: Some(k),
        aim_target: Some(target),
        tolerance: Some(q.tolerance),
        collider_glass_gone: collider_glass_gone.as_ref(),
        on_candidate: hooks.on_candidate,
        cancel: Some(cancel),
    };
    let mut verified =
        verify::verify_exact(&grid, &collider, &zone_crossings, &candidates, &verify_opts);
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
                q.tolerance,
                &types_list,
                q.strengths.as_deref(),
                Some(k),
                1.0,
                q.referee,
                None,
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
                        q.tolerance,
                        &types_list,
                        q.strengths.as_deref(),
                        Some(k),
                        2.0,
                        false,
                        Some(&missing),
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
                            q.tolerance,
                            &types_list,
                            q.strengths.as_deref(),
                            Some(k),
                            1.0,
                            false,
                            Some(&still_missing),
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
            target: V3::new(0.0, 0.0, 0.0),
            has_target_z: true,
            tolerance: 1.0,
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
}
