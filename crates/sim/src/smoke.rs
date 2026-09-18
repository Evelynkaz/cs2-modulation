//! Smoke volume: flood-fill from the grenade rest point into free voxel
//! cells, bounded by radius and cell budget. Ported from
//! `cs2-smoke-solver/src/Sim/SmokeParams.cs` and `SmokeFloodFill.cs`.

use std::collections::HashSet;

use geom::math::V3;
use geom::voxel::VoxelGrid;

use crate::SimError;

/// `SmokeParams.cs:28` (`DownwardPull`).
const DOWNWARD_PULL: f32 = 0.8;

/// `SmokeParams.cs:9` (`SmokeParams`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmokeParams {
    pub max_radius: f32,
    pub cell_budget: u32,
    pub contained_stretch: f32,
}

impl SmokeParams {
    /// `SmokeParams.UncalibratedDefault` (`SmokeParams.cs:53`).
    pub const UNCALIBRATED_DEFAULT: SmokeParams = SmokeParams {
        max_radius: 165.0,
        cell_budget: 3500,
        contained_stretch: 1.0,
    };
    /// `SmokeParams.Coverage` (`SmokeParams.cs:56`).
    pub const COVERAGE: SmokeParams = SmokeParams {
        max_radius: 128.0,
        cell_budget: 3500,
        contained_stretch: 1.75,
    };
    /// `SmokeParams.FullReach` (`SmokeParams.cs:59`).
    pub const FULL_REACH: SmokeParams = SmokeParams {
        max_radius: 144.0,
        cell_budget: 3500,
        contained_stretch: 1.75,
    };
    /// `SmokeParams.Conservative` (`SmokeParams.cs:61`).
    pub const CONSERVATIVE: SmokeParams = SmokeParams {
        max_radius: 100.0,
        cell_budget: u32::MAX,
        contained_stretch: 1.0,
    };
}

/// `SmokeFloodFill.cs:5-34` (`SmokeVolume`).
#[derive(Debug, Clone)]
pub struct SmokeVolume {
    pub rest: V3,
    pub cells: Vec<u32>,
    set: HashSet<u32>,
}

impl SmokeVolume {
    pub fn contains(&self, index: u32) -> bool {
        self.set.contains(&index)
    }
}

/// An exact port of .NET's `PriorityQueue<TElement, TPriority>`'s internal
/// 4-ary (quaternary) min-heap (`System.Collections.Generic.PriorityQueue`),
/// so a budget-limited fill claims cells in the SAME order the reference
/// does - `PriorityQueue<int, float>` is not a plain binary heap, and a
/// binary `BinaryHeap` (even a stable one) visits neighbors in a different
/// order once the budget binds, changing which cells a bounded fill ends up
/// claiming (measured: 3,128 / 6,000 real-mirage fills differed in cell set
/// before this port). `enqueue` "sift up" compares with strict `<` and
/// `dequeue` "sift down" compares the popped last element with `<=` against
/// the smallest child - both match the reference's `MoveUp`/`MoveDown`
/// exactly, including which of two equal-priority elements ends up on top.
fn enqueue(heap: &mut Vec<(usize, f32)>, element: usize, priority: f32) {
    let mut i = heap.len();
    heap.push((element, priority));
    while i > 0 {
        let parent = (i - 1) >> 2;
        if priority < heap[parent].1 {
            heap[i] = heap[parent];
            i = parent;
        } else {
            break;
        }
    }
    heap[i] = (element, priority);
}

fn dequeue(heap: &mut Vec<(usize, f32)>) -> Option<usize> {
    if heap.is_empty() {
        return None;
    }
    let root = heap[0].0;
    let last = heap.pop().unwrap();
    let size = heap.len();
    if size > 0 {
        let mut idx = 0;
        loop {
            let first_child = (idx << 2) + 1;
            if first_child >= size {
                break;
            }
            let mut min_child_idx = first_child;
            let mut min_child = heap[first_child];
            let upper_bound = (first_child + 4).min(size);
            for (offset, &candidate) in heap[first_child + 1..upper_bound].iter().enumerate() {
                if candidate.1 < min_child.1 {
                    min_child = candidate;
                    min_child_idx = first_child + 1 + offset;
                }
            }
            if last.1 <= min_child.1 {
                break;
            }
            heap[idx] = min_child;
            idx = min_child_idx;
        }
        heap[idx] = last;
    }
    Some(root)
}

