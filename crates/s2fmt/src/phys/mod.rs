//! Physics aggregate data (PHYS blocks, `*.vphys_c`): decodes a `VPhysXAggregateData_t` KV3
//! root into plain Rust structs, with no CS2 gameplay policy (`docs/FORMATS.md` section 6).
//!
//! Ground truth: VRF `Resource/ResourceTypes/PhysAggregateData.cs`,
//! `Resource/ResourceTypes/RubikonPhysics/{Part,Shape,ShapeDescriptor}.cs`,
//! `Resource/ResourceTypes/RubikonPhysics/Shapes/{Hull,Mesh,Sphere,Capsule}.cs`,
//! `Serialization/KeyValues/KVObjectExtensions.cs`.

mod decode;
mod hull;
#[cfg(test)]
mod tests;

use crate::kv3;

/// A physics aggregate (`VPhysXAggregateData_t`): the whole content of a PHYS block.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysAggregate {
    pub flags: i64,
    /// One entry per part, or empty. See `docs/FORMATS.md` 6.7: not applied by this crate.
    pub bind_pose: Vec<Mat3x4>,
    pub parts: Vec<Part>,
    /// `MurmurHash2` (seed `0x31415926`) of each referenced surfaceprop name, indexed by
    /// `ShapeDesc::surface_property_index`.
    pub surface_property_hashes: Vec<u32>,
    /// Indexed by `Part::collision_attribute_index` / `ShapeDesc::collision_attribute_index`.
    pub collision_attributes: Vec<CollisionAttribute>,
}

/// A row-major 3x4 affine transform: `p' = R*p + t`, `rows[i] = [R_i0, R_i1, R_i2, t_i]`
/// (`docs/FORMATS.md` 6.6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3x4 {
    pub rows: [[f32; 4]; 3],
}

impl Mat3x4 {
    pub const IDENTITY: Self = Mat3x4 {
        rows: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ],
    };

    /// Applies this transform to a point: `R*p + t`.
    pub fn transform_point(&self, p: [f32; 3]) -> [f32; 3] {
        [
            self.rows[0][0] * p[0]
                + self.rows[0][1] * p[1]
                + self.rows[0][2] * p[2]
                + self.rows[0][3],
            self.rows[1][0] * p[0]
                + self.rows[1][1] * p[1]
                + self.rows[1][2] * p[2]
                + self.rows[1][3],
            self.rows[2][0] * p[0]
                + self.rows[2][1] * p[1]
                + self.rows[2][2] * p[2]
                + self.rows[2][3],
        ]
    }

    /// Composes two transforms: `self ∘ other`, i.e. applying the result to a point is the same
    /// as applying `other` then `self`. Both matrices' implicit fourth row is `[0, 0, 0, 1]`.
    pub fn mul(&self, other: &Self) -> Self {
        let mut rows = [[0.0f32; 4]; 3];
        for (i, self_row) in self.rows.iter().enumerate() {
            for (j, out) in rows[i].iter_mut().enumerate() {
                *out = self_row[0] * other.rows[0][j]
                    + self_row[1] * other.rows[1][j]
                    + self_row[2] * other.rows[2][j]
                    + if j == 3 { self_row[3] } else { 0.0 };
            }
        }
        Mat3x4 { rows }
    }
}

/// A physics part (`VPhysXBodyPart_t`; VRF `RubikonPhysics/Part.cs`).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub flags: i64,
    pub collision_attribute_index: i64,
    pub shape: Shape,
}

/// A physics shape (`VPhysics2ShapeDef_t`; VRF `RubikonPhysics/Shape.cs`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shape {
    pub spheres: Vec<ShapeDesc<Sphere>>,
    pub capsules: Vec<ShapeDesc<Capsule>>,
    pub hulls: Vec<ShapeDesc<Hull>>,
    pub meshes: Vec<ShapeDesc<Mesh>>,
}

/// A shape descriptor (`RnShapeDesc_t`; VRF `RubikonPhysics/ShapeDescriptor.cs`). Collision index
/// here (not the part's) is what's used, per `docs/FORMATS.md` 6.2.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeDesc<T> {
    pub collision_attribute_index: i64,
    pub surface_property_index: i64,
    pub user_friendly_name: Option<String>,
    pub tool_material_hash: Option<u64>,
    pub shape: T,
}

/// `RnSphere_t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sphere {
    pub center: [f32; 3],
    pub radius: f32,
}

/// `RnCapsule_t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Capsule {
    pub centers: [[f32; 3]; 2],
    pub radius: f32,
}

