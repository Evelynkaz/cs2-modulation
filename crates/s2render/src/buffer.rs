//! Vertex/index buffers: the binary `VBIB` block (`VBIB.cs:182-333`) and the KV3 form
//! (`m_vertexBuffers`/`m_indexBuffers`, `VBIB.cs:335-417`) used by `MVTX`/`MIDX`-era resources.
//! zstd (whole-buffer) and meshoptimizer (per-element) decompression, in that order --
//! `VBIB.cs:283-333`'s `DecompressData`.

use s2fmt::kv3::Value;
use s2fmt::resource::Resource;

use crate::error::MeshError;
use crate::format::DxgiFormat;
use crate::reader::Reader;

/// One `RenderInputLayoutField_t` entry (`VBIB.cs:81-134`). Only the fields attribute decoding
/// needs; `m_nSlot`/`m_nSlotType`/`m_nInstanceStepRate`/`m_szShaderSemantic` are read but not
/// kept (draw calls already say which buffer index/stream they use).
#[derive(Debug, Clone, PartialEq)]
pub struct InputLayoutField {
    /// Upper-cased, e.g. `"POSITION"`, `"TEXCOORD"` (`VBIB.cs:236`, `:364`).
    pub semantic_name: String,
    pub semantic_index: i32,
    pub format: DxgiFormat,
    pub offset: u32,
}

/// How a buffer's on-disk bytes were actually compressed (not just what the flag bits nominally
/// said -- a buffer already stored at full size decompresses as `Default`, flags notwithstanding;
/// `s6f3a1_mesh.md`'s test item asks for a meshopt stream-version and zstd-fraction breakdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Compression {
    pub zstd: bool,
    pub meshopt: bool,
    /// The meshopt-compressed stream's own first byte (its version header: `0xa0`/`0xa1` for
    /// vertex streams, `0xe0`/`0xe1` for index streams), captured before decoding it.
    pub meshopt_header: Option<u8>,
}

/// One `OnDiskBufferData` (`VBIB.cs:49-75`): a fully decompressed vertex or index buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct Buffer {
    pub element_count: u32,
    /// Stride for a vertex buffer, index type size (2 or 4) for an index buffer.
    pub element_size: u32,
    /// Empty for index buffers.
    pub fields: Vec<InputLayoutField>,
    /// Decompressed bytes, exactly `element_count * element_size` long.
    pub data: Vec<u8>,
    pub compression: Compression,
}

impl Buffer {
    /// The first field matching a semantic name (case-insensitive) and index.
    pub fn field(&self, semantic_name: &str, semantic_index: i32) -> Option<&InputLayoutField> {
        self.fields.iter().find(|f| {
            f.semantic_index == semantic_index
                && f.semantic_name.eq_ignore_ascii_case(semantic_name)
        })
    }

    /// Reads one index (`element_size` 2 or 4 bytes, little-endian) at element position `i`.
    /// `None` if `i` is out of range or `element_size` isn't 2 or 4.
    pub fn index_at(&self, i: usize) -> Option<u32> {
        let size = self.element_size as usize;
        let start = i.checked_mul(size)?;
        let bytes = self.data.get(start..start.checked_add(size)?)?;
        match size {
            2 => Some(u32::from(u16::from_le_bytes(bytes.try_into().unwrap()))),
            4 => Some(u32::from_le_bytes(bytes.try_into().unwrap())),
            _ => None,
        }
    }
}

/// Sanity cap on a single decompressed buffer, matching `s2fmt::compress`'s
/// `MAX_DECOMPRESSED_SIZE`: real CS2 vertex/index buffers are at most a few MB.
const MAX_BUFFER_BYTES: u64 = 1 << 30;

fn checked_size(count: u32, size: u32, path: &str) -> Result<usize, MeshError> {
    let bytes = u64::from(count) * u64::from(size);
    if bytes > MAX_BUFFER_BYTES {
        return Err(MeshError::SizeTooLarge {
            path: path.to_string(),
            count: u64::from(count),
            size: u64::from(size),
            limit: MAX_BUFFER_BYTES,
        });
    }
    Ok(bytes as usize)
}

