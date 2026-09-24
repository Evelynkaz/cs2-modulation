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

const COMPONENT_TYPE_BYTE: u32 = 5120;
const COMPONENT_TYPE_SHORT: u32 = 5122;
const COMPONENT_TYPE_UNSIGNED_SHORT: u32 = 5123;
const COMPONENT_TYPE_UNSIGNED_INT: u32 = 5125;
const COMPONENT_TYPE_FLOAT: u32 = 5126;

const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// POSITION quantization step, shared by every mesh in a document, instead of a per-mesh AABB
/// scale: a vertex two adjacent meshes both emit (a shared seam edge) must decode to the *same*
/// world position bit for bit, or the seam opens a see-through crack (a downward ray landing
/// exactly on the seam finds no triangle there). A per-mesh scale can't guarantee that -- two
/// meshes with different bounding boxes round the same shared vertex to different grids. A single
/// power-of-two step shared by all meshes can: every mesh's own offset is snapped to a multiple of
/// this step (`add_positions`), so `q * POSITION_STEP + offset` collapses to `round(p /
/// POSITION_STEP) * POSITION_STEP` regardless of which mesh's offset produced `q` -- every
/// operation involved (dividing/multiplying by a power of two, adding two multiples of it) is
/// exact in IEEE 754 binary32, so two meshes sharing a vertex decode it identically. `1/16` inch
/// (0.0625) keeps sub-quantization error well under a pixel at CS2's scale while leaving
/// `32767 * 1/16 ≈ 2048` units of range around a mesh's own center -- comfortably larger than any
/// single mesh this crate emits (Mirage/Inferno's largest are ~2599/3370 units *across*, i.e.
/// ~1300/1685 from center) but not unbounded, so `add_positions` still falls back to an
/// uncompressed FLOAT accessor for the rare mesh that doesn't fit.
const POSITION_STEP: f32 = 1.0 / 16.0;

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
    /// Running length of the fictitious "fallback" buffer (index 1) every meshopt-compressed
    /// bufferView's outer (uncompressed-layout) half points into -- never actually written out as
    /// real bytes (`finish`'s own doc comment on why that's spec-legal).
    fallback_bytes: usize,
    used_meshopt: bool,
    used_quantization: bool,
    /// Meshes whose POSITION overflowed the shared `i16` grid and fell back to `FLOAT` (see
    /// `add_positions`) -- exposed so a caller can report how many, per map, hit that rare path.
    position_float_fallback_meshes: usize,
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

    /// Meshoptimizer-compresses one already-packed vertex stream (`raw` is `count * stride` bytes,
    /// `stride` a multiple of 4 -- KHR_mesh_quantization's own alignment note and
    /// KHR_meshopt_compression's "ATTRIBUTES mode requires byteStride divisible by 4" both land on
    /// the same rule) and records a bufferView using the extension's "no fallback" pattern
    /// (`s6f3a5_size.md`: "fallback-буферы без сжатия НЕ писать (вьювер наш)"): the *outer*
    /// bufferView keeps the normal, uncompressed layout (`buffer`/`byteOffset`/`byteLength`/
    /// `byteStride`) but points at a placeholder buffer (index 1, sized and emitted once in
    /// `finish`) that is never actually populated with bytes -- per the extension's own
    /// "Fallback buffers" section, a placeholder buffer with no `uri` is legal exactly when the
    /// extension is `extensionsRequired` (which `finish` also arranges). The real, compressed
    /// bytes live in `extensions.KHR_meshopt_compression`, referencing buffer 0 (this GLB's own
    /// BIN chunk).
    fn push_meshopt_attribute_view(
        &mut self,
        raw: &[u8],
        count: usize,
        stride: usize,
        target: Option<u32>,
    ) -> u32 {
        debug_assert!(
            stride > 0 && stride <= 256 && stride.is_multiple_of(4),
            "attribute stride must be a positive multiple of 4, at most 256"
        );
        debug_assert_eq!(raw.len(), count * stride);

        let compressed = if count == 0 {
            Vec::new()
        } else {
            meshopt_encode_attribute(raw, count, stride)
        };

        pad4(&mut self.bin, 0);
        let real_offset = self.bin.len();
        self.bin.extend_from_slice(&compressed);

        // The validator checks core alignment rules (accessor byteOffset a multiple of its
        // componentType size) against this *outer*, never-actually-read bufferView too, so its
        // virtual offset needs the same 4-byte alignment real bufferViews get, even though no
        // bytes ever back it.
        while !self.fallback_bytes.is_multiple_of(4) {
            self.fallback_bytes += 1;
        }
        let virtual_offset = self.fallback_bytes;
        self.fallback_bytes += count * stride;

        let mut view = Map::new();
        view.insert("buffer".into(), json!(1u32));
        view.insert("byteOffset".into(), json!(virtual_offset));
        view.insert("byteLength".into(), json!(count * stride));
        view.insert("byteStride".into(), json!(stride));
        if let Some(t) = target {
            view.insert("target".into(), json!(t));
        }
        view.insert(
            "extensions".into(),
            json!({
                "KHR_meshopt_compression": {
                    "buffer": 0,
                    "byteOffset": real_offset,
                    "byteLength": compressed.len(),
                    "byteStride": stride,
                    "count": count,
                    "mode": "ATTRIBUTES",
                }
            }),
        );
        self.buffer_views.push(Value::Object(view));
        self.used_meshopt = true;
        (self.buffer_views.len() - 1) as u32
    }

    /// Same "no fallback" pattern as [`Self::push_meshopt_attribute_view`], for an index buffer
    /// already meshopt-encoded as a triangle-list stream (mode `TRIANGLES`, `count` a multiple of
    /// 3). The outer bufferView omits `byteStride` -- core glTF forbids it on an
    /// `ELEMENT_ARRAY_BUFFER`-target view, which is also why the extension's own JSON schema
    /// requires `byteStride` on the *inner* object instead (its overview: "JSON schema prohibits
    /// specifying this for some types of storage such as index data").
    fn push_meshopt_index_view(
        &mut self,
        compressed: &[u8],
        count: usize,
        element_size: usize,
    ) -> u32 {
        pad4(&mut self.bin, 0);
        let real_offset = self.bin.len();
        self.bin.extend_from_slice(compressed);

        while !self.fallback_bytes.is_multiple_of(4) {
            self.fallback_bytes += 1;
        }
        let virtual_offset = self.fallback_bytes;
        self.fallback_bytes += count * element_size;

        let mut view = Map::new();
        view.insert("buffer".into(), json!(1u32));
        view.insert("byteOffset".into(), json!(virtual_offset));
        view.insert("byteLength".into(), json!(count * element_size));
        view.insert("target".into(), json!(TARGET_ELEMENT_ARRAY_BUFFER));
        view.insert(
            "extensions".into(),
            json!({
                "KHR_meshopt_compression": {
                    "buffer": 0,
                    "byteOffset": real_offset,
                    "byteLength": compressed.len(),
                    "byteStride": element_size,
                    "count": count,
                    "mode": "TRIANGLES",
                }
            }),
        );
        self.buffer_views.push(Value::Object(view));
        self.used_meshopt = true;
        (self.buffer_views.len() - 1) as u32
    }

    /// KHR_mesh_quantization POSITION: unnormalized `SHORT` VEC3 (padded to 8 bytes/vertex --
    /// the extension's own alignment note, "a BYTE normal is expected to have a stride of 4, not
    /// 3" applies the same way to a 6-byte SHORT VEC3), dequantized by a scale + translate the
    /// returned `[[f32;4];3]` placement (this crate's row-major 3x4 convention, see
    /// `node_matrix`/`compose`) encodes -- the caller composes it with each instance's own
    /// placement transform ("positions -- int16 с масштабом в матрице узла",
    /// `s6f3a5_size.md` change item 1). Unnormalized rather than normalized `SHORT`: the extension
    /// spec's own implementation note recommends it for POSITION to sidestep a WebGL1/ES2
    /// signed-normalized decoding discrepancy, and it needs no `1/32767` factor baked in twice.
    ///
    /// The quantization step is [`POSITION_STEP`] for every mesh (not a per-mesh AABB-derived
    /// scale) and this mesh's own offset is snapped to a multiple of that step -- see
    /// `POSITION_STEP`'s own doc comment for why that is what makes a vertex shared by two meshes
    /// (a seam) decode to the same world position from both. This also gives a uniform scale
    /// across X/Y/Z for free, which `KHR_mesh_quantization` recommends anyway ("to preserve the
    /// direction of normal/tangent vectors, it is recommended that the quantization scale
    /// specified in the transform is uniform across X/Y/Z axes"), since this project's node
    /// matrices double as the (naive) normal transform for anything reading `NORMAL` alongside a
    /// transformed `POSITION`.
    ///
    /// A mesh whose quantized coordinates would overflow `i16` (any `|q| > 32767`, i.e. it
    /// reaches more than `32767 * POSITION_STEP` units from its own snapped center) instead gets
    /// an uncompressed `FLOAT` VEC3 POSITION (still a meshopt `ATTRIBUTES` bufferView, stride 12)
    /// with an identity dequantization placement -- rare in practice (no map checked so far needs
    /// it), but correctness must not depend on every mesh fitting the grid.
    pub fn add_positions(&mut self, positions: &[[f32; 3]]) -> (u32, [[f32; 4]; 3]) {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in positions {
            for c in 0..3 {
                min[c] = min[c].min(p[c]);
                max[c] = max[c].max(p[c]);
            }
        }
        if positions.is_empty() {
            min = [0.0; 3];
            max = [0.0; 3];
        }
        let mut offset = [0.0f32; 3];
        for c in 0..3 {
            offset[c] = ((min[c] + max[c]) * 0.5 / POSITION_STEP).round() * POSITION_STEP;
        }

        let mut quantized: Vec<[i32; 3]> = Vec::with_capacity(positions.len());
        let mut fits_i16 = true;
        for p in positions {
            let mut q = [0i32; 3];
            for c in 0..3 {
                let qc = ((p[c] - offset[c]) / POSITION_STEP).round();
                if qc.abs() > 32767.0 {
                    fits_i16 = false;
                }
                q[c] = qc as i32;
            }
            quantized.push(q);
        }

        if !fits_i16 {
            let mut raw = Vec::with_capacity(positions.len() * 12);
            for p in positions {
                for &c in p {
                    raw.extend_from_slice(&c.to_le_bytes());
                }
            }
            let view = self.push_meshopt_attribute_view(
                &raw,
                positions.len(),
                12,
                Some(TARGET_ARRAY_BUFFER),
            );
            let accessor = self.push_accessor(
                view,
                COMPONENT_TYPE_FLOAT,
                positions.len(),
                "VEC3",
                false,
                Some(json!(min)),
                Some(json!(max)),
            );
            self.position_float_fallback_meshes += 1;
            return (accessor, crate::world::IDENTITY_TRANSFORM);
        }

        let mut raw = Vec::with_capacity(positions.len() * 8);
        let mut qmin = [i16::MAX; 3];
        let mut qmax = [i16::MIN; 3];
        for q in &quantized {
            for c in 0..3 {
                let qc = q[c] as i16;
                qmin[c] = qmin[c].min(qc);
                qmax[c] = qmax[c].max(qc);
                raw.extend_from_slice(&qc.to_le_bytes());
            }
            raw.extend_from_slice(&0i16.to_le_bytes());
        }
        if positions.is_empty() {
            qmin = [0; 3];
            qmax = [0; 3];
        }

        let view =
            self.push_meshopt_attribute_view(&raw, positions.len(), 8, Some(TARGET_ARRAY_BUFFER));
        let accessor = self.push_accessor(
            view,
            COMPONENT_TYPE_SHORT,
            positions.len(),
            "VEC3",
            false,
            Some(json!(qmin)),
            Some(json!(qmax)),
        );
        self.used_quantization = true;
        // The accessor is unnormalized (`normalized: false` above), so a glTF reader (and every
        // caller of this crate's own `compose(instance_transform, this)`) uses the raw integer
        // `q` as-is, with NO implicit `/32767` -- the dequantization scale baked into the node
        // matrix must carry that division itself, or every reconstructed position is off by a
        // factor of up to 32767.
        let transform = [
            [POSITION_STEP, 0.0, 0.0, offset[0]],
            [0.0, POSITION_STEP, 0.0, offset[1]],
            [0.0, 0.0, POSITION_STEP, offset[2]],
        ];
        (accessor, transform)
    }

    /// KHR_mesh_quantization NORMAL: `BYTE` normalized (padded to 4 bytes/vertex). Plain
    /// per-component quantization, not octahedral: `s6f3a5_size.md` change item 1 offers "int8
    /// normalized ИЛИ octahedral" as alternatives, and octahedral needs a `meshopt_encodeFilterOct`
    /// FFI call plus the extension's `OCTAHEDRAL` filter for a further few tenths of a byte/vertex
    /// -- not worth the extra unsafe surface here when the simpler option is explicitly allowed.
    pub fn add_normals_quantized(&mut self, normals: &[[f32; 3]]) -> u32 {
        let mut raw = Vec::with_capacity(normals.len() * 4);
        for n in normals {
            for &c in n {
                raw.push((c.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8);
            }
            raw.push(0);
        }
        let view =
            self.push_meshopt_attribute_view(&raw, normals.len(), 4, Some(TARGET_ARRAY_BUFFER));
        self.used_quantization = true;
        self.push_accessor(
            view,
            COMPONENT_TYPE_BYTE,
            normals.len(),
            "VEC3",
            true,
            None,
            None,
        )
    }

    pub fn add_vec3(&mut self, values: &[[f32; 3]]) -> u32 {
        let mut bytes = Vec::with_capacity(values.len() * 12);
        for v in values {
            for c in v {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        let view =
            self.push_meshopt_attribute_view(&bytes, values.len(), 12, Some(TARGET_ARRAY_BUFFER));
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
        let view =
            self.push_meshopt_attribute_view(&bytes, values.len(), 8, Some(TARGET_ARRAY_BUFFER));
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

    /// TEXCOORD_0: normalized `UNSIGNED_SHORT` when every value already lies in `[0,1]` (the
    /// common case), else plain `FLOAT` -- tiled materials routinely put UVs outside `[0,1]`, and
    /// `s6f3a5_size.md` change item 1 says to quantize UV "где диапазон позволяет" ("where the
    /// range allows"). Either way the bufferView is still meshopt-compressed.
    pub fn add_uv0(&mut self, values: &[[f32; 2]]) -> u32 {
        let fits_unit_range = values
            .iter()
            .all(|v| v.iter().all(|&c| (0.0..=1.0).contains(&c)));
        if fits_unit_range {
            self.push_normalized_uv16(values)
        } else {
            self.add_vec2(values)
        }
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
        let view =
            self.push_meshopt_attribute_view(&bytes, values.len(), 4, Some(TARGET_ARRAY_BUFFER));
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

    /// `_LPV` (§2 of `s6f3a4_lighting.md`): rgb = ambient-cube irradiance at the vertex normal,
    /// a = sun visibility (`1 - dlshd[bakedShadowChannel]`, or `1` when this map has no baked sun
    /// shadow -- §1), one accessor per probe-lit *instance* (never shared -- each placement
    /// samples a different world position). Encoded as `UNSIGNED_SHORT` normalized rather than
    /// `FLOAT`: halves the accessor's bytes (8 vs 16 per vertex) to stay inside REPORT.md §8's
    /// ~3 MB probe budget, and 65536 steps across `scale` is far finer than the eye can
    /// distinguish even for the dimmest texels -- `scale` (`render.json`'s `lpvScale`) is a fixed
    /// constant the caller picked in advance (`export.rs`'s `LPV_SCALE`), not computed from this
    /// export's own data.
    pub fn add_lpv_u16(&mut self, values: &[[f32; 4]], scale: f32) -> u32 {
        let safe_scale = if scale > 0.0 { scale } else { 1.0 };
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for v in values {
            for &c in v {
                let normalized = (c / safe_scale).clamp(0.0, 1.0);
                bytes.extend_from_slice(&((normalized * 65535.0).round() as u16).to_le_bytes());
            }
        }
        let view =
            self.push_meshopt_attribute_view(&bytes, values.len(), 8, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_UNSIGNED_SHORT,
            values.len(),
            "VEC4",
            true,
            None,
            None,
        )
    }

    /// Normalized `UNSIGNED_SHORT` VEC2, `values` already clamped into `[0,1]` by the caller --
    /// the shared encoding behind [`Self::add_uv1_u16`] (`TEXCOORD_1`) and [`Self::add_uv0`]'s
    /// in-range case. No extension needed: core glTF 2.0 already allows a normalized
    /// `UNSIGNED_SHORT` `TEXCOORD_n` accessor (`KHR_mesh_quantization` only extends
    /// POSITION/NORMAL/TANGENT/TEXCOORD to the *signed* byte/short types, which this crate never
    /// uses for TEXCOORD, so it's declared nowhere for this path).
    fn push_normalized_uv16(&mut self, values: &[[f32; 2]]) -> u32 {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for v in values {
            for &c in v {
                bytes.extend_from_slice(
                    &((c.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes(),
                );
            }
        }
        let view =
            self.push_meshopt_attribute_view(&bytes, values.len(), 4, Some(TARGET_ARRAY_BUFFER));
        self.push_accessor(
            view,
            COMPONENT_TYPE_UNSIGNED_SHORT,
            values.len(),
            "VEC2",
            true,
            None,
            None,
        )
    }

    /// `TEXCOORD_1` (the lightmap UV, §1 of `s6f3a4_lighting.md`): normalized `UNSIGNED_SHORT`
    /// rather than `FLOAT` -- halves the accessor's bytes (4 vs 8 per vertex, ~3.9 MB on Mirage)
    /// with no visible precision loss, since `values` are already scaled into `[0,1]` by the
    /// caller (`build_geometry`'s `uv1.push([raw[0] * lightmap_uv_scale[0], ...])`).
    pub fn add_uv1_u16(&mut self, values: &[[f32; 2]]) -> u32 {
        self.push_normalized_uv16(values)
    }

    /// Indices, packed as `u16` when they fit (most draw calls) or `u32` otherwise. `u16::MAX`
    /// (0xFFFF) itself is reserved as the primitive-restart value and must never appear as a real
    /// index (glTF 2.0's own indices-accessor rule), so a lone value of exactly 65535 forces the
    /// wider `u32` encoding even though it would otherwise fit a `u16`. Meshopt-compressed as a
    /// triangle-list stream (mode `TRIANGLES`, `s6f3a5_size.md` change item 1) whenever `indices`
    /// actually is one (non-empty, a multiple of 3) -- every draw call this crate emits is, but a
    /// non-triangle-list caller falls back to a plain uncompressed view instead of violating the
    /// extension's own "count must be divisible by 3" contract.
    pub fn add_indices(&mut self, indices: &[u32]) -> u32 {
        let fits_u16 = indices.iter().all(|&i| i < u32::from(u16::MAX));
        let element_size = if fits_u16 { 2usize } else { 4usize };
        let component_type = if fits_u16 {
            COMPONENT_TYPE_UNSIGNED_SHORT
        } else {
            COMPONENT_TYPE_UNSIGNED_INT
        };

        let view = if !indices.is_empty() && indices.len().is_multiple_of(3) {
            let vertex_count = indices
                .iter()
                .copied()
                .max()
                .map(|m| m as usize + 1)
                .unwrap_or(0);
            let compressed = meshopt::encode_index_buffer(indices, vertex_count).expect(
                "encode_index_buffer only fails when its output buffer is undersized, and it \
                 always sizes that buffer from meshopt_encodeIndexBufferBound first",
            );
            self.push_meshopt_index_view(&compressed, indices.len(), element_size)
        } else if fits_u16 {
            let mut bytes = Vec::with_capacity(indices.len() * 2);
            for &i in indices {
                bytes.extend_from_slice(&(i as u16).to_le_bytes());
            }
            self.push_buffer_view(&bytes, Some(TARGET_ELEMENT_ARRAY_BUFFER))
        } else {
            let mut bytes = Vec::with_capacity(indices.len() * 4);
            for &i in indices {
                bytes.extend_from_slice(&i.to_le_bytes());
            }
            self.push_buffer_view(&bytes, Some(TARGET_ELEMENT_ARRAY_BUFFER))
        };
        self.push_accessor(
            view,
            component_type,
            indices.len(),
            "SCALAR",
            false,
            None,
            None,
        )
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
    /// See `position_float_fallback_meshes`'s own field doc comment.
    pub fn position_float_fallback_meshes(&self) -> usize {
        self.position_float_fallback_meshes
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
    /// `extensions_used` is the caller-tracked, purely-optional `extensionsUsed` list (e.g.
    /// `["KHR_materials_unlit"]` when any material used it) -- `KHR_mesh_quantization` and
    /// `KHR_meshopt_compression` are tracked internally instead and added to *both*
    /// `extensionsUsed` and `extensionsRequired` whenever any geometry actually used them, since
    /// (unlike `KHR_materials_unlit`) this crate never writes an uncompressed/unquantized fallback
    /// for a viewer that doesn't support them (`s6f3a5_size.md`: "fallback-буферы без сжатия НЕ
    /// писать (вьювер наш)"). Errors with [`GlbTooLarge`] rather than silently wrapping a `u32`
    /// length if the JSON chunk, BIN chunk, or the whole file would exceed 4 GiB.
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

        let mut required: Vec<String> = Vec::new();
        let mut used: Vec<String> = Vec::new();
        if self.used_quantization {
            required.push("KHR_mesh_quantization".into());
            used.push("KHR_mesh_quantization".into());
        }
        if self.used_meshopt {
            required.push("KHR_meshopt_compression".into());
            used.push("KHR_meshopt_compression".into());
        }
        used.extend(extensions_used);

        let mut root = Map::new();
        root.insert(
            "asset".into(),
            json!({ "version": "2.0", "generator": "cs2mod export-glb" }),
        );
        if !used.is_empty() {
            root.insert("extensionsUsed".into(), json!(used));
        }
        if !required.is_empty() {
            root.insert("extensionsRequired".into(), json!(required));
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
        let mut buffers = vec![json!({ "byteLength": self.bin.len() })];
        if self.fallback_bytes > 0 {
            // The placeholder buffer KHR_meshopt_compression's "Fallback buffers" section
            // describes: no `uri`, index 1 (never 0, which the GLB's own BIN chunk owns), never
            // actually read by a loader that supports the extension (every bufferView pointing at
            // it also carries `extensions.KHR_meshopt_compression`, so the loader's own extension
            // handler substitutes buffer 0 instead of ever dereferencing this one).
            buffers.push(json!({ "byteLength": self.fallback_bytes }));
        }
        root.insert("buffers".into(), Value::Array(buffers));
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

/// Encodes one already-quantized, 4-byte-aligned vertex stream with meshoptimizer's generic
/// attribute codec (mode `ATTRIBUTES`), forcing bitstream version 1. The `meshopt` crate's own
/// safe `encode_vertex_buffer` always encodes at whatever version
/// `meshopt::ffi::meshopt_encodeVertexVersion` last set globally (default 0, the older/simpler
/// codec that ignores compression level) -- calling that setter would mean every vertex-buffer
/// encode anywhere in the process (including from other threads, since it mutates a plain global
/// without synchronization) has to happen after it and never concurrently with it. Passing
/// `version` directly to `meshopt_encodeVertexBufferLevel` instead needs no such choreography.
/// Version 1 is "decodable by 0.23+" per its own doc comment; three.js 0.186's
/// `meshopt_decoder.module.js` is built from meshoptimizer 1.1, so it decodes both versions.
/// Level 3 (meshoptimizer's own best-ratio setting, scale 0..3) since export runs are offline, not
/// latency sensitive.
fn meshopt_encode_attribute(raw: &[u8], count: usize, stride: usize) -> Vec<u8> {
    debug_assert!(stride > 0 && stride <= 256 && stride.is_multiple_of(4));
    debug_assert_eq!(raw.len(), count * stride);
    // SAFETY: `bound` is meshoptimizer's own worst-case output size for this exact `(count,
    // stride)`, and `out` is allocated to that many bytes before the encode call, matching
    // `meshopt_encodeVertexBufferLevel`'s own doc ("buffer must contain enough space..."). `raw`
    // is exactly `count * stride` bytes (checked above), matching what the encoder reads, and
    // `stride` is a positive multiple of 4 not exceeding 256 (checked above), satisfying the
    // vendored encoder's own `assert(vertex_size > 0 && vertex_size <= 256)` /
    // `assert(vertex_size % 4 == 0)` (`vertexcodec.cpp:1834-1835`, the same precondition
    // `buffer.rs::meshopt_decode_vertex`'s SAFETY note documents for the decode side).
    let mut out = unsafe {
        let bound = meshopt::ffi::meshopt_encodeVertexBufferBound(count, stride);
        let mut out = vec![0u8; bound];
        let size = meshopt::ffi::meshopt_encodeVertexBufferLevel(
            out.as_mut_ptr(),
            out.len(),
            raw.as_ptr().cast(),
            count,
            stride,
            3,
            1,
        );
        out.truncate(size);
        out
    };
    out.shrink_to_fit();
    out
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

    fn glb_json(bytes: &[u8]) -> Value {
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[20..20 + json_len]).unwrap()
    }

    #[test]
    fn glb_round_trip_parses_as_valid_header_and_json() {
        let mut b = GltfBuilder::new();
        let (pos, _quantize) =
            b.add_positions(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
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
        // Quantized positions + compressed indices: both KHR_mesh_quantization and
        // KHR_meshopt_compression are mandatory (no uncompressed fallback is ever written).
        let used: Vec<&str> = doc["extensionsUsed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let required: Vec<&str> = doc["extensionsRequired"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for ext in ["KHR_mesh_quantization", "KHR_meshopt_compression"] {
            assert!(used.contains(&ext), "{used:?}");
            assert!(required.contains(&ext), "{required:?}");
        }
        // Buffer 1 is the fictitious, byte-less placeholder every compressed bufferView's outer
        // half points into (`finish`'s own doc comment).
        assert_eq!(doc["buffers"].as_array().unwrap().len(), 2);
        assert!(doc["buffers"][1].get("uri").is_none());

        let bin_chunk_start = 20 + json_len;
        let bin_len = u32::from_le_bytes(
            bytes[bin_chunk_start..bin_chunk_start + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        assert_eq!(&bytes[bin_chunk_start + 4..bin_chunk_start + 8], b"BIN\0");
        assert!(bin_len > 0);
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
    fn lpv_u16_round_trips_within_quantization_error() {
        let mut b = GltfBuilder::new();
        let idx = b.add_lpv_u16(&[[1.0, 2.0, 4.0, 0.5]], 8.0);
        let acc = &b.accessors[idx as usize];
        assert_eq!(acc["componentType"], COMPONENT_TYPE_UNSIGNED_SHORT);
        assert_eq!(acc["type"], "VEC4");
        assert_eq!(acc["normalized"], true);
    }

    #[test]
    fn uv1_u16_does_not_declare_mesh_quantization() {
        // No `add_positions`/`add_normals_quantized` call here on purpose: TEXCOORD's normalized
        // `UNSIGNED_SHORT` encoding is core glTF, needing no `KHR_mesh_quantization` declaration,
        // unlike POSITION/NORMAL's integer component types -- this accessor's bufferView is still
        // meshopt-compressed (every attribute's is), so `KHR_meshopt_compression` alone is
        // expected.
        let mut b = GltfBuilder::new();
        let idx = b.add_uv1_u16(&[[0.5, 1.0]]);
        let acc = &b.accessors[idx as usize];
        assert_eq!(acc["componentType"], COMPONENT_TYPE_UNSIGNED_SHORT);
        assert_eq!(acc["type"], "VEC2");
        assert_eq!(acc["normalized"], true);
        let mesh = b.add_mesh(json!({ "primitives": [{ "attributes": { "TEXCOORD_1": idx } }] }));
        let node = b.add_node(json!({ "mesh": mesh }));
        let bytes = b.finish(vec![node], Vec::new(), json!({})).unwrap();
        let doc = glb_json(&bytes);
        let used: Vec<&str> = doc["extensionsUsed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(used, vec!["KHR_meshopt_compression"], "{doc:?}");
        assert_eq!(
            doc["extensionsRequired"].as_array().unwrap(),
            &vec![json!("KHR_meshopt_compression")],
            "{doc:?}"
        );
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

    #[test]
    fn positions_quantize_and_compress_round_trip_within_scale_over_32767() {
        let original = [
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [0.0, 50.0, 0.0],
            [0.0, 0.0, -25.0],
        ];
        let mut b = GltfBuilder::new();
        let (pos_idx, transform) = b.add_positions(&original);
        let view_idx = b.accessors[pos_idx as usize]["bufferView"]
            .as_u64()
            .unwrap() as usize;
        let mesh = b.add_mesh(json!({ "primitives": [{ "attributes": { "POSITION": pos_idx } }] }));
        let node = b.add_node(json!({ "mesh": mesh }));
        let bytes = b.finish(vec![node], Vec::new(), json!({})).unwrap();
        let doc = glb_json(&bytes);

        let ext = &doc["bufferViews"][view_idx]["extensions"]["KHR_meshopt_compression"];
        let byte_offset = ext["byteOffset"].as_u64().unwrap() as usize;
        let byte_length = ext["byteLength"].as_u64().unwrap() as usize;
        let count = ext["count"].as_u64().unwrap() as usize;
        assert_eq!(count, original.len());

        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len + 8;
        let compressed = &bytes[bin_start + byte_offset..bin_start + byte_offset + byte_length];
        let decoded: Vec<[i16; 4]> = meshopt::decode_vertex_buffer(compressed, count).unwrap();

        // The accessor is unnormalized `SHORT`: a reader (this crate's own `compose`d node
        // matrix, or any glTF consumer) uses the raw integer as-is, with no implicit `/32767` --
        // matching the reconstruction every real caller does (`node_matrix`/`compose` applied to
        // the raw decoded `q`, `s6f3a5_size.md`'s collision-parity test does exactly this).
        // Quantization error is at most half a `POSITION_STEP` (round-to-nearest).
        let tolerance = POSITION_STEP / 2.0 + 1e-4;
        for (p, q) in original.iter().zip(decoded.iter()) {
            for c in 0..3 {
                let approx = transform[c][c] * f32::from(q[c]) + transform[c][3];
                assert!(
                    (approx - p[c]).abs() <= tolerance,
                    "axis {c}: reconstructed {approx}, original {}, tolerance {tolerance}",
                    p[c]
                );
            }
        }
    }

    /// The bug `POSITION_STEP` fixes: two meshes with different bounding boxes (so different
    /// per-mesh AABB centers) that share a vertex (a seam) must decode that vertex to the same
    /// world position from both meshes' own accessors, or the seam opens a see-through crack.
    #[test]
    fn positions_from_different_meshes_share_a_vertex_bit_identically() {
        let shared = [296.0f32, -1596.5, -176.0];
        let mesh_a = [shared, [100.0, -1400.0, -176.0], [296.0, -1700.0, 0.0]];
        let mesh_b = [shared, [900.0, 200.0, 300.0], [-500.0, -2800.0, -600.0]];

        let mut b = GltfBuilder::new();
        let (idx_a, transform_a) = b.add_positions(&mesh_a);
        let (idx_b, transform_b) = b.add_positions(&mesh_b);
        assert_eq!(
            b.position_float_fallback_meshes(),
            0,
            "should fit the SHORT grid"
        );
        let view_a = b.accessors[idx_a as usize]["bufferView"].as_u64().unwrap() as usize;
        let view_b = b.accessors[idx_b as usize]["bufferView"].as_u64().unwrap() as usize;

        let mesh_a_idx =
            b.add_mesh(json!({ "primitives": [{ "attributes": { "POSITION": idx_a } }] }));
        let mesh_b_idx =
            b.add_mesh(json!({ "primitives": [{ "attributes": { "POSITION": idx_b } }] }));
        let node_a = b.add_node(json!({ "mesh": mesh_a_idx }));
        let node_b = b.add_node(json!({ "mesh": mesh_b_idx }));
        let bytes = b
            .finish(vec![node_a, node_b], Vec::new(), json!({}))
            .unwrap();
        let doc = glb_json(&bytes);
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len + 8;

        let decode_first_vertex = |view_idx: usize, transform: [[f32; 4]; 3]| -> [f32; 3] {
            let ext = &doc["bufferViews"][view_idx]["extensions"]["KHR_meshopt_compression"];
            let byte_offset = ext["byteOffset"].as_u64().unwrap() as usize;
            let byte_length = ext["byteLength"].as_u64().unwrap() as usize;
            let count = ext["count"].as_u64().unwrap() as usize;
            let compressed = &bytes[bin_start + byte_offset..bin_start + byte_offset + byte_length];
            let decoded: Vec<[i16; 4]> = meshopt::decode_vertex_buffer(compressed, count).unwrap();
            let q = decoded[0];
            [
                transform[0][0] * f32::from(q[0]) + transform[0][3],
                transform[1][1] * f32::from(q[1]) + transform[1][3],
                transform[2][2] * f32::from(q[2]) + transform[2][3],
            ]
        };

        let world_a = decode_first_vertex(view_a, transform_a);
        let world_b = decode_first_vertex(view_b, transform_b);
        assert_eq!(
            world_a, world_b,
            "the same shared vertex decoded to different world positions from mesh A vs mesh B"
        );
    }

    /// The reconstruction real callers actually do: `compose(instance_transform,
    /// add_positions_quantize_transform)` becomes a node `matrix`, and a reader recovers world
    /// positions as `node_matrix * raw_accessor_value` with no separate `/32767` step anywhere --
    /// catches a class of bug where `add_positions`'s own returned transform is only correct in
    /// isolation (identity outer) but wrong once composed with a real (rotated/translated)
    /// placement.
    #[test]
    fn positions_quantize_compose_with_instance_transform_matches_world_space() {
        let local = [
            [10.0, 20.0, -5.0],
            [110.0, 20.0, -5.0],
            [10.0, 70.0, -5.0],
            [10.0, 20.0, 15.0],
        ];
        // A non-trivial instance placement: rotation about Z plus translation (no scale, so this
        // stays exact up to quantization error).
        let angle = 0.4f32;
        let (s, c) = angle.sin_cos();
        let instance = [
            [c, -s, 0.0, 500.0],
            [s, c, 0.0, -300.0],
            [0.0, 0.0, 1.0, 64.0],
        ];

        let mut b = GltfBuilder::new();
        let (pos_idx, quantize) = b.add_positions(&local);
        let node_transform = compose(&instance, &quantize);
        let view_idx = b.accessors[pos_idx as usize]["bufferView"]
            .as_u64()
            .unwrap() as usize;
        let mesh = b.add_mesh(json!({ "primitives": [{ "attributes": { "POSITION": pos_idx } }] }));
        let node = b.add_node(json!({ "matrix": node_matrix(&node_transform), "mesh": mesh }));
        let bytes = b.finish(vec![node], Vec::new(), json!({})).unwrap();
        let doc = glb_json(&bytes);

        let ext = &doc["bufferViews"][view_idx]["extensions"]["KHR_meshopt_compression"];
        let byte_offset = ext["byteOffset"].as_u64().unwrap() as usize;
        let byte_length = ext["byteLength"].as_u64().unwrap() as usize;
        let count = ext["count"].as_u64().unwrap() as usize;
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len + 8;
        let compressed = &bytes[bin_start + byte_offset..bin_start + byte_offset + byte_length];
        let decoded: Vec<[i16; 4]> = meshopt::decode_vertex_buffer(compressed, count).unwrap();

        // Reconstruct exactly the way a real reader does: node matrix times the raw (unnormalized)
        // integer, nothing else. Quantization error is at most half a `POSITION_STEP`; the extra
        // 0.05 covers the instance rotation's own f32 rounding.
        let tolerance = POSITION_STEP / 2.0 + 0.05;
        for (p, q) in local.iter().zip(decoded.iter()) {
            let raw = [f32::from(q[0]), f32::from(q[1]), f32::from(q[2])];
            let got = [
                node_transform[0][0] * raw[0]
                    + node_transform[0][1] * raw[1]
                    + node_transform[0][2] * raw[2]
                    + node_transform[0][3],
                node_transform[1][0] * raw[0]
                    + node_transform[1][1] * raw[1]
                    + node_transform[1][2] * raw[2]
                    + node_transform[1][3],
                node_transform[2][0] * raw[0]
                    + node_transform[2][1] * raw[1]
                    + node_transform[2][2] * raw[2]
                    + node_transform[2][3],
            ];
            let want = [
                instance[0][0] * p[0]
                    + instance[0][1] * p[1]
                    + instance[0][2] * p[2]
                    + instance[0][3],
                instance[1][0] * p[0]
                    + instance[1][1] * p[1]
                    + instance[1][2] * p[2]
                    + instance[1][3],
                instance[2][0] * p[0]
                    + instance[2][1] * p[1]
                    + instance[2][2] * p[2]
                    + instance[2][3],
            ];
            for c in 0..3 {
                assert!(
                    (got[c] - want[c]).abs() <= tolerance,
                    "axis {c}: got {got:?}, want {want:?}, tolerance {tolerance}"
                );
            }
        }
    }

    #[test]
    fn normals_quantize_round_trip_within_one_over_127() {
        let original = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];
        let mut b = GltfBuilder::new();
        let idx = b.add_normals_quantized(&original);
        let acc = &b.accessors[idx as usize];
        assert_eq!(acc["componentType"], COMPONENT_TYPE_BYTE);
        assert_eq!(acc["type"], "VEC3");
        assert_eq!(acc["normalized"], true);

        let view_idx = acc["bufferView"].as_u64().unwrap() as usize;
        let mesh = b.add_mesh(json!({ "primitives": [{ "attributes": { "NORMAL": idx } }] }));
        let node = b.add_node(json!({ "mesh": mesh }));
        let bytes = b.finish(vec![node], Vec::new(), json!({})).unwrap();
        let doc = glb_json(&bytes);
        let ext = &doc["bufferViews"][view_idx]["extensions"]["KHR_meshopt_compression"];
        let byte_offset = ext["byteOffset"].as_u64().unwrap() as usize;
        let byte_length = ext["byteLength"].as_u64().unwrap() as usize;
        let count = ext["count"].as_u64().unwrap() as usize;
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len + 8;
        let compressed = &bytes[bin_start + byte_offset..bin_start + byte_offset + byte_length];
        let decoded: Vec<[i8; 4]> = meshopt::decode_vertex_buffer(compressed, count).unwrap();
        for (n, q) in original.iter().zip(decoded.iter()) {
            for c in 0..3 {
                let approx = f32::from(q[c]) / 127.0;
                assert!((approx - n[c]).abs() <= 1.0 / 127.0 + 1e-4);
            }
        }
    }

    #[test]
    fn uv0_uses_u16_in_range_and_float_outside_range() {
        let mut b = GltfBuilder::new();
        let in_range = b.add_uv0(&[[0.0, 1.0], [0.5, 0.25]]);
        let out_of_range = b.add_uv0(&[[2.5, 0.0]]);
        assert_eq!(
            b.accessors[in_range as usize]["componentType"],
            COMPONENT_TYPE_UNSIGNED_SHORT
        );
        assert_eq!(
            b.accessors[out_of_range as usize]["componentType"],
            COMPONENT_TYPE_FLOAT
        );
    }
}
