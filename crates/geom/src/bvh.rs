//! `Bvh`: a binned-SAH bounding volume hierarchy `Collider` over
//! `ColliderTriangles`, built to answer every query matching `UniformGrid`
//! (and brute force) to within one documented, bounded exception (see
//! below): candidate pruning here is always a conservative lower bound on
//! the true contact/hit time, so no candidate that could tie-or-beat the
//! current best is skipped, and the shared `t < best || (t == best &&
//! original < best_original)` tie rule (`collider.rs`) makes the answer
//! independent of traversal order.
//!
//! **Node bounds are padded by [`PAD`]`= 0.01`** (`make_leaf`, and the
//! internal-node bounds computed in `build_recursive`). Without this, a ray
//! passing exactly through a triangle's edge or vertex can be accepted by
//! `moller_trumbore` (its `u`/`v` barycentric test has its own rounding
//! slack) at a point numerically *outside* the triangle's own tight AABB,
//! so a node whose (unpadded) bounds don't quite reach that point would be
//! wrongly pruned by [`ray_aabb`]'s slab test, silently dropping a real hit
//! (or, for `blocked`, an occlusion). Padding every node's bounds by a fixed
//! margin comfortably larger than `f32` rounding at real map coordinate
//! magnitudes closes that gap. (Previously this crate tried to paper over
//! the symptom with a `t`-space epsilon on the prune comparison; that
//! doesn't scale - a fixed fraction of `t` is a wildly different world-space
//! distance depending on segment length - and didn't address the actual
//! cause. It has been removed now that node bounds themselves are padded.)
//!
//! **Documented residual exception**: a ray that lies almost exactly *in* a
//! triangle's plane (`direction` nearly perpendicular to the triangle
//! normal, `|cos(direction, normal)| ≲ 1.5e-6`) can make `moller_trumbore`
//! report a spurious accept/reject that's sensitive to which specific
//! triangle's vertex data computed it - this is a property of the ported
//! reference `MollerTrumbore` primitive itself (shared by `UniformGrid` and
//! `Bvh` alike), not of tree traversal order. Measured on real map data:
//! ~0 in 130,000 uniformly random rays, but ~1 in 2,500-5,000 rays aimed
//! exactly at a triangle vertex or edge midpoint (the case that most
//! stresses this, see `geom_real_mesh.rs`'s seam-probe test). `Bvh` vs
//! `UniformGrid` can also differ where the grid (which is reference-exact)
//! misses hits landing exactly on a cell corner or edge - unrelated to
//! `Bvh`'s own correctness, but visible in differential tests (e.g. 23/20,000
//! grenade-mask seam rays on mirage). Differential tests here count and
//! bound both classes rather than asserting zero.
//!
//! Node layout is the classic flattened "left child = this + 1, explicit
//! right child index" scheme (e.g. pbrt's `LinearBVHNode`): 32 bytes, one
//! child pointer, `count == 0` marks an internal node.

use crate::collider::{Collider, ColliderError, ColliderTriangles, HullHit, RayHit};
use crate::filter::AttributeMask;
use crate::math::{Aabb, V3};
use crate::mesh::CollisionMesh;
use crate::tri::{RayWindow, moller_trumbore, swept_box_triangle, tri_box_overlap};

const LEAF_SIZE: usize = 4;
const BINS: usize = 12;
/// Fixed traversal stack depth. Every split below (SAH bin or fallback) is
/// forced to be non-trivial (`build_recursive` switches to an exact
/// centroid-median split - guaranteed ~50/50 - once `depth` exceeds
/// [`FORCE_MEDIAN_DEPTH`]), so the tree height is bounded by
/// `FORCE_MEDIAN_DEPTH + ceil(log2(n))` regardless of how adversarial the
/// input geometry is; 96 comfortably covers that for any real or synthetic
/// triangle count used here.
const STACK_DEPTH: usize = 96;
/// Below this depth, `choose_split` is free to pick whatever axis/bin the
/// binned-SAH cost favours (which can occasionally be a very lopsided split
/// for adversarial centroid distributions). At or beyond it, every split is
/// forced to an exact centroid-median (`choose_split(.., force_median:
/// true)`), which always halves the triangle count, bounding total depth to
/// `FORCE_MEDIAN_DEPTH + ceil(log2(n))` and keeping `STACK_DEPTH` safe.
const FORCE_MEDIAN_DEPTH: u32 = 48;
/// Node bounds are inflated by this much on every axis (`make_leaf`, and the
/// internal-node bounds in `build_recursive`) to absorb `moller_trumbore`'s
/// own rounding slack at triangle edges/vertices - see the module doc for
/// why this is necessary and replaces the old `t`-space prune epsilon.
const PAD: f32 = 0.01;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct BvhNode {
    min: [f32; 3],
    max: [f32; 3],
    /// Leaf (`count > 0`): start offset into `Bvh::tri_indices`.
    /// Internal (`count == 0`): index of the right child; the left child is
    /// always this node's own index + 1.
    left_first: u32,
    count: u16,
    axis: u8,
    _pad: u8,
}

impl BvhNode {
    const EMPTY: BvhNode = BvhNode {
        min: [0.0; 3],
        max: [0.0; 3],
        left_first: 0,
        count: 0,
        axis: 0,
        _pad: 0,
    };
}

const _: () = assert!(std::mem::size_of::<BvhNode>() == 32);

/// A binned-SAH BVH `Collider` over `ColliderTriangles`.
#[derive(Debug)]
pub struct Bvh {
    triangles: ColliderTriangles,
    nodes: Vec<BvhNode>,
    /// Local (`ColliderTriangles`) indices, permuted into leaf-contiguous order.
    tri_indices: Vec<u32>,
}

