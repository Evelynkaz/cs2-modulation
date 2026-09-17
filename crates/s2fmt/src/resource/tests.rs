//! Synthetic-resource unit tests: builds resources (header + block table +
//! aligned data) using `kv3::test_writer` to produce the binary KV3 blocks.
//! See spec `resource.md` "unit tests (synthetic)".

use super::*;
use crate::kv3;
use crate::kv3::test_writer::{Compression, TestNode, build};

fn build_kv3_v5(root: &TestNode) -> Vec<u8> {
    build(root, 5, Compression::None, kv3::FORMAT_GENERIC)
}

fn ctrl_bytes(phys_data_block: i32) -> Vec<u8> {
    build_kv3_v5(&TestNode::Object(vec![(
        "embedded_physics".to_string(),
        TestNode::Object(vec![(
            "phys_data_block".to_string(),
            TestNode::Int32(phys_data_block),
        )]),
    )]))
}

// ---------------------------------------------------------------------------
// Resource container builder.
// ---------------------------------------------------------------------------

struct TestBlock {
    fourcc: FourCC,
    data: Vec<u8>,
}

fn align16(n: usize) -> usize {
    n.div_ceil(16) * 16
}

/// Builds a resource container: 16-byte header, 12-byte-per-entry block
/// table right after it, then each block's data 16-byte aligned
/// (`docs/FORMATS.md` section 2.1-2.2).
fn build_resource(header_version: u16, version: u16, blocks: &[TestBlock]) -> Vec<u8> {
    let table_start = 16usize;
    let table_size = blocks.len() * 12;
    let mut pos = align16(table_start + table_size);
    let mut offsets = Vec::with_capacity(blocks.len());
    for b in blocks {
        offsets.push(pos);
        pos = align16(pos + b.data.len());
    }
    let total_len = pos;

    let mut out = vec![0u8; total_len];
    out[0..4].copy_from_slice(&(total_len as u32).to_le_bytes());
    out[4..6].copy_from_slice(&header_version.to_le_bytes());
    out[6..8].copy_from_slice(&version.to_le_bytes());
    out[8..12].copy_from_slice(&8u32.to_le_bytes()); // block table right after this field
    out[12..16].copy_from_slice(&(blocks.len() as u32).to_le_bytes());

    for (i, b) in blocks.iter().enumerate() {
        let entry_start = table_start + i * 12;
        out[entry_start..entry_start + 4].copy_from_slice(&b.fourcc.0);
        let rel_field_pos = entry_start + 4;
        let rel_offset = offsets[i] as i64 - rel_field_pos as i64;
        out[rel_field_pos..rel_field_pos + 4].copy_from_slice(&(rel_offset as i32).to_le_bytes());
        out[entry_start + 8..entry_start + 12]
            .copy_from_slice(&(b.data.len() as u32).to_le_bytes());
        if !b.data.is_empty() {
            out[offsets[i]..offsets[i] + b.data.len()].copy_from_slice(&b.data);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn multiple_blocks_indices_and_lookup() {
    let bytes = build_resource(
        12,
        1,
        &[
            TestBlock {
                fourcc: FourCC::RED2,
                data: vec![1, 2, 3],
            },
            TestBlock {
                fourcc: FourCC::DATA,
                data: ctrl_bytes(0),
            },
            TestBlock {
                fourcc: FourCC::PHYS,
                data: vec![9, 9, 9, 9],
            },
        ],
    );
    let res = Resource::parse(bytes).unwrap();
    assert_eq!(res.header_version(), 12);
    assert_eq!(res.version(), 1);
    assert_eq!(res.blocks().len(), 3);
    assert_eq!(res.blocks()[0].raw_index, 0);
    assert_eq!(res.blocks()[0].filtered_index, Some(0));
    assert_eq!(res.blocks()[2].fourcc, FourCC::PHYS);
    assert_eq!(res.block(FourCC::PHYS).unwrap().size, 4);
    assert_eq!(
        res.block_bytes(res.block(FourCC::PHYS).unwrap()),
        &[9, 9, 9, 9]
    );
    assert!(res.file_size_mismatch().is_none());
}

#[test]
fn zero_size_block_is_excluded_from_filtered_index() {
    let bytes = build_resource(
        12,
        1,
        &[
            TestBlock {
                fourcc: FourCC::CTRL,
                data: ctrl_bytes(1),
            },
            TestBlock {
                fourcc: FourCC::NTRO,
                data: vec![],
            },
            TestBlock {
                fourcc: FourCC::PHYS,
                data: vec![1, 2, 3, 4],
            },
        ],
    );
    let res = Resource::parse(bytes).unwrap();
    let blocks = res.blocks();
    assert_eq!(blocks[1].size, 0);
    assert_eq!(blocks[1].filtered_index, None);
    assert_eq!(blocks[2].filtered_index, Some(1));
    assert!(res.block(FourCC::NTRO).is_none()); // size 0 -> not returned by `block`

    let phys = res.embedded_phys().unwrap().expect("embedded phys");
    assert_eq!(phys.block.fourcc, FourCC::PHYS);
    assert_eq!(phys.index_scheme, IndexScheme::Filtered);
}

#[test]
fn embedded_phys_falls_back_to_raw_index() {
    // phys_data_block=2: filtered index 2 doesn't exist (only 2 filtered
    // entries, 0 and 1), but raw index 2 is the PHYS block.
    let bytes = build_resource(
        12,
        1,
        &[
            TestBlock {
                fourcc: FourCC::CTRL,
                data: ctrl_bytes(2),
            },
            TestBlock {
                fourcc: FourCC::NTRO,
                data: vec![],
            },
            TestBlock {
                fourcc: FourCC::PHYS,
                data: vec![5, 6, 7, 8],
            },
        ],
    );
    let res = Resource::parse(bytes).unwrap();
    let phys = res.embedded_phys().unwrap().expect("embedded phys");
    assert_eq!(phys.block.fourcc, FourCC::PHYS);
    assert_eq!(phys.index_scheme, IndexScheme::Raw);
}

#[test]
fn embedded_phys_no_ctrl_block() {
    let bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: vec![1],
        }],
    );
    let res = Resource::parse(bytes).unwrap();
    assert!(res.embedded_phys().unwrap().is_none());
}