/// `SmokeFloodFill.cs:127-131` (`SettlingDistance`): straight-line distance
/// with the drop below the landing foreshortened, so the fill spills down
/// before it spreads.
fn settling_distance(offset: V3) -> f32 {
    let dz = if offset.z < 0.0 {
        offset.z * DOWNWARD_PULL
    } else {
        offset.z
    };
    (offset.x * offset.x + offset.y * offset.y + dz * dz).sqrt()
}

/// `SmokeFloodFill.cs:138-157` (`OpenAirCells`): how many lattice points of
/// the half-ball above the ground layer a flat-open-ground landing claims
/// inside `radius`. Not cached (unlike the reference's per-map cache): this
/// crate has no per-map fill cache to key off yet.
fn open_air_cells(voxel: f32, radius: f32) -> usize {
    let n = (radius / voxel).ceil() as i32;
    let mut count = 0usize;
    for dz in 0..=n {
        for dy in -n..=n {
            for dx in -n..=n {
                if ((dx * dx + dy * dy + dz * dz) as f32) * voxel * voxel <= radius * radius {
                    count += 1;
                }
            }
        }
    }
    count
}

/// `SmokeFloodFill.cs:42-122` (`Fill`). The frontier is the `enqueue`/
/// `dequeue` 4-ary heap above, ported to match the reference's
/// `PriorityQueue<int, float>` cell-claim order exactly, including which
/// cell wins a priority tie once the budget binds.
pub fn smoke_fill(grid: &VoxelGrid, rest: V3, p: &SmokeParams) -> Result<SmokeVolume, SimError> {
    if !rest.is_finite() {
        return Err(SimError::RestPointOutOfBounds { rest });
    }
    let (sx, sy, sz0) = grid.cell_of(rest);
    if !grid.in_bounds(sx, sy, sz0) {
        return Err(SimError::RestPointOutOfBounds { rest });
    }

    let lift_limit = (sz0 + 4).min(grid.nz - 1);
    let mut sz = sz0;
    while sz <= lift_limit && grid.is_solid(grid.index(sx, sy, sz)) {
        sz += 1;
    }
    // `sz` can walk past `lift_limit` (and even past `grid.nz - 1`, if
    // `lift_limit` itself was clamped to `grid.nz - 1` and that cell was
    // still solid) without ever finding free space above a solid column;
    // treat that exactly like the "still solid" case below rather than
    // indexing a possibly-out-of-range cell.
    if sz >= grid.nz || grid.is_solid(grid.index(sx, sy, sz)) {
        return Ok(SmokeVolume {
            rest,
            cells: vec![],
            set: HashSet::new(),
        });
    }
    let start_index = grid.index(sx, sy, sz);

    let start_center = grid.cell_center_xyz(sx, sy, sz);
    let reach = p.max_radius * p.contained_stretch;
    let max_radius_sq = reach * reach;
    let budget = (p.cell_budget as usize).min(open_air_cells(grid.voxel_size(), p.max_radius));

    let mut visited: HashSet<usize> = HashSet::new();
    visited.insert(start_index);
    let mut cells: Vec<usize> = vec![start_index];
    let mut frontier: Vec<(usize, f32)> = Vec::new();
    enqueue(&mut frontier, start_index, 0.0);

    const NEIGHBORS: [(i32, i32, i32); 6] = [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];

    while cells.len() < budget {
        let Some(current) = dequeue(&mut frontier) else {
            break;
        };
        let (cx, cy, cz) = grid.coords(current);
        for &(dx, dy, dz) in &NEIGHBORS {
            if cells.len() >= budget {
                break;
            }
            let (nx, ny, nz) = (cx + dx, cy + dy, cz + dz);
            if !grid.in_bounds(nx, ny, nz) {
                continue;
            }
            let ni = grid.index(nx, ny, nz);
            if visited.contains(&ni) || grid.is_solid(ni) {
                continue;
            }
            let offset = grid.cell_center_xyz(nx, ny, nz) - start_center;
            if offset.length_squared() > max_radius_sq {
                continue;
            }
            visited.insert(ni);
            cells.push(ni);
            enqueue(&mut frontier, ni, settling_distance(offset));
        }
    }

    Ok(SmokeVolume {
        rest,
        cells: cells.iter().map(|&c| c as u32).collect(),
        set: visited.into_iter().map(|c| c as u32).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::mesh::CollisionMesh;

    #[test]
    fn open_air_fill_matches_half_ball_cell_count() {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-128.0, -128.0, -128.0),
            max: V3::new(128.0, 128.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 8.0, bounds).unwrap();
        let rest = V3::new(0.0, 0.0, 0.0);
        let p = SmokeParams {
            max_radius: 64.0,
            cell_budget: 1_000_000,
            contained_stretch: 1.0,
        };
        let volume = smoke_fill(&grid, rest, &p).unwrap();
        let expected = open_air_cells(grid.voxel_size(), p.max_radius);
        assert_eq!(volume.cells.len(), expected);
    }

    #[test]
    fn wall_stops_fill_from_reaching_open_air_count() {
        use geom::mesh::{CollisionAttribute, MeshObject, ObjectKind, SurfaceProperty};
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
        // A closed 32-unit box room around the origin: the fill can never
        // spend its whole (much larger) open-air budget once it is walled in.
        let room_faces: [[[f32; 3]; 4]; 6] = [
            [
                [-16.0, -16.0, -16.0],
                [16.0, -16.0, -16.0],
                [16.0, 16.0, -16.0],
                [-16.0, 16.0, -16.0],
            ],
            [
                [-16.0, -16.0, 16.0],
                [16.0, -16.0, 16.0],
                [16.0, 16.0, 16.0],
                [-16.0, 16.0, 16.0],
            ],
            [
                [-16.0, -16.0, -16.0],
                [-16.0, 16.0, -16.0],
                [-16.0, 16.0, 16.0],
                [-16.0, -16.0, 16.0],
            ],
            [
                [16.0, -16.0, -16.0],
                [16.0, 16.0, -16.0],
                [16.0, 16.0, 16.0],
                [16.0, -16.0, 16.0],
            ],
            [
                [-16.0, -16.0, -16.0],
                [16.0, -16.0, -16.0],
                [16.0, -16.0, 16.0],
                [-16.0, -16.0, 16.0],
            ],
            [
                [-16.0, 16.0, -16.0],
                [16.0, 16.0, -16.0],
                [16.0, 16.0, 16.0],
                [-16.0, 16.0, 16.0],
            ],
        ];
        for face in room_faces {
            mesh.push_triangles(
                &face,
                &[[0, 1, 2], [0, 2, 3]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
        }
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-128.0, -128.0, -128.0),
            max: V3::new(128.0, 128.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 8.0, bounds).unwrap();
        let rest = V3::new(0.0, 0.0, 0.0);
        let p = SmokeParams {
            max_radius: 64.0,
            cell_budget: 1_000_000,
            contained_stretch: 1.0,
        };
        let volume = smoke_fill(&grid, rest, &p).unwrap();
        let expected_open = open_air_cells(grid.voxel_size(), p.max_radius);
        assert!(volume.cells.len() < expected_open);
        // No claimed cell should be outside the room's walls.
        for &cell in &volume.cells {
            let c = grid.cell_center(cell as usize);
            assert!(
                c.x.abs() < 16.0 && c.y.abs() < 16.0 && c.z.abs() < 16.0,
                "cell center {c:?} escaped the room"
            );
        }
    }

    #[test]
    fn rest_point_outside_grid_is_an_error() {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-8.0, -8.0, -8.0),
            max: V3::new(8.0, 8.0, 8.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 8.0, bounds).unwrap();
        let far = V3::new(10_000.0, 10_000.0, 10_000.0);
        assert!(smoke_fill(&grid, far, &SmokeParams::UNCALIBRATED_DEFAULT).is_err());
    }
}
