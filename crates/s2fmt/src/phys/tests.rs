use super::*;
use crate::kv3::{Object, Value};

fn obj(entries: Vec<(&str, Value)>) -> Value {
    let mut o = Object::with_capacity(entries.len());
    for (k, v) in entries {
        o.push(k, v);
    }
    Value::Object(o)
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn f(x: f32) -> Value {
    Value::Double(x as f64)
}

fn i(x: i64) -> Value {
    Value::Int(x)
}

fn s(x: &str) -> Value {
    Value::String(x.to_string())
}

fn vec3(v: [f32; 3]) -> Value {
    arr(vec![f(v[0]), f(v[1]), f(v[2])])
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Wraps a hull payload as `m_parts[0].m_rnShape.m_hulls[0].m_Hull`.
fn phys_root_with_hull(hull: Value) -> Value {
    obj(vec![
        ("m_nFlags", i(0)),
        (
            "m_parts",
            arr(vec![obj(vec![
                ("m_nFlags", i(0)),
                ("m_nCollisionAttributeIndex", i(0)),
                (
                    "m_rnShape",
                    obj(vec![(
                        "m_hulls",
                        arr(vec![obj(vec![
                            ("m_nCollisionAttributeIndex", i(0)),
                            ("m_nSurfacePropertyIndex", i(0)),
                            ("m_Hull", hull),
                        ])]),
                    )]),
                ),
            ])]),
        ),
    ])
}

/// Wraps a mesh payload as `m_parts[0].m_rnShape.m_meshes[0].m_Mesh`.
fn phys_root_with_mesh(mesh: Value) -> Value {
    obj(vec![
        ("m_nFlags", i(0)),
        (
            "m_parts",
            arr(vec![obj(vec![
                ("m_nFlags", i(0)),
                ("m_nCollisionAttributeIndex", i(0)),
                (
                    "m_rnShape",
                    obj(vec![(
                        "m_meshes",
                        arr(vec![obj(vec![
                            ("m_nCollisionAttributeIndex", i(0)),
                            ("m_nSurfacePropertyIndex", i(0)),
                            ("m_Mesh", mesh),
                        ])]),
                    )]),
                ),
            ])]),
        ),
    ])
}

// A unit cube (8 vertices, 6 quad faces, 12 edge pairs), used by the hull tests below. Vertex
// order and half-edge topology are hand-derived so every face's edge loop is CCW when viewed from
// outside (outward normals), matching `docs/FORMATS.md` 6.3.
const CUBE_POSITIONS: [[f32; 3]; 8] = [
    [-1.0, -1.0, -1.0],
    [1.0, -1.0, -1.0],
    [1.0, 1.0, -1.0],
    [-1.0, 1.0, -1.0],
    [-1.0, -1.0, 1.0],
    [1.0, -1.0, 1.0],
    [1.0, 1.0, 1.0],
    [-1.0, 1.0, 1.0],
];

/// `(next, twin, origin, face)` per half-edge.
const CUBE_EDGES: [(u8, u8, u8, u8); 24] = [
    (1, 15, 0, 0),
    (2, 19, 3, 0),
    (3, 8, 2, 0),
    (0, 20, 1, 0),
    (5, 22, 4, 1),
    (6, 10, 5, 1),
    (7, 17, 6, 1),
    (4, 13, 7, 1),
    (9, 2, 1, 2),
    (10, 18, 2, 2),
    (11, 5, 6, 2),
    (8, 21, 5, 2),
    (13, 23, 0, 3),
    (14, 7, 4, 3),
    (15, 16, 7, 3),
    (12, 0, 3, 3),
    (17, 14, 3, 4),
    (18, 6, 7, 4),
    (19, 9, 6, 4),
    (16, 1, 2, 4),
    (21, 3, 0, 5),
    (22, 11, 1, 5),
    (23, 4, 5, 5),
    (20, 12, 4, 5),
];

const CUBE_FACES: [u8; 6] = [0, 4, 8, 12, 16, 20];

/// Per-vertex outgoing half-edge index (`RnVertex_t::m_nEdge`, `m_Vertices` in the new hull
/// format): for every vertex `v`, the edge at `CUBE_EDGES[CUBE_VERTEX_OUTGOING_EDGES[v]]` has
/// `origin == v`, as [`Hull::validate`] requires.
const CUBE_VERTEX_OUTGOING_EDGES: [u8; 8] = [0, 3, 2, 1, 4, 5, 6, 7];

const CUBE_PLANE_NORMALS: [[f32; 3]; 6] = [
    [0.0, 0.0, -1.0],
    [0.0, 0.0, 1.0],
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, -1.0, 0.0],
];

fn cube_positions_blob() -> Vec<u8> {
    let mut b = Vec::new();
    for p in CUBE_POSITIONS {
        for c in p {
            b.extend_from_slice(&c.to_le_bytes());
        }
    }
    b
}

fn cube_edges_blob() -> Vec<u8> {
    let mut b = Vec::new();
    for (n, t, o, fc) in CUBE_EDGES {
        b.extend_from_slice(&[n, t, o, fc]);
    }
    b
}

fn cube_planes_blob() -> Vec<u8> {
    let mut b = Vec::new();
    for n in CUBE_PLANE_NORMALS {
        for c in n {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&1.0f32.to_le_bytes());
    }
    b
}

fn cube_bounds_and_centroid() -> Vec<(&'static str, Value)> {
    vec![
        ("m_vCentroid", vec3([0.0, 0.0, 0.0])),
        (
            "m_Bounds",
            obj(vec![
                ("m_vMinBounds", vec3([-1.0, -1.0, -1.0])),
                ("m_vMaxBounds", vec3([1.0, 1.0, 1.0])),
            ]),
        ),
        ("m_nFlags", i(0)),
    ]
}

fn cube_hull_new_format() -> Value {
    let mut entries = cube_bounds_and_centroid();
    entries.push(("m_VertexPositions", Value::Blob(cube_positions_blob())));
    entries.push((
        "m_Vertices",
        Value::Blob(CUBE_VERTEX_OUTGOING_EDGES.to_vec()),
    ));
    entries.push(("m_Edges", Value::Blob(cube_edges_blob())));
    entries.push(("m_Faces", Value::Blob(CUBE_FACES.to_vec())));
    entries.push(("m_Planes", Value::Blob(cube_planes_blob())));
    obj(entries)
}

fn cube_hull_old_format() -> Value {
    cube_hull_old_format_with_faces(&CUBE_FACES)
}

/// Like [`cube_hull_old_format`], but with a caller-chosen face list (used to build a
/// structurally-valid-but-corrupted hull for the Euler check test: every edge/face index still in
/// range, but the count doesn't satisfy `V - E/2 + F == 2`).
fn cube_hull_old_format_with_faces(faces: &[u8]) -> Value {
    let vertices = arr(CUBE_POSITIONS.iter().map(|p| vec3(*p)).collect());
    let edges = arr(CUBE_EDGES
        .iter()
        .map(|&(n, t, o, fc)| {
            obj(vec![
                ("m_nNext", i(n as i64)),
                ("m_nTwin", i(t as i64)),
                ("m_nOrigin", i(o as i64)),
                ("m_nFace", i(fc as i64)),
            ])
        })
        .collect());
    let faces = arr(faces
        .iter()
        .map(|&e| obj(vec![("m_nEdge", i(e as i64))]))
        .collect());
    let planes = arr(CUBE_PLANE_NORMALS
        .iter()
        .map(|n| obj(vec![("m_vNormal", vec3(*n)), ("m_flOffset", f(1.0))]))
        .collect());

    let mut entries = cube_bounds_and_centroid();
    entries.push(("m_Vertices", vertices));
    entries.push(("m_Edges", edges));
    entries.push(("m_Faces", faces));
    entries.push(("m_Planes", planes));
    obj(entries)
}

fn assert_outward_winding(hull: &Hull, tris: &[[u32; 3]]) {
    let positions = hull.positions();
    for tri in tris {
        let a = positions[tri[0] as usize];
        let b = positions[tri[1] as usize];
        let c = positions[tri[2] as usize];
        let n = cross(sub(b, a), sub(c, a));
        let center = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        let out_dir = sub(center, hull.centroid);
        let dot = n[0] * out_dir[0] + n[1] * out_dir[1] + n[2] * out_dir[2];
        assert!(dot > 0.0, "triangle {tri:?} normal not outward (dot={dot})");
    }
}

#[test]
fn cube_hull_new_format_triangulates_with_outward_normals() {
    let root = phys_root_with_hull(cube_hull_new_format());
    let agg = decode(&root).expect("decode");
    let hull = &agg.parts[0].shape.hulls[0].shape;
    assert!(
        hull.vertex_indices.is_some(),
        "new format carries m_Vertices as indices"
    );
    hull.validate().expect("validate");
    let tris = hull.triangles().expect("triangles");
    assert_eq!(tris.len(), 12);
    assert_outward_winding(hull, &tris);
}

#[test]
fn cube_hull_old_format_triangulates_with_outward_normals() {
    let root = phys_root_with_hull(cube_hull_old_format());
    let agg = decode(&root).expect("decode");
    let hull = &agg.parts[0].shape.hulls[0].shape;
    assert!(
        hull.vertex_indices.is_none(),
        "old format has no explicit index list"
    );
    hull.validate().expect("validate");
    let tris = hull.triangles().expect("triangles");
    assert_eq!(tris.len(), 12);
    assert_outward_winding(hull, &tris);
}

#[test]
fn hull_validate_fails_euler_check_on_corrupted_faces() {
    // Duplicate a face: every edge/face index stays in range (face indices referenced by edges
    // are all < 6, and 7 faces is still >= that), but V - E/2 + F = 8 - 12 + 7 = 3 != 2.
    let mut faces = CUBE_FACES.to_vec();
    faces.push(CUBE_FACES[0]);
    let hull = cube_hull_old_format_with_faces(&faces);
    let root = phys_root_with_hull(hull);
    let agg = decode(&root).expect("decode should still succeed structurally");
    let hull = &agg.parts[0].shape.hulls[0].shape;
    match hull.validate() {
        Err(PhysError::EulerCheck {
            v, e, f, result, ..
        }) => {
            assert_eq!((v, e, f, result), (8, 24, 7, 3));
        }
        other => panic!("expected EulerCheck failure, got {other:?}"),
    }
}

#[test]
fn hull_face_loop_that_never_closes_errors_quickly() {
    // faces=[0], edges=[{next:1,twin:1,origin:0,face:0},{next:1,twin:0,origin:1,face:0}]: edge 1's
    // `next` points at itself, so walking from face 0's start edge never returns to it. Both
    // `triangles()` and `validate()` must error instead of looping forever.
    let hull = Hull {
        centroid: [0.0, 0.0, 0.0],
        bounds_min: [0.0, 0.0, 0.0],
        bounds_max: [1.0, 1.0, 1.0],
        flags: 0,
        vertex_positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
        vertex_indices: None,
        edges: vec![
            HalfEdge {
                next: 1,
                twin: 1,
                origin: 0,
                face: 0,
            },
            HalfEdge {
                next: 1,
                twin: 0,
                origin: 1,
                face: 0,
            },
        ],
        faces: vec![0],
        planes: vec![],
    };

    match hull.triangles() {
        Err(PhysError::FaceLoopNotClosed { max_steps, .. }) => assert_eq!(max_steps, 2),
        other => panic!("expected FaceLoopNotClosed from triangles(), got {other:?}"),
    }
    match hull.validate() {
        Err(PhysError::FaceLoopNotClosed { max_steps, .. }) => assert_eq!(max_steps, 2),
        other => panic!("expected FaceLoopNotClosed from validate(), got {other:?}"),
    }
}

fn quad_mesh_vertices() -> [[f32; 3]; 4] {
    [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
    ]
}

fn vec3s_blob(vs: &[[f32; 3]]) -> Vec<u8> {
    let mut b = Vec::new();
    for p in vs {
        for c in p {
            b.extend_from_slice(&c.to_le_bytes());
        }
    }
    b
}

fn triangles_blob(tris: &[[i32; 3]]) -> Vec<u8> {
    let mut b = Vec::new();
    for t in tris {
        for c in t {
            b.extend_from_slice(&c.to_le_bytes());
        }
    }
    b
}

#[test]
fn mesh_blob_vertices_triangles_and_materials() {
    let vertices = quad_mesh_vertices();
    let mesh = obj(vec![
        ("m_vMin", vec3([0.0, 0.0, 0.0])),
        ("m_vMax", vec3([1.0, 1.0, 0.0])),
        ("m_nFlags", i(0)),
        ("m_Vertices", Value::Blob(vec3s_blob(&vertices))),
        (
            "m_Triangles",
            Value::Blob(triangles_blob(&[[0, 1, 2], [0, 2, 3]])),
        ),
        ("m_Materials", Value::Blob(vec![0u8, 1u8])),
    ]);

    let root = phys_root_with_mesh(mesh);
    let agg = decode(&root).expect("decode");
    let mesh = &agg.parts[0].shape.meshes[0].shape;
    assert_eq!(mesh.vertices.len(), 4);
    assert_eq!(mesh.triangles, vec![[0, 1, 2], [0, 2, 3]]);
    assert_eq!(mesh.materials, vec![0, 1]);
}

#[test]
fn mesh_array_vertices_triangles_and_empty_materials() {
    let vertices = quad_mesh_vertices();
    let mesh = obj(vec![
        ("m_vMin", vec3([0.0, 0.0, 0.0])),
        ("m_vMax", vec3([1.0, 1.0, 0.0])),
        ("m_nFlags", i(0)),
        (
            "m_Vertices",
            arr(vertices.iter().map(|p| vec3(*p)).collect()),
        ),
        (
            "m_Triangles",
            arr(vec![
                obj(vec![("m_nIndex", arr(vec![i(0), i(1), i(2)]))]),
                obj(vec![("m_nIndex", arr(vec![i(0), i(2), i(3)]))]),
            ]),
        ),
        // m_Materials intentionally absent: empty means one material for the whole mesh.
    ]);

    let root = phys_root_with_mesh(mesh);
    let agg = decode(&root).expect("decode");
    let mesh = &agg.parts[0].shape.meshes[0].shape;
    assert_eq!(mesh.vertices.len(), 4);
    assert_eq!(mesh.triangles, vec![[0, 1, 2], [0, 2, 3]]);
    assert!(mesh.materials.is_empty());
}

#[test]
fn mesh_array_materials() {
    let vertices = quad_mesh_vertices();
    let mesh = obj(vec![
        ("m_vMin", vec3([0.0, 0.0, 0.0])),
        ("m_vMax", vec3([1.0, 1.0, 0.0])),
        ("m_nFlags", i(0)),
        ("m_Vertices", Value::Blob(vec3s_blob(&vertices))),
        (
            "m_Triangles",
            Value::Blob(triangles_blob(&[[0, 1, 2], [0, 2, 3]])),
        ),
        ("m_Materials", arr(vec![i(3), i(7)])),
    ]);

    let root = phys_root_with_mesh(mesh);
    let agg = decode(&root).expect("decode");
    let mesh = &agg.parts[0].shape.meshes[0].shape;
    assert_eq!(mesh.materials, vec![3, 7]);
}

#[test]
fn collision_attribute_falls_back_to_physics_tag_strings() {
    let attr = obj(vec![
        ("m_CollisionGroupString", s("Default")),
        ("m_PhysicsTagStrings", arr(vec![s("passbullets")])),
    ]);
    let root = obj(vec![
        ("m_nFlags", i(0)),
        ("m_parts", arr(vec![])),
        ("m_collisionAttributes", arr(vec![attr])),
    ]);
    let agg = decode(&root).expect("decode");
    assert_eq!(
        agg.collision_attributes[0].group.as_deref(),
        Some("Default")
    );
    assert_eq!(
        agg.collision_attributes[0].interact_as,
        vec!["passbullets".to_string()]
    );
}

#[test]
fn collision_attribute_prefers_interact_as_strings_when_present() {
    let attr = obj(vec![
        ("m_InteractAsStrings", arr(vec![s("npcclip")])),
        ("m_PhysicsTagStrings", arr(vec![s("old_tag")])),
    ]);
    let root = obj(vec![
        ("m_nFlags", i(0)),
        ("m_parts", arr(vec![])),
        ("m_collisionAttributes", arr(vec![attr])),
    ]);
    let agg = decode(&root).expect("decode");
    assert_eq!(
        agg.collision_attributes[0].interact_as,
        vec!["npcclip".to_string()]
    );
}

#[test]
fn flat_matrix_12_floats() {
    let flat = arr((0..12).map(|n| f(n as f32)).collect());
    let root = obj(vec![
        ("m_nFlags", i(0)),
        ("m_parts", arr(vec![])),
        ("m_bindPose", arr(vec![flat])),
    ]);
    let agg = decode(&root).expect("decode");
    assert_eq!(
        agg.bind_pose[0].rows,
        [
            [0.0, 1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0, 7.0],
            [8.0, 9.0, 10.0, 11.0]
        ]
    );
}

#[test]
fn flat_matrix_16_floats_ignores_last_row() {
    let flat = arr((0..16).map(|n| f(n as f32)).collect());
    let root = obj(vec![
        ("m_nFlags", i(0)),
        ("m_parts", arr(vec![])),
        ("m_bindPose", arr(vec![flat])),
    ]);
    let agg = decode(&root).expect("decode");
    assert_eq!(
        agg.bind_pose[0].rows,
        [
            [0.0, 1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0, 7.0],
            [8.0, 9.0, 10.0, 11.0]
        ]
    );
}

#[test]
fn nested_matrix_rows() {
    let row = |a: f32, b: f32, c: f32, d: f32| arr(vec![f(a), f(b), f(c), f(d)]);
    let nested = arr(vec![
        row(0.0, 1.0, 2.0, 3.0),
        row(4.0, 5.0, 6.0, 7.0),
        row(8.0, 9.0, 10.0, 11.0),
    ]);
    let root = obj(vec![
        ("m_nFlags", i(0)),
        ("m_parts", arr(vec![])),
        ("m_bindPose", arr(vec![nested])),
    ]);
    let agg = decode(&root).expect("decode");
    assert_eq!(
        agg.bind_pose[0].rows,
        [
            [0.0, 1.0, 2.0, 3.0],
            [4.0, 5.0, 6.0, 7.0],
            [8.0, 9.0, 10.0, 11.0]
        ]
    );
}

#[test]
fn mat3x4_identity_transform_is_noop() {
    let p = [1.0, 2.0, 3.0];
    assert_eq!(Mat3x4::IDENTITY.transform_point(p), p);
}

#[test]
fn mat3x4_mul_composes_translations() {
    let mut t1 = Mat3x4::IDENTITY;
    t1.rows[0][3] = 1.0;
    let mut t2 = Mat3x4::IDENTITY;
    t2.rows[1][3] = 2.0;
    let combined = t1.mul(&t2);
    assert_eq!(combined.transform_point([0.0, 0.0, 0.0]), [1.0, 2.0, 0.0]);
}

#[test]
fn mat3x4_mul_does_not_commute() {
    let rotate_z_90 = Mat3x4 {
        rows: [
            [0.0, -1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ],
    };
    let translate_x_plus_1 = Mat3x4 {
        rows: [
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ],
    };
    assert_eq!(
        rotate_z_90
            .mul(&translate_x_plus_1)
            .transform_point([0.0, 0.0, 0.0]),
        [0.0, 1.0, 0.0]
    );
    assert_eq!(
        translate_x_plus_1
            .mul(&rotate_z_90)
            .transform_point([0.0, 0.0, 0.0]),
        [1.0, 0.0, 0.0]
    );
}

#[test]
fn missing_key_reports_key_path() {
    let hull = obj(vec![
        ("m_vCentroid", vec3([0.0, 0.0, 0.0])),
        (
            "m_Bounds",
            obj(vec![
                ("m_vMinBounds", vec3([-1.0, -1.0, -1.0])),
                ("m_vMaxBounds", vec3([1.0, 1.0, 1.0])),
            ]),
        ),
        ("m_nFlags", i(0)),
        (
            "m_Vertices",
            arr(CUBE_POSITIONS.iter().map(|p| vec3(*p)).collect()),
        ),
        (
            "m_Faces",
            arr(CUBE_FACES
                .iter()
                .map(|&e| obj(vec![("m_nEdge", i(e as i64))]))
                .collect()),
        ),
        ("m_Planes", arr(vec![])),
        // m_Edges intentionally missing.
    ]);
    let root = phys_root_with_hull(hull);
    match decode(&root) {
        Err(PhysError::Missing { path }) => {
            assert_eq!(path, "m_parts[0].m_rnShape.m_hulls[0].m_Hull.m_Edges");
        }
        other => panic!("expected Missing, got {other:?}"),
    }
}

#[test]
fn bad_blob_length_reports_key_path() {
    let mut entries = cube_bounds_and_centroid();
    entries.push(("m_VertexPositions", Value::Blob(cube_positions_blob())));
    entries.push(("m_Vertices", Value::Blob((0u8..8).collect())));
    entries.push(("m_Edges", Value::Blob(vec![0u8, 1, 2]))); // not a multiple of 4
    entries.push(("m_Faces", Value::Blob(CUBE_FACES.to_vec())));
    entries.push(("m_Planes", Value::Blob(cube_planes_blob())));
    let hull = obj(entries);

    let root = phys_root_with_hull(hull);
    match decode(&root) {
        Err(PhysError::BadBlobLength {
            path,
            len,
            element_size,
        }) => {
            assert_eq!(path, "m_parts[0].m_rnShape.m_hulls[0].m_Hull.m_Edges");
            assert_eq!(len, 3);
            assert_eq!(element_size, 4);
        }
        other => panic!("expected BadBlobLength, got {other:?}"),
    }
}
