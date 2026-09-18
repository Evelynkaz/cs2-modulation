//! Decodes a PHYS KV3 tree into [`super::PhysAggregate`]. Every hull/mesh array may be a KV3
//! binary blob (packed LE structs) or a KV3 array (of objects, numbers, or vec3 arrays) --
//! `docs/FORMATS.md` 6.3/6.4; VRF `Hull.cs`/`Mesh.cs` check `KVObject.IsNotBlobType` at each call
//! site the same way.

use super::{
    Capsule, CollisionAttribute, HalfEdge, Hull, Mat3x4, Mesh, Part, PhysAggregate, PhysError,
    Plane, Shape, ShapeDesc, Sphere,
};
use crate::kv3::Value;

/// Top-level decode entry point; see [`super::decode`].
pub(super) fn build(root: &Value) -> Result<PhysAggregate, PhysError> {
    let flags = root.get("m_nFlags").and_then(Value::as_i64).unwrap_or(0);

    let bind_pose = match root.get("m_bindPose") {
        Some(v) => {
            let arr = require_array(v, "m_bindPose")?;
            arr.iter()
                .enumerate()
                .map(|(i, m)| parse_mat3x4(m, &format!("m_bindPose[{i}]")))
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };

    let parts = match root.get("m_parts") {
        Some(v) => {
            let arr = require_array(v, "m_parts")?;
            arr.iter()
                .enumerate()
                .map(|(i, p)| parse_part(p, &format!("m_parts[{i}]")))
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };

    let surface_property_hashes = match root.get("m_surfacePropertyHashes") {
        Some(v) => {
            let arr = require_array(v, "m_surfacePropertyHashes")?;
            arr.iter()
                .enumerate()
                .map(|(i, e)| {
                    e.as_u64()
                        .map(|u| u as u32)
                        .ok_or_else(|| PhysError::WrongType {
                            path: format!("m_surfacePropertyHashes[{i}]"),
                            expected: "unsigned integer",
                        })
                })
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };

    let collision_attributes = match root.get("m_collisionAttributes") {
        Some(v) => {
            let arr = require_array(v, "m_collisionAttributes")?;
            arr.iter().map(parse_collision_attribute).collect()
        }
        None => Vec::new(),
    };

    Ok(PhysAggregate {
        flags,
        bind_pose,
        parts,
        surface_property_hashes,
        collision_attributes,
    })
}

fn require_array<'a>(v: &'a Value, path: &str) -> Result<&'a [Value], PhysError> {
    v.as_array().ok_or_else(|| PhysError::WrongType {
        path: path.to_string(),
        expected: "array",
    })
}

fn parse_part(v: &Value, path: &str) -> Result<Part, PhysError> {
    let flags = v.get("m_nFlags").and_then(Value::as_i64).unwrap_or(0);
    let collision_attribute_index = v
        .get("m_nCollisionAttributeIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let shape_val = v.get("m_rnShape").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_rnShape"),
    })?;
    let shape = parse_shape(shape_val, &format!("{path}.m_rnShape"))?;
    Ok(Part {
        flags,
        collision_attribute_index,
        shape,
    })
}

fn parse_shape(v: &Value, path: &str) -> Result<Shape, PhysError> {
    Ok(Shape {
        spheres: shape_descriptors(v, "m_spheres", path, "m_Sphere", parse_sphere)?,
        capsules: shape_descriptors(v, "m_capsules", path, "m_Capsule", parse_capsule)?,
        hulls: shape_descriptors(v, "m_hulls", path, "m_Hull", parse_hull)?,
        meshes: shape_descriptors(v, "m_meshes", path, "m_Mesh", parse_mesh)?,
    })
}

