//! `UniformGrid`: a CSR uniform-grid `Collider`, ported from
//! `cs2-smoke-solver/src/Sim/TriangleCollider.cs` (grid build) and
//! `TriangleCollider.Sat.cs` / `TriangleCollider.Raycast.cs` (queries).

use crate::collider::{Collider, ColliderError, ColliderTriangles, HullHit, RayHit};
use crate::filter::AttributeMask;
use crate::math::{Aabb, V3};
use crate::mesh::CollisionMesh;
use crate::tri::{RayWindow, moller_trumbore, swept_box_triangle, tri_box_overlap};

/// A uniform grid over `ColliderTriangles`, CSR-laid-out
/// (`TriangleCollider.cs:20-30`): triangles of cell `i` live in
/// `cell_tris[cell_start[i]..cell_start[i+1]]`.
#[derive(Debug)]
pub struct UniformGrid {
    triangles: ColliderTriangles,
    origin: V3,
    cell_size: f32,
    nx: i32,
    ny: i32,
    nz: i32,
    cell_start: Vec<u32>,
    /// Local (`ColliderTriangles`) indices, CSR payload.
    cell_tris: Vec<u32>,
}

fn for_each_covered_cell(
    aabb: Aabb,
    origin: V3,
    cell_size: f32,
    nx: i32,
    ny: i32,
    nz: i32,
    mut visit: impl FnMut(usize),
) {
    let cell_of = |p: V3| -> (i32, i32, i32) {
        (
            ((p.x - origin.x) / cell_size).floor() as i32,
            ((p.y - origin.y) / cell_size).floor() as i32,
            ((p.z - origin.z) / cell_size).floor() as i32,
        )
    };
    let (x0, y0, z0) = cell_of(aabb.min);
    let (x1, y1, z1) = cell_of(aabb.max);
    // Clamp both ends of each axis range independently (not just "clamp
    // below zero, clamp above n-1"): an AABB face sitting exactly on the
    // region's own upper bound floors to cell index `n` on that axis, which
    // is out of range on *both* sides of a naive one-sided clamp, producing
    // an inverted (and silently empty) range that drops the triangle from
    // every cell. Symmetric clamping instead pins it to the last valid cell.
    for z in z0.clamp(0, nz - 1)..=z1.clamp(0, nz - 1) {
        for y in y0.clamp(0, ny - 1)..=y1.clamp(0, ny - 1) {
            for x in x0.clamp(0, nx - 1)..=x1.clamp(0, nx - 1) {
                visit(((z * ny + y) * nx + x) as usize);
            }
        }
    }
}

