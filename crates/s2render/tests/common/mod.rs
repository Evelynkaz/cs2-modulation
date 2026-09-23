//! Shared helpers for building synthetic binary `VBIB` blocks and zstd frames, so the buffer
//! decode tests can exercise the real compression paths without needing an actual game install.
//! Not every test binary that includes this module uses every helper in it.
#![allow(dead_code)]

/// A single `OnDiskBufferData` entry to bake into a synthetic `VBIB` block.
pub struct BufferSpec {
    pub element_count: u32,
    /// The element size *before* the compression flag bits are OR'd in (`VBIB.cs:216-220`).
    pub element_size: u32,
    /// `true` => bit 26 clear (meshopt-compressed).
    pub meshopt: bool,
    /// `true` => bit 27 set (zstd-compressed).
    pub zstd: bool,
    /// `(semantic_name, semantic_index, raw DXGI_FORMAT, byte offset)`.
    pub fields: Vec<(String, i32, u32, u32)>,
    /// The bytes actually stored on disk for this buffer (already compressed, if any).
    pub stored: Vec<u8>,
}

fn write_entry(
    fixed: &mut [u8],
    payload: &mut Vec<u8>,
    payload_cursor: &mut usize,
    entry_start: usize,
    spec: &BufferSpec,
) {
    let ref_a = entry_start + 8;
    let ref_b = entry_start + 16;

    let attr_table_pos = *payload_cursor;
    for (name, semantic_index, format, offset) in &spec.fields {
        let mut name_bytes = [0u8; 32];
        let raw = name.as_bytes();
        let n = raw.len().min(31);
        name_bytes[..n].copy_from_slice(&raw[..n]);
        payload.extend_from_slice(&name_bytes);
        payload.extend_from_slice(&semantic_index.to_le_bytes());
        payload.extend_from_slice(&format.to_le_bytes());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&0i32.to_le_bytes()); // m_nSlot
        payload.extend_from_slice(&0u32.to_le_bytes()); // m_nSlotType
        payload.extend_from_slice(&0i32.to_le_bytes()); // m_nInstanceStepRate
    }
    *payload_cursor += spec.fields.len() * 56;

    let data_pos = *payload_cursor;
    payload.extend_from_slice(&spec.stored);
    *payload_cursor += spec.stored.len();

    let size_field: u32 = spec.element_size
        | if spec.meshopt { 0 } else { 0x0400_0000 }
        | if spec.zstd { 0x0800_0000 } else { 0 };

    fixed[entry_start..entry_start + 4].copy_from_slice(&spec.element_count.to_le_bytes());
    fixed[entry_start + 4..entry_start + 8].copy_from_slice(&size_field.to_le_bytes());
    fixed[entry_start + 8..entry_start + 12]
        .copy_from_slice(&((attr_table_pos - ref_a) as u32).to_le_bytes());
    fixed[entry_start + 12..entry_start + 16]
        .copy_from_slice(&(spec.fields.len() as u32).to_le_bytes());
    fixed[entry_start + 16..entry_start + 20]
        .copy_from_slice(&((data_pos - ref_b) as u32).to_le_bytes());
    fixed[entry_start + 20..entry_start + 24]
        .copy_from_slice(&(spec.stored.len() as i32).to_le_bytes());
}

/// Builds a binary `VBIB` block's bytes (`VBIB.cs:182-206`) from vertex/index buffer specs.
pub fn build_vbib(vertex: &[BufferSpec], index: &[BufferSpec]) -> Vec<u8> {
    let header_len = 16;
    let vb_table_len = vertex.len() * 24;
    let ib_table_len = index.len() * 24;
    let payload_start = header_len + vb_table_len + ib_table_len;

    let mut fixed = vec![0u8; payload_start];
    fixed[0..4].copy_from_slice(&(header_len as u32).to_le_bytes());
    fixed[4..8].copy_from_slice(&(vertex.len() as u32).to_le_bytes());
    let ib_target = header_len + vb_table_len;
    fixed[8..12].copy_from_slice(&((ib_target - 8) as u32).to_le_bytes());
    fixed[12..16].copy_from_slice(&(index.len() as u32).to_le_bytes());

    let mut payload = Vec::new();
    let mut payload_cursor = payload_start;

    for (i, spec) in vertex.iter().enumerate() {
        write_entry(
            &mut fixed,
            &mut payload,
            &mut payload_cursor,
            header_len + i * 24,
            spec,
        );
    }
    for (i, spec) in index.iter().enumerate() {
        write_entry(
            &mut fixed,
            &mut payload,
            &mut payload_cursor,
            header_len + vb_table_len + i * 24,
            spec,
        );
    }

    fixed.extend_from_slice(&payload);
    fixed
}

/// Compresses `data` into a zstd frame (mirrors `s2fmt::compress`'s own test helper).
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ruzstd::encoding::compress(
        std::io::Cursor::new(data),
        &mut out,
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    out
}

/// A resource container (`docs/FORMATS.md` section 2.1-2.2) with no blocks: enough to satisfy
/// [`s2render::buffer::parse_kv3_buffers`]'s `&Resource` parameter for the inline-`m_pData` path,
/// which never actually looks a block up.
pub fn empty_resource() -> s2fmt::resource::Resource {
    let mut bytes = vec![0u8; 16];
    bytes[4..6].copy_from_slice(&12u16.to_le_bytes()); // header version
    bytes[8..12].copy_from_slice(&8u32.to_le_bytes()); // block_offset, self-relative to byte 8
    bytes[0..4].copy_from_slice(&16u32.to_le_bytes()); // file size
    s2fmt::resource::Resource::parse(bytes).unwrap()
}

/// A resource container with exactly one block, holding `data` verbatim under `fourcc`.
pub fn resource_with_block(fourcc: [u8; 4], data: &[u8]) -> s2fmt::resource::Resource {
    let entry_pos = 16usize;
    let data_pos = entry_pos + 12;
    let total = data_pos + data.len();

    let mut bytes = vec![0u8; total];
    bytes[4..6].copy_from_slice(&12u16.to_le_bytes());
    bytes[8..12].copy_from_slice(&8u32.to_le_bytes()); // block_offset -> table at byte 16
    bytes[12..16].copy_from_slice(&1u32.to_le_bytes()); // block_count

    bytes[entry_pos..entry_pos + 4].copy_from_slice(&fourcc);
    let rel_offset_field_pos = entry_pos + 4;
    let rel_offset = (data_pos - rel_offset_field_pos) as u32;
    bytes[rel_offset_field_pos..rel_offset_field_pos + 4]
        .copy_from_slice(&rel_offset.to_le_bytes());
    bytes[entry_pos + 8..entry_pos + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());

    bytes[data_pos..data_pos + data.len()].copy_from_slice(data);
    bytes[0..4].copy_from_slice(&(total as u32).to_le_bytes());

    s2fmt::resource::Resource::parse(bytes).unwrap()
}
