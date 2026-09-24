//! A minimal glTF 2.0 / GLB writer: just enough of the structural plumbing (buffers,
//! bufferViews, accessors, images) that `export.rs` needs, leaving every domain decision
//! (what a material's JSON looks like, node grouping, `extras`) to the caller. Hand-rolled
//! rather than an external crate: the surface used here is small, fixed, and fully under this
//! project's control (`s6f3a3_map.md`'s constraint: new dependencies only when there is no other
//! way).
//!
//! Geometry stays in Source 2's own units and axes (inches, Z-up) -- §6's deliberate deviation
//! from the glTF convention (Y-up, meters) so a click in the 3D view is already a `setpos`.

use serde_json::{Map, Value, json};

/// A GLB chunk (JSON or BIN) grew past glTF's own `u32` chunk-length field -- geometry/textures
/// beyond 4 GiB, which this crate refuses to silently truncate into a corrupt file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("GLB output exceeds the 4 GiB glTF binary chunk-length limit: {bytes} bytes")]
pub struct GlbTooLarge {
    pub bytes: usize,
}

const COMPONENT_TYPE_UNSIGNED_SHORT: u32 = 5123;
const COMPONENT_TYPE_UNSIGNED_INT: u32 = 5125;
const COMPONENT_TYPE_FLOAT: u32 = 5126;

const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// Accumulates a glTF document's binary blob and JSON arrays, and writes the final `.glb`.
#[derive(Default)]
pub struct GltfBuilder {
    bin: Vec<u8>,
    buffer_views: Vec<Value>,
    accessors: Vec<Value>,
    images: Vec<Value>,
    textures: Vec<Value>,
    samplers: Vec<Value>,
    materials: Vec<Value>,
    meshes: Vec<Value>,
    nodes: Vec<Value>,
    image_bytes: usize,
}

fn pad4(buf: &mut Vec<u8>, pad_byte: u8) {
    while !buf.len().is_multiple_of(4) {
        buf.push(pad_byte);
    }
}

