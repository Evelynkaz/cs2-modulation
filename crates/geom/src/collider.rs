//! The `Collider` trait, its hit/ray result types, and `ColliderTriangles`:
//! a compact, mesh-independent per-triangle vertex+AABB+breakable copy
//! shared by every acceleration structure (`UniformGrid`, and the Part B
//! `Bvh`) so they answer identical queries over identical triangle data.

use thiserror::Error;

use crate::filter::AttributeMask;
use crate::math::{Aabb, V3};
use crate::mesh::CollisionMesh;

#[derive(Debug, Error)]
pub enum ColliderError {
    #[error("non-finite vertex coordinate in mesh triangle {triangle}")]
    NonFiniteVertex { triangle: u32 },
}

/// The result of a swept-hull query (`Collider::first_hit_hull`), ported
/// from `TriangleCollider.Sat.cs:FirstHitHullIndexed`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HullHit {
    pub t: f32,
    pub normal: V3,
    /// Original `CollisionMesh` triangle index.
    pub triangle: u32,
    pub face: bool,
}

/// The result of a closest-hit ray query (`Collider::first_hit_ray`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    pub t: f32,
    /// Oriented against the ray direction, like `HitTriangle`.
    pub normal: V3,
    /// Original `CollisionMesh` triangle index.
    pub triangle: u32,
}

/// A queryable triangle acceleration structure. All triangle indices
/// (`HullHit::triangle`, `RayHit::triangle`, and the arguments to
/// `triangle`/`is_breakable`) are **original `CollisionMesh` triangle
/// indices**, not indices local to this collider's internal storage, so a
/// hit can always be traced back to `CollisionMesh::tri_attribute` etc.
///
/// Tie rule: the two colliders in this crate resolve an exact-`t` tie
/// *differently*, and that's deliberate, not an oversight:
/// - `UniformGrid` matches the reference bit-for-bit: candidates only
///   replace the current best on strict `t < best`, and cells (then
///   triangles within a cell) are visited in the same order the reference
///   does, so ties resolve to "first encountered in cell order" exactly as
///   the reference does.
/// - `Bvh` instead always resolves an exact-`t` tie to the **lowest original
///   mesh triangle index**, independent of node-traversal order (its own
///   internal comparison is `t < best || (t == best && original <
///   best_original)`). This is *not* generally the same triangle
///   `UniformGrid` picks: which of two coplanar/adjacent triangles a grid
///   cell walk happens to visit first is an accident of cell geometry, not
///   of triangle index. On the real mirage mesh, differential testing found
///   grenade-mask hull-sweep ties changed the reported bounce normal in
///   15/23 cases and player-mask hull-sweep ties in 19/34 cases (see
///   `bvh.rs`'s and `geom_real_mesh.rs`'s differential tests) - callers that
///   need the exact reference bounce on a tie must use `UniformGrid`, not
///   `Bvh`.
///
/// Collider choice: `UniformGrid` reproduces the reference's cell-visitation
/// order exactly, but it also inherits the reference's own blind spot -
/// hits landing exactly on a cell corner or edge can be missed by its DDA
/// walk (`bvh.rs`'s module doc; `geom_real_mesh.rs`'s seam-probe test found
/// this on real map data). So sightlines and other long rays, where missing
/// an occluder is the worse failure mode, should use `Bvh`; grenade hull
/// sweeps should keep using `UniformGrid` for its reference-exact tie
/// behaviour, since real-map differential testing found zero hull-sweep
/// mismatches between the two there.
pub trait Collider: Send + Sync {
    /// Swept-AABB-vs-mesh query (`FirstHitHullIndexed`). `min_normal_z`
    /// filters candidate contacts by `normal.z >= min_normal_z`; the
    /// reference's caller default is `-2.0` (accept everything).
    fn first_hit_hull(
        &self,
        from: V3,
        to: V3,
        half: V3,
        min_normal_z: f32,
        ignore: Option<&dyn Fn(u32) -> bool>,
    ) -> Option<HullHit>;

    /// Closest-hit ray query in the physics window `(1e-5, 1]`.
    fn first_hit_ray(&self, from: V3, to: V3) -> Option<RayHit>;

