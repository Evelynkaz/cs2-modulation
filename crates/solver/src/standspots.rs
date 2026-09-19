//! Every position a player can actually stand, derived from the collision
//! mesh with the real player hull instead of inferred from Valve's nav mesh.
//! Ported from `cs2-smoke-solver/src/Solver/StandSpots.cs` (whole file).

use std::collections::{HashMap, HashSet, VecDeque};

use geom::collider::Collider;
use geom::math::V3;

/// Source player collision hull: 32x32x72 standing, 32x32x54 crouched.
/// `StandSpots.cs:27`.
pub const HULL_HALF_WIDTH: f32 = 16.0;
/// `StandSpots.cs:28`.
pub const STANDING_HEIGHT: f32 = 72.0;
/// `StandSpots.cs:29`.
pub const CROUCH_HEIGHT: f32 = 54.0;

/// `sv_stepsize`: how high a player walks up without jumping. `StandSpots.cs:32`.
pub const STEP_HEIGHT: f32 = 18.0;

/// `sv_standable_normal`, the same limit the grenade sim's floor test uses.
/// `StandSpots.cs:36`.
pub const STANDABLE_NORMAL_Z: f32 = sim::FLOOR_NORMAL_Z;

/// Apex of a standing jump. `StandSpots.cs:43`.
pub const JUMP_RISE: f32 = 57.0;

/// Apex of a crouch jump. TODO: calibrate - the one constant here not
/// derived from an engine value (`StandSpots.cs:46-53`).
pub const CROUCH_JUMP_RISE: f32 = 66.0;

/// Keeps the hull off the very surface it rests on. `StandSpots.cs:58`.
const SKIN_WIDTH: f32 = 0.5;

/// Vertical clearance used to step past a surface, and the minimum
/// separation between two surfaces treated as distinct. `StandSpots.cs:63`.
const SURFACE_GAP: f32 = 8.0;

/// A fall of more than this is lethal. `StandSpots.cs:71`.
pub const MAX_SAFE_FALL_HEIGHT: f32 = 210.0;

/// How far the search may wander from nav-vouched ground, in lattice steps.
/// `StandSpots.cs:86`.
pub const MAX_STEPS_FROM_NAV: i32 = 6;

/// `float.Lerp(a, b, t)`: .NET 10's `float.Lerp` computes this as a single
/// fused multiply-add, `a.mul_add(1 - t, b * t)`, not the plain two-op
/// `(a * (1 - t)) + (b * t)` (different rounding).
pub(crate) fn float_lerp(a: f32, b: f32, t: f32) -> f32 {
    a.mul_add(1.0 - t, b * t)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    None,
    Standing,
    Crouching,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    pub feet: V3,
    pub stance: Stance,
    pub nav_covered: bool,
}

/// Can the player hull occupy this spot, and what stance does it need?
/// Standing is tried first: it is worth more than a spot that must be
/// crouched on, since the eye height (and the whole throw) differs.
/// `StandSpots.cs:98-111`.
pub fn stance_at(collider: &dyn Collider, feet: V3) -> Stance {
    for (stance, height) in [
        (Stance::Standing, STANDING_HEIGHT),
        (Stance::Crouching, CROUCH_HEIGHT),
    ] {
        let half = V3::new(
            HULL_HALF_WIDTH - SKIN_WIDTH,
            HULL_HALF_WIDTH - SKIN_WIDTH,
            height / 2.0 - SKIN_WIDTH,
        );
        let center = feet + V3::new(0.0, 0.0, height / 2.0);
        if !collider.box_intersects(center, half) {
            return stance;
        }
    }
    Stance::None
}

