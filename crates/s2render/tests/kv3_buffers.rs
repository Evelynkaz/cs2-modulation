//! The KV3 `m_vertexBuffers`/`m_indexBuffers` form: inline `m_pData` and `m_nBlockIndex` +
//! `m_bMeshoptCompressed`/`m_bCompressedZSTD` pointing at an `MVTX`/`MIDX` block.

mod common;

use common::{empty_resource, resource_with_block, zstd_compress};
use s2fmt::kv3::{Object, Value};
use s2render::attributes::decode_positions;
use s2render::buffer::parse_kv3_buffers;

const POSITION_FORMAT: u32 = 6; // DXGI_FORMAT.R32G32B32_FLOAT

fn input_layout_field(name: &str, format: u32, offset: u32) -> Value {
    let mut o = Object::new();
    o.push("m_pSemanticName", Value::String(name.to_string()));
    o.push("m_nSemanticIndex", Value::Int(0));
    o.push("m_Format", Value::UInt(u64::from(format)));
    o.push("m_nOffset", Value::UInt(u64::from(offset)));
    Value::Object(o)
}

fn positions_to_bytes(positions: &[[f32; 3]]) -> Vec<u8> {
    positions
        .iter()
        .flat_map(|p| p.iter().flat_map(|f| f.to_le_bytes()))
        .collect()
}

fn wrap_vertex_buffers(entry: Value) -> Value {
    let mut root = Object::new();
    root.push("m_vertexBuffers", Value::Array(vec![entry]));
    Value::Object(root)
}

#[test]
fn inline_data_raw_when_already_full_size() {
    let positions = vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]];
    let mut entry = Object::new();
    entry.push("m_nElementCount", Value::UInt(positions.len() as u64));
    entry.push("m_nElementSizeInBytes", Value::UInt(12));
    entry.push(
        "m_inputLayoutFields",
        Value::Array(vec![input_layout_field("POSITION", POSITION_FORMAT, 0)]),
    );
    entry.push("m_pData", Value::Blob(positions_to_bytes(&positions)));
    let root = wrap_vertex_buffers(Value::Object(entry));

    let resource = empty_resource();
    let (vertex, index) = parse_kv3_buffers(&resource, &root, "DATA").unwrap();
    assert_eq!(index.len(), 0);
    let field = vertex[0].field("POSITION", 0).unwrap();
    assert_eq!(decode_positions(&vertex[0], field).unwrap(), positions);
}

#[test]
fn inline_data_meshopt_compressed() {
    let positions = vec![[0.0, 0.0, 0.0], [1.0, -2.0, 3.5], [9.0, 9.0, 9.0]];
    let compressed = meshopt::encode_vertex_buffer(&positions).unwrap();

    let mut entry = Object::new();
    entry.push("m_nElementCount", Value::UInt(positions.len() as u64));
    entry.push("m_nElementSizeInBytes", Value::UInt(12));
    entry.push(
        "m_inputLayoutFields",
        Value::Array(vec![input_layout_field("POSITION", POSITION_FORMAT, 0)]),
    );
    entry.push("m_pData", Value::Blob(compressed));
    let root = wrap_vertex_buffers(Value::Object(entry));

    let resource = empty_resource();
    let (vertex, _) = parse_kv3_buffers(&resource, &root, "DATA").unwrap();
    let field = vertex[0].field("POSITION", 0).unwrap();
    assert_eq!(decode_positions(&vertex[0], field).unwrap(), positions);
}

#[test]
fn block_indexed_zstd_plus_meshopt() {
    let positions = vec![[1.0, 2.0, 3.0]; 16];
    let meshopt_bytes = meshopt::encode_vertex_buffer(&positions).unwrap();
    let stored = zstd_compress(&meshopt_bytes);
    let resource = resource_with_block(*b"MVTX", &stored);

    let mut entry = Object::new();
    entry.push("m_nElementCount", Value::UInt(positions.len() as u64));
    entry.push("m_nElementSizeInBytes", Value::UInt(12));
    entry.push(
        "m_inputLayoutFields",
        Value::Array(vec![input_layout_field("POSITION", POSITION_FORMAT, 0)]),
    );
    entry.push("m_nBlockIndex", Value::Int(0));
    entry.push("m_bMeshoptCompressed", Value::Bool(true));
    entry.push("m_bCompressedZSTD", Value::Bool(true));
    let root = wrap_vertex_buffers(Value::Object(entry));

    let (vertex, _) = parse_kv3_buffers(&resource, &root, "DATA").unwrap();
    let field = vertex[0].field("POSITION", 0).unwrap();
    assert_eq!(decode_positions(&vertex[0], field).unwrap(), positions);
}

#[test]
fn block_indexed_missing_index_errors_not_panics() {
    let resource = empty_resource();
    let mut entry = Object::new();
    entry.push("m_nElementCount", Value::UInt(1));
    entry.push("m_nElementSizeInBytes", Value::UInt(12));
    entry.push("m_nBlockIndex", Value::Int(0)); // resource has no blocks at all
    entry.push("m_bMeshoptCompressed", Value::Bool(false));
    entry.push("m_bCompressedZSTD", Value::Bool(false));
    let root = wrap_vertex_buffers(Value::Object(entry));

    assert!(parse_kv3_buffers(&resource, &root, "DATA").is_err());
}
