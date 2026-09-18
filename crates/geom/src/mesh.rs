//! Collision mesh with per-triangle attributes, surfaces and source objects.

use thiserror::Error;

/// A collision attribute group: the physics interaction layers a set of
/// triangles participates in (`m_InteractAsStrings`), what it is transparent
/// to (`m_InteractExcludeStrings`), and its group name. See FORMATS.md §10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionAttribute {
    /// Group string (e.g. "Default", "ConditionallySolid"), or a synthetic
    /// name for attributes merged in from entities (e.g. "EntitySolid").
    pub name: String,
    pub interact_as: Vec<String>,
    pub interact_with: Vec<String>,
    pub interact_exclude: Vec<String>,
    /// True for attributes synthesized from entity/prop classification
    /// rather than read directly from `m_collisionAttributes`.
    pub synthetic: bool,
}

/// A surface property (`m_surfacePropertyHashes` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceProperty {
    pub hash: u32,
    pub name: Option<String>,
}

impl SurfaceProperty {
    /// No surface property assigned to a triangle.
    pub const NONE: u16 = u16::MAX;
}

/// What kind of source object a mesh triangle range came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    WorldHull,
    WorldMesh,
    WorldSphere,
    WorldCapsule,
    Entity,
    StaticProp,
}

/// A source object (world physics part, entity or scene object) that
/// contributed triangles to the mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshObject {
    pub kind: ObjectKind,
    pub classname: Option<String>,
    pub targetname: Option<String>,
    pub model: Option<String>,
    pub hammer_id: Option<String>,
    /// Index of this object in its source list (hull/mesh/entity/scene-object index).
    pub source_index: u32,
    pub hull_flags: Option<u32>,
}

/// Aggregated statistics over a mesh, for logging/summaries.
#[derive(Debug, Clone, Default)]
pub struct MeshStats {
    pub triangles_per_attribute: Vec<(String, usize)>,
    pub triangles_per_kind: Vec<(ObjectKind, usize)>,
    pub degenerate_skipped: u64,
    pub bounds: Option<([f32; 3], [f32; 3])>,
}

#[derive(Debug, Error)]
pub enum MeshError {
    #[error("vertex index {index} out of range (mesh has {len} vertices)")]
    VertexIndexOutOfRange { index: u32, len: usize },
    #[error("non-finite vertex coordinate at index {index}")]
    NonFiniteVertex { index: usize },
    #[error("attribute index {index} out of range (mesh has {len} attributes)")]
    AttributeIndexOutOfRange { index: u16, len: usize },
    #[error("surface index {index} out of range (mesh has {len} surfaces)")]
    SurfaceIndexOutOfRange { index: u16, len: usize },
    #[error("object index {index} out of range (mesh has {len} objects)")]
    ObjectIndexOutOfRange { index: u32, len: usize },
    #[error("too many attributes: cannot exceed {}", u16::MAX - 1)]
    TooManyAttributes,
    #[error("too many surfaces: cannot exceed {}", u16::MAX - 1)]
    TooManySurfaces,
    #[error("parallel array length mismatch: {what} has {got}, expected {expected}")]
    LengthMismatch {
        what: &'static str,
        got: usize,
        expected: usize,
    },
}

/// A map's physics collision geometry: a triangle soup annotated per-triangle
/// with the collision attribute, surface property and source object it came
/// from.
#[derive(Debug, Clone, Default)]
pub struct CollisionMesh {
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Index into `attributes`.
    pub tri_attribute: Vec<u16>,
    /// Index into `surfaces`, `SurfaceProperty::NONE` if unknown.
    pub tri_surface: Vec<u16>,
    /// Index into `objects`.
    pub tri_object: Vec<u32>,
    pub attributes: Vec<CollisionAttribute>,
    pub surfaces: Vec<SurfaceProperty>,
    pub objects: Vec<MeshObject>,
    /// Zero-area triangles dropped by `push_triangles`.
    pub degenerate_skipped: u64,
}