/// Decodes a zstd frame into at most `max_size` bytes, erroring rather than growing past it
/// (`ruzstd`'s `decode_all_to_vec` treats the output `Vec`'s capacity as a hard bound).
fn zstd_decode_bounded(input: &[u8], max_size: usize, path: &str) -> Result<Vec<u8>, MeshError> {
    let mut out = Vec::with_capacity(max_size);
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    decoder
        .decode_all_to_vec(input, &mut out)
        .map_err(|e| MeshError::Zstd {
            path: path.to_string(),
            message: e.to_string(),
        })?;
    Ok(out)
}

/// meshoptimizer's own precondition for `meshopt_decodeIndexBuffer` (vendored
/// `indexcodec.cpp:386`, mirrored by VRF's `MeshOptimizerIndexDecoder.cs:90-92`): the vendored
/// C++ enforces this with a bare `assert`, which is compiled in (our release profile leaves
/// `NDEBUG` undefined) and aborts the process; were `NDEBUG` defined instead, the same violation
/// would silently read/write out of bounds. `meshopt::decode_index_buffer`'s safe wrapper also
/// allocates its output `Vec` before calling into the FFI, so this must run first.
fn check_meshopt_index_layout(
    element_count: usize,
    element_size: usize,
    path: &str,
) -> Result<(), MeshError> {
    if !element_count.is_multiple_of(3) {
        return Err(MeshError::MeshoptLayout {
            path: path.to_string(),
            element_count: element_count as u64,
            element_size: element_size as u64,
        });
    }
    Ok(())
}

/// Decodes a meshoptimizer-compressed index buffer via the `meshopt` crate's safe API (index
/// element size is always 2 or 4 bytes, so no dynamic-stride FFI is needed here).
fn meshopt_decode_index(
    data: &[u8],
    element_count: usize,
    element_size: usize,
    path: &str,
) -> Result<Vec<u8>, MeshError> {
    check_meshopt_index_layout(element_count, element_size, path)?;
    match element_size {
        2 => {
            let decoded: Vec<u16> =
                meshopt::decode_index_buffer(data, element_count).map_err(|source| {
                    MeshError::MeshoptIndex {
                        path: path.to_string(),
                        message: source.to_string(),
                    }
                })?;
            Ok(decoded.iter().flat_map(|v| v.to_le_bytes()).collect())
        }
        4 => {
            let decoded: Vec<u32> =
                meshopt::decode_index_buffer(data, element_count).map_err(|source| {
                    MeshError::MeshoptIndex {
                        path: path.to_string(),
                        message: source.to_string(),
                    }
                })?;
            Ok(decoded.iter().flat_map(|v| v.to_le_bytes()).collect())
        }
        other => Err(MeshError::BadIndexElementSize {
            path: path.to_string(),
            size: other as u32,
        }),
    }
}

/// meshoptimizer's own precondition for `meshopt_decodeVertexBuffer` (vendored
/// `vertexcodec.cpp:1834-1835`, mirrored by VRF's `MeshOptimizerVertexDecoder.cs:362-369`): same
/// `assert`-is-compiled-in hazard as [`check_meshopt_index_layout`], and must run before `out` is
/// allocated below.
fn check_meshopt_vertex_layout(
    element_count: usize,
    element_size: usize,
    path: &str,
) -> Result<(), MeshError> {
    if element_size == 0 || element_size > 256 || !element_size.is_multiple_of(4) {
        return Err(MeshError::MeshoptLayout {
            path: path.to_string(),
            element_count: element_count as u64,
            element_size: element_size as u64,
        });
    }
    Ok(())
}

/// Decodes a meshoptimizer-compressed vertex buffer. The stride is only known at runtime (it's
/// the mesh's own vertex size), so this calls the C API directly rather than the crate's
/// generic-`T` safe wrapper (which needs a compile-time-sized type).
fn meshopt_decode_vertex(
    data: &[u8],
    element_count: usize,
    element_size: usize,
    path: &str,
) -> Result<Vec<u8>, MeshError> {
    check_meshopt_vertex_layout(element_count, element_size, path)?;
    let mut out = vec![0u8; element_count * element_size];
    // Safety: `out` is exactly `element_count * element_size` bytes, matching what
    // `meshopt_decodeVertexBuffer` requires of its destination buffer (its C header doc: "must
    // contain enough space for the resulting vertex buffer"); `data` is passed as a `(ptr, len)`
    // pair the callee only reads from. `check_meshopt_vertex_layout` above has already rejected
    // `element_size` values the C function's own `assert(vertex_size > 0 && vertex_size <= 256)`
    // and `assert(vertex_size % 4 == 0)` (`vertexcodec.cpp:1834-1835`) would otherwise reach --
    // an `assert` our release profile compiles in (aborting the process), or that a build with
    // `NDEBUG` defined would silently turn into an out-of-bounds write instead.
    let code = unsafe {
        meshopt::ffi::meshopt_decodeVertexBuffer(
            out.as_mut_ptr().cast(),
            element_count,
            element_size,
            data.as_ptr(),
            data.len(),
        )
    };
    if code != 0 {
        return Err(MeshError::MeshoptVertex {
            path: path.to_string(),
            code,
        });
    }
    Ok(out)
}