/// Loads `shape_obj.<key>` (an array of shape descriptors), each carrying its payload under
/// `<member>` (VRF `ShapeDescriptor.KV3Transfer`: `"m_" + typeof(T).Name`).
fn shape_descriptors<T>(
    shape_obj: &Value,
    key: &str,
    path: &str,
    member: &str,
    parse_shape: impl Fn(&Value, &str) -> Result<T, PhysError>,
) -> Result<Vec<ShapeDesc<T>>, PhysError> {
    let arr = match shape_obj.get(key) {
        Some(v) => require_array(v, &format!("{path}.{key}"))?,
        None => return Ok(Vec::new()),
    };
    arr.iter()
        .enumerate()
        .map(|(i, d)| {
            let p = format!("{path}.{key}[{i}]");
            let collision_attribute_index = d
                .get("m_nCollisionAttributeIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let surface_property_index = d
                .get("m_nSurfacePropertyIndex")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let user_friendly_name = d
                .get("m_UserFriendlyName")
                .and_then(Value::as_str)
                .map(str::to_string);
            let tool_material_hash = d.get("m_nToolMaterialHash").and_then(Value::as_u64);
            let shape_val = d.get(member).ok_or_else(|| PhysError::Missing {
                path: format!("{p}.{member}"),
            })?;
            let shape = parse_shape(shape_val, &format!("{p}.{member}"))?;
            Ok(ShapeDesc {
                collision_attribute_index,
                surface_property_index,
                user_friendly_name,
                tool_material_hash,
                shape,
            })
        })
        .collect()
}

fn parse_sphere(v: &Value, path: &str) -> Result<Sphere, PhysError> {
    let center = vec3_field(v, "m_vCenter", path)?;
    let radius = float_field(v, "m_flRadius");
    Ok(Sphere { center, radius })
}

fn parse_capsule(v: &Value, path: &str) -> Result<Capsule, PhysError> {
    let center_val = v.get("m_vCenter").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_vCenter"),
    })?;
    let arr = require_array(center_val, &format!("{path}.m_vCenter"))?;
    if arr.len() != 2 {
        return Err(PhysError::WrongType {
            path: format!("{path}.m_vCenter"),
            expected: "array of exactly 2 vec3 (capsule endpoints)",
        });
    }
    let centers = [
        vec3_of(&arr[0], &format!("{path}.m_vCenter[0]"))?,
        vec3_of(&arr[1], &format!("{path}.m_vCenter[1]"))?,
    ];
    let radius = float_field(v, "m_flRadius");
    Ok(Capsule { centers, radius })
}

fn parse_hull(v: &Value, path: &str) -> Result<Hull, PhysError> {
    let centroid = vec3_field(v, "m_vCentroid", path)?;
    let bounds = v.get("m_Bounds").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Bounds"),
    })?;
    let bounds_path = format!("{path}.m_Bounds");
    let bounds_min = vec3_field(bounds, "m_vMinBounds", &bounds_path)?;
    let bounds_max = vec3_field(bounds, "m_vMaxBounds", &bounds_path)?;
    let flags = v.get("m_nFlags").and_then(Value::as_u64).unwrap_or(0) as u32;

    // 2023-11-04+: explicit vertex indices (VRF `Hull.cs` `HasExplicitVertexIndices`, ~223-224).
    let (vertex_positions, vertex_indices) = if let Some(positions_val) = v.get("m_VertexPositions")
    {
        let positions = vec3_array(positions_val, &format!("{path}.m_VertexPositions"))?;
        let indices_val = v.get("m_Vertices").ok_or_else(|| PhysError::Missing {
            path: format!("{path}.m_Vertices"),
        })?;
        let indices = u8_array(indices_val, &format!("{path}.m_Vertices"))?;
        (positions, Some(indices))
    } else {
        let vertices_val = v.get("m_Vertices").ok_or_else(|| PhysError::Missing {
            path: format!("{path}.m_Vertices"),
        })?;
        let positions = vec3_array(vertices_val, &format!("{path}.m_Vertices"))?;
        (positions, None)
    };

    let edges_val = v.get("m_Edges").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Edges"),
    })?;
    let edges = edges_array(edges_val, &format!("{path}.m_Edges"))?;

    let faces_val = v.get("m_Faces").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Faces"),
    })?;
    let faces = faces_array(faces_val, &format!("{path}.m_Faces"))?;

    let planes_val = v.get("m_Planes").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Planes"),
    })?;
    let planes = planes_array(planes_val, &format!("{path}.m_Planes"))?;

    Ok(Hull {
        centroid,
        bounds_min,
        bounds_max,
        flags,
        vertex_positions,
        vertex_indices,
        edges,
        faces,
        planes,
    })
}