/// Feet heights at this column where a hull-wide footprint finds support,
/// discovered top-down (highest first), matching the reference's own sweep
/// direction: since `Compute` (below) seeds and walks spots in this same
/// per-column order, the order here drives its output order too, not just
/// the set of heights. Swept with the hull's own 32x32 footprint, not a ray.
/// `StandSpots.cs:120-148`.
pub fn supported_heights(
    collider: &dyn Collider,
    x: f32,
    y: f32,
    z_min: f32,
    z_max: f32,
    max_surfaces: usize,
) -> Vec<f32> {
    let mut heights = Vec::new();
    let probe_half = V3::new(
        HULL_HALF_WIDTH - SKIN_WIDTH,
        HULL_HALF_WIDTH - SKIN_WIDTH,
        0.5,
    );
    let mut top = z_max;
    let mut i = 0;
    while i < max_surfaces && top > z_min {
        let from = V3::new(x, y, top);
        let to = V3::new(x, y, z_min);
        let Some(hit) = collider.first_hit_hull(from, to, probe_half, STANDABLE_NORMAL_Z, None)
        else {
            break;
        };
        let z = float_lerp(from.z, to.z, hit.t) - probe_half.z;
        heights.push(z);
        top = z - SURFACE_GAP;
        i += 1;
    }
    heights
}

/// Could a player move between two stand spots in one step, jump or drop?
/// `StandSpots.cs:153-199`.
pub fn can_traverse(collider: &dyn Collider, from: V3, to: V3, to_stance: Stance) -> bool {
    let rise = to.z - from.z;
    let max_rise = if to_stance == Stance::Crouching {
        CROUCH_JUMP_RISE
    } else {
        JUMP_RISE
    };
    if rise > max_rise || rise < -MAX_SAFE_FALL_HEIGHT {
        return false;
    }
    let cross_z = from.z.max(to.z);
    let clearance = if rise > STEP_HEIGHT {
        SKIN_WIDTH * 2.0
    } else {
        0.0
    };
    let height = if to_stance == Stance::Crouching {
        CROUCH_HEIGHT
    } else {
        STANDING_HEIGHT
    };
    let half = V3::new(
        HULL_HALF_WIDTH - SKIN_WIDTH,
        HULL_HALF_WIDTH - SKIN_WIDTH,
        height / 2.0 - SKIN_WIDTH,
    );
    let a = V3::new(from.x, from.y, cross_z + clearance) + V3::new(0.0, 0.0, height / 2.0);
    let b = V3::new(to.x, to.y, cross_z + clearance) + V3::new(0.0, 0.0, height / 2.0);
    if collider.first_hit_hull(a, b, half, -2.0, None).is_some() {
        return false;
    }
    if rise >= -STEP_HEIGHT {
        return true;
    }
    // Walking off an edge is a fall, and a fall lands on the FIRST surface
    // under the player, tested with a point ray from just above the lip.
    let drop_from = V3::new(to.x, to.y, cross_z + 1.0);
    let drop_to = V3::new(to.x, to.y, to.z - SURFACE_GAP);
    match collider.first_hit_ray(drop_from, drop_to) {
        Some(landing) => {
            landing.normal.z >= STANDABLE_NORMAL_Z
                && (float_lerp(drop_from.z, drop_to.z, landing.t) - to.z).abs() <= SURFACE_GAP
        }
        None => false,
    }
}