impl CollisionMesh {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Axis-aligned bounds over all vertices, or `None` if the mesh is empty.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let mut it = self.vertices.iter();
        let first = *it.next()?;
        let mut min = first;
        let mut max = first;
        for v in it {
            for i in 0..3 {
                min[i] = min[i].min(v[i]);
                max[i] = max[i].max(v[i]);
            }
        }
        Some((min, max))
    }

    /// Adds a collision attribute, deduplicating identical entries.
    pub fn add_attribute(&mut self, a: CollisionAttribute) -> Result<u16, MeshError> {
        if let Some(i) = self.attributes.iter().position(|x| *x == a) {
            return Ok(i as u16);
        }
        if self.attributes.len() >= usize::from(u16::MAX - 1) {
            return Err(MeshError::TooManyAttributes);
        }
        self.attributes.push(a);
        Ok((self.attributes.len() - 1) as u16)
    }

    /// Adds a surface property, deduplicating by hash. If an existing entry
    /// for the hash has no name and `s` does, the name is filled in.
    pub fn add_surface(&mut self, s: SurfaceProperty) -> Result<u16, MeshError> {
        if let Some(i) = self.surfaces.iter().position(|x| x.hash == s.hash) {
            if self.surfaces[i].name.is_none() && s.name.is_some() {
                self.surfaces[i].name = s.name;
            }
            return Ok(i as u16);
        }
        if self.surfaces.len() >= usize::from(u16::MAX - 1) {
            return Err(MeshError::TooManySurfaces);
        }
        self.surfaces.push(s);
        Ok((self.surfaces.len() - 1) as u16)
    }

    pub fn add_object(&mut self, o: MeshObject) -> u32 {
        self.objects.push(o);
        (self.objects.len() - 1) as u32
    }

    /// Appends `vertices`/`triangles`, offsetting indices into this mesh's
    /// vertex array, validating that vertices are finite and triangle/surface
    /// indices are in range, and skipping zero-area (degenerate) triangles.
    /// All validation happens before any mutation, so an error leaves the
    /// mesh unchanged.
    pub fn push_triangles(
        &mut self,
        vertices: &[[f32; 3]],
        triangles: &[[u32; 3]],
        attribute: u16,
        surface: impl Fn(usize) -> u16,
        object: u32,
    ) -> Result<(), MeshError> {
        if usize::from(attribute) >= self.attributes.len() {
            return Err(MeshError::AttributeIndexOutOfRange {
                index: attribute,
                len: self.attributes.len(),
            });
        }
        if object as usize >= self.objects.len() {
            return Err(MeshError::ObjectIndexOutOfRange {
                index: object,
                len: self.objects.len(),
            });
        }
        for (i, v) in vertices.iter().enumerate() {
            if v.iter().any(|c| !c.is_finite()) {
                return Err(MeshError::NonFiniteVertex { index: i });
            }
        }
        let mut surfaces = Vec::with_capacity(triangles.len());
        for (i, tri) in triangles.iter().enumerate() {
            for &idx in tri {
                if idx as usize >= vertices.len() {
                    return Err(MeshError::VertexIndexOutOfRange {
                        index: idx,
                        len: vertices.len(),
                    });
                }
            }
            let surf = surface(i);
            if surf != SurfaceProperty::NONE && usize::from(surf) >= self.surfaces.len() {
                return Err(MeshError::SurfaceIndexOutOfRange {
                    index: surf,
                    len: self.surfaces.len(),
                });
            }
            surfaces.push(surf);
        }

        // Everything validated; mutate now.
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(vertices);
        for (i, tri) in triangles.iter().enumerate() {
            let a = self.vertices[(base + tri[0]) as usize];
            let b = self.vertices[(base + tri[1]) as usize];
            let c = self.vertices[(base + tri[2]) as usize];
            if is_degenerate(a, b, c) {
                self.degenerate_skipped += 1;
                continue;
            }
            self.triangles
                .push([base + tri[0], base + tri[1], base + tri[2]]);
            self.tri_attribute.push(attribute);
            self.tri_surface.push(surfaces[i]);
            self.tri_object.push(object);
        }
        Ok(())
    }

    /// Validates that all parallel arrays have matching lengths, all indices
    /// are in range, and all vertex coordinates are finite.
    pub fn validate(&self) -> Result<(), MeshError> {
        let n = self.triangles.len();
        if self.tri_attribute.len() != n {
            return Err(MeshError::LengthMismatch {
                what: "tri_attribute",
                got: self.tri_attribute.len(),
                expected: n,
            });
        }
        if self.tri_surface.len() != n {
            return Err(MeshError::LengthMismatch {
                what: "tri_surface",
                got: self.tri_surface.len(),
                expected: n,
            });
        }
        if self.tri_object.len() != n {
            return Err(MeshError::LengthMismatch {
                what: "tri_object",
                got: self.tri_object.len(),
                expected: n,
            });
        }
        for (i, v) in self.vertices.iter().enumerate() {
            if v.iter().any(|c| !c.is_finite()) {
                return Err(MeshError::NonFiniteVertex { index: i });
            }
        }
        for tri in &self.triangles {
            for &idx in tri {
                if idx as usize >= self.vertices.len() {
                    return Err(MeshError::VertexIndexOutOfRange {
                        index: idx,
                        len: self.vertices.len(),
                    });
                }
            }
        }
        for &a in &self.tri_attribute {
            if usize::from(a) >= self.attributes.len() {
                return Err(MeshError::AttributeIndexOutOfRange {
                    index: a,
                    len: self.attributes.len(),
                });
            }
        }
        for &s in &self.tri_surface {
            if s != SurfaceProperty::NONE && usize::from(s) >= self.surfaces.len() {
                return Err(MeshError::SurfaceIndexOutOfRange {
                    index: s,
                    len: self.surfaces.len(),
                });
            }
        }
        for &o in &self.tri_object {
            if o as usize >= self.objects.len() {
                return Err(MeshError::ObjectIndexOutOfRange {
                    index: o,
                    len: self.objects.len(),
                });
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> MeshStats {
        let mut per_attribute: Vec<(String, usize)> = self
            .attributes
            .iter()
            .map(|a| (a.name.clone(), 0))
            .collect();
        for &a in &self.tri_attribute {
            if let Some(slot) = per_attribute.get_mut(usize::from(a)) {
                slot.1 += 1;
            }
        }
        let mut per_kind: Vec<(ObjectKind, usize)> = Vec::new();
        for &o in &self.tri_object {
            if let Some(obj) = self.objects.get(o as usize) {
                let kind = obj.kind;
                match per_kind.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, c)) => *c += 1,
                    None => per_kind.push((kind, 1)),
                }
            }
        }
        MeshStats {
            triangles_per_attribute: per_attribute,
            triangles_per_kind: per_kind,
            degenerate_skipped: self.degenerate_skipped,
            bounds: self.bounds(),
        }
    }
}