fn axis_component(v: V3, axis: usize) -> f32 {
    match axis {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}

fn centroid(triangles: &ColliderTriangles, local: usize) -> V3 {
    let aabb = triangles.aabb(local);
    (aabb.min + aabb.max) * 0.5
}

fn compute_bounds(order: &[u32], triangles: &ColliderTriangles) -> Aabb {
    let mut it = order.iter();
    let first = triangles.aabb(*it.next().expect("non-empty range") as usize);
    it.fold(first, |acc, &t| acc.union(triangles.aabb(t as usize)))
}

fn compute_centroid_bounds(order: &[u32], triangles: &ColliderTriangles) -> Aabb {
    let mut mn = V3::new(f32::MAX, f32::MAX, f32::MAX);
    let mut mx = V3::new(f32::MIN, f32::MIN, f32::MIN);
    for &t in order {
        let c = centroid(triangles, t as usize);
        mn = mn.min(c);
        mx = mx.max(c);
    }
    Aabb { min: mn, max: mx }
}

fn surface_area(min: [f32; 3], max: [f32; 3]) -> f32 {
    if min[0] > max[0] {
        return 0.0; // empty bin
    }
    let dx = max[0] - min[0];
    let dy = max[1] - min[1];
    let dz = max[2] - min[2];
    2.0 * (dx * dy + dy * dz + dz * dx)
}

/// Chooses a binned-SAH split axis and partitions `order` in place,
/// returning `(axis, split_pos)`. Always produces a non-trivial split
/// (`0 < split_pos < order.len()`) when `order.len() > LEAF_SIZE`, falling
/// back to a centroid-median split if every axis' centroid extent is
/// degenerate or the SAH bin boundary happens to be empty on one side.
///
/// `force_median`: when true (`build_recursive` sets this once `depth >=
/// FORCE_MEDIAN_DEPTH`), skips the SAH scan entirely and always does an
/// exact centroid-median split on the widest axis. An exact median always
/// halves `order`, which is what bounds the tree height regardless of how
/// adversarial the SAH cost landscape is for the remaining subtree.
fn choose_split(
    order: &mut [u32],
    triangles: &ColliderTriangles,
    force_median: bool,
) -> (usize, usize) {
    let centroid_bounds = compute_centroid_bounds(order, triangles);

    let median_fallback = |order: &mut [u32], axis: usize| -> usize {
        order.sort_by(|&a, &b| {
            let ca = axis_component(centroid(triangles, a as usize), axis);
            let cb = axis_component(centroid(triangles, b as usize), axis);
            ca.partial_cmp(&cb).unwrap()
        });
        order.len() / 2
    };

    if force_median {
        let mut axis = 0usize;
        let mut widest = -1.0f32;
        for a in 0..3 {
            let extent =
                axis_component(centroid_bounds.max, a) - axis_component(centroid_bounds.min, a);
            if extent > widest {
                widest = extent;
                axis = a;
            }
        }
        let boundary = median_fallback(order, axis);
        return (axis, boundary);
    }

    let mut best_axis: Option<usize> = None;
    let mut best_cost = f32::MAX;
    let mut best_bin = 0usize;

    for axis in 0..3 {
        let lo = axis_component(centroid_bounds.min, axis);
        let hi = axis_component(centroid_bounds.max, axis);
        let extent = hi - lo;
        if extent < 1e-6 {
            continue;
        }
        let mut bin_count = [0u32; BINS];
        let mut bin_min = [[f32::MAX; 3]; BINS];
        let mut bin_max = [[f32::MIN; 3]; BINS];
        for &t in order.iter() {
            let c = axis_component(centroid(triangles, t as usize), axis);
            let mut b = (((c - lo) / extent) * BINS as f32) as usize;
            if b >= BINS {
                b = BINS - 1;
            }
            bin_count[b] += 1;
            let aabb = triangles.aabb(t as usize);
            for k in 0..3 {
                bin_min[b][k] = bin_min[b][k].min(axis_component(aabb.min, k));
                bin_max[b][k] = bin_max[b][k].max(axis_component(aabb.max, k));
            }
        }

        let mut left_count = [0u32; BINS];
        let mut left_area = [0f32; BINS];
        let mut acc_count = 0u32;
        let mut acc_min = [f32::MAX; 3];
        let mut acc_max = [f32::MIN; 3];
        for i in 0..BINS {
            acc_count += bin_count[i];
            for k in 0..3 {
                acc_min[k] = acc_min[k].min(bin_min[i][k]);
                acc_max[k] = acc_max[k].max(bin_max[i][k]);
            }
            left_count[i] = acc_count;
            left_area[i] = surface_area(acc_min, acc_max);
        }

        let mut right_count = [0u32; BINS];
        let mut right_area = [0f32; BINS];
        acc_count = 0;
        acc_min = [f32::MAX; 3];
        acc_max = [f32::MIN; 3];
        for i in (0..BINS).rev() {
            acc_count += bin_count[i];
            for k in 0..3 {
                acc_min[k] = acc_min[k].min(bin_min[i][k]);
                acc_max[k] = acc_max[k].max(bin_max[i][k]);
            }
            right_count[i] = acc_count;
            right_area[i] = surface_area(acc_min, acc_max);
        }

        for split in 1..BINS {
            if left_count[split - 1] == 0 || right_count[split] == 0 {
                continue;
            }
            let cost = left_count[split - 1] as f32 * left_area[split - 1]
                + right_count[split] as f32 * right_area[split];
            if cost < best_cost {
                best_cost = cost;
                best_axis = Some(axis);
                best_bin = split;
            }
        }
    }

    let Some(axis) = best_axis else {
        // Every axis' centroid extent is degenerate (all centroids coincide):
        // any split is arbitrary, so split by array position.
        return (0, order.len() / 2);
    };

    let lo = axis_component(centroid_bounds.min, axis);
    let hi = axis_component(centroid_bounds.max, axis);
    let extent = hi - lo;

    let mut boundary = 0usize;
    for k in 0..order.len() {
        let t = order[k];
        let c = axis_component(centroid(triangles, t as usize), axis);
        let mut b = (((c - lo) / extent) * BINS as f32) as usize;
        if b >= BINS {
            b = BINS - 1;
        }
        if b < best_bin {
            order.swap(k, boundary);
            boundary += 1;
        }
    }

    if boundary == 0 || boundary == order.len() {
        boundary = median_fallback(order, axis);
    }
    (axis, boundary)
}

/// Inflates `bounds` by [`PAD`] on every axis - see the module doc.
fn pad(bounds: Aabb) -> Aabb {
    let p = V3::new(PAD, PAD, PAD);
    Aabb {
        min: bounds.min - p,
        max: bounds.max + p,
    }
}

fn make_leaf(bounds: Aabb, start: usize, count: usize) -> BvhNode {
    let bounds = pad(bounds);
    BvhNode {
        min: bounds.min.to_array(),
        max: bounds.max.to_array(),
        left_first: start as u32,
        count: count as u16,
        axis: 0,
        _pad: 0,
    }
}

fn build_recursive(
    order: &mut [u32],
    start: usize,
    triangles: &ColliderTriangles,
    nodes: &mut Vec<BvhNode>,
    depth: u32,
) -> u32 {
    let this_idx = nodes.len() as u32;
    nodes.push(BvhNode::EMPTY);
    let bounds = compute_bounds(order, triangles);
    if order.len() <= LEAF_SIZE {
        nodes[this_idx as usize] = make_leaf(bounds, start, order.len());
        return this_idx;
    }
    let (axis, split_pos) = choose_split(order, triangles, depth >= FORCE_MEDIAN_DEPTH);
    let (left, right) = order.split_at_mut(split_pos);
    let _left_idx = build_recursive(left, start, triangles, nodes, depth + 1);
    let right_idx = build_recursive(right, start + split_pos, triangles, nodes, depth + 1);
    let bounds = pad(bounds);
    nodes[this_idx as usize] = BvhNode {
        min: bounds.min.to_array(),
        max: bounds.max.to_array(),
        left_first: right_idx,
        count: 0,
        axis: axis as u8,
        _pad: 0,
    };
    this_idx
}

/// Ray-vs-AABB slab test restricted to `t` in `[0, 1]` along `direction`
/// (same convention as `UniformGrid`'s DDA grid-bounds test): returns the
/// entry/exit `t`, or `None` if the segment misses `[min, max]` entirely.
fn ray_aabb(from: V3, direction: V3, min: V3, max: V3) -> Option<(f32, f32)> {
    let mut t0 = 0.0f32;
    let mut t1 = 1.0f32;
    for axis in 0..3 {
        let (o, d, lo, hi) = match axis {
            0 => (from.x, direction.x, min.x, max.x),
            1 => (from.y, direction.y, min.y, max.y),
            _ => (from.z, direction.z, min.z, max.z),
        };
        if d.abs() < 1e-12 {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let mut ta = (lo - o) / d;
        let mut tb = (hi - o) / d;
        if ta > tb {
            std::mem::swap(&mut ta, &mut tb);
        }
        t0 = t0.max(ta);
        t1 = t1.min(tb);
        if t0 > t1 {
            return None;
        }
    }
    Some((t0, t1))
}

impl Bvh {
    /// Builds a BVH over `triangles` (already filtered by mask/region), so
    /// callers can share one `ColliderTriangles` with a `UniformGrid` and
    /// guarantee both traverse the exact same triangle universe.
    pub fn from_triangles(triangles: ColliderTriangles) -> Self {
        let n = triangles.len();
        if n == 0 {
            return Bvh {
                triangles,
                nodes: vec![BvhNode::EMPTY],
                tri_indices: Vec::new(),
            };
        }
        let mut order: Vec<u32> = (0..n as u32).collect();
        let mut nodes = Vec::new();
        build_recursive(&mut order, 0, &triangles, &mut nodes, 0);
        Bvh {
            triangles,
            nodes,
            tri_indices: order,
        }
    }

    /// Builds a BVH over `mesh`'s triangles solid in `mask`, mirroring
    /// `UniformGrid::build`'s filtering semantics.
    pub fn build(
        mesh: &CollisionMesh,
        mask: &AttributeMask,
        region: Option<Aabb>,
    ) -> Result<Self, ColliderError> {
        Ok(Self::from_triangles(ColliderTriangles::build(
            mesh, mask, region,
        )?))
    }

    fn cast_ray(&self, from: V3, to: V3, window: RayWindow, early_exit: bool) -> Option<RayHit> {
        if self.tri_indices.is_empty() {
            return None;
        }
        let direction = to - from;
        let mut best_t = f32::MAX;
        let mut best_normal = V3::ZERO;
        let mut best_triangle = u32::MAX;

        let mut stack = [0u32; STACK_DEPTH];
        let mut sp = 1usize;
        stack[0] = 0;
        while sp > 0 {
            sp -= 1;
            let idx = stack[sp] as usize;
            let node = &self.nodes[idx];
            let Some((t_enter, _)) = ray_aabb(
                from,
                direction,
                V3::from_array(node.min),
                V3::from_array(node.max),
            ) else {
                continue;
            };
            if t_enter > best_t {
                continue;
            }
            if node.count > 0 {
                let start = node.left_first as usize;
                for &local in &self.tri_indices[start..start + node.count as usize] {
                    let local = local as usize;
                    let [a, b, c] = self.triangles.vertices(local);
                    let Some(t) = moller_trumbore(from, direction, a, b, c) else {
                        continue;
                    };
                    if !window.accepts(t) {
                        continue;
                    }
                    let original = self.triangles.original_index(local);
                    if t < best_t || (t == best_t && original < best_triangle) {
                        let mut n = (b - a).cross(c - a).normalize();
                        if n.dot(direction) > 0.0 {
                            n = -n;
                        }
                        best_t = t;
                        best_normal = n;
                        best_triangle = original;
                        if early_exit {
                            return Some(RayHit {
                                t: best_t,
                                normal: best_normal,
                                triangle: best_triangle,
                            });
                        }
                    }
                }
            } else {
                // Near-child-first: `left` (this node's own index + 1)
                // holds the lower-coordinate-value half of `node.axis`
                // (`choose_split`'s partition convention), `right` the
                // upper half. Whichever side `direction` is heading away
                // from zero on that axis is entered first, so pushing the
                // far child before the near child (stack is LIFO) means the
                // near child is explored - and can tighten `best_t` - before
                // the far child is even popped, maximising how often the
                // `t_enter > best_t` prune above actually fires.
                let left = idx as u32 + 1;
                let right = node.left_first;
                let dir_comp = match node.axis {
                    0 => direction.x,
                    1 => direction.y,
                    _ => direction.z,
                };
                let (near, far) = if dir_comp >= 0.0 {
                    (left, right)
                } else {
                    (right, left)
                };
                stack[sp] = far;
                sp += 1;
                stack[sp] = near;
                sp += 1;
            }
        }
        if best_t <= 1.0 {
            Some(RayHit {
                t: best_t,
                normal: best_normal,
                triangle: best_triangle,
            })
        } else {
            None
        }
    }
}

impl Collider for Bvh {
    fn first_hit_hull(
        &self,
        from: V3,
        to: V3,
        half: V3,
        min_normal_z: f32,
        ignore: Option<&dyn Fn(u32) -> bool>,
    ) -> Option<HullHit> {
        if self.tri_indices.is_empty() {
            return None;
        }
        let direction = to - from;
        let lo = from.min(to) - half;
        let hi = from.max(to) + half;

        let mut best_t = f32::MAX;
        let mut best_normal = V3::ZERO;
        let mut best_triangle = u32::MAX;
        let mut best_face = false;

        let mut stack = [0u32; STACK_DEPTH];
        let mut sp = 1usize;
        stack[0] = 0;
        while sp > 0 {
            sp -= 1;
            let idx = stack[sp] as usize;
            let node = &self.nodes[idx];
            let node_min = V3::from_array(node.min) - half;
            let node_max = V3::from_array(node.max) + half;
            let Some((t_enter, _)) = ray_aabb(from, direction, node_min, node_max) else {
                continue;
            };
            if t_enter > best_t {
                continue;
            }
            if node.count > 0 {
                let start = node.left_first as usize;
                for &local in &self.tri_indices[start..start + node.count as usize] {
                    let local = local as usize;
                    let aabb = self.triangles.aabb(local);
                    let touches = aabb.max.x >= lo.x
                        && aabb.min.x <= hi.x
                        && aabb.max.y >= lo.y
                        && aabb.min.y <= hi.y
                        && aabb.max.z >= lo.z
                        && aabb.min.z <= hi.z;
                    if !touches {
                        continue;
                    }
                    let original = self.triangles.original_index(local);
                    if let Some(ig) = ignore
                        && ig(original)
                    {
                        continue;
                    }
                    let [a, b, c] = self.triangles.vertices(local);
                    if let Some(hit) = swept_box_triangle(from, direction, half, a, b, c) {
                        let better =
                            hit.t < best_t || (hit.t == best_t && original < best_triangle);
                        if better && hit.normal.z >= min_normal_z {
                            best_t = hit.t;
                            best_normal = hit.normal;
                            best_triangle = original;
                            best_face = hit.face;
                        }
                    }
                }
            } else {
                stack[sp] = idx as u32 + 1;
                sp += 1;
                stack[sp] = node.left_first;
                sp += 1;
            }
        }
        if best_t <= 1.0 {
            Some(HullHit {
                t: best_t,
                normal: best_normal,
                triangle: best_triangle,
                face: best_face,
            })
        } else {
            None
        }
    }

    fn first_hit_ray(&self, from: V3, to: V3) -> Option<RayHit> {
        self.cast_ray(from, to, RayWindow::Physics, false)
    }

    fn blocked(&self, from: V3, to: V3) -> bool {
        self.cast_ray(from, to, RayWindow::Sightline, true)
            .is_some()
    }

    fn box_intersects(&self, center: V3, half: V3) -> bool {
        if self.tri_indices.is_empty() {
            return false;
        }
        let q = Aabb {
            min: center - half,
            max: center + half,
        };
        let mut stack = [0u32; STACK_DEPTH];
        let mut sp = 1usize;
        stack[0] = 0;
        while sp > 0 {
            sp -= 1;
            let idx = stack[sp] as usize;
            let node = &self.nodes[idx];
            let node_aabb = Aabb {
                min: V3::from_array(node.min),
                max: V3::from_array(node.max),
            };
            if !node_aabb.intersects(q) {
                continue;
            }
            if node.count > 0 {
                let start = node.left_first as usize;
                for &local in &self.tri_indices[start..start + node.count as usize] {
                    let [a, b, c] = self.triangles.vertices(local as usize);
                    if tri_box_overlap(center, half, a, b, c) {
                        return true;
                    }
                }
            } else {
                stack[sp] = idx as u32 + 1;
                sp += 1;
                stack[sp] = node.left_first;
                sp += 1;
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
impl Bvh {
    /// The tree's maximum root-to-leaf depth (root = depth 1), computed by
    /// walking `nodes` (index 0 is the root; `left = idx + 1`). Tests assert
    /// this stays below `STACK_DEPTH`, since a deeper tree would silently
    /// overflow the fixed traversal stacks used by every query.
    fn max_depth(&self) -> usize {
        fn walk(nodes: &[BvhNode], idx: usize, depth: usize) -> usize {
            let node = &nodes[idx];
            if node.count > 0 {
                depth
            } else {
                let left = walk(nodes, idx + 1, depth + 1);
                let right = walk(nodes, node.left_first as usize, depth + 1);
                left.max(right)
            }
        }
        walk(&self.nodes, 0, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::all_mask;
    use crate::grid::UniformGrid;
    use crate::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    /// Fixed-seed xorshift32 PRNG (no `rand` dependency, matches `grid.rs`).
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
            if let Some(t) = moller_trumbore(from, direction, a, b, c)
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

    fn brute_blocked(mesh: &CollisionMesh, from: V3, to: V3) -> bool {
        let direction = to - from;
        mesh.triangles.iter().any(|tri| {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            moller_trumbore(from, direction, a, b, c)
                .is_some_and(|t| RayWindow::Sightline.accepts(t))
        })
    }

    #[test]
    fn node_size_is_32_bytes() {
        assert_eq!(std::mem::size_of::<BvhNode>(), 32);
    }

    /// `force_median: true` always halves `order` on the widest centroid
    /// axis, regardless of the SAH cost landscape - the property that bounds
    /// `Bvh`'s depth once `build_recursive` hits `FORCE_MEDIAN_DEPTH`.
    #[test]
    fn choose_split_force_median_splits_widest_axis_in_half() {
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
        // Widest centroid extent is along x (0..99), y and z are constant -
        // `force_median` must pick axis 0 and split the 100 triangles evenly.
        for i in 0..100u32 {
            let x = i as f32;
            mesh.push_triangles(
                &[[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, 0.0]],
                &[[0, 1, 2]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
        }
        let mask = all_mask(&mesh);
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let mut order: Vec<u32> = (0..triangles.len() as u32).collect();
        let (axis, split_pos) = choose_split(&mut order, &triangles, true);
        assert_eq!(axis, 0);
        assert_eq!(split_pos, order.len() / 2);
    }

    /// `UniformGrid` resolves exact-`t` ties in reference cell-visitation
    /// order (matching the C# original bit-for-bit), while `Bvh` resolves
    /// them by lowest original triangle index (deterministic, traversal-
    /// order independent) — see `collider.rs`'s tie-rule doc. So a
    /// triangle-index mismatch is only acceptable when both candidates are a
    /// *genuine* tie: independently re-testing each triangle against the
    /// same query must reproduce the exact same reported `t`.
    fn confirm_ray_tie(
        grid: &UniformGrid,
        from: V3,
        direction: V3,
        ta: u32,
        tb: u32,
        t: f32,
    ) -> bool {
        let [a0, b0, c0] = grid.triangle(ta);
        let [a1, b1, c1] = grid.triangle(tb);
        moller_trumbore(from, direction, a0, b0, c0) == Some(t)
            && moller_trumbore(from, direction, a1, b1, c1) == Some(t)
    }

    fn confirm_hull_tie(
        grid: &UniformGrid,
        from: V3,
        direction: V3,
        half: V3,
        ta: u32,
        tb: u32,
        t: f32,
    ) -> bool {
        let [a0, b0, c0] = grid.triangle(ta);
        let [a1, b1, c1] = grid.triangle(tb);
        swept_box_triangle(from, direction, half, a0, b0, c0).map(|h| h.t) == Some(t)
            && swept_box_triangle(from, direction, half, a1, b1, c1).map(|h| h.t) == Some(t)
    }

    #[test]
    fn bvh_matches_grid_and_brute_force_50k_queries() {
        let mut rng = Rng::new(0x0B00_B1E5);
        let mesh = random_mesh(&mut rng, 400, 100.0, 18.0);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 12.0).unwrap();
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let bvh = Bvh::from_triangles(triangles);
        assert!(
            bvh.max_depth() < STACK_DEPTH,
            "tree depth {} must stay below STACK_DEPTH {STACK_DEPTH}",
            bvh.max_depth()
        );

        let mut ray_ties = 0u64;
        let mut ray_ties_changed_normal = 0u64;
        for _ in 0..50_000 {
            let from = rng.v3(-130.0, 130.0);
            let to = rng.v3(-130.0, 130.0);
            let bvh_hit = bvh.first_hit_ray(from, to);
            let grid_hit = grid.first_hit_ray(from, to);
            let brute = brute_first_hit_ray(&mesh, from, to);
            match (bvh_hit, grid_hit, brute) {
                (Some(bv), Some(g), Some(b)) => {
                    assert_eq!(bv.t, g.t, "t must match exactly: bvh={bv:?} grid={g:?}");
                    assert_eq!(bv.t, b.0);
                    assert_eq!(bv.triangle, b.2);
                    assert_eq!(bv.normal.to_array(), b.1.to_array());
                    if bv.triangle != g.triangle {
                        assert!(
                            confirm_ray_tie(&grid, from, to - from, bv.triangle, g.triangle, bv.t),
                            "not a genuine tie: bvh={bv:?} grid={g:?} from={from:?} to={to:?}"
                        );
                        ray_ties += 1;
                        if bv.normal.to_array() != g.normal.to_array() {
                            ray_ties_changed_normal += 1;
                        }
                    }
                }
                (None, None, None) => {}
                (bv, g, b) => panic!(
                    "ray mismatch: bvh={bv:?} grid={g:?} brute={b:?} from={from:?} to={to:?}"
                ),
            }
        }
        println!("ray ties: {ray_ties} (normal changed: {ray_ties_changed_normal})");

        let mut hull_ties = 0u64;
        let mut hull_ties_changed_normal = 0u64;
        for _ in 0..50_000 {
            let from = rng.v3(-130.0, 130.0);
            let to = rng.v3(-130.0, 130.0);
            let half = V3::new(rng.f32(0.5, 3.0), rng.f32(0.5, 3.0), rng.f32(0.5, 3.0));
            let bvh_hit = bvh.first_hit_hull(from, to, half, -2.0, None);
            let grid_hit = grid.first_hit_hull(from, to, half, -2.0, None);
            let brute = brute_first_hit_hull(&mesh, from, to, half, -2.0);
            match (bvh_hit, grid_hit, brute) {
                (Some(bv), Some(g), Some(b)) => {
                    assert_eq!(bv.t, g.t, "t must match exactly: bvh={bv:?} grid={g:?}");
                    assert_eq!(bv.t, b.0);
                    assert_eq!(bv.triangle, b.2);
                    assert_eq!(bv.face, b.3);
                    if bv.triangle != g.triangle {
                        assert!(
                            confirm_hull_tie(
                                &grid,
                                from,
                                to - from,
                                half,
                                bv.triangle,
                                g.triangle,
                                bv.t
                            ),
                            "not a genuine tie: bvh={bv:?} grid={g:?} from={from:?} to={to:?} half={half:?}"
                        );
                        hull_ties += 1;
                        if bv.normal.to_array() != g.normal.to_array() {
                            hull_ties_changed_normal += 1;
                        }
                    }
                }
                (None, None, None) => {}
                (bv, g, b) => panic!(
                    "hull mismatch: bvh={bv:?} grid={g:?} brute={b:?} from={from:?} to={to:?} half={half:?}"
                ),
            }
        }
        println!("hull ties: {hull_ties} (normal changed: {hull_ties_changed_normal})");

        for _ in 0..50_000 {
            let center = rng.v3(-130.0, 130.0);
            let half = V3::new(rng.f32(0.5, 5.0), rng.f32(0.5, 5.0), rng.f32(0.5, 5.0));
            let bv = bvh.box_intersects(center, half);
            let g = grid.box_intersects(center, half);
            let b = brute_box_intersects(&mesh, center, half);
            assert_eq!(bv, g);
            assert_eq!(bv, b);
        }
    }

    #[test]
    fn bvh_matches_grid_and_brute_force_blocked_and_surface_starts() {
        let mut rng = Rng::new(0xFACE_5EED);
        let mesh = random_mesh(&mut rng, 300, 90.0, 15.0);
        let mask = all_mask(&mesh);
        let grid = UniformGrid::build(&mesh, &mask, None, 12.0).unwrap();
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let bvh = Bvh::from_triangles(triangles);

        for _ in 0..50_000 {
            let from = rng.v3(-110.0, 110.0);
            let to = rng.v3(-110.0, 110.0);
            let bv = bvh.blocked(from, to);
            let g = grid.blocked(from, to);
            let b = brute_blocked(&mesh, from, to);
            assert_eq!(bv, g);
            assert_eq!(bv, b);
        }

        // Axis-aligned rays and rays starting exactly on a triangle vertex,
        // which stress the ray-aabb epsilon and moller_trumbore edge cases.
        let mut axis_ties = 0u64;
        for i in 0..mesh.triangles.len().min(2000) {
            let tri = mesh.triangles[i];
            let v0 = V3::from_array(mesh.vertices[tri[0] as usize]);
            let from = v0;
            let dirs = [
                V3::new(1.0, 0.0, 0.0),
                V3::new(0.0, 1.0, 0.0),
                V3::new(0.0, 0.0, 1.0),
                V3::new(-1.0, 0.0, 0.0),
            ];
            for d in dirs {
                let to = from + d * 200.0;
                let bv = bvh.first_hit_ray(from, to);
                let g = grid.first_hit_ray(from, to);
                match (bv, g) {
                    (Some(bv), Some(g)) => {
                        assert_eq!(bv.t, g.t);
                        if bv.triangle != g.triangle {
                            assert!(
                                confirm_ray_tie(
                                    &grid,
                                    from,
                                    to - from,
                                    bv.triangle,
                                    g.triangle,
                                    bv.t
                                ),
                                "not a genuine tie: bvh={bv:?} grid={g:?} from={from:?} to={to:?}"
                            );
                            axis_ties += 1;
                        }
                    }
                    (None, None) => {}
                    (bv, g) => {
                        panic!("axis-ray mismatch: bvh={bv:?} grid={g:?} from={from:?} to={to:?}")
                    }
                }
            }
        }
        println!("axis-ray ties: {axis_ties}");
    }

    #[test]
    fn empty_mesh_builds_and_reports_no_hits() {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bvh = Bvh::build(&mesh, &mask, None).unwrap();
        assert_eq!(bvh.triangle_count(), 0);
        assert!(
            bvh.first_hit_ray(V3::ZERO, V3::new(10.0, 0.0, 0.0))
                .is_none()
        );
        assert!(!bvh.blocked(V3::ZERO, V3::new(10.0, 0.0, 0.0)));
        assert!(!bvh.box_intersects(V3::ZERO, V3::new(1.0, 1.0, 1.0)));
    }

    /// 10k triangles all crammed into a tiny cluster (near-identical
    /// centroids) so every axis' centroid extent starts out degenerate -
    /// exactly the input that most stresses the split/depth logic. Must
    /// build and answer ordinary queries without panicking (stack overflow
    /// on the fixed `[u32; STACK_DEPTH]` traversal stacks would panic on
    /// out-of-bounds indexing, not silently corrupt memory, so a clean pass
    /// here is a real guarantee).
    #[test]
    fn pathological_shared_centroid_builds_and_queries_without_panic() {
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
        for i in 0..10_000u32 {
            // Tiny, strictly increasing offset so triangles are distinct
            // (not exact duplicates) but their x centroids span a ~0.01-unit
            // extent (not degenerate, but far below `choose_split`'s SAH
            // scan being able to usefully bin them) while y/z centroids are
            // exactly degenerate - `choose_split`'s `extent < 1e-6` guard
            // will call the y/z axes degenerate at the top levels.
            let e = i as f32 * 1e-6;
            mesh.push_triangles(
                &[[e, 0.0, 0.0], [1.0 + e, 0.0, 0.0], [e, 1.0, 0.0]],
                &[[0, 1, 2]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
        }
        let mask = all_mask(&mesh);
        let bvh = Bvh::build(&mesh, &mask, None).unwrap();
        assert_eq!(bvh.triangle_count(), 10_000);
        assert!(
            bvh.max_depth() < STACK_DEPTH,
            "tree depth {} must stay below STACK_DEPTH {STACK_DEPTH}",
            bvh.max_depth()
        );

        // Ordinary queries must still work (and not panic) afterwards.
        let hit = bvh.first_hit_ray(V3::new(0.3, 0.3, 10.0), V3::new(0.3, 0.3, -10.0));
        assert!(hit.is_some());
        assert!(!bvh.blocked(V3::new(500.0, 500.0, 500.0), V3::new(600.0, 600.0, 600.0)));
        assert!(!bvh.box_intersects(V3::new(500.0, 500.0, 500.0), V3::new(1.0, 1.0, 1.0)));
        assert!(
            bvh.first_hit_hull(
                V3::new(0.3, 0.3, 10.0),
                V3::new(0.3, 0.3, -10.0),
                V3::new(1.0, 1.0, 1.0),
                -2.0,
                None
            )
            .is_some()
        );
    }

    /// Rays through triangle vertices and edge midpoints (both a vertical
    /// direction and random ones) - the seam cases most likely to expose
    /// the AABB-vs-`moller_trumbore` rounding gap `PAD` exists to close.
    /// BVH must match brute force exactly (brute force iterates in
    /// ascending original-index order and only replaces its best on strict
    /// `t <`, so it already implements the same "lowest index wins ties"
    /// rule as `Bvh`), except for a small, explicitly bounded rate of
    /// in-plane-ray cases (see the module doc).
    #[test]
    fn bvh_matches_brute_force_on_seam_probes() {
        let mut rng = Rng::new(0x5EAF_5EED);
        let mesh = random_mesh(&mut rng, 500, 100.0, 20.0);
        let mask = all_mask(&mesh);
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let bvh = Bvh::from_triangles(triangles);

        let mut probes: Vec<V3> = Vec::new();
        for tri in &mesh.triangles {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            probes.push(a);
            probes.push(b);
            probes.push(c);
            probes.push((a + b) * 0.5);
            probes.push((b + c) * 0.5);
            probes.push((c + a) * 0.5);
        }

        let mut mismatches = 0u64;
        let mut total = 0u64;
        for p in &probes {
            let mut dirs = vec![V3::new(0.0, 0.0, 1.0), V3::new(0.0, 0.0, -1.0)];
            dirs.push(rng.v3(-1.0, 1.0));
            dirs.push(rng.v3(-1.0, 1.0));
            for d in dirs {
                if d.length_squared() < 1e-6 {
                    continue;
                }
                let from = *p + d * 50.0;
                let to = *p - d * 50.0;
                total += 1;
                let bv = bvh.first_hit_ray(from, to);
                let br = brute_first_hit_ray(&mesh, from, to);
                let matches = match (bv, br) {
                    (Some(bv), Some(br)) => bv.t == br.0 && bv.triangle == br.2,
                    (None, None) => true,
                    _ => false,
                };
                if !matches {
                    mismatches += 1;
                }
            }
        }
        println!("seam probe mismatches: {mismatches}/{total}");
        // Bounded, not zero: documented in-plane-ray exception (module doc).
        assert!(
            (mismatches as f64) <= 0.002 * total as f64,
            "too many seam probe mismatches: {mismatches}/{total}"
        );
    }

    /// Hull sweeps starting exactly on a surface, offset a hair along its
    /// own normal on either side (the "start-overlap" scenario `tri.rs`'s
    /// own unit tests cover for the primitive itself). Must match brute
    /// force exactly - `swept_box_triangle`'s start-overlap rule doesn't
    /// have the in-plane-ray ambiguity `moller_trumbore` does, so no
    /// tolerance is needed here.
    #[test]
    fn bvh_matches_brute_force_hull_sweep_starting_on_surface() {
        let mut rng = Rng::new(0x50FA_CE00);
        let mesh = random_mesh(&mut rng, 300, 80.0, 15.0);
        let mask = all_mask(&mesh);
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let bvh = Bvh::from_triangles(triangles);
        let half = V3::new(1.0, 1.0, 1.0);

        let mut mismatches = 0u64;
        let mut total = 0u64;
        for tri in mesh.triangles.iter().take(300) {
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            let centroid = (a + b + c) / 3.0;
            let raw_normal = (b - a).cross(c - a);
            let len2 = raw_normal.length_squared();
            if len2 < 1e-12 {
                continue;
            }
            let normal = raw_normal / len2.sqrt();
            for sign in [1.0f32, -1.0] {
                let start = centroid + normal * (sign * 0.05);
                let to = start + normal * (-sign * 4.0);
                total += 1;
                let bv = bvh.first_hit_hull(start, to, half, -2.0, None);
                let br = brute_first_hit_hull(&mesh, start, to, half, -2.0);
                let matches = match (bv, br) {
                    (Some(bv), Some(br)) => bv.t == br.0 && bv.triangle == br.2,
                    (None, None) => true,
                    _ => false,
                };
                if !matches {
                    mismatches += 1;
                    println!(
                        "surface-start mismatch: start={start:?} to={to:?} bvh={bv:?} brute={br:?}"
                    );
                }
            }
        }
        println!("surface-start hull sweep mismatches: {mismatches}/{total}");
        assert_eq!(mismatches, 0);
    }

    /// Map-like geometry (a 32x32 grid of 64u floor quads plus a full grid
    /// of vertical 16u walls along every grid line, offset out to real map
    /// coordinate magnitudes) probed at every shared vertex and edge
    /// midpoint - exactly the seams `PAD` exists to keep from being pruned.
    /// Rays lying (near-)in a triangle's own plane are excluded by
    /// construction (`|cos(dir, n)| < 1e-3` against every triangle incident
    /// to that point is rejected), so unlike the seam-probe tests above this
    /// one asserts an *exact* match against brute force, not a bounded rate.
    #[test]
    fn bvh_matches_brute_force_on_map_like_seams() {
        use std::collections::HashMap;

        fn key(p: V3) -> (u32, u32, u32) {
            (p.x.to_bits(), p.y.to_bits(), p.z.to_bits())
        }

        fn add_tri(
            mesh: &mut CollisionMesh,
            attr: u16,
            obj: u32,
            incident: &mut HashMap<(u32, u32, u32), (V3, Vec<V3>)>,
            a: V3,
            b: V3,
            c: V3,
        ) {
            let n = (b - a).cross(c - a).normalize();
            mesh.push_triangles(
                &[a.to_array(), b.to_array(), c.to_array()],
                &[[0, 1, 2]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
            for p in [a, b, c, (a + b) * 0.5, (b + c) * 0.5, (c + a) * 0.5] {
                incident
                    .entry(key(p))
                    .or_insert_with(|| (p, Vec::new()))
                    .1
                    .push(n);
            }
        }

        const N: i32 = 32;
        const CELL: f32 = 64.0;
        const WALL_H: f32 = 16.0;
        const OX: f32 = 2000.0;
        const OY: f32 = 2000.0;

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

        let mut incident: HashMap<(u32, u32, u32), (V3, Vec<V3>)> = HashMap::new();

        // Floor: 32x32 grid of 64u quads (2 triangles each) at z=0.
        for i in 0..N {
            for j in 0..N {
                let x0 = OX + i as f32 * CELL;
                let x1 = OX + (i + 1) as f32 * CELL;
                let y0 = OY + j as f32 * CELL;
                let y1 = OY + (j + 1) as f32 * CELL;
                let v00 = V3::new(x0, y0, 0.0);
                let v10 = V3::new(x1, y0, 0.0);
                let v01 = V3::new(x0, y1, 0.0);
                let v11 = V3::new(x1, y1, 0.0);
                add_tri(&mut mesh, attr, obj, &mut incident, v00, v10, v11);
                add_tri(&mut mesh, attr, obj, &mut incident, v00, v11, v01);
            }
        }
        // Vertical walls along every x grid-line (varying y), 16u tall.
        for i in 0..=N {
            let x = OX + i as f32 * CELL;
            for j in 0..N {
                let y0 = OY + j as f32 * CELL;
                let y1 = OY + (j + 1) as f32 * CELL;
                let a = V3::new(x, y0, 0.0);
                let b = V3::new(x, y1, 0.0);
                let c = V3::new(x, y1, WALL_H);
                let d = V3::new(x, y0, WALL_H);
                add_tri(&mut mesh, attr, obj, &mut incident, a, b, c);
                add_tri(&mut mesh, attr, obj, &mut incident, a, c, d);
            }
        }
        // Vertical walls along every y grid-line (varying x), 16u tall.
        for j in 0..=N {
            let y = OY + j as f32 * CELL;
            for i in 0..N {
                let x0 = OX + i as f32 * CELL;
                let x1 = OX + (i + 1) as f32 * CELL;
                let a = V3::new(x0, y, 0.0);
                let b = V3::new(x1, y, 0.0);
                let c = V3::new(x1, y, WALL_H);
                let d = V3::new(x0, y, WALL_H);
                add_tri(&mut mesh, attr, obj, &mut incident, a, b, c);
                add_tri(&mut mesh, attr, obj, &mut incident, a, c, d);
            }
        }

        let mask = all_mask(&mesh);
        let triangles = ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let bvh = Bvh::from_triangles(triangles);

        let mut rng = Rng::new(0x5EED_5EED);
        const REACH: f32 = 6000.0;

        let valid_dir = |rng: &mut Rng, normals: &[V3]| -> V3 {
            loop {
                let d = rng.v3(-1.0, 1.0);
                let len2 = d.length_squared();
                if !(1e-6..=1.0).contains(&len2) {
                    continue;
                }
                let dn = d / len2.sqrt();
                if normals.iter().all(|n| dn.dot(*n).abs() >= 1e-3) {
                    return dn;
                }
            }
        };

        let mut total = 0u64;
        for (point, normals) in incident.values() {
            for _ in 0..2 {
                let d = valid_dir(&mut rng, normals);
                let from = *point + d * REACH;
                let to = *point - d * REACH;
                total += 1;

                let bv = bvh.first_hit_ray(from, to);
                let br = brute_first_hit_ray(&mesh, from, to);
                match (bv, br) {
                    (Some(bv), Some(br)) => {
                        assert_eq!(
                            bv.t.to_bits(),
                            br.0.to_bits(),
                            "t mismatch: bvh={bv:?} brute_t={} from={from:?} to={to:?}",
                            br.0
                        );
                        assert_eq!(
                            bv.triangle, br.2,
                            "triangle mismatch: bvh={bv:?} brute={br:?} from={from:?} to={to:?}"
                        );
                    }
                    (None, None) => {}
                    (bv, br) => {
                        panic!("ray mismatch: bvh={bv:?} brute={br:?} from={from:?} to={to:?}")
                    }
                }

                let bvb = bvh.blocked(from, to);
                let brb = brute_blocked(&mesh, from, to);
                assert_eq!(
                    bvb, brb,
                    "blocked mismatch: bvh={bvb} brute={brb} from={from:?} to={to:?}"
                );
            }
        }
        println!("map-like seam probes: {total} rays, 0 mismatches (exact)");
    }
}