fn parse_mesh(v: &Value, path: &str) -> Result<Mesh, PhysError> {
    let min = vec3_field(v, "m_vMin", path)?;
    let max = vec3_field(v, "m_vMax", path)?;
    let flags = v.get("m_nFlags").and_then(Value::as_u64).unwrap_or(0) as u32;

    let vertices_val = v.get("m_Vertices").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Vertices"),
    })?;
    let vertices = vec3_array(vertices_val, &format!("{path}.m_Vertices"))?;

    let triangles_val = v.get("m_Triangles").ok_or_else(|| PhysError::Missing {
        path: format!("{path}.m_Triangles"),
    })?;
    let triangles = triangles_array(triangles_val, &format!("{path}.m_Triangles"))?;

    let materials = materials_array(v.get("m_Materials"), &format!("{path}.m_Materials"))?;
    if !materials.is_empty() && materials.len() != triangles.len() {
        return Err(PhysError::LengthMismatch {
            path: format!("{path}.m_Materials"),
            expected: triangles.len(),
            actual: materials.len(),
        });
    }

    Ok(Mesh {
        min,
        max,
        flags,
        vertices,
        triangles,
        materials,
    })
}

fn parse_collision_attribute(v: &Value) -> CollisionAttribute {
    let group = v
        .get("m_CollisionGroupString")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Older assets carry the tags under `m_PhysicsTagStrings` (`docs/FORMATS.md` 6.5).
    let interact_as = str_list(v, "m_InteractAsStrings")
        .or_else(|| str_list(v, "m_PhysicsTagStrings"))
        .unwrap_or_default();
    let interact_with = str_list(v, "m_InteractWithStrings").unwrap_or_default();
    let interact_exclude = str_list(v, "m_InteractExcludeStrings").unwrap_or_default();
    CollisionAttribute {
        group,
        interact_as,
        interact_with,
        interact_exclude,
    }
}

fn str_list(v: &Value, key: &str) -> Option<Vec<String>> {
    v.get(key).and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect()
    })
}

fn float_field(v: &Value, key: &str) -> f32 {
    v.get(key).and_then(Value::as_f32).unwrap_or(0.0)
}

fn byte_field(v: &Value, key: &str) -> u8 {
    v.get(key)
        .and_then(Value::as_i64)
        .map(|i| i as u8)
        .unwrap_or(0)
}

fn vec3_of(v: &Value, path: &str) -> Result<[f32; 3], PhysError> {
    let arr = require_array(v, path)?;
    if arr.len() < 3 {
        return Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "array of >= 3 numbers (vec3)",
        });
    }
    Ok([
        arr[0].as_f32().unwrap_or(0.0),
        arr[1].as_f32().unwrap_or(0.0),
        arr[2].as_f32().unwrap_or(0.0),
    ])
}

fn vec3_field(v: &Value, key: &str, path: &str) -> Result<[f32; 3], PhysError> {
    let sub = v.get(key).ok_or_else(|| PhysError::Missing {
        path: format!("{path}.{key}"),
    })?;
    vec3_of(sub, &format!("{path}.{key}"))
}

/// A vec3 array: KV3 blob of packed `f32×3` (12 bytes each), or an array of vec3 arrays
/// (`docs/FORMATS.md` 6.3/6.4).
fn vec3_array(v: &Value, path: &str) -> Result<Vec<[f32; 3]>, PhysError> {
    if let Some(blob) = v.as_blob() {
        if !blob.len().is_multiple_of(12) {
            return Err(PhysError::BadBlobLength {
                path: path.to_string(),
                len: blob.len(),
                element_size: 12,
            });
        }
        Ok(blob
            .as_chunks::<12>()
            .0
            .iter()
            .map(|c| {
                [
                    f32::from_le_bytes(c[0..4].try_into().unwrap()),
                    f32::from_le_bytes(c[4..8].try_into().unwrap()),
                    f32::from_le_bytes(c[8..12].try_into().unwrap()),
                ]
            })
            .collect())
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .enumerate()
            .map(|(i, e)| vec3_of(e, &format!("{path}[{i}]")))
            .collect()
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "vec3 array or blob",
        })
    }
}