impl GltfBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `bytes` to the binary blob (4-byte aligned) and records a `bufferView`, returning
    /// its index.
    fn push_buffer_view(&mut self, bytes: &[u8], target: Option<u32>) -> u32 {
        pad4(&mut self.bin, 0);
        let offset = self.bin.len();
        self.bin.extend_from_slice(bytes);
        let mut view = Map::new();
        view.insert("buffer".into(), json!(0));
        view.insert("byteOffset".into(), json!(offset));
        view.insert("byteLength".into(), json!(bytes.len()));
        if let Some(t) = target {
            view.insert("target".into(), json!(t));
        }
        self.buffer_views.push(Value::Object(view));
        (self.buffer_views.len() - 1) as u32
    }

    #[allow(clippy::too_many_arguments)]
    fn push_accessor(
        &mut self,
        view: u32,
        component_type: u32,
        count: usize,
        ty: &str,
        normalized: bool,
        min: Option<Value>,
        max: Option<Value>,
    ) -> u32 {
        let mut a = Map::new();
        a.insert("bufferView".into(), json!(view));
        a.insert("componentType".into(), json!(component_type));
        a.insert("count".into(), json!(count));
        a.insert("type".into(), json!(ty));
        if normalized {
            a.insert("normalized".into(), json!(true));
        }
        if let Some(min) = min {
            a.insert("min".into(), min);
        }
        if let Some(max) = max {
            a.insert("max".into(), max);
        }
        self.accessors.push(Value::Object(a));
        (self.accessors.len() - 1) as u32
    }

    /// POSITION accessor (glTF requires `min`/`max` on it).
    pub fn add_positions(&mut self, positions: &[[f32; 3]]) -> u32 {
        let mut bytes = Vec::with_capacity(positions.len() * 12);
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in positions {
            for c in 0..3 {
                bytes.extend_from_slice(&p[c].to_le_bytes());
                min[c] = min[c].min(p[c]);
                max[c] = max[c].max(p[c]);
            }
        }
        let view = self.push_buffer_view(&bytes, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_FLOAT,
            positions.len(),
            "VEC3",
            false,
            Some(json!(min)),
            Some(json!(max)),
        )
    }

    pub fn add_vec3(&mut self, values: &[[f32; 3]]) -> u32 {
        let mut bytes = Vec::with_capacity(values.len() * 12);
        for v in values {
            for c in v {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        let view = self.push_buffer_view(&bytes, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_FLOAT,
            values.len(),
            "VEC3",
            false,
            None,
            None,
        )
    }

    pub fn add_vec2(&mut self, values: &[[f32; 2]]) -> u32 {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for v in values {
            for c in v {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        let view = self.push_buffer_view(&bytes, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_FLOAT,
            values.len(),
            "VEC2",
            false,
            None,
            None,
        )
    }

    /// `_BLEND`: one plain (non-normalized) `f32` per vertex, clamped to `0..1`. The source data
    /// is an 8-bit UNORM channel (`VertexPaintBlendParams`, `R8G8B8A8_UNORM`; `REPORT.md` §5 item
    /// 5), so a tightly-packed normalized `u8` SCALAR accessor would be the smaller encoding --
    /// but glTF 2.0 §3.6.2.4 requires every vertex attribute accessor's elements to be 4-byte
    /// aligned, which a 1-byte `SCALAR` accessor violates (the Khronos validator reported 360
    /// `MESH_PRIMITIVE_ACCESSOR_UNALIGNED` errors on Mirage, 57 on Nuke); `f32` is the smallest
    /// component type that stays validator-clean for a `SCALAR` accessor.
    pub fn add_blend_f32(&mut self, values: &[f32]) -> u32 {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for &v in values {
            bytes.extend_from_slice(&v.clamp(0.0, 1.0).to_le_bytes());
        }
        let view = self.push_buffer_view(&bytes, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_FLOAT,
            values.len(),
            "SCALAR",
            false,
            None,
            None,
        )
    }

    /// Indices, packed as `u16` when they fit (most draw calls) or `u32` otherwise. `u16::MAX`
    /// (0xFFFF) itself is reserved as the primitive-restart value and must never appear as a real
    /// index (glTF 2.0's own indices-accessor rule), so a lone value of exactly 65535 forces the
    /// wider `u32` encoding even though it would otherwise fit a `u16`.
    pub fn add_indices(&mut self, indices: &[u32]) -> u32 {
        let fits_u16 = indices.iter().all(|&i| i < u32::from(u16::MAX));
        if fits_u16 {
            let mut bytes = Vec::with_capacity(indices.len() * 2);
            for &i in indices {
                bytes.extend_from_slice(&(i as u16).to_le_bytes());
            }
            let view = self.push_buffer_view(&bytes, Some(TARGET_ELEMENT_ARRAY_BUFFER));
            self.push_accessor(
                view,
                COMPONENT_TYPE_UNSIGNED_SHORT,
                indices.len(),
                "SCALAR",
                false,
                None,
                None,
            )
        } else {
            let mut bytes = Vec::with_capacity(indices.len() * 4);
            for &i in indices {
                bytes.extend_from_slice(&i.to_le_bytes());
            }
            let view = self.push_buffer_view(&bytes, Some(TARGET_ELEMENT_ARRAY_BUFFER));
            self.push_accessor(
                view,
                COMPONENT_TYPE_UNSIGNED_INT,
                indices.len(),
                "SCALAR",
                false,
                None,
                None,
            )
        }
    }

    /// Embeds an already-encoded image (JPEG/PNG bytes from `s2tex`) and returns its `images[]`
    /// index. One file, no satellite images (unlike the reference's `SatelliteImages` mode) --
    /// our viewer only ever has `render.glb` and `render.json` to fetch.
    pub fn add_image(&mut self, bytes: &[u8], mime_type: &str) -> u32 {
        self.image_bytes += bytes.len();
        let view = self.push_buffer_view(bytes, None);
        let mut img = Map::new();
        img.insert("bufferView".into(), json!(view));
        img.insert("mimeType".into(), json!(mime_type));
        self.images.push(Value::Object(img));
        (self.images.len() - 1) as u32
    }

    pub fn add_texture(&mut self, image_index: u32) -> u32 {
        self.textures.push(json!({ "source": image_index }));
        (self.textures.len() - 1) as u32
    }

    /// Pushes a caller-built material JSON object (`pbrMetallicRoughness`, `alphaMode`, `extras`,
    /// ...), returning its `materials[]` index.
    pub fn add_material(&mut self, material: Value) -> u32 {
        self.materials.push(material);
        (self.materials.len() - 1) as u32
    }

    /// Pushes a caller-built mesh JSON object (`primitives: [...]`), returning its `meshes[]`
    /// index.
    pub fn add_mesh(&mut self, mesh: Value) -> u32 {
        self.meshes.push(mesh);
        (self.meshes.len() - 1) as u32
    }

    /// Pushes a caller-built node JSON object (`mesh`/`matrix`/`children`/`name`), returning its
    /// `nodes[]` index.
    pub fn add_node(&mut self, node: Value) -> u32 {
        self.nodes.push(node);
        (self.nodes.len() - 1) as u32
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }
    pub fn material_count(&self) -> usize {
        self.materials.len()
    }
    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }
    /// Vertex/index buffer bytes, excluding embedded images (§5's "байты геометрии и текстур
    /// отдельно").
    pub fn geometry_bytes(&self) -> usize {
        self.bin.len() - self.image_bytes
    }

    /// Embedded image (JPEG/PNG) bytes.
    pub fn texture_bytes(&self) -> usize {
        self.image_bytes
    }

    /// Serializes the accumulated document to `.glb` bytes: a 12-byte header, a JSON chunk, then
    /// a BIN chunk (`docs` glTF 2.0 §Binary glTF Layout; magic `glTF`, version 2).
    /// `extensions_used` is the caller-tracked `extensionsUsed` list (e.g.
    /// `["KHR_materials_unlit"]` when any material used it). Errors with [`GlbTooLarge`] rather
    /// than silently wrapping a `u32` length if the JSON chunk, BIN chunk, or the whole file would
    /// exceed 4 GiB.
    pub fn finish(
        mut self,
        scene_node_roots: Vec<u32>,
        extensions_used: Vec<String>,
        extras: Value,
    ) -> Result<Vec<u8>, GlbTooLarge> {
        // A texture without an explicit sampler uses the default; declaring one keeps it
        // explicit (repeat wrap, linear filter) rather than leaving it to the viewer's default.
        if !self.textures.is_empty() {
            self.samplers.push(
                json!({ "magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497 }),
            );
            for t in &mut self.textures {
                t.as_object_mut()
                    .unwrap()
                    .insert("sampler".into(), json!(0));
            }
        }

        let mut root = Map::new();
        root.insert(
            "asset".into(),
            json!({ "version": "2.0", "generator": "cs2mod export-glb" }),
        );
        if !extensions_used.is_empty() {
            root.insert("extensionsUsed".into(), json!(extensions_used));
        }
        root.insert("scene".into(), json!(0));
        root.insert("scenes".into(), json!([{ "nodes": scene_node_roots }]));
        root.insert("nodes".into(), Value::Array(self.nodes));
        root.insert("meshes".into(), Value::Array(self.meshes));
        if !self.materials.is_empty() {
            root.insert("materials".into(), Value::Array(self.materials));
        }
        if !self.textures.is_empty() {
            root.insert("textures".into(), Value::Array(self.textures));
            root.insert("images".into(), Value::Array(self.images));
            root.insert("samplers".into(), Value::Array(self.samplers));
        }
        root.insert("accessors".into(), Value::Array(self.accessors));
        root.insert("bufferViews".into(), Value::Array(self.buffer_views));
        root.insert("buffers".into(), json!([{ "byteLength": self.bin.len() }]));
        root.insert("extras".into(), extras);

        let json_bytes_unpadded =
            serde_json::to_vec(&Value::Object(root)).expect("glTF JSON is always serializable");
        let mut json_bytes = json_bytes_unpadded;
        pad4(&mut json_bytes, b' ');

        let mut bin_bytes = self.bin;
        pad4(&mut bin_bytes, 0);

        let total_len = 12 + 8 + json_bytes.len() + 8 + bin_bytes.len();
        for &len in &[json_bytes.len(), bin_bytes.len(), total_len] {
            if len > u32::MAX as usize {
                return Err(GlbTooLarge { bytes: len });
            }
        }
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total_len as u32).to_le_bytes());

        out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json_bytes);

        out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin_bytes);

        Ok(out)
    }
}

