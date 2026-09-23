//! Errors decoding VBIB/MBUF/MVTX/MIDX buffers and vmesh_c/vmdl_c KV3 structures into
//! [`crate::mesh::Mesh`]/[`crate::model::Model`]. Every variant carries a `path`: a dotted/
//! indexed key path into the source KV3 value, or a block/field description for the binary VBIB
//! form -- mirrors `s2fmt::phys::PhysError`'s style. `source` fields that wrap another crate's
//! (large) error type are boxed, same reasoning as `extract::ExtractError`
//! (`clippy::result_large_err`).

use crate::format::DxgiFormat;

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("missing required key {path}")]
    Missing { path: String },
    #[error("{path}: expected {expected}")]
    WrongType {
        path: String,
        expected: &'static str,
    },
    #[error("{path}: index {index} out of range (have {len})")]
    IndexOutOfRange {
        path: String,
        index: i64,
        len: usize,
    },
    #[error("{path}: block index {index} does not resolve to a block of this resource")]
    BlockNotFound { path: String, index: i64 },
    #[error(
        "{path}: element count {count} * element size {size} exceeds the sanity limit of {limit} bytes"
    )]
    SizeTooLarge {
        path: String,
        count: u64,
        size: u64,
        limit: u64,
    },
    #[error("{path}: truncated: {source}")]
    Truncated {
        path: String,
        #[source]
        source: crate::reader::OutOfBounds,
    },
    #[error("{path}: zstd decode failed: {message}")]
    Zstd { path: String, message: String },
    #[error(
        "{path}: decompressed size {actual} bytes does not match the declared element_count * element_size of {expected} bytes"
    )]
    LengthMismatch {
        path: String,
        actual: usize,
        expected: usize,
    },
    #[error("{path}: meshopt vertex buffer decode failed with code {code}")]
    MeshoptVertex { path: String, code: i32 },
    #[error("{path}: meshopt index buffer decode failed: {message}")]
    MeshoptIndex { path: String, message: String },
    #[error("{path}: index element size {size} is neither 2 nor 4 bytes")]
    BadIndexElementSize { path: String, size: u32 },
    #[error(
        "{path}: element_count {element_count} / element_size {element_size} violates meshoptimizer's own decode precondition (vertex element_size must be a multiple of 4 in 4..=256, index element_count must be a multiple of 3)"
    )]
    MeshoptLayout {
        path: String,
        element_count: u64,
        element_size: u64,
    },
    #[error(
        "{path}: vertex {vertex} attribute at offset {offset} len {len} exceeds the stride of {stride} bytes"
    )]
    AttributeRange {
        path: String,
        vertex: usize,
        offset: usize,
        len: usize,
        stride: usize,
    },
    #[error("{path}: unsupported {semantic} attribute format {format:?}")]
    UnsupportedFormat {
        path: String,
        semantic: String,
        format: DxgiFormat,
    },
    #[error("KV3 error at {path}: {source}")]
    Kv3 {
        path: String,
        #[source]
        source: Box<s2fmt::kv3::Kv3Error>,
    },
    #[error("resource error at {path}: {source}")]
    Resource {
        path: String,
        #[source]
        source: Box<s2fmt::resource::ResourceError>,
    },
}
