//! Turns a decoded [`PhysAggregate`] into triangles pushed onto a [`CollisionMesh`], applying a
//! per-part transform (bind pose, if any, composed with the caller's placement transform).
//! Ported from `MapExtractor.cs:203-285 AppendPhys` / `:1441-1490 TriangulateHull`.

use std::collections::HashMap;

use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use s2fmt::phys::{Mat3x4, PhysAggregate};

use crate::report::ExtractReport;

/// Where a phys aggregate's triangles are being appended from, for report bucketing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    World,
    Entity,
    StaticProp,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::World => "world",
            Source::Entity => "entity",
            Source::StaticProp => "static_prop",
        }
    }
}

fn add_source_count(list: &mut Vec<(String, usize)>, source: Source, n: usize) {
    if n == 0 {
        return;
    }
    match list.iter_mut().find(|(s, _)| s == source.label()) {
        Some((_, c)) => *c += n,
        None => list.push((source.label().to_string(), n)),
    }
}

fn bump_histogram(hist: &mut Vec<(u32, usize)>, flags: u32) {
    match hist.iter_mut().find(|(f, _)| *f == flags) {
        Some((_, c)) => *c += 1,
        None => hist.push((flags, 1)),
    }
}

/// How to pick the mesh attribute id for a shape descriptor's triangles. World physics resolves
/// each descriptor's own `collision_attribute_index` into the mesh's attribute table
/// (`AttributeMode::PerDescriptor`); entity/static-prop geometry uses one fixed (synthetic)
/// attribute for every triangle regardless of the descriptor's own index, since `AppendPhys` is
/// always invoked with a constant `attributeMap` for those two callers
/// (`MapExtractor.cs:1226-1228`, `:1345-1347`).
pub enum AttributeMode {
    PerDescriptor,
    Fixed(u16),
}

/// How to pick the mesh object id (for `tri_object`) for a shape's triangles. World physics
/// creates one object per hull/mesh descriptor (so `ObjectKind`/`hull_flags` differ per shape);
/// entity/static-prop geometry reuses one caller-supplied object for the whole placement.
pub enum ObjectMode {
    PerWorldShape,
    Fixed(u32),
}

/// Appends every hull/mesh triangle in `phys` to `mesh`, applying `outer_transform` composed
/// with each part's bind pose (when present; `bind_pose.len() > part index`; VRF behaviour, not
/// applied by the reference extractor -- `docs/FORMATS.md` 6.7), bucketing
/// spheres/capsules/hull-flags/bind-pose stats into `report` under `source`. Hull
/// validation/triangulation failures are reported as warnings and that hull is skipped, rather
/// than aborting the whole extraction.
#[allow(clippy::too_many_arguments)]
pub fn append_phys(
    mesh: &mut CollisionMesh,
    phys: &PhysAggregate,
    outer_transform: &Mat3x4,
    attribute_mode: AttributeMode,
    object_mode: ObjectMode,
    source: Source,
    report: &mut ExtractReport,
    warn: &mut dyn FnMut(String),
) {
    let mut world_attr_cache: HashMap<i64, u16> = HashMap::new();
    let mut surface_cache: HashMap<i64, u16> = HashMap::new();

    for (part_index, part) in phys.parts.iter().enumerate() {
        let bind = phys.bind_pose.get(part_index);
        if let Some(bind) = bind
            && *bind != Mat3x4::IDENTITY
        {
            report.bind_pose_non_identity_count += 1;
        }
        let transform = match bind {
            Some(bind) => outer_transform.mul(bind),
            None => *outer_transform,
        };

        add_source_count(
            &mut report.spheres_by_source,
            source,
            part.shape.spheres.len(),
        );
        add_source_count(
            &mut report.capsules_by_source,
            source,
            part.shape.capsules.len(),
        );

        for (hull_index, desc) in part.shape.hulls.iter().enumerate() {
            let hull = &desc.shape;
            if source == Source::World {
                bump_histogram(&mut report.world_hull_flags_histogram, hull.flags);
            }
            if let Err(e) = hull.validate() {
                warn(format!("skipping malformed hull: {e}"));
                continue;
            }
            let tris = match hull.triangles() {
                Ok(t) => t,
                Err(e) => {
                    warn(format!("skipping hull with bad triangulation: {e}"));
                    continue;
                }
            };
            let positions: Vec<[f32; 3]> = hull
                .positions()
                .iter()
                .map(|p| transform.transform_point(*p))
                .collect();

            let attribute = resolve_attribute(
                mesh,
                phys,
                &attribute_mode,
                &mut world_attr_cache,
                desc.collision_attribute_index,
            );
            let object = match &object_mode {
                ObjectMode::Fixed(id) => *id,
                ObjectMode::PerWorldShape => mesh.add_object(MeshObject {
                    kind: ObjectKind::WorldHull,
                    classname: None,
                    targetname: None,
                    model: None,
                    hammer_id: None,
                    source_index: hull_index as u32,
                    hull_flags: Some(hull.flags),
                }),
            };
            let surface = resolve_surface(
                phys,
                mesh,
                &mut surface_cache,
                desc.surface_property_index,
                report,
            );

            if let Err(e) = mesh.push_triangles(&positions, &tris, attribute, |_| surface, object) {
                warn(format!("skipping hull: {e}"));
            }
        }

        for (mesh_index, desc) in part.shape.meshes.iter().enumerate() {
            let m = &desc.shape;
            let positions: Vec<[f32; 3]> = m
                .vertices
                .iter()
                .map(|p| transform.transform_point(*p))
                .collect();

            let attribute = resolve_attribute(
                mesh,
                phys,
                &attribute_mode,
                &mut world_attr_cache,
                desc.collision_attribute_index,
            );
            let object = match &object_mode {
                ObjectMode::Fixed(id) => *id,
                ObjectMode::PerWorldShape => mesh.add_object(MeshObject {
                    kind: ObjectKind::WorldMesh,
                    classname: None,
                    targetname: None,
                    model: None,
                    hammer_id: None,
                    source_index: mesh_index as u32,
                    hull_flags: None,
                }),
            };
            let per_tri_surface: Vec<u16> = (0..m.triangles.len())
                .map(|i| {
                    let idx = if !m.materials.is_empty() {
                        i64::from(m.materials[i])
                    } else {
                        desc.surface_property_index
                    };
                    resolve_surface(phys, mesh, &mut surface_cache, idx, report)
                })
                .collect();

            if let Err(e) = mesh.push_triangles(
                &positions,
                &m.triangles,
                attribute,
                |i| per_tri_surface[i],
                object,
            ) {
                warn(format!("skipping mesh: {e}"));
            }
        }
    }
}

