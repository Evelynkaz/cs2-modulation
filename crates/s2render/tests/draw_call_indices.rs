//! `DrawCall::resolve_indices` on hostile `m_nIndexCount`/`m_nStartIndex`/`m_nBaseVertex` values
//! (`s6f3a1_mesh.md` change item 2): must error, not panic on `Vec::with_capacity`, abort on a
//! huge allocation, or silently wrap a negative resolved index into a huge `u32`.

use s2render::buffer::{Buffer, Compression};
use s2render::mesh::{DrawCall, DrawCallFlags, Mesh, SceneObject};

fn index_buffer(indices: &[u32], element_size: u32) -> Buffer {
    let data: Vec<u8> = match element_size {
        2 => indices
            .iter()
            .flat_map(|&i| (i as u16).to_le_bytes())
            .collect(),
        4 => indices.iter().flat_map(|&i| i.to_le_bytes()).collect(),
        other => panic!("unsupported element_size {other}"),
    };
    Buffer {
        element_count: indices.len() as u32,
        element_size,
        fields: vec![],
        data,
        compression: Compression::default(),
    }
}

fn vertex_buffer(element_count: u32) -> Buffer {
    Buffer {
        element_count,
        element_size: 12,
        fields: vec![],
        data: vec![0u8; element_count as usize * 12],
        compression: Compression::default(),
    }
}

fn draw_call(start_index: i64, index_count: i64, base_vertex: i64) -> DrawCall {
    DrawCall {
        material_path: None,
        is_triangle_list: true,
        base_vertex,
        start_index,
        index_count,
        vertex_count: 0,
        index_buffer: 0,
        vertex_buffers: vec![0],
        tint_color: None,
        alpha: None,
        flags: DrawCallFlags::None,
    }
}

#[test]
fn index_count_i64_max_errors_not_panics() {
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(3)],
        index_buffers: vec![index_buffer(&[0, 1, 2], 2)],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, i64::MAX, 0);
    assert!(
        dc.resolve_indices(&mesh).is_err(),
        "a hostile m_nIndexCount must error, not panic with 'capacity overflow'"
    );
}

#[test]
fn index_count_huge_but_in_range_i64_errors_not_panics() {
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(3)],
        index_buffers: vec![index_buffer(&[0, 1, 2], 2)],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, 1i64 << 40, 0);
    assert!(
        dc.resolve_indices(&mesh).is_err(),
        "a hostile m_nIndexCount must error, not attempt a multi-terabyte allocation"
    );
}

#[test]
fn base_vertex_negative_one_errors_not_panics() {
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(1)],
        index_buffers: vec![index_buffer(&[0], 2)],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, 1, -1);
    let err = dc
        .resolve_indices(&mesh)
        .expect_err("index 0 + base_vertex -1 must not silently wrap to u32::MAX - 0");
    assert!(!format!("{err}").is_empty());
}

#[test]
fn resolved_index_beyond_vertex_buffer_errors_not_panics() {
    // index buffer has one element (value 5), but the vertex buffer it's drawn against only has
    // 3 vertices -- a caller indexing attribute arrays with 5 would panic in `FloatAttribute::get`.
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(3)],
        index_buffers: vec![index_buffer(&[5], 2)],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, 1, 0);
    assert!(dc.resolve_indices(&mesh).is_err());
}

#[test]
fn index_count_u32_max_with_zero_element_size_errors_not_aborts() {
    // A crafted VBIB index buffer: element_count u32::MAX, element_size 0 (not 2 or 4), no data
    // -- `buf.element_count` alone is not tied to real bytes, so a bound derived from it would let
    // a `m_nIndexCount` of u32::MAX through to `Vec::with_capacity`.
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(3)],
        index_buffers: vec![Buffer {
            element_count: u32::MAX,
            element_size: 0,
            fields: vec![],
            data: vec![],
            compression: Compression::default(),
        }],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, u32::MAX as i64, 0);
    assert!(
        dc.resolve_indices(&mesh).is_err(),
        "a zero index element size with a huge m_nIndexCount must error, not allocate ~17GB"
    );
}

#[test]
fn well_formed_draw_call_resolves() {
    let mesh = Mesh {
        vertex_buffers: vec![vertex_buffer(4)],
        index_buffers: vec![index_buffer(&[0, 1, 2, 2, 1, 3], 2)],
        scene_objects: vec![SceneObject { draw_calls: vec![] }],
    };
    let dc = draw_call(0, 6, 0);
    assert_eq!(dc.resolve_indices(&mesh).unwrap(), vec![0, 1, 2, 2, 1, 3]);
}