/// A u8 index list: KV3 blob of raw bytes, or an array of integers (hull `m_Vertices` in the new
/// on-disk format).
fn u8_array(v: &Value, path: &str) -> Result<Vec<u8>, PhysError> {
    if let Some(blob) = v.as_blob() {
        Ok(blob.to_vec())
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .map(|e| {
                e.as_i64()
                    .map(|i| i as u8)
                    .ok_or_else(|| PhysError::WrongType {
                        path: path.to_string(),
                        expected: "array of u8 indices",
                    })
            })
            .collect()
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "u8 array or blob",
        })
    }
}

/// A half-edge array: KV3 blob of packed `{u8 next, u8 twin, u8 origin, u8 face}` (4 bytes each),
/// or an array of objects with `m_nNext`/`m_nTwin`/`m_nOrigin`/`m_nFace`.
fn edges_array(v: &Value, path: &str) -> Result<Vec<HalfEdge>, PhysError> {
    if let Some(blob) = v.as_blob() {
        if !blob.len().is_multiple_of(4) {
            return Err(PhysError::BadBlobLength {
                path: path.to_string(),
                len: blob.len(),
                element_size: 4,
            });
        }
        Ok(blob
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| HalfEdge {
                next: c[0],
                twin: c[1],
                origin: c[2],
                face: c[3],
            })
            .collect())
    } else if let Some(arr) = v.as_array() {
        Ok(arr
            .iter()
            .map(|e| HalfEdge {
                next: byte_field(e, "m_nNext"),
                twin: byte_field(e, "m_nTwin"),
                origin: byte_field(e, "m_nOrigin"),
                face: byte_field(e, "m_nFace"),
            })
            .collect())
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "edge array or blob",
        })
    }
}

/// A face array: KV3 blob of packed `{u8 edge}` (1 byte each), or an array of `{m_nEdge}`
/// objects.
fn faces_array(v: &Value, path: &str) -> Result<Vec<u8>, PhysError> {
    if let Some(blob) = v.as_blob() {
        Ok(blob.to_vec())
    } else if let Some(arr) = v.as_array() {
        Ok(arr.iter().map(|e| byte_field(e, "m_nEdge")).collect())
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "face array or blob",
        })
    }
}

/// A plane array: KV3 blob of packed `{vec3 normal, f32 offset}` (16 bytes each), or an array of
/// objects with `m_vNormal`/`m_flOffset`.
fn planes_array(v: &Value, path: &str) -> Result<Vec<Plane>, PhysError> {
    if let Some(blob) = v.as_blob() {
        if !blob.len().is_multiple_of(16) {
            return Err(PhysError::BadBlobLength {
                path: path.to_string(),
                len: blob.len(),
                element_size: 16,
            });
        }
        Ok(blob
            .as_chunks::<16>()
            .0
            .iter()
            .map(|c| Plane {
                normal: [
                    f32::from_le_bytes(c[0..4].try_into().unwrap()),
                    f32::from_le_bytes(c[4..8].try_into().unwrap()),
                    f32::from_le_bytes(c[8..12].try_into().unwrap()),
                ],
                offset: f32::from_le_bytes(c[12..16].try_into().unwrap()),
            })
            .collect())
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .enumerate()
            .map(|(i, e)| {
                let p = format!("{path}[{i}]");
                let normal = vec3_field(e, "m_vNormal", &p)?;
                Ok(Plane {
                    normal,
                    offset: float_field(e, "m_flOffset"),
                })
            })
            .collect()
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "plane array or blob",
        })
    }
}

/// A mesh triangle array: KV3 blob of packed `{i32 a, i32 b, i32 c}` (12 bytes each), or an array
/// of `{m_nIndex: [3]}` objects.
fn triangles_array(v: &Value, path: &str) -> Result<Vec<[u32; 3]>, PhysError> {
    if let Some(blob) = v.as_blob() {
        if !blob.len().is_multiple_of(12) {
            return Err(PhysError::BadBlobLength {
                path: path.to_string(),
                len: blob.len(),
                element_size: 12,
            });
        }
        Ok(blob
            .as_chunks::<12>()
            .0
            .iter()
            .map(|c| {
                [
                    i32::from_le_bytes(c[0..4].try_into().unwrap()) as u32,
                    i32::from_le_bytes(c[4..8].try_into().unwrap()) as u32,
                    i32::from_le_bytes(c[8..12].try_into().unwrap()) as u32,
                ]
            })
            .collect())
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .enumerate()
            .map(|(i, e)| {
                let p = format!("{path}[{i}]");
                let idx_val = e.get("m_nIndex").ok_or_else(|| PhysError::Missing {
                    path: format!("{p}.m_nIndex"),
                })?;
                let idx = require_array(idx_val, &format!("{p}.m_nIndex"))?;
                if idx.len() != 3 {
                    return Err(PhysError::WrongType {
                        path: format!("{p}.m_nIndex"),
                        expected: "exactly 3 indices",
                    });
                }
                Ok([
                    idx[0].as_i64().unwrap_or(0) as u32,
                    idx[1].as_i64().unwrap_or(0) as u32,
                    idx[2].as_i64().unwrap_or(0) as u32,
                ])
            })
            .collect()
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "triangle array or blob",
        })
    }
}