fn meshopt_decode(
    data: &[u8],
    element_count: usize,
    element_size: usize,
    is_vertex: bool,
    path: &str,
) -> Result<Vec<u8>, MeshError> {
    if is_vertex {
        meshopt_decode_vertex(data, element_count, element_size, path)
    } else {
        meshopt_decode_index(data, element_count, element_size, path)
    }
}

/// Applies the "stored smaller than declared -> decompress" rule shared by both the binary VBIB
/// block (`VBIB.cs:255-276`) and the KV3 `MVTX`/`MIDX` form (`s6f3a1_mesh.md`'s contract): zstd
/// outer, meshopt inner, only when `stored.len()` is smaller than `element_count *
/// element_size`; otherwise the stored bytes must already be exactly that size.
fn decode_buffer_bytes(
    stored: &[u8],
    element_count: u32,
    element_size: u32,
    is_vertex: bool,
    is_zstd: bool,
    is_meshopt: bool,
    path: &str,
) -> Result<(Vec<u8>, Compression), MeshError> {
    let decompressed_size = checked_size(element_count, element_size, path)?;
    if stored.len() >= decompressed_size {
        if stored.len() != decompressed_size {
            return Err(MeshError::LengthMismatch {
                path: path.to_string(),
                actual: stored.len(),
                expected: decompressed_size,
            });
        }
        return Ok((stored.to_vec(), Compression::default()));
    }

    let after_zstd = if is_zstd {
        zstd_decode_bounded(stored, decompressed_size, path)?
    } else {
        stored.to_vec()
    };

    if is_meshopt {
        let decoded = meshopt_decode(
            &after_zstd,
            element_count as usize,
            element_size as usize,
            is_vertex,
            path,
        )?;
        Ok((
            decoded,
            Compression {
                zstd: is_zstd,
                meshopt: true,
                meshopt_header: after_zstd.first().copied(),
            },
        ))
    } else if after_zstd.len() == decompressed_size {
        Ok((
            after_zstd,
            Compression {
                zstd: is_zstd,
                meshopt: false,
                meshopt_header: None,
            },
        ))
    } else {
        Err(MeshError::LengthMismatch {
            path: path.to_string(),
            actual: after_zstd.len(),
            expected: decompressed_size,
        })
    }
}

// ---- binary VBIB block (old dialect / standalone vmesh_c) ----

fn trunc(path: &str, source: crate::reader::OutOfBounds) -> MeshError {
    MeshError::Truncated {
        path: path.to_string(),
        source,
    }
}

