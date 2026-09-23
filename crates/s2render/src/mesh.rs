//! A render mesh (`vmesh_c`, or one mesh embedded in a `vmdl_c`): decompressed vertex/index
//! buffers plus draw calls (`m_sceneObjects[].m_drawCalls[]`; `VBIB.cs:36-71`,
//! `GltfModelExporter.Mesh.cs:36-71`, `319-408`).

use s2fmt::kv3::Value;
use s2fmt::resource::{FourCC, Resource};

use crate::buffer::{self, Buffer};
use crate::error::MeshError;

/// `m_nFlags` on a draw call: named (KV3 flag-name string) or numeric, kept as-is since decoding
/// the flag bits themselves is out of scope here (`s6f3_common.md`).
#[derive(Debug, Clone, PartialEq)]
pub enum DrawCallFlags {
    None,
    Named(String),
    Int(i64),
}

/// One `m_drawCalls[]` entry (`VBIB.cs`'s consumer, `GltfModelExporter.Mesh.cs:319-408`).
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCall {
    /// `m_material`, falling back to `m_pMaterial` (`Mesh.cs:228-233 GetMaterialName`).
    pub material_path: Option<String>,
    /// `m_nPrimitiveType == RENDER_PRIM_TRIANGLES` (either the numeric value 5 or that name as a
    /// KV3 flag string); anything else isn't decoded further here, only reported.
    pub is_triangle_list: bool,
    pub base_vertex: i64,
    pub start_index: i64,
    pub index_count: i64,
    pub vertex_count: i64,
    /// Index into the owning [`Mesh::index_buffers`] (`m_indexBuffer.m_hBuffer`).
    pub index_buffer: usize,
    /// Indices into the owning [`Mesh::vertex_buffers`], one per stream (`m_vertexBuffers[].m_hBuffer`).
    pub vertex_buffers: Vec<usize>,
    pub tint_color: Option<[f32; 3]>,
    pub alpha: Option<f32>,
    pub flags: DrawCallFlags,
}

impl DrawCall {
    /// Absolute vertex indices for this draw call: `[m_nStartIndex, m_nStartIndex +
    /// m_nIndexCount)` of the index buffer, each plus `m_nBaseVertex`
    /// (`GltfModelExporter.Mesh.cs:711-736 ReadIndices`). Every step is checked against the file's
    /// own data before it's used to size or index anything: `m_nStartIndex`/`m_nIndexCount` must
    /// fit inside the index buffer *before* the output `Vec` is allocated (a hostile
    /// `m_nIndexCount` must not reach `Vec::with_capacity` at all), and each resolved index must
    /// land below the smallest of the draw call's vertex buffers so callers can use it to index
    /// attribute arrays (`FloatAttribute::get`) without panicking.
    pub fn resolve_indices(&self, mesh: &Mesh) -> Result<Vec<u32>, MeshError> {
        let buf = mesh.index_buffers.get(self.index_buffer).ok_or_else(|| {
            MeshError::IndexOutOfRange {
                path: "m_indexBuffer.m_hBuffer".to_string(),
                index: self.index_buffer as i64,
                len: mesh.index_buffers.len(),
            }
        })?;
        let start = usize::try_from(self.start_index).map_err(|_| MeshError::WrongType {
            path: "m_nStartIndex".to_string(),
            expected: "non-negative",
        })?;
        let count = usize::try_from(self.index_count).map_err(|_| MeshError::WrongType {
            path: "m_nIndexCount".to_string(),
            expected: "non-negative",
        })?;
        let buf_len = match buf.element_size {
            2 | 4 => buf.data.len() / buf.element_size as usize,
            other => {
                return Err(MeshError::BadIndexElementSize {
                    path: "m_indexBuffer".to_string(),
                    size: other,
                });
            }
        };
        let end = start
            .checked_add(count)
            .filter(|&end| end <= buf_len)
            .ok_or_else(|| MeshError::IndexOutOfRange {
                path: "m_nStartIndex + m_nIndexCount".to_string(),
                index: count as i64,
                len: buf_len,
            })?;

        // The smallest vertex buffer this draw call reads from: every resolved index must stay
        // below it. A handle that doesn't resolve to a real buffer contributes no bound (its own
        // out-of-range error surfaces wherever that buffer is actually looked up).
        let vertex_bound = self
            .vertex_buffers
            .iter()
            .filter_map(|&vb| {
                mesh.vertex_buffers
                    .get(vb)
                    .map(|b| b.element_count as usize)
            })
            .min();

        let mut out = Vec::with_capacity(count);
        for i in start..end {
            let raw = buf.index_at(i).ok_or_else(|| MeshError::IndexOutOfRange {
                path: "m_indexBuffer".to_string(),
                index: i as i64,
                len: buf_len,
            })?;
            let resolved = i64::from(raw)
                .checked_add(self.base_vertex)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| MeshError::IndexOutOfRange {
                    path: "m_nBaseVertex".to_string(),
                    index: i64::from(raw).saturating_add(self.base_vertex),
                    len: vertex_bound.unwrap_or(0),
                })?;
            if let Some(bound) = vertex_bound
                && resolved as usize >= bound
            {
                return Err(MeshError::IndexOutOfRange {
                    path: "m_vertexBuffers".to_string(),
                    index: i64::from(resolved),
                    len: bound,
                });
            }
            out.push(resolved);
        }
        Ok(out)
    }
}

/// One `m_sceneObjects[]` entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneObject {
    pub draw_calls: Vec<DrawCall>,
}

/// A fully decoded render mesh: its own buffers plus draw calls.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mesh {
    pub vertex_buffers: Vec<Buffer>,
    pub index_buffers: Vec<Buffer>,
    pub scene_objects: Vec<SceneObject>,
}

