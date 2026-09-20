//! `VoxelGrid`: a solid-occupancy voxel grid built from collision
//! triangles, ported from `cs2-smoke-solver/src/Sim/VoxelGrid.cs`, plus a
//! DDA `traverse` equivalent to `Occlusion.cs`'s segment walk.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::collider::ColliderError;
use crate::filter::AttributeMask;
use crate::math::{Aabb, V3};
use crate::mesh::CollisionMesh;
use crate::tri::tri_box_overlap;

/// A solid-occupancy voxel grid. The origin is snapped to multiples of
/// `voxel_size`, minus one cell of padding, so cell boundaries are stable
/// across runs and meshes of the same map (`VoxelGrid.cs:97-101`).
pub struct VoxelGrid {
    voxel_size: f32,
    inv_voxel_size: f32,
    pub origin: V3,
    pub nx: i32,
    pub ny: i32,
    pub nz: i32,
    solid: Vec<AtomicU64>,
}

impl VoxelGrid {
    /// The cell edge length this grid was built with.
    pub fn voxel_size(&self) -> f32 {
        self.voxel_size
    }

    /// `1.0 / voxel_size`, precomputed so `cell_of` multiplies instead of
    /// dividing (see its doc comment).
    pub fn inv_voxel_size(&self) -> f32 {
        self.inv_voxel_size
    }

    pub fn cell_count(&self) -> i64 {
        self.nx as i64 * self.ny as i64 * self.nz as i64
    }