/// A convex hull (`RnHull_t`; VRF `RubikonPhysics/Shapes/Hull.cs`). See [`Hull::positions`],
/// [`Hull::triangles`] and [`Hull::validate`] for the vertex/triangulation semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct Hull {
    pub centroid: [f32; 3],
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub flags: u32,
    /// Hull vertex positions. When [`Self::vertex_indices`] is `Some`, this came from
    /// `m_VertexPositions`; otherwise it came from `m_Vertices` directly (old format).
    pub vertex_positions: Vec<[f32; 3]>,
    /// `m_Vertices`, only present in the new (2023-11-04+) on-disk format (VRF `Hull.cs`
    /// `HasExplicitVertexIndices`/`GetVertices`, ~222-238). This is **not** a position index:
    /// it's `RnVertex_t::m_nEdge`, one *outgoing half-edge index* per vertex, the same length as
    /// [`Self::vertex_positions`], such that `edges[vertex_indices[v]].origin == v` for every `v`
    /// (verified against every real de_mirage hull). [`Hull::validate`] checks this invariant
    /// when present. `HalfEdge::origin` always indexes [`Self::vertex_positions`] directly and
    /// never consults this list.
    pub vertex_indices: Option<Vec<u8>>,
    pub edges: Vec<HalfEdge>,
    /// First edge index per face (`RnFace_t::m_nEdge`).
    pub faces: Vec<u8>,
    pub planes: Vec<Plane>,
}

/// `RnHalfEdge_t`: edges are stored in (edge, twin) pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HalfEdge {
    pub next: u8,
    pub twin: u8,
    pub origin: u8,
    pub face: u8,
}

/// `RnPlane_t`: outward-facing plane, `n·x - d = 0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane {
    pub normal: [f32; 3],
    pub offset: f32,
}

/// A concave mesh (`RnMesh_t`; VRF `RubikonPhysics/Shapes/Mesh.cs`). This crate doesn't build the
/// mesh's own BVH (`docs/FORMATS.md` 6.4: "Свой BVH строим сами, `m_Nodes` не нужен").
#[derive(Debug, Clone, PartialEq)]
pub struct Mesh {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub flags: u32,
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Per-triangle surface property index; empty means the whole mesh shares one material.
    pub materials: Vec<u32>,
}

/// A collision attribute set (`m_collisionAttributes[i]`; `docs/FORMATS.md` 6.5). Missing keys
/// decode as empty lists / `None`; no "Default" substitution (that's gameplay policy).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CollisionAttribute {
    pub group: Option<String>,
    /// `m_InteractAsStrings`, falling back to the older `m_PhysicsTagStrings`.
    pub interact_as: Vec<String>,
    pub interact_with: Vec<String>,
    pub interact_exclude: Vec<String>,
}

/// An error decoding a PHYS KV3 tree. `path` is a dotted/indexed key path into the root value,
/// e.g. `m_parts[0].m_rnShape.m_hulls[17].m_Hull.m_Edges`.
#[derive(Debug, thiserror::Error)]
pub enum PhysError {
    #[error("missing required key {path}")]
    Missing { path: String },
    #[error("{path}: expected {expected}")]
    WrongType {
        path: String,
        expected: &'static str,
    },
    #[error("{path}: blob length {len} is not a multiple of the element size {element_size}")]
    BadBlobLength {
        path: String,
        len: usize,
        element_size: usize,
    },
    #[error("{path}: index {index} out of range (have {len})")]
    IndexOutOfRange {
        path: String,
        index: usize,
        len: usize,
    },
    #[error(
        "{path}: hull fails Euler check: V={v} E={e} F={f} (V - E/2 + F = {result}, expected 2)"
    )]
    EulerCheck {
        path: String,
        v: usize,
        e: usize,
        f: usize,
        result: i64,
    },
    #[error(
        "{path}: face edge loop did not return to its start edge within {max_steps} steps (corrupt m_nNext cycle)"
    )]
    FaceLoopNotClosed { path: String, max_steps: usize },
    #[error(
        "{path}: vertex {vertex}'s outgoing edge (m_Vertices[{vertex}] = {edge_index}) has origin {edge_origin}, expected {vertex}"
    )]
    VertexOutgoingEdgeMismatch {
        path: String,
        vertex: usize,
        edge_index: usize,
        edge_origin: usize,
    },
    #[error("{path}: expected length {expected}, found {actual}")]
    LengthMismatch {
        path: String,
        expected: usize,
        actual: usize,
    },
}

/// Decodes a PHYS KV3 root (or a `vphys_c` DATA root) into a [`PhysAggregate`].
pub fn decode(root: &kv3::Value) -> Result<PhysAggregate, PhysError> {
    decode::build(root)
}
