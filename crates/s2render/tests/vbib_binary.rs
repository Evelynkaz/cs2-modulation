//! Synthetic binary `VBIB` blocks covering every stored-size/compression-flag combination
//! `s6f3a1_mesh.md`'s contract lists: uncompressed, meshoptimizer-only, and zstd+meshoptimizer.

mod common;

use common::{BufferSpec, build_vbib, zstd_compress};
use s2render::attributes::decode_positions;
use s2render::buffer::parse_vbib_block;

const POSITION_FORMAT: u32 = 6; // DXGI_FORMAT.R32G32B32_FLOAT (DXGIFormat.cs:16)

fn position_field() -> (String, i32, u32, u32) {
    ("POSITION".to_string(), 0, POSITION_FORMAT, 0)
}

fn positions_to_bytes(positions: &[[f32; 3]]) -> Vec<u8> {
    positions
        .iter()
        .flat_map(|p| p.iter().flat_map(|f| f.to_le_bytes()))
        .collect()
}

#[test]
fn vertex_buffer_uncompressed_roundtrips() {
    let positions = vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [-1.5, 0.0, 7.25]];
    let stored = positions_to_bytes(&positions);
    let bytes = build_vbib(
        &[BufferSpec {
            element_count: positions.len() as u32,
            element_size: 12,
            meshopt: false,
            zstd: false,
            fields: vec![position_field()],
            stored,
        }],
        &[],
    );

    let (vertex, index) = parse_vbib_block(&bytes).unwrap();
    assert_eq!(index.len(), 0);
    assert_eq!(vertex.len(), 1);
    let field = vertex[0].field("POSITION", 0).unwrap();
    let decoded = decode_positions(&vertex[0], field).unwrap();
    assert_eq!(decoded, positions);
}

#[test]
fn vertex_buffer_meshopt_compressed_roundtrips() {
    // A longer, mesh-like (smoothly varying) sequence: tiny inputs don't compress smaller than
    // they started, so this needs enough redundancy for the assertion below to be meaningful.
    let positions: Vec<[f32; 3]> = (0..200)
        .map(|i| {
            let t = i as f32;
            [t * 0.1, (t * 0.1).sin(), 1.0]
        })
        .collect();
    let compressed = meshopt::encode_vertex_buffer(&positions).unwrap();
    assert!(
        compressed.len() < positions_to_bytes(&positions).len(),
        "test is only meaningful if compression actually shrinks the buffer"
    );

    let bytes = build_vbib(
        &[BufferSpec {
            element_count: positions.len() as u32,
            element_size: 12,
            meshopt: true,
            zstd: false,
            fields: vec![position_field()],
            stored: compressed,
        }],
        &[],
    );

    let (vertex, _) = parse_vbib_block(&bytes).unwrap();
    let field = vertex[0].field("POSITION", 0).unwrap();
    let decoded = decode_positions(&vertex[0], field).unwrap();
    assert_eq!(decoded, positions);
}

#[test]
fn vertex_buffer_zstd_plus_meshopt_compressed_roundtrips() {
    let positions = vec![[1.0, 2.0, 3.0]; 20]; // repetitive data compresses well under zstd too
    let meshopt_bytes = meshopt::encode_vertex_buffer(&positions).unwrap();
    let stored = zstd_compress(&meshopt_bytes);

    let bytes = build_vbib(
        &[BufferSpec {
            element_count: positions.len() as u32,
            element_size: 12,
            meshopt: true,
            zstd: true,
            fields: vec![position_field()],
            stored,
        }],
        &[],
    );

    let (vertex, _) = parse_vbib_block(&bytes).unwrap();
    let field = vertex[0].field("POSITION", 0).unwrap();
    let decoded = decode_positions(&vertex[0], field).unwrap();
    assert_eq!(decoded, positions);
}

#[test]
fn index_buffer_meshopt_compressed_roundtrips() {
    let indices: Vec<u32> = vec![0, 1, 2, 2, 1, 3, 3, 1, 4];
    let compressed = meshopt::encode_index_buffer(&indices, 5).unwrap();

    let bytes = build_vbib(
        &[],
        &[BufferSpec {
            element_count: indices.len() as u32,
            element_size: 4,
            meshopt: true,
            zstd: false,
            fields: vec![],
            stored: compressed,
        }],
    );

    let (_, index) = parse_vbib_block(&bytes).unwrap();
    assert_eq!(index.len(), 1);
    let decoded: Vec<u32> = (0..indices.len())
        .map(|i| index[0].index_at(i).unwrap())
        .collect();
    assert_eq!(decoded, indices);
}