    /// Any-hit occlusion query in the sightline window `(1e-4, 1 - 1e-4)`,
    /// matching `TriangleRaycaster.Blocked`.
    fn blocked(&self, from: V3, to: V3) -> bool;

    /// Static box-vs-mesh intersection (`BoxIntersects`).
    fn box_intersects(&self, center: V3, half: V3) -> bool;

    /// The three vertices of an original mesh triangle this collider knows
    /// about. Panics if `t` was not part of the triangle set this collider
    /// was built over.
    fn triangle(&self, t: u32) -> [V3; 3];

    /// Whether `t` belongs to the `EntityBreakable` attribute group.
    /// Panics if `t` was not part of the triangle set this collider was
    /// built over.
    fn is_breakable(&self, t: u32) -> bool;

    /// Number of triangles this collider was built over (post attribute-
    /// mask and region filtering).
    fn triangle_count(&self) -> usize;
}

/// A compact, mesh-independent copy of the triangles a collider was built
/// over: vertices, per-triangle AABB, and breakable flag, indexed
/// contiguously ("local" index) in ascending original-mesh-triangle order.
/// Shared by `UniformGrid` and (Part B) `Bvh` so both traverse the exact
/// same triangle universe and preserve the same tie order.
#[derive(Debug, Clone, Default)]
pub struct ColliderTriangles {
    verts: Vec<[V3; 3]>,
    aabbs: Vec<Aabb>,
    breakable: Vec<bool>,
    /// Ascending: `original_index[local]` is the `CollisionMesh` triangle
    /// index that local triangle `local` came from.
    original_index: Vec<u32>,
}

impl ColliderTriangles {
    /// Builds the compact triangle set: only triangles whose attribute is
    /// solid in `mask` and whose AABB intersects `region` (if given) are
    /// kept, mirroring `TriangleCollider`'s `ForEachCoveredCell` region
    /// rejection and `TriangleRaycaster`'s constructor filter. Rejects with
    /// [`ColliderError::NonFiniteVertex`] instead of silently including
    /// NaN/inf geometry.
    pub fn build(
        mesh: &CollisionMesh,
        mask: &AttributeMask,
        region: Option<Aabb>,
    ) -> Result<Self, ColliderError> {
        let breakable_attr: Vec<bool> = mesh
            .attributes
            .iter()
            .map(|a| a.name == "EntityBreakable")
            .collect();

        let mut verts = Vec::new();
        let mut aabbs = Vec::new();
        let mut breakable = Vec::new();
        let mut original_index = Vec::new();

        for (i, tri) in mesh.triangles.iter().enumerate() {
            let attr = mesh.tri_attribute[i];
            if !mask.is_solid(attr) {
                continue;
            }
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            if !a.is_finite() || !b.is_finite() || !c.is_finite() {
                return Err(ColliderError::NonFiniteVertex { triangle: i as u32 });
            }
            let aabb = Aabb::from_points(a, b, c);
            if let Some(region) = region
                && !aabb.intersects(region)
            {
                continue;
            }
            verts.push([a, b, c]);
            aabbs.push(aabb);
            breakable.push(breakable_attr.get(attr as usize).copied().unwrap_or(false));
            original_index.push(i as u32);
        }

        Ok(ColliderTriangles {
            verts,
            aabbs,
            breakable,
            original_index,
        })
    }

    pub fn len(&self) -> usize {
        self.verts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.verts.is_empty()
    }

    pub fn vertices(&self, local: usize) -> [V3; 3] {
        self.verts[local]
    }

    pub fn aabb(&self, local: usize) -> Aabb {
        self.aabbs[local]
    }

    pub fn is_breakable_local(&self, local: usize) -> bool {
        self.breakable[local]
    }

    pub fn original_index(&self, local: usize) -> u32 {
        self.original_index[local]
    }

    /// Maps an original `CollisionMesh` triangle index back to its local
    /// index in this collider, or `None` if that triangle was filtered out.
    pub fn local_index(&self, original: u32) -> Option<usize> {
        self.original_index.binary_search(&original).ok()
    }

    /// Union of all triangle AABBs, or `None` if empty.
    pub fn bounds(&self) -> Option<Aabb> {
        self.aabbs.iter().copied().reduce(Aabb::union)
    }
}
