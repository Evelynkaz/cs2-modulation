//! Origin candidates: where a player can stand. Ported from
//! `cs2-smoke-solver/src/Solver/LineupSolver.Origins.cs` (whole file).

use std::collections::HashSet;

use geom::collider::Collider;
use geom::math::V3;
use geom::voxel::VoxelGrid;
use rayon::prelude::*;

use crate::standspots::{
    CROUCH_HEIGHT, HULL_HALF_WIDTH, STANDABLE_NORMAL_Z, STANDING_HEIGHT, STEP_HEIGHT, Stance,
    float_lerp, point_in_polygon, stance_at, supported_heights,
};

/// Support probe for [`exact_origin_only`]'s "is anything under the given
/// seed" check. `LineupSolver.Origins.cs:513`.
const SUPPORT_PROBE: f32 = 4.0;

fn area_z(corners: &[V3]) -> f32 {
    (corners.iter().map(|c| c.z as f64).sum::<f64>() / corners.len() as f64) as f32
}

fn area_bounds_xy(corners: &[V3]) -> (f32, f32, f32, f32) {
    let min_x = corners.iter().map(|c| c.x).fold(f32::INFINITY, f32::min);
    let max_x = corners
        .iter()
        .map(|c| c.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = corners.iter().map(|c| c.y).fold(f32::INFINITY, f32::min);
    let max_y = corners
        .iter()
        .map(|c| c.y)
        .fold(f32::NEG_INFINITY, f32::max);
    (min_x, max_x, min_y, max_y)
}

/// Feet positions sampled from nav-mesh walkable areas: reachable by
/// definition, unlike raw geometry scanning which happily stands players on
/// rooftops. `LineupSolver.Origins.cs:16-70`.
pub fn origins_from_nav_areas(
    grid: &VoxelGrid,
    area_corners: &[Vec<V3>],
    min: V3,
    max: V3,
    sample_step: f32,
    collider: Option<&dyn Collider>,
) -> Vec<V3> {
    let mut origins: Vec<V3> = Vec::new();
    for corners in area_corners {
        let (min_x, max_x, min_y, max_y) = area_bounds_xy(corners);
        if max_x < min.x || min_x > max.x || max_y < min.y || min_y > max.y {
            continue;
        }
        let avg_z = area_z(corners);
        if avg_z < min.z || avg_z > max.z {
            continue;
        }
        let count_before = origins.len();
        let mut x = (min_x / sample_step).ceil() * sample_step;
        while x <= max_x {
            let mut y = (min_y / sample_step).ceil() * sample_step;
            while y <= max_y {
                if x >= min.x
                    && x <= max.x
                    && y >= min.y
                    && y <= max.y
                    && point_in_polygon(corners, x, y)
                {
                    origins.push(snap_to_ground(grid, collider, V3::new(x, y, avg_z)));
                }
                y += sample_step;
            }
            x += sample_step;
        }
        // Tiny areas can miss every grid sample; keep their centroid so narrow
        // ledges and stair areas still contribute origins.
        if origins.len() == count_before {
            let cx =
                (corners.iter().map(|c| c.x as f64).sum::<f64>() / corners.len() as f64) as f32;
            let cy =
                (corners.iter().map(|c| c.y as f64).sum::<f64>() / corners.len() as f64) as f32;
            if cx >= min.x && cx <= max.x && cy >= min.y && cy <= max.y {
                origins.push(snap_to_ground(grid, collider, V3::new(cx, cy, avg_z)));
            }
        }
    }
    add_elevated_origins(
        grid,
        area_corners,
        min,
        max,
        sample_step,
        collider,
        &mut origins,
    );
    if let Some(collider) = collider {
        add_pinned_origins(grid, collider, &mut origins, None);
    }
    origins
}

/// How much higher than the seeding nav area an elevated surface may sit.
const ELEVATED_MIN_RISE: f32 = 20.0;
const ELEVATED_MAX_RISE: f32 = 65.0;
/// How far outside a nav area the elevated surface may sit and still be
/// steppable/jumpable onto from it.
const ELEVATED_REACH: f32 = 48.0;

/// Enough of the 32x32 player hull is supported at this height to stand on.
/// `LineupSolver.Origins.cs:115-132`.
fn fits_player_hull(grid: &VoxelGrid, cx: i32, cy: i32, k: i32) -> bool {
    let mut supported = 0;
    for dx in -1..=1 {
        for dy in -1..=1 {
            let (nx, ny) = (cx + dx, cy + dy);
            if grid.in_bounds(nx, ny, k)
                && grid.is_solid(grid.index(nx, ny, k - 1))
                && !grid.is_solid(grid.index(nx, ny, k))
            {
                supported += 1;
            }
        }
    }
    supported >= 5
}

/// Recovers stand spots the nav mesh cannot see because bots never jump onto
/// things: crates, platforms and ledges within a crouch-jump of a nav area.
/// `LineupSolver.Origins.cs:134-236`.
#[allow(clippy::too_many_arguments)]
fn add_elevated_origins(
    grid: &VoxelGrid,
    area_corners: &[Vec<V3>],
    min: V3,
    max: V3,
    sample_step: f32,
    collider: Option<&dyn Collider>,
    origins: &mut Vec<V3>,
) {
    let mut seen: HashSet<(i32, i32, i32)> = HashSet::new();
    let mut elevated: Vec<V3> = Vec::new();
    for corners in area_corners {
        let avg_z = area_z(corners);
        if avg_z < min.z || avg_z > max.z {
            continue;
        }
        let (raw_min_x, raw_max_x, raw_min_y, raw_max_y) = area_bounds_xy(corners);
        let min_x = (raw_min_x - ELEVATED_REACH).max(min.x);
        let max_x = (raw_max_x + ELEVATED_REACH).min(max.x);
        let min_y = (raw_min_y - ELEVATED_REACH).max(min.y);
        let max_y = (raw_max_y + ELEVATED_REACH).min(max.y);

        let mut x = (min_x / sample_step).ceil() * sample_step;
        while x <= max_x {
            let mut y = (min_y / sample_step).ceil() * sample_step;
            while y <= max_y {
                try_elevated_column(grid, collider, x, y, avg_z, &mut seen, &mut elevated);
                y += sample_step;
            }
            x += sample_step;
        }
    }
    origins.extend(elevated);
}

#[allow(clippy::too_many_arguments)]
fn try_elevated_column(
    grid: &VoxelGrid,
    collider: Option<&dyn Collider>,
    x: f32,
    y: f32,
    avg_z: f32,
    seen: &mut HashSet<(i32, i32, i32)>,
    elevated: &mut Vec<V3>,
) {
    let (cx, cy, _) = grid.cell_of(V3::new(x, y, avg_z));
    // Widened by a voxel at each end because the grid can only place a floor
    // on a cell boundary - up to a full voxel above the real surface.
    let (_, _, mut k_lo) =
        grid.cell_of(V3::new(x, y, avg_z + ELEVATED_MIN_RISE - grid.voxel_size()));
    let (_, _, mut k_hi) =
        grid.cell_of(V3::new(x, y, avg_z + ELEVATED_MAX_RISE + grid.voxel_size()));
    k_lo = k_lo.max(1);
    k_hi = k_hi.min(grid.nz - 6);
    if !grid.in_bounds(cx, cy, k_lo) {
        return;
    }
    for k in k_lo..=k_hi {
        // Standing room: floor below, free here, and a player's height of
        // clearance above.
        if !grid.is_solid(grid.index(cx, cy, k - 1)) || grid.is_solid(grid.index(cx, cy, k)) {
            continue;
        }
        let mut headroom = true;
        for h in 1..=5 {
            if grid.is_solid(grid.index(cx, cy, k + h)) {
                headroom = false;
                break;
            }
        }
        if !headroom {
            continue;
        }
        if !fits_player_hull(grid, cx, cy, k) {
            continue;
        }
        let floor_z = grid.cell_center_xyz(cx, cy, k).z - grid.voxel_size() / 2.0;
        // Demand an actual player-standable surface at the snapped height
        // instead of assuming the voxel grid's cell-quantized one.
        let Some(collider) = collider else {
            continue;
        };
        let probe_top = V3::new(x, y, floor_z + grid.voxel_size());
        let probe_bottom = V3::new(x, y, floor_z - grid.voxel_size());
        let Some(floor_hit) = collider.first_hit_ray(probe_top, probe_bottom) else {
            continue;
        };
        if floor_hit.normal.z < STANDABLE_NORMAL_Z {
            continue;
        }
        let feet = V3::new(x, y, float_lerp(probe_top.z, probe_bottom.z, floor_hit.t));
        let rise = feet.z - avg_z;
        if rise < ELEVATED_MIN_RISE || rise > ELEVATED_MAX_RISE {
            continue;
        }
        if seen.insert((
            (x / 8.0).round_ties_even() as i32,
            (y / 8.0).round_ties_even() as i32,
            k,
        )) {
            elevated.push(feet);
        }
        break; // the lowest reachable surface in this column is the one stood on
    }
}

/// The CS2 player hull is 32x32; feet pressed against a wall sit exactly
/// this far from its plane. `LineupSolver.Origins.cs:241`.
const PLAYER_HALF_WIDTH: f32 = 16.0;
/// How far from a nav sample a wall still counts as "walk into it" range.
const WALL_PROBE_RANGE: f32 = 64.0;
/// A surface steeper than this is a wall for pinning purposes.
const WALL_NORMAL_MAX_Z: f32 = 0.35;
/// Waist-height probe, above step height so a step is not mistaken for a wall.
const LOW_PROBE_HEIGHT: f32 = 22.0;
const PROBE_HEIGHTS: [f32; 3] = [LOW_PROBE_HEIGHT, 36.0, 46.0];

fn probe_dir(i: i32) -> V3 {
    let a = (i as f32) * std::f32::consts::PI / 4.0;
    V3::new(a.cos(), a.sin(), 0.0)
}

/// The wall planes within walk-into range of `feet`, deduped across probe
/// rays and heights, as `(Normal.xy, PlaneD)` (`N.x = d` Hesse form).
/// `LineupSolver.Origins.cs:265-300`.
fn nearby_wall_planes(collider: &dyn Collider, feet: V3, range: f32) -> Vec<((f32, f32), f32)> {
    let mut walls: Vec<((f32, f32), f32)> = Vec::new();
    for &height in &PROBE_HEIGHTS {
        let probe = feet + V3::new(0.0, 0.0, height);
        for i in 0..8 {
            let dir = probe_dir(i);
            let Some(hit) = collider.first_hit_ray(probe, probe + dir * range) else {
                continue;
            };
            if hit.normal.z.abs() > WALL_NORMAL_MAX_Z {
                continue;
            }
            let len = (hit.normal.x * hit.normal.x + hit.normal.y * hit.normal.y).sqrt();
            if len < 0.8 {
                continue;
            }
            let n = (hit.normal.x / len, hit.normal.y / len);
            let hit_point = probe + dir * (hit.t * range);
            if height <= LOW_PROBE_HEIGHT && climbable(collider, feet, dir, hit.t * range) {
                continue;
            }
            let d = n.0 * hit_point.x + n.1 * hit_point.y;
            if walls
                .iter()
                .any(|w: &((f32, f32), f32)| w.0.0 * n.0 + w.0.1 * n.1 > 0.9)
            {
                continue; // same wall seen from a neighboring probe
            }
            walls.push((n, d));
        }
    }
    walls
}

/// Whether walking into the low obstacle the probe hit simply climbs it.
/// `LineupSolver.Origins.cs:306-324`.
fn climbable(collider: &dyn Collider, feet: V3, dir: V3, hit_distance: f32) -> bool {
    let short_of = feet + dir * (hit_distance - PLAYER_HALF_WIDTH - 1.0);
    let floor = match floor_under_hull(collider, short_of, STEP_HEIGHT * 2.0, STEP_HEIGHT * 2.0) {
        Some(f) if f <= feet.z + STEP_HEIGHT => f,
        _ => feet.z,
    };
    let pushed = feet + dir * (hit_distance - PLAYER_HALF_WIDTH + 2.0);
    let lifted = V3::new(pushed.x, pushed.y, floor + STEP_HEIGHT);
    let half = V3::new(
        PLAYER_HALF_WIDTH - 0.5,
        PLAYER_HALF_WIDTH - 0.5,
        CROUCH_HEIGHT / 2.0 - 0.5,
    );
    !collider.box_intersects(lifted + V3::new(0.0, 0.0, CROUCH_HEIGHT / 2.0), half)
}

/// Adds wall- and corner-pinned variants of `origins` in place.
/// `LineupSolver.Origins.cs:337-338`.
pub fn add_pinned_origins_to(
    grid: &VoxelGrid,
    collider: &dyn Collider,
    origins: &mut Vec<V3>,
    crouch_only_out: Option<&mut Vec<V3>>,
) {
    add_pinned_origins(grid, collider, origins, crouch_only_out);
}

/// `LineupSolver.Origins.cs:340-461`. Three passes matching the reference:
/// parallel wall-plane proposals, a serial first-come 4u dedupe (so origin
/// order decides which pin wins a cell), then a parallel re-seat onto the
/// real floor.
fn add_pinned_origins(
    grid: &VoxelGrid,
    collider: &dyn Collider,
    origins: &mut Vec<V3>,
    mut crouch_only_out: Option<&mut Vec<V3>>,
) {
    let proposals: Vec<Vec<(f32, f32)>> = origins
        .par_iter()
        .map(|&feet| {
            let walls = nearby_wall_planes(collider, feet, WALL_PROBE_RANGE);
            let mut xy: Vec<(f32, f32)> = Vec::new();
            let feet_xy = (feet.x, feet.y);
            for &(n, d) in &walls {
                let dist = n.0 * feet_xy.0 + n.1 * feet_xy.1 - d;
                if dist > PLAYER_HALF_WIDTH + 0.5 {
                    xy.push((
                        feet_xy.0 - n.0 * (dist - PLAYER_HALF_WIDTH),
                        feet_xy.1 - n.1 * (dist - PLAYER_HALF_WIDTH),
                    ));
                }
            }
            for a1 in 0..walls.len() {
                for b1 in (a1 + 1)..walls.len() {
                    let (a, da) = walls[a1];
                    let (b, db) = walls[b1];
                    let det = a.0 * b.1 - a.1 * b.0;
                    if (a.0 * b.0 + a.1 * b.1).abs() > 0.5 || det.abs() < 0.3 {
                        continue; // not corner-like
                    }
                    let ra = da + PLAYER_HALF_WIDTH;
                    let rb = db + PLAYER_HALF_WIDTH;
                    xy.push(((ra * b.1 - rb * a.1) / det, (rb * a.0 - ra * b.0) / det));
                }
            }
            xy
        })
        .collect();

    let mut seen: HashSet<(i32, i32)> = origins
        .iter()
        .map(|o| {
            (
                (o.x / 4.0).round_ties_even() as i32,
                (o.y / 4.0).round_ties_even() as i32,
            )
        })
        .collect();
    let mut accepted: Vec<(V3, (f32, f32))> = Vec::new();
    for (i, &base_feet) in origins.iter().enumerate() {
        for &xy in &proposals[i] {
            let (dx, dy) = (xy.0 - base_feet.x, xy.1 - base_feet.y);
            let dist = (dx * dx + dy * dy).sqrt();
            let key = (
                (xy.0 / 4.0).round_ties_even() as i32,
                (xy.1 / 4.0).round_ties_even() as i32,
            );
            if dist > WALL_PROBE_RANGE + PLAYER_HALF_WIDTH || !seen.insert(key) {
                continue;
            }
            accepted.push((base_feet, xy));
        }
    }

    let seated: Vec<Option<(V3, Stance)>> = accepted
        .par_iter()
        .map(|&(base_feet, xy)| {
            let snapped = snap_to_ground(grid, Some(collider), V3::new(xy.0, xy.1, base_feet.z));
            let on_floor = hull_rest_height(
                collider,
                snapped,
                grid.voxel_size() * 2.0,
                grid.voxel_size(),
            )?;
            let stance = stance_at(collider, on_floor);
            if stance == Stance::None {
                return None;
            }
            // Sanity-check torso height, not ankle height.
            let (cx, cy, cz) = grid.cell_of(on_floor + V3::new(0.0, 0.0, grid.voxel_size() * 1.5));
            if grid.in_bounds(cx, cy, cz) && !grid.is_solid(grid.index(cx, cy, cz)) {
                Some((on_floor, stance))
            } else {
                None
            }
        })
        .collect();
    for pin in seated.into_iter().flatten() {
        origins.push(pin.0);
        if pin.1 == Stance::Crouching
            && let Some(out) = crouch_only_out.as_mut()
        {
            out.push(pin.0);
        }
    }
}

/// The height a player's feet rest at over `at`: the highest surface under
/// the whole 32x32 hull footprint, not under its centre alone.
/// `LineupSolver.Origins.cs:490-508`.
pub fn floor_under_hull(collider: &dyn Collider, at: V3, up: f32, down: f32) -> Option<f32> {
    const HALF: f32 = HULL_HALF_WIDTH;
    let mut best: Option<f32> = None;
    for &(dx, dy) in &[
        (0.0, 0.0),
        (HALF, HALF),
        (HALF, -HALF),
        (-HALF, HALF),
        (-HALF, -HALF),
    ] {
        let from = V3::new(at.x + dx, at.y + dy, at.z + up);
        let to = V3::new(at.x + dx, at.y + dy, at.z - down);
        if let Some(hit) = collider.first_hit_ray(from, to) {
            let z = from.z + (to.z - from.z) * hit.t;
            if best.is_none_or(|b| z > b) {
                best = Some(z);
            }
        }
    }
    best
}

/// The feet height the player hull actually rests at in this column,
/// nearest to `seed`'s own, or `None` when no floor within a step of it
/// holds the hull up. `LineupSolver.Origins.cs:527-541`.
fn hull_rest_height(collider: &dyn Collider, seed: V3, down: f32, up: f32) -> Option<V3> {
    let heights = supported_heights(
        collider,
        seed.x,
        seed.y,
        seed.z - down,
        seed.z + up + STANDING_HEIGHT,
        8,
    );
    let mut best: Option<f32> = None;
    for h in heights {
        let within = h >= seed.z - down && h <= seed.z + up;
        if within && best.is_none_or(|b| (h - seed.z).abs() < (b - seed.z).abs()) {
            best = Some(h);
        }
    }
    best.map(|z| V3::new(seed.x, seed.y, z))
}

/// The one origin a player standing at `seed` actually occupies - snapped to
/// the ground under it and checked against the player hull - or empty when
/// nobody can stand there. `LineupSolver.Origins.cs:543-584`.
pub fn exact_origin_only(
    grid: &VoxelGrid,
    collider: Option<&dyn Collider>,
    seed: V3,
    mut crouch_only_out: Option<&mut Vec<V3>>,
) -> Vec<V3> {
    let Some(collider) = collider else {
        return vec![snap_to_ground(grid, None, seed)];
    };
    let supported = collider
        .first_hit_ray(
            seed + V3::new(0.0, 0.0, SUPPORT_PROBE),
            seed - V3::new(0.0, 0.0, SUPPORT_PROBE),
        )
        .is_some();
    let mut candidates: Vec<V3> = Vec::new();
    if supported {
        candidates.push(seed);
    }
    if let Some(resting) = hull_rest_height(collider, seed, STEP_HEIGHT, STEP_HEIGHT) {
        candidates.push(resting);
    }
    candidates.push(snap_to_ground(grid, Some(collider), seed));
    for candidate in candidates {
        let stance = stance_at(collider, candidate);
        if stance == Stance::None {
            continue;
        }
        if stance == Stance::Crouching
            && let Some(out) = crouch_only_out.as_mut()
        {
            out.push(candidate);
        }
        return vec![candidate];
    }
    Vec::new()
}

/// A user-named stand spot, taken literally: the seed itself ground-snapped,
/// plus its wall/corner-pinned variants. `LineupSolver.Origins.cs:586-625`.
pub fn exact_origin_with_pins(
    grid: &VoxelGrid,
    collider: Option<&dyn Collider>,
    seed: V3,
    mut crouch_only_out: Option<&mut Vec<V3>>,
) -> Vec<V3> {
    let mut snapped = snap_to_ground(grid, collider, seed);
    let Some(collider) = collider else {
        return vec![snapped];
    };
    let mut list: Vec<V3> = Vec::new();
    if let Some(resting) = hull_rest_height(collider, seed, STEP_HEIGHT, STEP_HEIGHT)
        && stance_at(collider, resting) != Stance::None
    {
        snapped = resting;
    }
    let stance = stance_at(collider, snapped);
    if stance != Stance::None {
        list.push(snapped);
        if stance == Stance::Crouching
            && let Some(out) = crouch_only_out.as_mut()
        {
            out.push(snapped);
        }
    }
    let mut pinned = vec![snapped];
    // Not `as_deref_mut()`: that would coerce to `Option<&mut [V3]>`, and
    // `add_pinned_origins` needs `Vec::push` on the far end.
    #[allow(clippy::option_as_ref_deref)]
    let reborrowed = crouch_only_out.as_mut().map(|v| &mut **v);
    add_pinned_origins(grid, collider, &mut pinned, reborrowed);
    list.extend(pinned.into_iter().skip(1));
    list
}

/// How geometry pins a lineup's stand spot: 2 = wedged into a corner, 1 =
/// pressed against one wall, 0 = open ground. `LineupSolver.Origins.cs:631-632`.
pub fn position_pin(collider: &dyn Collider, feet: V3) -> i32 {
    position_stance(collider, feet).0
}

/// How far past touching a wall may sit and still count as pressed against it.
const TOUCH_SLACK: f32 = 1.5;
/// How far out a non-touching wall is still worth reporting.
const WALL_NOTICE_RANGE: f32 = 16.0;

/// The stand spot's relationship to nearby walls: the pin class, and the gap
/// from the player's shoulder to the nearest wall when it is not touching
/// one. `LineupSolver.Origins.cs:657-687`.
pub fn position_stance(collider: &dyn Collider, feet: V3) -> (i32, Option<f32>) {
    let feet_xy = (feet.x, feet.y);
    let walls = nearby_wall_planes(collider, feet, PLAYER_HALF_WIDTH + WALL_NOTICE_RANGE);
    let mut touching: Vec<(f32, f32)> = Vec::new();
    let mut nearest_gap: Option<f32> = None;
    for &(n, plane_d) in &walls {
        let gap = n.0 * feet_xy.0 + n.1 * feet_xy.1 - plane_d - PLAYER_HALF_WIDTH;
        if nearest_gap.is_none_or(|b| gap < b) {
            nearest_gap = Some(gap);
        }
        if gap <= TOUCH_SLACK {
            touching.push(n);
        }
    }
    if touching.len() >= 2
        && touching.iter().any(|a| {
            touching
                .iter()
                .any(|b| a != b && (a.0 * b.0 + a.1 * b.1).abs() < 0.7)
        })
    {
        return (2, nearest_gap);
    }
    (if touching.is_empty() { 0 } else { 1 }, nearest_gap)
}

/// Drops a voxel-derived foot position onto the collision surface underneath it.
/// `LineupSolver.Origins.cs:854-871`.
fn snap_to_ground(grid: &VoxelGrid, collider: Option<&dyn Collider>, p: V3) -> V3 {
    let (x, y, z_raw) = grid.cell_of(p + V3::new(0.0, 0.0, 40.0));
    let z_clamped = z_raw.clamp(1, grid.nz - 1);
    if !grid.in_bounds(x, y, z_clamped) {
        return p;
    }
    let lower = (z_clamped - 8).max(1);
    let mut k = z_clamped;
    while k >= lower {
        if !grid.is_solid(grid.index(x, y, k)) && grid.is_solid(grid.index(x, y, k - 1)) {
            let center = grid.cell_center_xyz(x, y, k);
            return on_surface(
                grid,
                collider,
                V3::new(p.x, p.y, center.z - grid.voxel_size() / 2.0),
            );
        }
        k -= 1;
    }
    p
}

/// Ray-drops a voxel-snapped foot position onto the real collision surface
/// underneath it, matching the standable-normal test. `LineupSolver.Origins.cs:887-900`.
fn on_surface(grid: &VoxelGrid, collider: Option<&dyn Collider>, feet: V3) -> V3 {
    let Some(collider) = collider else {
        return feet;
    };
    let from = V3::new(feet.x, feet.y, feet.z + grid.voxel_size());
    let to = V3::new(feet.x, feet.y, feet.z - grid.voxel_size());
    match collider.first_hit_ray(from, to) {
        Some(hit) if hit.normal.z >= STANDABLE_NORMAL_Z => {
            V3::new(feet.x, feet.y, float_lerp(from.z, to.z, hit.t))
        }
        _ => feet,
    }
}

/// Feet positions a player can stand at: a free cell over solid ground with
/// head room, sampled every second column to keep the sweep tractable.
/// `LineupSolver.Origins.cs:906-948`.
pub fn find_standable_origins(
    grid: &VoxelGrid,
    min: V3,
    max: V3,
    collider: Option<&dyn Collider>,
) -> Vec<V3> {
    let mut origins = Vec::new();
    let (x0i, y0i, z0i) = grid.cell_of(min);
    let (x1i, y1i, z1i) = grid.cell_of(max);
    let x0 = x0i.max(0);
    let y0 = y0i.max(0);
    let z0 = z0i.max(1);
    let x1 = x1i.min(grid.nx - 1);
    let y1 = y1i.min(grid.ny - 1);
    let z1 = z1i.min(grid.nz - 6);

    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            let mut z = z0;
            while z <= z1 {
                let mut step = 1;
                if !grid.is_solid(grid.index(x, y, z - 1)) || grid.is_solid(grid.index(x, y, z)) {
                    z += step;
                    continue;
                }
                let mut headroom = true;
                for h in 1..=5 {
                    if grid.is_solid(grid.index(x, y, z + h)) {
                        headroom = false;
                        break;
                    }
                }
                if headroom {
                    let center = grid.cell_center_xyz(x, y, z);
                    origins.push(on_surface(
                        grid,
                        collider,
                        V3::new(center.x, center.y, center.z - grid.voxel_size() / 2.0),
                    ));
                    step += 4;
                }
                z += step;
            }
            x += 2;
        }
        y += 2;
    }
    origins
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::math::Aabb;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    fn quad_mesh(quads: &[[[f32; 3]; 4]]) -> CollisionMesh {
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
        for q in quads {
            let _ = mesh.push_triangles(
                q,
                &[[0, 1, 2], [0, 2, 3]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            );
        }
        mesh
    }

    fn flat_floor(z: f32, half: f32) -> [[f32; 3]; 4] {
        [
            [-half, -half, z],
            [half, -half, z],
            [half, half, z],
            [-half, half, z],
        ]
    }

    fn wall(x: f32, half_y: f32, z0: f32, z1: f32) -> [[f32; 3]; 4] {
        [
            [x, -half_y, z0],
            [x, half_y, z0],
            [x, half_y, z1],
            [x, -half_y, z1],
        ]
    }

    #[test]
    fn floor_under_hull_on_sloped_ground_rests_on_highest_probe() {
        // A ramp: z rises with x, roughly a 6% slope over a 32-wide hull.
        let mesh = quad_mesh(&[[
            [-200.0, -200.0, -20.0],
            [200.0, -200.0, 4.0],
            [200.0, 200.0, 4.0],
            [-200.0, 200.0, -20.0],
        ]]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let flat = floor_under_hull(&grid, V3::new(0.0, 0.0, 50.0), 60.0, 60.0).unwrap();
        let centre_only = {
            let from = V3::new(0.0, 0.0, 50.0);
            let to = V3::new(0.0, 0.0, -50.0);
            let hit = grid.first_hit_ray(from, to).unwrap();
            from.z + (to.z - from.z) * hit.t
        };
        assert!(flat >= centre_only);
    }

    #[test]
    fn position_pin_wall_and_open() {
        let mesh = quad_mesh(&[flat_floor(0.0, 300.0), wall(50.0, 300.0, 0.0, 100.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let (pin_open, _) = position_stance(&grid, V3::new(-100.0, 0.0, 0.0));
        assert_eq!(pin_open, 0);
        let (pin_wall, _) = position_stance(&grid, V3::new(34.0, 0.0, 0.0));
        assert_eq!(pin_wall, 1);
    }

    #[test]
    fn point_query_helpers_do_not_panic_off_grid() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0)]);
        let mask = all_mask(&mesh);
        let region = Aabb {
            min: V3::new(-200.0, -200.0, -50.0),
            max: V3::new(200.0, 200.0, 200.0),
        };
        let voxel = VoxelGrid::build(&mesh, &mask, 16.0, region).unwrap();
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let origins = find_standable_origins(&voxel, region.min, region.max, Some(&grid));
        assert!(!origins.is_empty());
    }
}