    pub fn solid_count(&self) -> i64 {
        self.solid
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as i64)
            .sum()
    }

    pub fn index(&self, x: i32, y: i32, z: i32) -> usize {
        ((z * self.ny + y) * self.nx + x) as usize
    }

    pub fn coords(&self, index: usize) -> (i32, i32, i32) {
        let x = (index as i32) % self.nx;
        let rest = (index as i32) / self.nx;
        (x, rest % self.ny, rest / self.ny)
    }

    pub fn in_bounds(&self, x: i32, y: i32, z: i32) -> bool {
        x >= 0 && x < self.nx && y >= 0 && y < self.ny && z >= 0 && z < self.nz
    }

    pub fn is_solid(&self, index: usize) -> bool {
        (self.solid[index >> 6].load(Ordering::Relaxed) & (1u64 << (index & 63))) != 0
    }

    fn set_solid_atomic(&self, index: usize) {
        self.solid[index >> 6].fetch_or(1u64 << (index & 63), Ordering::Relaxed);
    }

    /// Multiply by the reciprocal instead of dividing (`VoxelGrid.cs:67-73`):
    /// for power-of-two voxel sizes `1/size` is exact, so this is
    /// bit-identical to `x / size`, not an approximation.
    pub fn cell_of(&self, p: V3) -> (i32, i32, i32) {
        (
            ((p.x - self.origin.x) * self.inv_voxel_size).floor() as i32,
            ((p.y - self.origin.y) * self.inv_voxel_size).floor() as i32,
            ((p.z - self.origin.z) * self.inv_voxel_size).floor() as i32,
        )
    }

    pub fn cell_center_xyz(&self, x: i32, y: i32, z: i32) -> V3 {
        self.origin
            + V3::new(
                (x as f32 + 0.5) * self.voxel_size,
                (y as f32 + 0.5) * self.voxel_size,
                (z as f32 + 0.5) * self.voxel_size,
            )
    }

    pub fn cell_center(&self, index: usize) -> V3 {
        let (x, y, z) = self.coords(index);
        self.cell_center_xyz(x, y, z)
    }

    /// Builds the grid over `bounds`, voxelizing every triangle solid in
    /// `mask` in parallel (`VoxelGrid.cs:90-156`). Rejects with
    /// [`ColliderError::NonFiniteVertex`] on NaN/inf vertices.
    pub fn build(
        mesh: &CollisionMesh,
        mask: &AttributeMask,
        voxel_size: f32,
        bounds: Aabb,
    ) -> Result<Self, ColliderError> {
        let origin = V3::new(
            ((bounds.min.x / voxel_size).floor() - 1.0) * voxel_size,
            ((bounds.min.y / voxel_size).floor() - 1.0) * voxel_size,
            ((bounds.min.z / voxel_size).floor() - 1.0) * voxel_size,
        );
        // `bounds` can be degenerate or inverted (e.g. a caller-computed region whose `max` ends
        // up below its `min`) - saturate every axis at 1 cell rather than letting a negative
        // count slip into the product below, and multiply in `u64` with saturation so the
        // product itself can never wrap around to a huge `usize` and abort the allocation.
        let nx = ((((bounds.max.x - origin.x) / voxel_size).ceil() as i32) + 1).max(1);
        let ny = ((((bounds.max.y - origin.y) / voxel_size).ceil() as i32) + 1).max(1);
        let nz = ((((bounds.max.z - origin.z) / voxel_size).ceil() as i32) + 1).max(1);
        let cell_count = (nx as u64)
            .saturating_mul(ny as u64)
            .saturating_mul(nz as u64) as usize;
        let words = cell_count.div_ceil(64);
        let solid: Vec<AtomicU64> = (0..words).map(|_| AtomicU64::new(0)).collect();
        let grid = VoxelGrid {
            voxel_size,
            inv_voxel_size: 1.0 / voxel_size,
            origin,
            nx,
            ny,
            nz,
            solid,
        };

        let half_size = V3::new(voxel_size / 2.0, voxel_size / 2.0, voxel_size / 2.0);
        let tri_count = mesh.triangles.len();
        (0..tri_count)
            .into_par_iter()
            .try_for_each(|i| -> Result<(), ColliderError> {
                let attr = mesh.tri_attribute[i];
                if !mask.is_solid(attr) {
                    return Ok(());
                }
                let tri = mesh.triangles[i];
                let v0 = V3::from_array(mesh.vertices[tri[0] as usize]);
                let v1 = V3::from_array(mesh.vertices[tri[1] as usize]);
                let v2 = V3::from_array(mesh.vertices[tri[2] as usize]);
                if !v0.is_finite() || !v1.is_finite() || !v2.is_finite() {
                    return Err(ColliderError::NonFiniteVertex { triangle: i as u32 });
                }

                let tri_min = v0.min(v1.min(v2));
                let tri_max = v0.max(v1.max(v2));
                let (cx0, cy0, cz0) = grid.cell_of(tri_min);
                let (cx1, cy1, cz1) = grid.cell_of(tri_max);
                let cx0 = cx0.max(0);
                let cy0 = cy0.max(0);
                let cz0 = cz0.max(0);
                let cx1 = cx1.min(nx - 1);
                let cy1 = cy1.min(ny - 1);
                let cz1 = cz1.min(nz - 1);

                for z in cz0..=cz1 {
                    for y in cy0..=cy1 {
                        for x in cx0..=cx1 {
                            let index = grid.index(x, y, z);
                            if grid.is_solid(index) {
                                continue;
                            }
                            let center = grid.cell_center_xyz(x, y, z);
                            if tri_box_overlap(center, half_size, v0, v1, v2) {
                                grid.set_solid_atomic(index);
                            }
                        }
                    }
                }
                Ok(())
            })?;
        Ok(grid)
    }

    /// Amanatides-Woo DDA walk from `from` to `to`, visiting every in-bounds
    /// cell the segment passes through and calling `visit(index, (x, y,
    /// z))`; stops early if `visit` returns `ControlFlow::Break`. Ported
    /// from `Occlusion.cs:16-95` (segment stepping and termination
    /// conditions only; smoke/solid bookkeeping is the caller's job here).
    pub fn traverse(
        &self,
        from: V3,
        to: V3,
        mut visit: impl FnMut(usize, (i32, i32, i32)) -> ControlFlow<()>,
    ) {
        let direction_raw = to - from;
        let length = direction_raw.length();
        // Reject NaN/inf explicitly: `length < 1e-4` is false for NaN (and
        // for +inf), which would fall through into a loop that never
        // reaches `length` and never terminates.
        if !(length.is_finite() && length >= 1e-4) {
            return;
        }
        let direction = direction_raw / length;

        let (mut x, mut y, mut z) = self.cell_of(from);
        let (end_x, end_y, end_z) = self.cell_of(to);

        let step_x = signum(direction.x);
        let step_y = signum(direction.y);
        let step_z = signum(direction.z);

        let mut t_max_x = time_to_boundary(from.x, self.origin.x, self.voxel_size, x, direction.x);
        let mut t_max_y = time_to_boundary(from.y, self.origin.y, self.voxel_size, y, direction.y);
        let mut t_max_z = time_to_boundary(from.z, self.origin.z, self.voxel_size, z, direction.z);

        let t_delta_x = if direction.x == 0.0 {
            f32::INFINITY
        } else {
            self.voxel_size / direction.x.abs()
        };
        let t_delta_y = if direction.y == 0.0 {
            f32::INFINITY
        } else {
            self.voxel_size / direction.y.abs()
        };
        let t_delta_z = if direction.z == 0.0 {
            f32::INFINITY
        } else {
            self.voxel_size / direction.z.abs()
        };

        loop {
            if self.in_bounds(x, y, z) {
                let index = self.index(x, y, z);
                if visit(index, (x, y, z)).is_break() {
                    return;
                }
            }
            if x == end_x && y == end_y && z == end_z {
                return;
            }
            if t_max_x <= t_max_y && t_max_x <= t_max_z {
                if t_max_x > length {
                    return;
                }
                x = x.wrapping_add(step_x);
                t_max_x += t_delta_x;
            } else if t_max_y <= t_max_z {
                if t_max_y > length {
                    return;
                }
                y = y.wrapping_add(step_y);
                t_max_y += t_delta_y;
            } else {
                if t_max_z > length {
                    return;
                }
                z = z.wrapping_add(step_z);
                t_max_z += t_delta_z;
            }
        }
    }
}