fn read_on_disk_buffer_data(
    r: &mut Reader<'_>,
    is_vertex: bool,
    path: &str,
) -> Result<Buffer, MeshError> {
    let element_count = r.u32().map_err(|e| trunc(path, e))?;
    let size = r.i32().map_err(|e| trunc(path, e))? as u32;
    // `size & 0x3FFFFFF` is the element size; bit 26 CLEAR means meshopt-compressed, bit 27 set
    // means zstd-compressed (`s6f3a1_mesh.md`'s change item 1; `VBIB.cs:214-220`).
    let element_size = size & 0x03FF_FFFF;
    let is_meshopt = size & 0x0400_0000 == 0;
    let is_zstd = size & 0x0800_0000 != 0;

    let ref_a = r.pos();
    let attribute_offset = r.u32().map_err(|e| trunc(path, e))?;
    let attribute_count = r.u32().map_err(|e| trunc(path, e))?;

    let ref_b = r.pos();
    let data_offset = r.u32().map_err(|e| trunc(path, e))?;
    let total_size = r.i32().map_err(|e| trunc(path, e))?;
    let total_size = usize::try_from(total_size).map_err(|_| MeshError::WrongType {
        path: path.to_string(),
        expected: "non-negative total size",
    })?;

    r.set_pos(ref_a + attribute_offset as usize);
    let mut fields = Vec::with_capacity((attribute_count as usize).min(1 << 16));
    for i in 0..attribute_count {
        let field_path = format!("{path}.field[{i}]");
        let semantic_name = r
            .fixed_cstr(32)
            .map_err(|e| trunc(&field_path, e))?
            .to_ascii_uppercase();
        let semantic_index = r.i32().map_err(|e| trunc(&field_path, e))?;
        let format = DxgiFormat::from_raw(r.u32().map_err(|e| trunc(&field_path, e))?);
        let offset = r.u32().map_err(|e| trunc(&field_path, e))?;
        let _slot = r.i32().map_err(|e| trunc(&field_path, e))?;
        let _slot_type = r.u32().map_err(|e| trunc(&field_path, e))?;
        let _instance_step_rate = r.i32().map_err(|e| trunc(&field_path, e))?;
        fields.push(InputLayoutField {
            semantic_name,
            semantic_index,
            format,
            offset,
        });
    }

    r.set_pos(ref_b + data_offset as usize);
    let stored = r.bytes(total_size).map_err(|e| trunc(path, e))?;
    let (data, compression) = decode_buffer_bytes(
        stored,
        element_count,
        element_size,
        is_vertex,
        is_zstd,
        is_meshopt,
        path,
    )?;

    r.set_pos(ref_b + 8);

    Ok(Buffer {
        element_count,
        element_size,
        fields,
        data,
        compression,
    })
}

/// Parses a binary `VBIB` block's bytes (`VBIB.cs:182-206`).
pub fn parse_vbib_block(bytes: &[u8]) -> Result<(Vec<Buffer>, Vec<Buffer>), MeshError> {
    let path = "VBIB";
    let mut r = Reader::new(bytes);
    let vertex_buffer_offset = r.u32().map_err(|e| trunc(path, e))?;
    let vertex_buffer_count = r.u32().map_err(|e| trunc(path, e))?;
    let index_buffer_offset = r.u32().map_err(|e| trunc(path, e))?;
    let index_buffer_count = r.u32().map_err(|e| trunc(path, e))?;

    r.set_pos(vertex_buffer_offset as usize);
    let mut vertex_buffers = Vec::with_capacity((vertex_buffer_count as usize).min(1 << 16));
    for i in 0..vertex_buffer_count {
        vertex_buffers.push(read_on_disk_buffer_data(
            &mut r,
            true,
            &format!("{path}.vertex[{i}]"),
        )?);
    }

    r.set_pos(8 + index_buffer_offset as usize);
    let mut index_buffers = Vec::with_capacity((index_buffer_count as usize).min(1 << 16));
    for i in 0..index_buffer_count {
        index_buffers.push(read_on_disk_buffer_data(
            &mut r,
            false,
            &format!("{path}.index[{i}]"),
        )?);
    }

    Ok((vertex_buffers, index_buffers))
}

// ---- KV3 form (m_vertexBuffers / m_indexBuffers) ----

fn require_array<'a>(v: &'a Value, path: &str) -> Result<&'a [Value], MeshError> {
    v.as_array().ok_or_else(|| MeshError::WrongType {
        path: path.to_string(),
        expected: "array",
    })
}

fn require_u32(v: &Value, key: &str, path: &str) -> Result<u32, MeshError> {
    v.get(key)
        .and_then(Value::as_u64)
        .map(|u| u as u32)
        .ok_or_else(|| MeshError::Missing {
            path: format!("{path}.{key}"),
        })
}

/// `m_bMeshoptCompressed`/`m_bCompressedZSTD`: a KV3 bool, or (leniently) a non-zero integer.
fn flag(v: &Value, key: &str) -> bool {
    v.get(key)
        .and_then(|v| v.as_bool().or_else(|| v.as_i64().map(|i| i != 0)))
        .unwrap_or(false)
}

fn semantic_name_of(field: &Value, path: &str) -> Result<String, MeshError> {
    let name = field
        .get("m_pSemanticName")
        .ok_or_else(|| MeshError::Missing {
            path: format!("{path}.m_pSemanticName"),
        })?;
    if let Some(s) = name.as_str() {
        return Ok(s.to_ascii_uppercase());
    }
    if let Some(b) = name.as_blob() {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        return Ok(String::from_utf8_lossy(&b[..end]).to_ascii_uppercase());
    }
    Err(MeshError::WrongType {
        path: format!("{path}.m_pSemanticName"),
        expected: "string or blob",
    })
}