impl UniformGrid {
    /// Builds the grid over `mesh`'s triangles solid in `mask`. `region`
    /// restricts both which triangles are kept (as in `ColliderTriangles`)
    /// and the grid's own extent (matching the reference constructor, which
    /// uses one `regionMin`/`regionMax` pair for both); `None` defaults to
    /// the AABB union of the kept triangles. `cell_size` is typically 128
    /// (`TriangleCollider.cs:38`, the reference's default).
    ///
    /// **Deliberate deviation from the reference**: `TriangleCollider.cs`'s
    /// `ForEachCoveredCell` clamps each covered-cell range one-sided
    /// (`max(lo, 0)..min(hi, n-1)`). A triangle whose AABB face sits exactly
    /// on the grid's own upper bound on some axis (e.g. a ceiling triangle
    /// at `region.max.z`, which is common on real maps since `region`
    /// defaults to the exact bounds of the kept triangles) floors to cell
    /// index `n` on *both* ends of that axis's range, so the one-sided clamp
    /// produces `n..=(n-1)` - inverted, silently empty - and the reference
    /// drops that triangle from every cell of the grid it builds. This is a
    /// reference bug, not an intentional exclusion: it also affects
    /// `first_hit_hull`/`box_intersects`'s own covered-cell ranges. This
    /// port instead clamps both ends of each axis symmetrically
    /// (`.clamp(0, n-1)`), pinning a boundary-flush triangle to the last
    /// valid cell instead of losing it (see `for_each_covered_cell`,
    /// `first_hit_hull`, `box_intersects`).
    pub fn build(
        mesh: &CollisionMesh,
        mask: &AttributeMask,
        region: Option<Aabb>,
        cell_size: f32,
    ) -> Result<Self, ColliderError> {
        let triangles = ColliderTriangles::build(mesh, mask, region)?;
        let region = region.or_else(|| triangles.bounds()).unwrap_or(Aabb {
            min: V3::ZERO,
            max: V3::ZERO,
        });
        let origin = region.min;
        let nx = (((region.max.x - origin.x) / cell_size).ceil() as i32).max(1);
        let ny = (((region.max.y - origin.y) / cell_size).ceil() as i32).max(1);
        let nz = (((region.max.z - origin.z) / cell_size).ceil() as i32).max(1);
        let cell_count = (nx as usize) * (ny as usize) * (nz as usize);

        let mut cell_start = vec![0u32; cell_count + 1];
        for local in 0..triangles.len() {
            for_each_covered_cell(
                triangles.aabb(local),
                origin,
                cell_size,
                nx,
                ny,
                nz,
                |cell| {
                    cell_start[cell + 1] += 1;
                },
            );
        }
        for i in 0..cell_count {
            cell_start[i + 1] += cell_start[i];
        }
        let mut cell_tris = vec![0u32; cell_start[cell_count] as usize];
        let mut fill = vec![0u32; cell_count];
        for local in 0..triangles.len() {
            for_each_covered_cell(
                triangles.aabb(local),
                origin,
                cell_size,
                nx,
                ny,
                nz,
                |cell| {
                    let pos = cell_start[cell] + fill[cell];
                    cell_tris[pos as usize] = local as u32;
                    fill[cell] += 1;
                },
            );
        }

        Ok(UniformGrid {
            triangles,
            origin,
            cell_size,
            nx,
            ny,
            nz,
            cell_start,
            cell_tris,
        })
    }

    fn cell_of(&self, p: V3) -> (i32, i32, i32) {
        (
            ((p.x - self.origin.x) / self.cell_size).floor() as i32,
            ((p.y - self.origin.y) / self.cell_size).floor() as i32,
            ((p.z - self.origin.z) / self.cell_size).floor() as i32,
        )
    }

    fn cell_range(&self, index: usize) -> std::ops::Range<usize> {
        self.cell_start[index] as usize..self.cell_start[index + 1] as usize
    }

    fn bounds_touch(&self, local: usize, lo: V3, hi: V3) -> bool {
        let b = self.triangles.aabb(local);
        b.max.x >= lo.x
            && b.min.x <= hi.x
            && b.max.y >= lo.y
            && b.min.y <= hi.y
            && b.max.z >= lo.z
            && b.min.z <= hi.z
    }

    fn hit_triangle(
        &self,
        from: V3,
        direction: V3,
        local: usize,
        window: RayWindow,
    ) -> Option<(f32, V3)> {
        let [a, b, c] = self.triangles.vertices(local);
        let t = moller_trumbore(from, direction, a, b, c)?;
        if !window.accepts(t) {
            return None;
        }
        let mut normal = (b - a).cross(c - a).normalize();
        if normal.dot(direction) > 0.0 {
            normal = -normal;
        }
        Some((t, normal))
    }