/// A row-major 3x4 placement (§6.6 convention) to a glTF `matrix`: a column-major flattened 4x4
/// (`node.matrix[12..15]` is the translation).
pub fn node_matrix(t: &[[f32; 4]; 3]) -> [f32; 16] {
    [
        t[0][0], t[1][0], t[2][0], 0.0, t[0][1], t[1][1], t[2][1], 0.0, t[0][2], t[1][2], t[2][2],
        0.0, t[0][3], t[1][3], t[2][3], 1.0,
    ]
}

/// Composes two row-major 3x4 placements (`outer` applied after `inner`, i.e. `inner` is local to
/// `outer`'s space): `result(v) = outer(inner(v))`.
pub fn compose(outer: &[[f32; 4]; 3], inner: &[[f32; 4]; 3]) -> [[f32; 4]; 3] {
    let mut out = [[0f32; 4]; 3];
    for row in 0..3 {
        for col in 0..3 {
            out[row][col] = (0..3).map(|k| outer[row][k] * inner[k][col]).sum();
        }
        out[row][3] = (0..3).map(|k| outer[row][k] * inner[k][3]).sum::<f32>() + outer[row][3];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glb_round_trip_parses_as_valid_header_and_json() {
        let mut b = GltfBuilder::new();
        let pos = b.add_positions(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        let idx = b.add_indices(&[0, 1, 2]);
        let mesh = b.add_mesh(
            json!({ "primitives": [{ "attributes": { "POSITION": pos }, "indices": idx }] }),
        );
        let node = b.add_node(json!({ "mesh": mesh }));
        let bytes = b.finish(vec![node], Vec::new(), json!({})).unwrap();

        assert_eq!(&bytes[0..4], b"glTF");
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(version, 2);
        let total_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(total_len, bytes.len());

        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        assert_eq!(&bytes[16..20], b"JSON");
        let json_bytes = &bytes[20..20 + json_len];
        let doc: Value = serde_json::from_slice(json_bytes).unwrap();
        assert_eq!(doc["asset"]["version"], "2.0");
        assert_eq!(doc["meshes"].as_array().unwrap().len(), 1);

        let bin_chunk_start = 20 + json_len;
        let bin_len = u32::from_le_bytes(
            bytes[bin_chunk_start..bin_chunk_start + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        assert_eq!(&bytes[bin_chunk_start + 4..bin_chunk_start + 8], b"BIN\0");
        assert!(bin_len >= 3 * 12 + 3 * 2); // positions + indices, at least.
    }

    #[test]
    fn node_matrix_identity_is_gltf_identity() {
        let m = node_matrix(&crate::world::IDENTITY_TRANSFORM);
        assert_eq!(
            m,
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0
            ]
        );
    }

    #[test]
    fn node_matrix_carries_translation_in_the_last_column() {
        let mut t = crate::world::IDENTITY_TRANSFORM;
        t[0][3] = 10.0;
        t[1][3] = 20.0;
        t[2][3] = 30.0;
        let m = node_matrix(&t);
        assert_eq!([m[12], m[13], m[14]], [10.0, 20.0, 30.0]);
    }

    #[test]
    fn compose_applies_inner_then_outer() {
        let mut translate_x = crate::world::IDENTITY_TRANSFORM;
        translate_x[0][3] = 5.0;
        let mut translate_y = crate::world::IDENTITY_TRANSFORM;
        translate_y[1][3] = 7.0;
        let composed = compose(&translate_y, &translate_x);
        assert_eq!(composed[0][3], 5.0);
        assert_eq!(composed[1][3], 7.0);
    }

    #[test]
    fn indices_below_u16_max_use_u16_and_65535_forces_u32() {
        let mut b = GltfBuilder::new();
        let small = b.add_indices(&[0, 1, 65534]);
        let large = b.add_indices(&[0, 1, 65535]);
        assert_eq!(
            b.accessors[small as usize]["componentType"],
            COMPONENT_TYPE_UNSIGNED_SHORT
        );
        assert_eq!(
            b.accessors[large as usize]["componentType"],
            COMPONENT_TYPE_UNSIGNED_INT
        );
    }
}