fn resolve_attribute(
    mesh: &mut CollisionMesh,
    phys: &PhysAggregate,
    mode: &AttributeMode,
    world_attr_cache: &mut HashMap<i64, u16>,
    descriptor_index: i64,
) -> u16 {
    match mode {
        AttributeMode::Fixed(id) => *id,
        AttributeMode::PerDescriptor => {
            if let Some(&id) = world_attr_cache.get(&descriptor_index) {
                return id;
            }
            let attr = match usize::try_from(descriptor_index)
                .ok()
                .and_then(|i| phys.collision_attributes.get(i))
            {
                Some(a) => CollisionAttribute {
                    name: a.group.clone().unwrap_or_else(|| "Default".to_string()),
                    interact_as: a.interact_as.clone(),
                    interact_with: a.interact_with.clone(),
                    interact_exclude: a.interact_exclude.clone(),
                    synthetic: false,
                },
                None => CollisionAttribute {
                    name: "Default".to_string(),
                    interact_as: Vec::new(),
                    interact_with: Vec::new(),
                    interact_exclude: Vec::new(),
                    synthetic: false,
                },
            };
            // `add_attribute` only fails past `u16::MAX - 1` distinct attributes, far more than
            // any real map has; fall back to attribute 0 rather than propagating an error that
            // can't realistically happen.
            let id = mesh.add_attribute(attr).unwrap_or(0);
            world_attr_cache.insert(descriptor_index, id);
            id
        }
    }
}