#[test]
fn index_buffer_zstd_only_roundtrips() {
    // Long and repetitive enough that the zstd frame is smaller than the raw bytes (tiny inputs
    // don't compress smaller than they started, given zstd's own frame overhead).
    let indices: Vec<u16> = (0..300u16).map(|i| i % 4).collect();
    let raw: Vec<u8> = indices.iter().flat_map(|v| v.to_le_bytes()).collect();
    let stored = zstd_compress(&raw);
    assert!(
        stored.len() < raw.len(),
        "test needs the zstd frame to actually shrink the data"
    );

    let bytes = build_vbib(
        &[],
        &[BufferSpec {
            element_count: indices.len() as u32,
            element_size: 2,
            meshopt: false,
            zstd: true,
            fields: vec![],
            stored,
        }],
    );

    let (_, index) = parse_vbib_block(&bytes).unwrap();
    let decoded: Vec<u32> = (0..indices.len())
        .map(|i| index[0].index_at(i).unwrap())
        .collect();
    assert_eq!(
        decoded,
        indices.iter().map(|&v| u32::from(v)).collect::<Vec<_>>()
    );
}

#[test]
fn corrupt_meshopt_payload_errors_not_panics() {
    let bytes = build_vbib(
        &[BufferSpec {
            element_count: 4,
            element_size: 12,
            meshopt: true,
            zstd: false,
            fields: vec![position_field()],
            stored: vec![0xFFu8; 3], // far too short to be valid meshopt data
        }],
        &[],
    );

    let result = parse_vbib_block(&bytes);
    assert!(
        result.is_err(),
        "corrupt meshopt payload must error, not panic"
    );
}

// ---- meshoptimizer's own decode preconditions (vendored vertexcodec.cpp/indexcodec.cpp), which
// the vendored C++ enforces with `assert` -- compiled in by our release profile (process abort),
// or (with NDEBUG defined) silently skipped, turning the same violation into an out-of-bounds
// write. `s6f3a1_mesh.md` change item 1's repro cases. ----

#[test]
fn meshopt_vertex_stride_6_not_multiple_of_4_errors_not_panics() {
    let bytes = build_vbib(
        &[BufferSpec {
            element_count: 4,
            element_size: 6, // meshoptimizer requires vertex_size % 4 == 0 (vertexcodec.cpp:1835)
            meshopt: true,
            zstd: false,
            fields: vec![],
            stored: vec![0u8; 4], // shorter than element_count * element_size so decode is tried
        }],
        &[],
    );
    assert!(parse_vbib_block(&bytes).is_err());
}

#[test]
fn meshopt_vertex_stride_300_exceeds_256_errors_not_panics() {
    let bytes = build_vbib(
        &[BufferSpec {
            element_count: 1,
            element_size: 300, // meshoptimizer requires vertex_size <= 256 (vertexcodec.cpp:1834)
            meshopt: true,
            zstd: false,
            fields: vec![],
            stored: vec![0u8; 10],
        }],
        &[],
    );
    assert!(parse_vbib_block(&bytes).is_err());
}

#[test]
fn meshopt_index_count_4_not_multiple_of_3_errors_not_panics() {
    let bytes = build_vbib(
        &[],
        &[BufferSpec {
            element_count: 4, // meshoptimizer requires index_count % 3 == 0 (indexcodec.cpp:386)
            element_size: 2,
            meshopt: true,
            zstd: false,
            fields: vec![],
            stored: vec![0u8; 3],
        }],
    );
    assert!(parse_vbib_block(&bytes).is_err());
}

#[test]
fn meshopt_index_element_size_3_errors_not_panics() {
    let bytes = build_vbib(
        &[],
        &[BufferSpec {
            element_count: 6, // multiple of 3, but 3 is neither of meshoptimizer's index sizes
            element_size: 3,  // (indexcodec.cpp:387: index_size == 2 || index_size == 4)
            meshopt: true,
            zstd: false,
            fields: vec![],
            stored: vec![0u8; 3],
        }],
    );
    assert!(parse_vbib_block(&bytes).is_err());
}