    /// Shared ray core for both `first_hit_ray` (closest hit, physics
    /// window) and `blocked` (any hit, sightline window, `early_exit`),
    /// ported from `TriangleCollider.Raycast.cs:44-160` (`RayHit`): a short
    /// AABB walk when the segment spans <= 3 cells, else an Amanatides-Woo
    /// DDA with early exit once the recorded hit precedes every cell ahead.
    fn cast(&self, from: V3, to: V3, window: RayWindow, early_exit: bool) -> Option<RayHit> {
        let direction = to - from;
        let mut best_t = f32::MAX;
        let mut best_normal = V3::ZERO;
        let mut best_triangle: Option<u32> = None;

        let (x0, y0, z0) = self.cell_of(from);
        let (x1, y1, z1) = self.cell_of(to);
        // Wrapping: `from`/`to` far outside the grid can produce cell
        // coordinates near i32::MIN/MAX (`cell_of` floors an unbounded
        // division), and a plain `-`/`abs` would panic on overflow in debug
        // builds. Wrapping arithmetic keeps this a saturating-ish heuristic
        // (only used to pick short-AABB-walk vs. DDA) rather than a panic.
        let span = x1
            .wrapping_sub(x0)
            .wrapping_abs()
            .wrapping_add(y1.wrapping_sub(y0).wrapping_abs())
            .wrapping_add(z1.wrapping_sub(z0).wrapping_abs());
        // Same symmetric-clamp deviation as `for_each_covered_cell` (see
        // `build`'s rustdoc): this also means that if *both* `from` and `to`
        // project outside the grid on the same side of some axis (e.g. a
        // segment entirely above `region.max.z`), this walk still visits the
        // single boundary-cell layer on that axis instead of the empty range
        // a one-sided clamp (or the reference) would produce. That's a
        // superset of the reference's candidate cells in that case, not a
        // subset: visiting more cells can only ever *find* a hit the
        // reference's own off-by-one bug would have missed (it restores the
        // boundary-flush triangles `build`'s symmetric clamp keeps in the
        // grid in the first place), never introduce a false one - every
        // candidate found here still gets the exact same `hit_triangle`
        // test as any other cell's candidates.
        if span <= 3 {
            'outer: for z in z0.min(z1).clamp(0, self.nz - 1)..=z0.max(z1).clamp(0, self.nz - 1) {
                for y in y0.min(y1).clamp(0, self.ny - 1)..=y0.max(y1).clamp(0, self.ny - 1) {
                    for x in x0.min(x1).clamp(0, self.nx - 1)..=x0.max(x1).clamp(0, self.nx - 1) {
                        let cell = ((z * self.ny + y) * self.nx + x) as usize;
                        for &local in &self.cell_tris[self.cell_range(cell)] {
                            let local = local as usize;
                            if let Some((t, n)) = self.hit_triangle(from, direction, local, window)
                                && t < best_t
                            {
                                best_t = t;
                                best_normal = n;
                                best_triangle = Some(self.triangles.original_index(local));
                                if early_exit {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            return if best_t <= 1.0 {
                best_triangle.map(|triangle| RayHit {
                    t: best_t,
                    normal: best_normal,
                    triangle,
                })
            } else {
                None
            };
        }

        let grid_min = self.origin;
        let grid_max =
            self.origin + V3::new(self.nx as f32, self.ny as f32, self.nz as f32) * self.cell_size;
        let mut t_min = 0.0f32;
        let mut t_max = 1.0f32;
        for axis in 0..3 {
            let (d, o, lo, hi) = match axis {
                0 => (direction.x, from.x, grid_min.x, grid_max.x),
                1 => (direction.y, from.y, grid_min.y, grid_max.y),
                _ => (direction.z, from.z, grid_min.z, grid_max.z),
            };
            if d.abs() < 1e-9 {
                if o < lo || o > hi {
                    return None;
                }
                continue;
            }
            let mut t0 = (lo - o) / d;
            let mut t1 = (hi - o) / d;
            if t0 > t1 {
                std::mem::swap(&mut t0, &mut t1);
            }
            t_min = t_min.max(t0);
            t_max = t_max.min(t1);
            if t_min > t_max {
                return None;
            }
        }

        let start = from + direction * t_min;
        let mut cx =
            (((start.x - self.origin.x) / self.cell_size).floor() as i32).clamp(0, self.nx - 1);
        let mut cy =
            (((start.y - self.origin.y) / self.cell_size).floor() as i32).clamp(0, self.ny - 1);
        let mut cz =
            (((start.z - self.origin.z) / self.cell_size).floor() as i32).clamp(0, self.nz - 1);
        let step_x = if direction.x > 0.0 {
            1
        } else if direction.x < 0.0 {
            -1
        } else {
            0
        };
        let step_y = if direction.y > 0.0 {
            1
        } else if direction.y < 0.0 {
            -1
        } else {
            0
        };
        let step_z = if direction.z > 0.0 {
            1
        } else if direction.z < 0.0 {
            -1
        } else {
            0
        };

        let next_boundary = |cell: i32, step: i32, origin: f32| {
            origin + ((cell + if step > 0 { 1 } else { 0 }) as f32) * self.cell_size
        };

        let t_delta_x = if step_x != 0 {
            self.cell_size / direction.x.abs()
        } else {
            f32::MAX
        };
        let t_delta_y = if step_y != 0 {
            self.cell_size / direction.y.abs()
        } else {
            f32::MAX
        };
        let t_delta_z = if step_z != 0 {
            self.cell_size / direction.z.abs()
        } else {
            f32::MAX
        };
        let mut t_next_x = if step_x != 0 {
            (next_boundary(cx, step_x, self.origin.x) - from.x) / direction.x
        } else {
            f32::MAX
        };
        let mut t_next_y = if step_y != 0 {
            (next_boundary(cy, step_y, self.origin.y) - from.y) / direction.y
        } else {
            f32::MAX
        };
        let mut t_next_z = if step_z != 0 {
            (next_boundary(cz, step_z, self.origin.z) - from.z) / direction.z
        } else {
            f32::MAX
        };

        let mut t_cell_enter = t_min;
        while t_cell_enter <= t_max {
            if best_t < t_cell_enter {
                break;
            }
            let cell = ((cz * self.ny + cy) * self.nx + cx) as usize;
            let mut hit_this_cell = false;
            for &local in &self.cell_tris[self.cell_range(cell)] {
                let local = local as usize;
                if let Some((t, n)) = self.hit_triangle(from, direction, local, window)
                    && t < best_t
                {
                    best_t = t;
                    best_normal = n;
                    best_triangle = Some(self.triangles.original_index(local));
                    hit_this_cell = true;
                }
            }
            if early_exit && hit_this_cell {
                break;
            }
            if t_next_x <= t_next_y && t_next_x <= t_next_z {
                t_cell_enter = t_next_x;
                cx += step_x;
                t_next_x += t_delta_x;
                if cx < 0 || cx >= self.nx {
                    break;
                }
            } else if t_next_y <= t_next_z {
                t_cell_enter = t_next_y;
                cy += step_y;
                t_next_y += t_delta_y;
                if cy < 0 || cy >= self.ny {
                    break;
                }
            } else {
                t_cell_enter = t_next_z;
                cz += step_z;
                t_next_z += t_delta_z;
                if cz < 0 || cz >= self.nz {
                    break;
                }
            }
        }

        if best_t <= 1.0 {
            best_triangle.map(|triangle| RayHit {
                t: best_t,
                normal: best_normal,
                triangle,
            })
        } else {
            None
        }
    }
}

impl Collider for UniformGrid {
    fn first_hit_hull(
        &self,
        from: V3,
        to: V3,
        half: V3,
        min_normal_z: f32,
        ignore: Option<&dyn Fn(u32) -> bool>,
    ) -> Option<HullHit> {
        let direction = to - from;
        let mut best_t = f32::MAX;
        let mut best_normal = V3::ZERO;
        let mut best_face = false;
        let mut best_triangle: Option<u32> = None;

        let lo = from.min(to) - half;
        let hi = from.max(to) + half;
        let (x0, y0, z0) = self.cell_of(lo);
        let (x1, y1, z1) = self.cell_of(hi);
        for z in z0.clamp(0, self.nz - 1)..=z1.clamp(0, self.nz - 1) {
            for y in y0.clamp(0, self.ny - 1)..=y1.clamp(0, self.ny - 1) {
                for x in x0.clamp(0, self.nx - 1)..=x1.clamp(0, self.nx - 1) {
                    let cell = ((z * self.ny + y) * self.nx + x) as usize;
                    for &local in &self.cell_tris[self.cell_range(cell)] {
                        let local = local as usize;
                        if !self.bounds_touch(local, lo, hi) {
                            continue;
                        }
                        let original = self.triangles.original_index(local);
                        if let Some(ignore) = ignore
                            && ignore(original)
                        {
                            continue;
                        }
                        let [a, b, c] = self.triangles.vertices(local);
                        if let Some(hit) = swept_box_triangle(from, direction, half, a, b, c)
                            && hit.t < best_t
                            && hit.normal.z >= min_normal_z
                        {
                            best_t = hit.t;
                            best_normal = hit.normal;
                            best_face = hit.face;
                            best_triangle = Some(original);
                        }
                    }
                }
            }
        }
        if best_t <= 1.0 {
            best_triangle.map(|triangle| HullHit {
                t: best_t,
                normal: best_normal,
                triangle,
                face: best_face,
            })
        } else {
            None
        }
    }

    fn first_hit_ray(&self, from: V3, to: V3) -> Option<RayHit> {
        self.cast(from, to, RayWindow::Physics, false)
    }

    /// Matches `TriangleRaycaster.Blocked` exactly when this grid was built
    /// with `region: None` (or a region containing every masked triangle):
    /// `TriangleRaycaster` keeps no acceleration structure and tests every
    /// triangle it was constructed with, so as long as this grid's triangle
    /// set is the same, an any-hit search over the grid's cells finds a hit
    /// iff the reference's linear scan does. A smaller `region` here means
    /// fewer candidate triangles than a `TriangleRaycaster` built without a
    /// region restriction, so `blocked` can differ from the reference in
    /// that case; this is unchanged, existing behavior, not a new
    /// restriction.
    fn blocked(&self, from: V3, to: V3) -> bool {
        self.cast(from, to, RayWindow::Sightline, true).is_some()
    }

    fn box_intersects(&self, center: V3, half: V3) -> bool {
        let (x0, y0, z0) = self.cell_of(center - half);
        let (x1, y1, z1) = self.cell_of(center + half);
        for z in z0.clamp(0, self.nz - 1)..=z1.clamp(0, self.nz - 1) {
            for y in y0.clamp(0, self.ny - 1)..=y1.clamp(0, self.ny - 1) {
                for x in x0.clamp(0, self.nx - 1)..=x1.clamp(0, self.nx - 1) {
                    let cell = ((z * self.ny + y) * self.nx + x) as usize;
                    for &local in &self.cell_tris[self.cell_range(cell)] {
                        let [a, b, c] = self.triangles.vertices(local as usize);
                        if tri_box_overlap(center, half, a, b, c) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn triangle(&self, t: u32) -> [V3; 3] {
        let local = self
            .triangles
            .local_index(t)
            .expect("triangle index not in this collider");
        self.triangles.vertices(local)
    }

    fn is_breakable(&self, t: u32) -> bool {
        let local = self
            .triangles
            .local_index(t)
            .expect("triangle index not in this collider");
        self.triangles.is_breakable_local(local)
    }

    fn triangle_count(&self) -> usize {
        self.triangles.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::all_mask;
    use crate::mesh::{CollisionAttribute, MeshObject, ObjectKind, SurfaceProperty};

    /// A tiny fixed-seed xorshift32 PRNG - the spec forbids a `rand`
    /// dependency for these differential tests.
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

    fn brute_first_hit_ray(mesh: &CollisionMesh, from: V3, to: V3) -> Option<(f32, V3, u32)> {
        let direction = to - from;
        let mut best: Option<(f32, V3, u32)> = None;
        for (i, tri) in mesh.triangles.iter().enumerate() {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            if let Some(t) = crate::tri::moller_trumbore(from, direction, a, b, c)
                && RayWindow::Physics.accepts(t)
                && best.is_none_or(|(bt, _, _)| t < bt)
            {
                let mut normal = (b - a).cross(c - a).normalize();
                if normal.dot(direction) > 0.0 {
                    normal = -normal;
                }
                best = Some((t, normal, i as u32));
            }
        }
        best
    }

    fn brute_blocked(mesh: &CollisionMesh, from: V3, to: V3) -> bool {
        let direction = to - from;
        mesh.triangles.iter().any(|tri| {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            crate::tri::moller_trumbore(from, direction, a, b, c)
                .is_some_and(|t| RayWindow::Sightline.accepts(t))
        })
    }

    fn brute_first_hit_hull(
        mesh: &CollisionMesh,
        from: V3,
        to: V3,
        half: V3,
        min_normal_z: f32,
    ) -> Option<(f32, V3, u32, bool)> {
        let direction = to - from;
        let mut best: Option<(f32, V3, u32, bool)> = None;
        for (i, tri) in mesh.triangles.iter().enumerate() {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            if let Some(hit) = swept_box_triangle(from, direction, half, a, b, c)
                && hit.normal.z >= min_normal_z
                && best.is_none_or(|(bt, _, _, _)| hit.t < bt)
            {
                best = Some((hit.t, hit.normal, i as u32, hit.face));
            }
        }
        best
    }

    fn brute_box_intersects(mesh: &CollisionMesh, center: V3, half: V3) -> bool {
        mesh.triangles.iter().any(|tri| {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            tri_box_overlap(center, half, a, b, c)
        })
    }

    #[test]
    fn grid_matches_brute_force_on_random_soup() {
        let mut rng = Rng::new(0xC0FFEE);
        let mesh = random_mesh(&mut rng, 200, 80.0, 15.0);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 8.0).unwrap();

        // Tie rule note: `UniformGrid` resolves an exact-`t` tie between two
        // triangles to whichever it visits first in its cell iteration order
        // (z, y, x, then ascending index within a cell) - bit-identical to
        // the reference `TriangleCollider` for the same region/cell size,
        // but not necessarily the lowest original mesh triangle index across
        // cells. `brute_first_hit_ray`/`brute_first_hit_hull` below use a
        // plain ascending-index scan instead, so on a genuine tie they can
        // legitimately pick a *different* (but equally valid, same `t`)
        // triangle/normal/face than the grid. Only `t` (and hit/no-hit) are
        // required to match exactly in that case.
        for _ in 0..10_000 {
            let from = rng.v3(-100.0, 100.0);
            let to = rng.v3(-100.0, 100.0);
            let grid_hit = grid.first_hit_ray(from, to);
            let brute = brute_first_hit_ray(&mesh, from, to);
            match (grid_hit, brute) {
                (Some(g), Some(b)) => {
                    assert_eq!(g.t, b.0);
                    if g.triangle == b.2 {
                        assert_eq!(g.normal.to_array(), b.1.to_array());
                    }
                }
                (None, None) => {}
                (g, b) => panic!("mismatch: grid={g:?} brute={b:?} from={from:?} to={to:?}"),
            }
        }

        for _ in 0..10_000 {
            let from = rng.v3(-100.0, 100.0);
            let to = rng.v3(-100.0, 100.0);
            let half = V3::new(rng.f32(0.5, 3.0), rng.f32(0.5, 3.0), rng.f32(0.5, 3.0));
            let grid_hit = grid.first_hit_hull(from, to, half, -2.0, None);
            let brute = brute_first_hit_hull(&mesh, from, to, half, -2.0);
            match (grid_hit, brute) {
                (Some(g), Some(b)) => {
                    assert_eq!(g.t, b.0);
                    if g.triangle == b.2 {
                        assert_eq!(g.normal.to_array(), b.1.to_array());
                        assert_eq!(g.face, b.3);
                    }
                }
                (None, None) => {}
                (g, b) => panic!(
                    "hull mismatch: grid={g:?} brute={b:?} from={from:?} to={to:?} half={half:?}"
                ),
            }
        }

        for _ in 0..10_000 {
            let center = rng.v3(-100.0, 100.0);
            let half = V3::new(rng.f32(0.5, 5.0), rng.f32(0.5, 5.0), rng.f32(0.5, 5.0));
            assert_eq!(
                grid.box_intersects(center, half),
                brute_box_intersects(&mesh, center, half)
            );
        }
    }

    /// `region: None` parity: with no region restriction, every triangle
    /// `all_mask` keeps solid ends up in the grid, so `blocked` must match a
    /// full linear scan over the same triangles - exactly the semantics
    /// `TriangleRaycaster.Blocked` has (see `Collider::blocked`'s doc on
    /// `UniformGrid` for the caveat when `region` narrows the triangle set).
    #[test]
    fn blocked_matches_reference_semantics_with_region_none() {
        let mut rng = Rng::new(0xBEEF);
        let mesh = random_mesh(&mut rng, 150, 60.0, 10.0);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 8.0).unwrap();
        for _ in 0..5_000 {
            let from = rng.v3(-80.0, 80.0);
            let to = rng.v3(-80.0, 80.0);
            assert_eq!(grid.blocked(from, to), brute_blocked(&mesh, from, to));
        }
    }

    #[test]
    fn grid_matches_brute_force_on_long_rays() {
        // Exercise the DDA branch (span > 3 cells) specifically.
        let mut rng = Rng::new(0x1234_5678);
        let mesh = random_mesh(&mut rng, 100, 200.0, 20.0);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 8.0).unwrap();
        for _ in 0..2_000 {
            let from = rng.v3(-400.0, 400.0);
            let to = rng.v3(-400.0, 400.0);
            let grid_hit = grid.first_hit_ray(from, to);
            let brute = brute_first_hit_ray(&mesh, from, to);
            match (grid_hit, brute) {
                // Same tie caveat as `grid_matches_brute_force_on_random_soup`.
                (Some(g), Some(b)) => assert_eq!(g.t, b.0),
                (None, None) => {}
                (g, b) => panic!("long-ray mismatch: grid={g:?} brute={b:?}"),
            }
        }
    }

    #[test]
    fn empty_mesh_builds_and_reports_no_hits() {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 8.0).unwrap();
        assert_eq!(grid.triangle_count(), 0);
        assert!(
            grid.first_hit_ray(V3::ZERO, V3::new(10.0, 0.0, 0.0))
                .is_none()
        );
        assert!(!grid.blocked(V3::ZERO, V3::new(10.0, 0.0, 0.0)));
        assert!(!grid.box_intersects(V3::ZERO, V3::new(1.0, 1.0, 1.0)));
    }

    #[test]
    fn triangle_flush_with_region_upper_bound_is_found() {
        // Regression test for the reference's off-by-one boundary bug (see
        // `UniformGrid::build`'s rustdoc): a triangle exactly on the grid's
        // own upper Z bound must still be reachable by every query.
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
        // Floor triangle at z=0, establishing region.min.z = 0.
        mesh.push_triangles(
            &[[-50.0, -50.0, 0.0], [50.0, -50.0, 0.0], [-50.0, 50.0, 0.0]],
            &[[0, 1, 2]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        // Ceiling triangle flush with the region's exact upper Z bound: with
        // cell_size=16 and region z in [0, 160], (160-0)/16 == 10.0 exactly,
        // so `floor` maps z=160 to cell index 10 == nz, one past the last
        // valid cell (9).
        mesh.push_triangles(
            &[
                [-10.0, -10.0, 160.0],
                [10.0, -10.0, 160.0],
                [-10.0, 10.0, 160.0],
            ],
            &[[0, 1, 2]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 16.0).unwrap();
        assert_eq!(grid.nz, 10);
        assert_eq!(grid.triangle_count(), 2);

        let ray_hit = grid.first_hit_ray(V3::new(0.0, 0.0, 200.0), V3::new(0.0, 0.0, 150.0));
        assert_eq!(ray_hit.map(|h| h.triangle), Some(1));

        let hull_hit = grid.first_hit_hull(
            V3::new(0.0, 0.0, 165.0),
            V3::new(0.0, 0.0, 155.0),
            V3::new(1.0, 1.0, 1.0),
            -2.0,
            None,
        );
        assert_eq!(hull_hit.map(|h| h.triangle), Some(1));

        assert!(grid.box_intersects(V3::new(0.0, 0.0, 160.0), V3::new(1.0, 1.0, 1.0)));
    }

    #[test]
    fn non_finite_vertex_rejected() {
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
        // Bypass CollisionMesh::push_triangles' own finite check by writing
        // fields directly, to exercise ColliderTriangles' own guard.
        mesh.vertices = vec![[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        mesh.triangles = vec![[0, 1, 2]];
        mesh.tri_attribute = vec![attr];
        mesh.tri_surface = vec![SurfaceProperty::NONE];
        mesh.tri_object = vec![obj];
        let mask = all_mask(&mesh);
        let err = UniformGrid::build(&mesh, &mask, None, 8.0).unwrap_err();
        assert!(matches!(
            err,
            ColliderError::NonFiniteVertex { triangle: 0 }
        ));
    }
}