fn signum(v: f32) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

fn time_to_boundary(pos: f32, origin: f32, voxel_size: f32, cell: i32, dir: f32) -> f32 {
    if dir > 0.0 {
        (origin + cell.wrapping_add(1) as f32 * voxel_size - pos) / dir
    } else if dir < 0.0 {
        (origin + cell as f32 * voxel_size - pos) / dir
    } else {
        f32::INFINITY
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::filter::all_mask;
    use crate::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    struct Rng(u32);
    impl Rng {
        fn new(seed: u32) -> Self {
            Rng(seed | 1)
        }
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn f32(&mut self, lo: f32, hi: f32) -> f32 {
            let t = self.next_u32() as f32 / u32::MAX as f32;
            lo + t * (hi - lo)
        }
        fn v3(&mut self, lo: f32, hi: f32) -> V3 {
            V3::new(self.f32(lo, hi), self.f32(lo, hi), self.f32(lo, hi))
        }
    }

    fn random_mesh(rng: &mut Rng, n: usize, center_bounds: f32, spread: f32) -> CollisionMesh {
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
        for _ in 0..n {
            let base = rng.v3(-center_bounds, center_bounds);
            let vert = |rng: &mut Rng| {
                let d = rng.v3(-spread, spread);
                [base.x + d.x, base.y + d.y, base.z + d.z]
            };
            let a = vert(rng);
            let b = vert(rng);
            let c = vert(rng);
            let _ = mesh.push_triangles(
                &[a, b, c],
                &[[0, 1, 2]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            );
        }
        mesh
    }

    /// An inverted z range (`max.z < min.z`, as a caller-computed region can end up with) used to
    /// make `nz` negative, wrapping `nx * ny * nz` to a huge `usize` and aborting the allocation.
    /// It must now build a small, sane grid instead.
    #[test]
    fn build_survives_an_inverted_z_range() {
        let mut rng = Rng::new(0x1234_5678);
        let mesh = random_mesh(&mut rng, 20, 60.0, 12.0);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-80.0, -80.0, 0.0),
            max: V3::new(80.0, 80.0, -1000.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        assert!(grid.nx >= 1 && grid.ny >= 1 && grid.nz >= 1);
        assert!(
            grid.cell_count() < 1_000_000,
            "grid ballooned: nx={} ny={} nz={}",
            grid.nx,
            grid.ny,
            grid.nz
        );
    }

    #[test]
    fn build_matches_brute_force_tri_box_overlap() {
        let mut rng = Rng::new(0xA5A5_A5A5);
        let mesh = random_mesh(&mut rng, 120, 60.0, 12.0);
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-80.0, -80.0, -80.0),
            max: V3::new(80.0, 80.0, 80.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();

        let half = V3::new(8.0, 8.0, 8.0);
        for z in 0..grid.nz {
            for y in 0..grid.ny {
                for x in 0..grid.nx {
                    let index = grid.index(x, y, z);
                    let center = grid.cell_center_xyz(x, y, z);
                    let expected = mesh.triangles.iter().any(|tri| {
                        let a = V3::from_array(mesh.vertices[tri[0] as usize]);
                        let b = V3::from_array(mesh.vertices[tri[1] as usize]);
                        let c = V3::from_array(mesh.vertices[tri[2] as usize]);
                        tri_box_overlap(center, half, a, b, c)
                    });
                    assert_eq!(grid.is_solid(index), expected, "cell ({x},{y},{z})");
                }
            }
        }
    }

    #[test]
    fn dda_visits_match_dense_sampling() {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bounds = Aabb {
            min: V3::new(-64.0, -64.0, -64.0),
            max: V3::new(64.0, 64.0, 64.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();

        let mut rng = Rng::new(0xDDA5_EED5);
        for _ in 0..2_000 {
            let from = rng.v3(-60.0, 60.0);
            let to = rng.v3(-60.0, 60.0);

            let mut visited = HashSet::new();
            grid.traverse(from, to, |_index, coords| {
                visited.insert(coords);
                std::ops::ControlFlow::Continue(())
            });

            let length = (to - from).length();
            if length < 1e-4 {
                assert!(visited.is_empty());
                continue;
            }
            let steps = ((length / (grid.voxel_size() * 0.1)).ceil() as i32).max(1);
            let mut sampled = HashSet::new();
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let p = from + (to - from) * t;
                let c = grid.cell_of(p);
                if grid.in_bounds(c.0, c.1, c.2) {
                    sampled.insert(c);
                }
            }
            // Every densely-sampled cell must have been visited by the DDA
            // (it cannot skip a cell the segment truly passes through).
            for cell in &sampled {
                assert!(
                    visited.contains(cell),
                    "DDA missed cell {cell:?} from={from:?} to={to:?}"
                );
            }
        }
    }
}