#[test]
fn embedded_phys_no_key() {
    let bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::CTRL,
            data: build_kv3_v5(&TestNode::Object(vec![])),
        }],
    );
    let res = Resource::parse(bytes).unwrap();
    assert!(res.embedded_phys().unwrap().is_none());
}

#[test]
fn embedded_phys_bad_index_errors() {
    let bytes = build_resource(
        12,
        1,
        &[
            TestBlock {
                fourcc: FourCC::CTRL,
                data: ctrl_bytes(5),
            },
            TestBlock {
                fourcc: FourCC::DATA,
                data: vec![1, 2],
            },
        ],
    );
    let res = Resource::parse(bytes).unwrap();
    match res.embedded_phys() {
        Err(ResourceError::EmbeddedPhysIndex { index, .. }) => assert_eq!(index, 5),
        other => panic!("expected EmbeddedPhysIndex error, got {other:?}"),
    }
}

#[test]
fn data_kv3_round_trip() {
    let bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: ctrl_bytes(42),
        }],
    );
    let res = Resource::parse(bytes).unwrap();
    let doc = res.data_kv3().unwrap();
    let index = doc
        .root
        .get("embedded_physics")
        .and_then(|v| v.get("phys_data_block"))
        .and_then(|v| v.as_i64());
    assert_eq!(index, Some(42));
}

#[test]
fn kv3_on_non_kv3_block_errors() {
    let bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: vec![1, 2, 3, 4],
        }],
    );
    let res = Resource::parse(bytes).unwrap();
    let block = res.block(FourCC::DATA).unwrap();
    assert!(matches!(res.kv3(block), Err(ResourceError::NotKv3 { .. })));
}

#[test]
fn out_of_range_block_errors() {
    let mut bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: vec![1, 2, 3, 4],
        }],
    );
    // Corrupt the size field of the one block entry to run past the buffer.
    let size_pos = 16 + 8;
    bytes[size_pos..size_pos + 4].copy_from_slice(&1_000_000u32.to_le_bytes());
    assert!(matches!(
        Resource::parse(bytes),
        Err(ResourceError::BlockOutOfRange { .. })
    ));
}

#[test]
fn bad_header_version_errors() {
    let mut bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: vec![1],
        }],
    );
    bytes[4..6].copy_from_slice(&11u16.to_le_bytes());
    assert!(matches!(
        Resource::parse(bytes),
        Err(ResourceError::UnsupportedHeaderVersion { actual: 11 })
    ));
}

#[test]
fn vpk_magic_errors() {
    let mut bytes = vec![0u8; 16];
    bytes[0..4].copy_from_slice(&0x55AA_1234u32.to_le_bytes());
    assert!(matches!(Resource::parse(bytes), Err(ResourceError::IsVpk)));
}

#[test]
fn shader_magic_errors() {
    let mut bytes = vec![0u8; 16];
    bytes[0..4].copy_from_slice(b"vcs2");
    assert!(matches!(
        Resource::parse(bytes),
        Err(ResourceError::IsShader)
    ));
}

#[test]
fn file_size_mismatch_is_reported_not_an_error() {
    let mut bytes = build_resource(
        12,
        1,
        &[TestBlock {
            fourcc: FourCC::DATA,
            data: vec![1, 2, 3, 4],
        }],
    );
    let actual_len = bytes.len();
    bytes[0..4].copy_from_slice(&(actual_len as u32 + 100).to_le_bytes());
    let res = Resource::parse(bytes).unwrap();
    assert_eq!(
        res.file_size_mismatch(),
        Some((actual_len as u32 + 100, actual_len))
    );
}

#[test]
fn truncated_and_bit_flipped_inputs_never_panic() {
    let mut good = build_resource(
        12,
        1,
        &[
            // Zero-size: rel_offset patched below to a huge, unvalidated value
            // (`Resource::parse` never bounds-checks a size-0 entry's
            // offset). `block_bytes`/`kv3` on it must not panic.
            TestBlock {
                fourcc: FourCC::NTRO,
                data: vec![],
            },
            TestBlock {
                fourcc: FourCC::CTRL,
                data: ctrl_bytes(1),
            },
            TestBlock {
                fourcc: FourCC::PHYS,
                data: vec![1, 2, 3, 4],
            },
            TestBlock {
                fourcc: FourCC::DATA,
                data: ctrl_bytes(0),
            },
        ],
    );
    // The first entry's rel_offset field is at table_start(16) + 4.
    good[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    exercise(good.clone());

    for len in 0..good.len() {
        let truncated = good[..len].to_vec();
        exercise(truncated);
    }
    for bit in 0..(good.len() * 8) {
        let mut flipped = good.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        exercise(flipped);
    }
}

/// Parses `bytes` and, on success, exercises every read-only accessor. Never
/// expected to panic; errors are fine and ignored.
fn exercise(bytes: Vec<u8>) {
    if let Ok(res) = Resource::parse(bytes) {
        for block in res.blocks() {
            let _ = res.block_bytes(block);
            let _ = res.kv3(block);
        }
        let _ = res.data_kv3();
        let _ = res.embedded_phys();
        for i in 0..res.blocks().len() + 1 {
            let _ = res.block_by_raw_index(i);
            let _ = res.block_by_filtered_index(i);
        }
    }
}