fn is_degenerate(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> bool {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let cross = [
        ab[1] * ac[2] - ab[2] * ac[1],
        ab[2] * ac[0] - ab[0] * ac[2],
        ab[0] * ac[1] - ab[1] * ac[0],
    ];
    let area_sq = cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2];
    area_sq == 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(name: &str) -> CollisionAttribute {
        CollisionAttribute {
            name: name.to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        }
    }

    fn obj() -> MeshObject {
        MeshObject {
            kind: ObjectKind::WorldHull,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        }
    }

    #[test]
    fn add_attribute_dedupes() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let b = mesh.add_attribute(attr("Default")).unwrap();
        assert_eq!(a, b);
        assert_eq!(mesh.attributes.len(), 1);
    }

    #[test]
    fn add_surface_dedupes_by_hash() {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_surface(SurfaceProperty {
                hash: 1,
                name: None,
            })
            .unwrap();
        let b = mesh
            .add_surface(SurfaceProperty {
                hash: 1,
                name: Some("concrete".to_string()),
            })
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(mesh.surfaces.len(), 1);
    }

    #[test]
    fn add_surface_dedupe_fills_in_missing_name() {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_surface(SurfaceProperty {
                hash: 7,
                name: None,
            })
            .unwrap();
        let b = mesh
            .add_surface(SurfaceProperty {
                hash: 7,
                name: Some("concrete".to_string()),
            })
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(
            mesh.surfaces[usize::from(a)].name.as_deref(),
            Some("concrete")
        );
    }

    #[test]
    fn push_triangles_appends_and_offsets() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        mesh.push_triangles(
            &[[2.0, 0.0, 0.0], [3.0, 0.0, 0.0], [2.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        assert_eq!(mesh.triangles.len(), 2);
        assert_eq!(mesh.triangles[1], [3, 4, 5]);
        assert_eq!(mesh.vertices.len(), 6);
        mesh.validate().unwrap();
    }

    #[test]
    fn push_triangles_skips_degenerate() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        assert_eq!(mesh.triangles.len(), 0);
        assert_eq!(mesh.degenerate_skipped, 1);
        assert_eq!(mesh.stats().degenerate_skipped, 1);
    }

    #[test]
    fn push_triangles_keeps_thin_but_nonzero_area_triangles() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        // Small but genuinely non-degenerate triangle.
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [0.01, 0.0, 0.0], [0.0, 0.01, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        // Sliver: 1 unit long, 0.0003 units wide.
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 0.0003, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        assert_eq!(mesh.triangles.len(), 2);
        assert_eq!(mesh.degenerate_skipped, 0);
    }

    #[test]
    fn push_triangles_leaves_mesh_unchanged_on_surface_error() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        let vertices_before = mesh.vertices.clone();
        let triangles_before = mesh.triangles.clone();
        // Second triangle in the batch has an out-of-range surface index;
        // the first (valid) triangle must not be appended either.
        let err = mesh
            .push_triangles(
                &[
                    [2.0, 0.0, 0.0],
                    [3.0, 0.0, 0.0],
                    [2.0, 1.0, 0.0],
                    [4.0, 0.0, 0.0],
                    [5.0, 0.0, 0.0],
                    [4.0, 1.0, 0.0],
                ],
                &[[0, 1, 2], [3, 4, 5]],
                a,
                |i| if i == 0 { SurfaceProperty::NONE } else { 999 },
                o,
            )
            .unwrap_err();
        assert!(matches!(err, MeshError::SurfaceIndexOutOfRange { .. }));
        assert_eq!(mesh.vertices, vertices_before);
        assert_eq!(mesh.triangles, triangles_before);
    }

    #[test]
    fn push_triangles_rejects_out_of_range_index() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        let err = mesh
            .push_triangles(
                &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
                &[[0, 1, 5]],
                a,
                |_| SurfaceProperty::NONE,
                o,
            )
            .unwrap_err();
        assert!(matches!(err, MeshError::VertexIndexOutOfRange { .. }));
    }

    #[test]
    fn push_triangles_rejects_non_finite() {
        let mut mesh = CollisionMesh::new();
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        let err = mesh
            .push_triangles(
                &[[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                &[[0, 1, 2]],
                a,
                |_| SurfaceProperty::NONE,
                o,
            )
            .unwrap_err();
        assert!(matches!(err, MeshError::NonFiniteVertex { .. }));
    }

    #[test]
    fn validate_detects_length_mismatch() {
        let mut mesh = CollisionMesh::new();
        mesh.add_attribute(attr("Default")).unwrap();
        mesh.add_object(obj());
        mesh.vertices = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        mesh.triangles = vec![[0, 1, 2]];
        mesh.tri_attribute = vec![0, 0];
        mesh.tri_surface = vec![SurfaceProperty::NONE];
        mesh.tri_object = vec![0];
        let err = mesh.validate().unwrap_err();
        assert!(matches!(err, MeshError::LengthMismatch { .. }));
    }

    #[test]
    fn bounds_over_vertices() {
        let mut mesh = CollisionMesh::new();
        assert!(mesh.bounds().is_none());
        let a = mesh.add_attribute(attr("Default")).unwrap();
        let o = mesh.add_object(obj());
        mesh.push_triangles(
            &[[-1.0, 0.0, 0.0], [1.0, 2.0, 0.0], [0.0, 1.0, 3.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        let (min, max) = mesh.bounds().unwrap();
        assert_eq!(min, [-1.0, 0.0, 0.0]);
        assert_eq!(max, [1.0, 2.0, 3.0]);
    }
}