/// Even-odd point-in-polygon test, shared with nav-area sampling and ground
/// lookups. `StandSpots.cs:333-346`.
pub fn point_in_polygon(corners: &[V3], x: f32, y: f32) -> bool {
    let mut inside = false;
    let n = corners.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (xi, yi) = (corners[i].x, corners[i].y);
        let (xj, yj) = (corners[j].x, corners[j].y);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Every reachable stand spot in the region, seeded from the nav mesh and
/// grown outward across the geometry by walking, jumping and dropping.
/// `StandSpots.cs:210-329`. `nav_areas` is each area's corner ring
/// (`float[][]` in the reference).
pub fn compute(
    collider: &dyn Collider,
    nav_areas: &[Vec<V3>],
    region_min: V3,
    region_max: V3,
    step: f32,
    mut on_progress: Option<&mut dyn FnMut(i32, i32)>,
) -> Vec<Spot> {
    let origin_x = (region_min.x / step).ceil() * step;
    let origin_y = (region_min.y / step).ceil() * step;
    let nx = (((region_max.x - origin_x) / step).floor() as i32) + 1;
    let ny = (((region_max.y - origin_y) / step).floor() as i32) + 1;

    let mut nav_by_cell: HashMap<(i32, i32), Vec<f32>> = HashMap::new();
    for corners in nav_areas {
        let z: f32 =
            (corners.iter().map(|c| c.z as f64).sum::<f64>() / corners.len() as f64) as f32;
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
        let gx_lo = ((min_x - origin_x) / step).floor() as i32;
        let gx_hi = ((max_x - origin_x) / step).ceil() as i32;
        let gy_lo = ((min_y - origin_y) / step).floor() as i32;
        let gy_hi = ((max_y - origin_y) / step).ceil() as i32;
        for gx in gx_lo..=gx_hi {
            for gy in gy_lo..=gy_hi {
                let px = origin_x + gx as f32 * step;
                let py = origin_y + gy as f32 * step;
                if !point_in_polygon(corners, px, py) {
                    continue;
                }
                nav_by_cell.entry((gx, gy)).or_default().push(z);
            }
        }
    }

    let mut columns: HashMap<(i32, i32), Vec<Spot>> = HashMap::new();
    let mut queue: VecDeque<(i32, i32, usize, i32)> = VecDeque::new();
    let mut done = 0;
    for gx in 0..nx {
        for gy in 0..ny {
            let x = origin_x + gx as f32 * step;
            let y = origin_y + gy as f32 * step;
            let mut spots = Vec::new();
            for z in supported_heights(collider, x, y, region_min.z, region_max.z, 8) {
                let feet = V3::new(x, y, z);
                let stance = stance_at(collider, feet);
                if stance == Stance::None {
                    continue;
                }
                let nav_covered = nav_by_cell
                    .get(&(gx, gy))
                    .is_some_and(|zs| zs.iter().any(|nz| (nz - z).abs() <= STEP_HEIGHT * 2.0));
                spots.push(Spot {
                    feet,
                    stance,
                    nav_covered,
                });
            }
            if !spots.is_empty() {
                for (i, s) in spots.iter().enumerate() {
                    if s.nav_covered {
                        queue.push_back((gx, gy, i, 0));
                    }
                }
                columns.insert((gx, gy), spots);
            }
        }
        done += 1;
        if let Some(cb) = on_progress.as_deref_mut() {
            cb(done, nx);
        }
    }

    // Insertion-ordered set: C# `HashSet<T>` enumeration order is insertion
    // order absent removals, and none happen here, so `order` reproduces the
    // reference's final `reachable.Select(...)` output order exactly.
    let mut seen: HashSet<(i32, i32, usize)> = HashSet::new();
    let mut order: Vec<(i32, i32, usize)> = Vec::new();
    for &(x, y, i, _) in &queue {
        if seen.insert((x, y, i)) {
            order.push((x, y, i));
        }
    }

    const DX: [i32; 8] = [1, -1, 0, 0, 1, 1, -1, -1];
    const DY: [i32; 8] = [0, 0, 1, -1, 1, -1, 1, -1];
    while let Some((cx, cy, ci, depth)) = queue.pop_front() {
        if depth >= MAX_STEPS_FROM_NAV {
            continue;
        }
        let from = columns[&(cx, cy)][ci].feet;
        for d in 0..DX.len() {
            let key = (cx + DX[d], cy + DY[d]);
            let Some(neighbours) = columns.get(&key) else {
                continue;
            };
            for (i, &n) in neighbours.iter().enumerate() {
                if seen.contains(&(key.0, key.1, i)) {
                    continue;
                }
                if !can_traverse(collider, from, n.feet, n.stance) {
                    continue;
                }
                seen.insert((key.0, key.1, i));
                order.push((key.0, key.1, i));
                queue.push_back((key.0, key.1, i, if n.nav_covered { 0 } else { depth + 1 }));
            }
        }
    }

    order
        .into_iter()
        .map(|(x, y, i)| columns[&(x, y)][i])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
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

    #[test]
    fn stance_at_open_ground() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        assert_eq!(stance_at(&grid, V3::new(0.0, 0.0, 0.0)), Stance::Standing);
    }

    #[test]
    fn stance_at_low_ceiling_forces_crouch() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0), flat_floor(60.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        assert_eq!(stance_at(&grid, V3::new(0.0, 0.0, 0.0)), Stance::Crouching);
    }

    #[test]
    fn stance_at_blocked() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0), flat_floor(40.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        assert_eq!(stance_at(&grid, V3::new(0.0, 0.0, 0.0)), Stance::None);
    }

    #[test]
    fn supported_heights_stacked_floors() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0), flat_floor(100.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let heights = supported_heights(&grid, 0.0, 0.0, -50.0, 300.0, 8);
        assert_eq!(heights.len(), 2);
        // The probe sweeps down from `z_max`, so it meets the higher floor
        // first: the reference's own `SupportedHeights` (`StandSpots.cs:120-148`)
        // returns them in that top-down discovery order too, and `Compute`
        // relies on it for its own output order.
        assert!(heights[0] > heights[1]);
        assert!((heights[0] - 100.0).abs() < 1.0);
        assert!((heights[1] - 0.0).abs() < 1.0);
    }

    #[test]
    fn can_traverse_step_ok_but_wall_blocked() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        assert!(can_traverse(
            &grid,
            V3::new(0.0, 0.0, 0.0),
            V3::new(16.0, 0.0, 10.0),
            Stance::Standing
        ));
    }

    #[test]
    fn can_traverse_jump_and_lethal_fall() {
        let mesh = quad_mesh(&[flat_floor(0.0, 200.0), flat_floor(50.0, 200.0)]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        assert!(can_traverse(
            &grid,
            V3::new(-60.0, 0.0, 0.0),
            V3::new(-16.0, 0.0, 50.0),
            Stance::Standing
        ));
        assert!(!can_traverse(
            &grid,
            V3::new(0.0, 0.0, 300.0),
            V3::new(16.0, 0.0, 0.0),
            Stance::Standing
        ));
    }

    #[test]
    fn point_in_polygon_basic() {
        let square = vec![
            V3::new(0.0, 0.0, 0.0),
            V3::new(10.0, 0.0, 0.0),
            V3::new(10.0, 10.0, 0.0),
            V3::new(0.0, 10.0, 0.0),
        ];
        assert!(point_in_polygon(&square, 5.0, 5.0));
        assert!(!point_in_polygon(&square, 20.0, 5.0));
    }

    #[test]
    fn compute_finds_jump_only_crate_and_excludes_isolated_platform() {
        let mesh = quad_mesh(&[
            flat_floor(0.0, 300.0),
            // Crate reachable only by a jump.
            [
                [80.0, -16.0, 40.0],
                [112.0, -16.0, 40.0],
                [112.0, 16.0, 40.0],
                [80.0, 16.0, 40.0],
            ],
            // Isolated platform far away, unreachable.
            [
                [900.0, 900.0, 20.0],
                [932.0, 900.0, 20.0],
                [932.0, 932.0, 20.0],
                [900.0, 932.0, 20.0],
            ],
        ]);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 128.0).unwrap();
        let nav_area = vec![
            V3::new(-64.0, -64.0, 0.0),
            V3::new(64.0, -64.0, 0.0),
            V3::new(64.0, 64.0, 0.0),
            V3::new(-64.0, 64.0, 0.0),
        ];
        let spots = compute(
            &grid,
            &[nav_area],
            V3::new(-200.0, -200.0, -50.0),
            V3::new(200.0, 200.0, 200.0),
            16.0,
            None,
        );
        assert!(spots.iter().any(|s| (s.feet.z - 40.0).abs() < 1.0));
        assert!(!spots.iter().any(|s| s.feet.x > 800.0));
    }
}