/// Per-triangle surface property index: KV3 blob of one `u8` per triangle, or an array of
/// integers. Missing or empty means the whole mesh shares one material (checked against the
/// triangle count by the caller).
fn materials_array(v: Option<&Value>, path: &str) -> Result<Vec<u32>, PhysError> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    if let Some(blob) = v.as_blob() {
        Ok(blob.iter().map(|&b| b as u32).collect())
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .enumerate()
            .map(|(i, e)| {
                e.as_i64()
                    .map(|n| n as u32)
                    .ok_or_else(|| PhysError::WrongType {
                        path: format!("{path}[{i}]"),
                        expected: "integer",
                    })
            })
            .collect()
    } else {
        Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "materials array or blob",
        })
    }
}

/// A `Mat3x4`: flat form (12 or 16 floats, row-major, `rows[i] = [f[4i], f[4i+1], f[4i+2],
/// f[4i+3]]`), a KV3 blob of the same floats, or the nested form (3 or 4 vec4 rows) --
/// `docs/FORMATS.md` 6.6.
fn parse_mat3x4(v: &Value, path: &str) -> Result<Mat3x4, PhysError> {
    if let Some(blob) = v.as_blob() {
        let floats = blob_f32s(blob, path)?;
        return mat_from_flat_floats(&floats, path);
    }
    if let Some(arr) = v.as_array() {
        if arr.first().is_some_and(|e| e.as_array().is_some()) {
            // Nested form: 3 or 4 vec4 rows, each row directly `rows[i]`.
            let mut rows = [[0.0f32; 4]; 3];
            for (i, row) in rows.iter_mut().enumerate() {
                let row_val = arr.get(i).ok_or_else(|| PhysError::IndexOutOfRange {
                    path: path.to_string(),
                    index: i,
                    len: arr.len(),
                })?;
                let row_arr = row_val.as_array().ok_or_else(|| PhysError::WrongType {
                    path: format!("{path}[{i}]"),
                    expected: "vec4 row",
                })?;
                for (j, out) in row.iter_mut().enumerate() {
                    *out = row_arr.get(j).and_then(Value::as_f32).unwrap_or(0.0);
                }
            }
            return Ok(Mat3x4 { rows });
        }
        let floats: Vec<f32> = arr.iter().map(|e| e.as_f32().unwrap_or(0.0)).collect();
        return mat_from_flat_floats(&floats, path);
    }
    Err(PhysError::WrongType {
        path: path.to_string(),
        expected: "matrix (blob, flat array, or nested array)",
    })
}

fn blob_f32s(blob: &[u8], path: &str) -> Result<Vec<f32>, PhysError> {
    if !blob.len().is_multiple_of(4) {
        return Err(PhysError::BadBlobLength {
            path: path.to_string(),
            len: blob.len(),
            element_size: 4,
        });
    }
    Ok(blob
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect())
}

fn mat_from_flat_floats(f: &[f32], path: &str) -> Result<Mat3x4, PhysError> {
    if f.len() != 12 && f.len() != 16 {
        return Err(PhysError::WrongType {
            path: path.to_string(),
            expected: "12 or 16 floats",
        });
    }
    let mut rows = [[0.0f32; 4]; 3];
    for (i, row) in rows.iter_mut().enumerate() {
        *row = [f[4 * i], f[4 * i + 1], f[4 * i + 2], f[4 * i + 3]];
    }
    Ok(Mat3x4 { rows })
}