fn resolve_surface(
    phys: &PhysAggregate,
    mesh: &mut CollisionMesh,
    cache: &mut HashMap<i64, u16>,
    hash_index: i64,
    report: &mut ExtractReport,
) -> u16 {
    if let Some(&id) = cache.get(&hash_index) {
        return id;
    }
    let id = match usize::try_from(hash_index)
        .ok()
        .and_then(|i| phys.surface_property_hashes.get(i))
    {
        Some(&hash) => mesh
            .add_surface(SurfaceProperty { hash, name: None })
            .unwrap_or(SurfaceProperty::NONE),
        None => {
            report.surface_indices_out_of_range += 1;
            SurfaceProperty::NONE
        }
    };
    cache.insert(hash_index, id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2fmt::phys::{
        Capsule, CollisionAttribute as PhysCollisionAttribute, HalfEdge, Hull, Part, Shape,
        ShapeDesc, Sphere,
    };

    /// A minimal valid convex hull (Euler check `V - E/2 + F == 2`): a tetrahedron with 4
    /// vertices, 4 triangular faces and 12 half-edges (6 undirected edges, each an (edge, twin)
    /// pair), hand-built rather than decoded from KV3 so this test doesn't depend on the PHYS
    /// decoder.
    fn tetrahedron() -> Hull {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ];
        // Faces (CCW from outside): (0,2,1), (0,1,3), (1,2,3), (2,0,3).
        let edges = vec![
            HalfEdge {
                next: 1,
                twin: 9,
                origin: 0,
                face: 0,
            },
            HalfEdge {
                next: 2,
                twin: 6,
                origin: 2,
                face: 0,
            },
            HalfEdge {
                next: 0,
                twin: 3,
                origin: 1,
                face: 0,
            },
            HalfEdge {
                next: 4,
                twin: 2,
                origin: 0,
                face: 1,
            },
            HalfEdge {
                next: 5,
                twin: 8,
                origin: 1,
                face: 1,
            },
            HalfEdge {
                next: 3,
                twin: 10,
                origin: 3,
                face: 1,
            },
            HalfEdge {
                next: 7,
                twin: 1,
                origin: 1,
                face: 2,
            },
            HalfEdge {
                next: 8,
                twin: 11,
                origin: 2,
                face: 2,
            },
            HalfEdge {
                next: 6,
                twin: 4,
                origin: 3,
                face: 2,
            },
            HalfEdge {
                next: 10,
                twin: 0,
                origin: 2,
                face: 3,
            },
            HalfEdge {
                next: 11,
                twin: 5,
                origin: 0,
                face: 3,
            },
            HalfEdge {
                next: 9,
                twin: 7,
                origin: 3,
                face: 3,
            },
        ];
        Hull {
            centroid: [0.25, 0.25, 0.25],
            bounds_min: [0.0, 0.0, 0.0],
            bounds_max: [1.0, 1.0, 1.0],
            flags: 0,
            vertex_positions: positions,
            vertex_indices: None,
            edges,
            faces: vec![0, 3, 6, 9],
            planes: Vec::new(),
        }
    }

    fn hull_desc(collision_attribute_index: i64, surface_property_index: i64) -> ShapeDesc<Hull> {
        ShapeDesc {
            collision_attribute_index,
            surface_property_index,
            user_friendly_name: None,
            tool_material_hash: None,
            shape: tetrahedron(),
        }
    }

    fn aggregate_with(
        part: Part,
        collision_attributes: Vec<PhysCollisionAttribute>,
        hashes: Vec<u32>,
    ) -> PhysAggregate {
        PhysAggregate {
            flags: 0,
            bind_pose: Vec::new(),
            parts: vec![part],
            surface_property_hashes: hashes,
            collision_attributes,
        }
    }

    #[test]
    fn hull_validates_and_triangulates_as_expected() {
        let hull = tetrahedron();
        hull.validate().unwrap();
        let tris = hull.triangles().unwrap();
        assert_eq!(tris, vec![[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]]);
    }

    #[test]
    fn world_hull_maps_attribute_and_surface() {
        let part = Part {
            flags: 0,
            collision_attribute_index: 0,
            shape: Shape {
                hulls: vec![hull_desc(0, 0)],
                ..Shape::default()
            },
        };
        let attrs = vec![PhysCollisionAttribute {
            group: Some("Default".to_string()),
            interact_as: vec!["passbullets".to_string()],
            interact_with: Vec::new(),
            interact_exclude: Vec::new(),
        }];
        let phys = aggregate_with(part, attrs, vec![12345]);

        let mut mesh = CollisionMesh::new();
        let mut report = ExtractReport::default();
        let mut warn = |s: String| panic!("unexpected warning: {s}");
        append_phys(
            &mut mesh,
            &phys,
            &Mat3x4::IDENTITY,
            AttributeMode::PerDescriptor,
            ObjectMode::PerWorldShape,
            Source::World,
            &mut report,
            &mut warn,
        );

        assert_eq!(mesh.triangle_count(), 4);
        assert_eq!(mesh.attributes.len(), 1);
        assert_eq!(mesh.attributes[0].name, "Default");
        assert_eq!(
            mesh.attributes[0].interact_as,
            vec!["passbullets".to_string()]
        );
        assert!(mesh.tri_attribute.iter().all(|&a| a == 0));
        assert_eq!(mesh.surfaces.len(), 1);
        assert_eq!(mesh.surfaces[0].hash, 12345);
        assert!(mesh.tri_surface.iter().all(|&s| s == 0));
        mesh.validate().unwrap();
    }

    #[test]
    fn spheres_and_capsules_are_skipped_but_counted() {
        let part = Part {
            flags: 0,
            collision_attribute_index: 0,
            shape: Shape {
                spheres: vec![ShapeDesc {
                    collision_attribute_index: 0,
                    surface_property_index: -1,
                    user_friendly_name: None,
                    tool_material_hash: None,
                    shape: Sphere {
                        center: [0.0, 0.0, 0.0],
                        radius: 1.0,
                    },
                }],
                capsules: vec![ShapeDesc {
                    collision_attribute_index: 0,
                    surface_property_index: -1,
                    user_friendly_name: None,
                    tool_material_hash: None,
                    shape: Capsule {
                        centers: [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
                        radius: 1.0,
                    },
                }],
                ..Shape::default()
            },
        };
        let phys = aggregate_with(part, Vec::new(), Vec::new());

        let mut mesh = CollisionMesh::new();
        let mut report = ExtractReport::default();
        let mut warn = |s: String| panic!("unexpected warning: {s}");
        append_phys(
            &mut mesh,
            &phys,
            &Mat3x4::IDENTITY,
            AttributeMode::PerDescriptor,
            ObjectMode::PerWorldShape,
            Source::World,
            &mut report,
            &mut warn,
        );

        assert_eq!(mesh.triangle_count(), 0);
        assert_eq!(report.spheres_by_source, vec![("world".to_string(), 1)]);
        assert_eq!(report.capsules_by_source, vec![("world".to_string(), 1)]);
    }

    #[test]
    fn out_of_range_attribute_and_surface_fall_back_and_are_counted() {
        let part = Part {
            flags: 0,
            collision_attribute_index: 0,
            shape: Shape {
                hulls: vec![hull_desc(99, 99)],
                ..Shape::default()
            },
        };
        let phys = aggregate_with(part, Vec::new(), Vec::new());

        let mut mesh = CollisionMesh::new();
        let mut report = ExtractReport::default();
        let mut warn = |s: String| panic!("unexpected warning: {s}");
        append_phys(
            &mut mesh,
            &phys,
            &Mat3x4::IDENTITY,
            AttributeMode::PerDescriptor,
            ObjectMode::PerWorldShape,
            Source::World,
            &mut report,
            &mut warn,
        );

        assert_eq!(mesh.attributes.len(), 1);
        assert_eq!(mesh.attributes[0].name, "Default");
        assert!(mesh.tri_surface.iter().all(|&s| s == SurfaceProperty::NONE));
        assert_eq!(report.surface_indices_out_of_range, 1);
    }

    #[test]
    fn bind_pose_is_composed_with_outer_transform_and_counted() {
        let mut bind = Mat3x4::IDENTITY;
        bind.rows[0][3] = 1.0; // translate local geometry by +1 on X.
        let part = Part {
            flags: 0,
            collision_attribute_index: 0,
            shape: Shape {
                hulls: vec![hull_desc(0, -1)],
                ..Shape::default()
            },
        };
        let phys = PhysAggregate {
            flags: 0,
            bind_pose: vec![bind],
            parts: vec![part],
            surface_property_hashes: Vec::new(),
            collision_attributes: Vec::new(),
        };

        // entity_transform-style outer placement: yaw 90 at origin (10, 0, 0).
        let outer = Mat3x4 {
            rows: s2fmt::entities::entity_transform(
                [10.0, 0.0, 0.0],
                [0.0, 90.0, 0.0],
                [1.0, 1.0, 1.0],
            ),
        };

        let mut mesh = CollisionMesh::new();
        let mut report = ExtractReport::default();
        let mut warn = |s: String| panic!("unexpected warning: {s}");
        append_phys(
            &mut mesh,
            &phys,
            &outer,
            AttributeMode::PerDescriptor,
            ObjectMode::PerWorldShape,
            Source::World,
            &mut report,
            &mut warn,
        );

        assert_eq!(report.bind_pose_non_identity_count, 1);
        // Vertex 0 of the tetrahedron is (0,0,0) in model space; bind-pose translates it to
        // (1,0,0), then the outer yaw-90 placement maps local +X to world +Y and adds (10,0,0):
        // (10, 1, 0).
        let v0 = mesh.vertices[0];
        assert!((v0[0] - 10.0).abs() < 1e-5, "{v0:?}");
        assert!((v0[1] - 1.0).abs() < 1e-5, "{v0:?}");
        assert!((v0[2] - 0.0).abs() < 1e-5, "{v0:?}");
    }
}