fn require_array<'a>(v: &'a Value, path: &str) -> Result<&'a [Value], MeshError> {
    v.as_array().ok_or_else(|| MeshError::WrongType {
        path: path.to_string(),
        expected: "array",
    })
}

fn require_i64(v: &Value, key: &str, path: &str) -> Result<i64, MeshError> {
    v.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| MeshError::Missing {
            path: format!("{path}.{key}"),
        })
}

fn vec3(v: &Value, key: &str) -> Option<[f32; 3]> {
    let arr = v.get(key)?.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    Some([arr[0].as_f32()?, arr[1].as_f32()?, arr[2].as_f32()?])
}

fn draw_call_flags(v: &Value, key: &str) -> DrawCallFlags {
    match v.get(key) {
        Some(f) => {
            if let Some(s) = f.as_str() {
                DrawCallFlags::Named(s.to_string())
            } else if let Some(i) = f.as_i64() {
                DrawCallFlags::Int(i)
            } else {
                DrawCallFlags::None
            }
        }
        None => DrawCallFlags::None,
    }
}

fn buffer_handle(v: &Value, path: &str) -> Result<usize, MeshError> {
    let h = v
        .get("m_hBuffer")
        .and_then(Value::as_i64)
        .ok_or_else(|| MeshError::Missing {
            path: format!("{path}.m_hBuffer"),
        })?;
    usize::try_from(h).map_err(|_| MeshError::WrongType {
        path: format!("{path}.m_hBuffer"),
        expected: "non-negative buffer handle",
    })
}

fn parse_draw_call(v: &Value, path: &str) -> Result<DrawCall, MeshError> {
    let material_path = v
        .get("m_material")
        .and_then(Value::as_str)
        .or_else(|| v.get("m_pMaterial").and_then(Value::as_str))
        .map(str::to_string);

    let is_triangle_list = match v.get("m_nPrimitiveType") {
        Some(p) => {
            if let Some(s) = p.as_str() {
                s.eq_ignore_ascii_case("RENDER_PRIM_TRIANGLES")
            } else if let Some(i) = p.as_i64() {
                i == 5 // RenderPrimitiveType.RENDER_PRIM_TRIANGLES (RenderPrimitiveType.cs:15)
            } else {
                false
            }
        }
        None => false,
    };

    let index_buffer_val = v.get("m_indexBuffer").ok_or_else(|| MeshError::Missing {
        path: format!("{path}.m_indexBuffer"),
    })?;
    let index_buffer = buffer_handle(index_buffer_val, &format!("{path}.m_indexBuffer"))?;

    let vertex_buffers = match v.get("m_vertexBuffers") {
        Some(vb) => {
            let arr = require_array(vb, &format!("{path}.m_vertexBuffers"))?;
            arr.iter()
                .enumerate()
                .map(|(i, e)| buffer_handle(e, &format!("{path}.m_vertexBuffers[{i}]")))
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };

    Ok(DrawCall {
        material_path,
        is_triangle_list,
        base_vertex: require_i64(v, "m_nBaseVertex", path)?,
        start_index: require_i64(v, "m_nStartIndex", path)?,
        index_count: require_i64(v, "m_nIndexCount", path)?,
        vertex_count: require_i64(v, "m_nVertexCount", path)?,
        index_buffer,
        vertex_buffers,
        tint_color: vec3(v, "m_vTintColor"),
        alpha: v.get("m_flAlpha").and_then(Value::as_f32),
        flags: draw_call_flags(v, "m_nFlags"),
    })
}

/// Decodes `root.m_sceneObjects[].m_drawCalls[]`. Missing `m_sceneObjects` decodes as no scene
/// objects, matching this codebase's tolerance for absent optional KV3 keys.
pub fn decode_scene_objects(root: &Value, path: &str) -> Result<Vec<SceneObject>, MeshError> {
    let Some(v) = root.get("m_sceneObjects") else {
        return Ok(Vec::new());
    };
    let arr = require_array(v, &format!("{path}.m_sceneObjects"))?;
    arr.iter()
        .enumerate()
        .map(|(i, so)| {
            let p = format!("{path}.m_sceneObjects[{i}]");
            let draw_calls = match so.get("m_drawCalls") {
                Some(dv) => {
                    let darr = require_array(dv, &format!("{p}.m_drawCalls"))?;
                    darr.iter()
                        .enumerate()
                        .map(|(j, dc)| parse_draw_call(dc, &format!("{p}.m_drawCalls[{j}]")))
                        .collect::<Result<_, _>>()?
                }
                None => Vec::new(),
            };
            Ok(SceneObject { draw_calls })
        })
        .collect()
}

/// Decodes a standalone `vmesh_c` resource: its own `DATA` root carries both the buffers (either
/// a binary `VBIB` block, or the KV3 `m_vertexBuffers`/`m_indexBuffers` form) and the scene
/// objects (`Mesh.cs:20-32`'s `VBIB` getter; `GetBounds`).
pub fn decode_mesh_resource(resource: &Resource, path: &str) -> Result<Mesh, MeshError> {
    let doc = resource.data_kv3().map_err(|source| MeshError::Resource {
        path: path.to_string(),
        source: Box::new(source),
    })?;
    let root = &doc.root;

    let (vertex_buffers, index_buffers) = if let Some(vbib_block) = resource.block(FourCC(*b"VBIB"))
    {
        buffer::parse_vbib_block(resource.block_bytes(vbib_block))?
    } else {
        buffer::parse_kv3_buffers(resource, root, path)?
    };

    let scene_objects = decode_scene_objects(root, path)?;

    Ok(Mesh {
        vertex_buffers,
        index_buffers,
        scene_objects,
    })
}
