//! How far the landing zone is from each part of the map THROUGH OPEN AIR,
//! measured by flooding the free voxels outward from the zone. Ported from
//! `cs2-smoke-solver/src/Solver/FreeSpaceReach.cs`.

use std::collections::VecDeque;

use geom::math::V3;
use geom::voxel::VoxelGrid;

/// `FreeSpaceReach.cs:28` (`Unreached`).
const UNREACHED: u8 = 255;

/// `FreeSpaceReach.cs:30-55` (`Field`).
pub struct Field<'a> {
    grid: &'a VoxelGrid,
    steps: Vec<u8>,
}

impl<'a> Field<'a> {
    /// `FreeSpaceReach.cs:41-54` (`DistanceFrom`): shortest free-space
    /// distance from this point to the zone, or `None` when no open path
    /// within the search budget exists.
    pub fn distance_from(&self, point: V3) -> Option<f32> {
        let (x, y, z) = self.grid.cell_of(point);
        if !self.grid.in_bounds(x, y, z) {
            return None;
        }
        let steps = self.steps[self.grid.index(x, y, z)];
        if steps == UNREACHED {
            None
        } else {
            Some(steps as f32 * self.grid.voxel_size())
        }
    }
}

/// `FreeSpaceReach.cs:66-124` (`Build`): floods open cells outward from the
/// zone, stopping past `max_distance`. Six-connected on purpose (see the
/// reference's doc comment): a diagonal step through the gap between two
/// solid cells would claim a path a grenade cannot take.
pub fn build<'a>(
    grid: &'a VoxelGrid,
    zone_cells: impl IntoIterator<Item = usize>,
    max_distance: f32,
) -> Field<'a> {
    let cell_count = (grid.nx as usize) * (grid.ny as usize) * (grid.nz as usize);
    let mut steps = vec![UNREACHED; cell_count];
    let max_steps = (max_distance / grid.voxel_size())
        .ceil()
        .min((UNREACHED - 1) as f32) as u8;

    let mut queue: VecDeque<usize> = VecDeque::new();
    for cell in zone_cells {
        if grid.is_solid(cell) || steps[cell] == 0 {
            continue;
        }
        steps[cell] = 0;
        queue.push_back(cell);
    }

    let layer_xy = (grid.nx as usize) * (grid.ny as usize);
    while let Some(cell) = queue.pop_front() {
        let next = steps[cell] + 1;
        if next > max_steps {
            continue;
        }
        let z = cell / layer_xy;
        let rem = cell - z * layer_xy;
        let y = rem / grid.nx as usize;
        let x = rem - y * grid.nx as usize;
        let (x, y, z) = (x as i32, y as i32, z as i32);

        let visit =
            |nx: i32, ny: i32, nz: i32, steps: &mut Vec<u8>, queue: &mut VecDeque<usize>| {
                if !grid.in_bounds(nx, ny, nz) {
                    return;
                }
                let index = grid.index(nx, ny, nz);
                if steps[index] != UNREACHED || grid.is_solid(index) {
                    return;
                }
                steps[index] = next;
                queue.push_back(index);
            };
        visit(x - 1, y, z, &mut steps, &mut queue);
        visit(x + 1, y, z, &mut steps, &mut queue);
        visit(x, y - 1, z, &mut steps, &mut queue);
        visit(x, y + 1, z, &mut steps, &mut queue);
        visit(x, y, z - 1, &mut steps, &mut queue);
        visit(x, y, z + 1, &mut steps, &mut queue);
    }

    Field { grid, steps }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    /// A room split by a wall with a gap: BFS distance around the wall must
    /// exceed the straight-line distance through it.
    fn room_with_wall() -> VoxelGrid {
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
        // A floor plus a wall spanning y in [-64,64) at x=0, leaving a gap
        // above z=32 (so the grid, from z=0..64, has the wall solid only in
        // its lower half).
        mesh.push_triangles(
            &[
                [-100.0, -100.0, -8.0],
                [100.0, -100.0, -8.0],
                [100.0, 100.0, -8.0],
                [-100.0, 100.0, -8.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        mesh.push_triangles(
            &[
                [-1.0, -100.0, -8.0],
                [1.0, -100.0, -8.0],
                [1.0, 100.0, 24.0],
                [-1.0, 100.0, 24.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-100.0, -100.0, -8.0),
            max: V3::new(100.0, 100.0, 64.0),
        };
        VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap()
    }

    #[test]
    fn distances_grow_around_a_wall_gap() {
        let grid = room_with_wall();
        // Zone cell: one open cell on the +x side, near the floor.
        let (zx, zy, zz) = grid.cell_of(V3::new(50.0, 0.0, 8.0));
        let zone_index = grid.index(zx, zy, zz);
        assert!(!grid.is_solid(zone_index), "zone cell must be open");
        let field = build(&grid, [zone_index], 2000.0);

        // Directly across the wall (blocked at this height): distance must
        // route around, so it's larger than a point on the same side with
        // equal straight-line distance but no wall between them.
        let blocked = field
            .distance_from(V3::new(-50.0, 0.0, 8.0))
            .expect("path exists via the gap above the wall");
        let same_side = field
            .distance_from(V3::new(50.0, -80.0, 8.0))
            .expect("path exists on the same side");
        assert!(
            blocked > same_side,
            "blocked {blocked} should exceed same-side {same_side} (forced to detour over the wall)"
        );
    }

    #[test]
    fn unreached_beyond_budget_is_none() {
        let grid = room_with_wall();
        let (zx, zy, zz) = grid.cell_of(V3::new(50.0, 0.0, 8.0));
        let zone_index = grid.index(zx, zy, zz);
        let field = build(&grid, [zone_index], 16.0);
        assert!(field.distance_from(V3::new(-50.0, 0.0, 8.0)).is_none());
    }
}