fn parse_input_layout_fields(
    entry: &Value,
    path: &str,
) -> Result<Vec<InputLayoutField>, MeshError> {
    let Some(v) = entry.get("m_inputLayoutFields") else {
        return Ok(Vec::new());
    };
    let arr = require_array(v, &format!("{path}.m_inputLayoutFields"))?;
    arr.iter()
        .enumerate()
        .map(|(i, f)| {
            let p = format!("{path}.m_inputLayoutFields[{i}]");
            Ok(InputLayoutField {
                semantic_name: semantic_name_of(f, &p)?,
                semantic_index: f
                    .get("m_nSemanticIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as i32,
                format: DxgiFormat::from_raw(
                    f.get("m_Format").and_then(Value::as_u64).unwrap_or(0) as u32,
                ),
                offset: f.get("m_nOffset").and_then(Value::as_u64).unwrap_or(0) as u32,
            })
        })
        .collect()
}

fn parse_kv3_buffer(
    resource: &Resource,
    entry: &Value,
    is_vertex: bool,
    path: &str,
) -> Result<Buffer, MeshError> {
    let element_count = require_u32(entry, "m_nElementCount", path)?;
    let element_size = require_u32(entry, "m_nElementSizeInBytes", path)?;
    let fields = parse_input_layout_fields(entry, path)?;

    let (data, compression) = if let Some(pdata) = entry.get("m_pData") {
        let raw = pdata.as_blob().ok_or_else(|| MeshError::WrongType {
            path: format!("{path}.m_pData"),
            expected: "blob",
        })?;
        let decompressed_size = checked_size(element_count, element_size, path)?;
        if raw.len() == decompressed_size {
            (raw.to_vec(), Compression::default())
        } else {
            // Inline `m_pData` is only ever meshopt-compressed, never zstd (`VBIB.cs:375-383`).
            let decoded = meshopt_decode(
                raw,
                element_count as usize,
                element_size as usize,
                is_vertex,
                path,
            )?;
            let compression = Compression {
                zstd: false,
                meshopt: true,
                meshopt_header: raw.first().copied(),
            };
            (decoded, compression)
        }
    } else {
        let block_index = entry
            .get("m_nBlockIndex")
            .and_then(Value::as_i64)
            .ok_or_else(|| MeshError::Missing {
                path: format!("{path}.m_nBlockIndex"),
            })?;
        let block = usize::try_from(block_index)
            .ok()
            .and_then(|i| resource.block_by_filtered_index(i))
            .ok_or_else(|| MeshError::BlockNotFound {
                path: path.to_string(),
                index: block_index,
            })?;
        let stored = resource.block_bytes(block);
        decode_buffer_bytes(
            stored,
            element_count,
            element_size,
            is_vertex,
            flag(entry, "m_bCompressedZSTD"),
            flag(entry, "m_bMeshoptCompressed"),
            path,
        )?
    };

    Ok(Buffer {
        element_count,
        element_size,
        fields,
        data,
        compression,
    })
}

/// Parses `root.m_vertexBuffers`/`root.m_indexBuffers` (`VBIB.cs:150-179`, `:335-417`). Missing
/// keys decode as no buffers, matching the rest of this codebase's tolerance for absent optional
/// KV3 keys.
pub fn parse_kv3_buffers(
    resource: &Resource,
    root: &Value,
    path: &str,
) -> Result<(Vec<Buffer>, Vec<Buffer>), MeshError> {
    let vertex = match root.get("m_vertexBuffers") {
        Some(v) => {
            let arr = require_array(v, &format!("{path}.m_vertexBuffers"))?;
            arr.iter()
                .enumerate()
                .map(|(i, e)| {
                    parse_kv3_buffer(resource, e, true, &format!("{path}.m_vertexBuffers[{i}]"))
                })
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };
    let index = match root.get("m_indexBuffers") {
        Some(v) => {
            let arr = require_array(v, &format!("{path}.m_indexBuffers"))?;
            arr.iter()
                .enumerate()
                .map(|(i, e)| {
                    parse_kv3_buffer(resource, e, false, &format!("{path}.m_indexBuffers[{i}]"))
                })
                .collect::<Result<_, _>>()?
        }
        None => Vec::new(),
    };
    Ok((vertex, index))
}
