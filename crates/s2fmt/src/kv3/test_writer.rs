//! Test-only binary KV3 writer, used to build synthetic documents for round-trip tests against
//! [`super::binary::parse_binary`]. Mirrors the lane layout of
//! `BinaryKV3.Serialization.cs` (v4/v5) and extends it to v1-v3 and legacy v0, per
//! `docs/FORMATS.md` sections 3.5-3.11.
//!
//! Note (spec `kv3.md`): a writer sharing a misunderstanding with the reader would pass
//! round-trips while both are wrong. The hand-assembled v5 fixture in `binary.rs`'s tests pins
//! the byte layout independently of this writer; the `kv3_real_files` tests check against real
//! Valve data.

use crate::kv3::Guid;
use crate::kv3::guid::{ENCODING_BINARY, ENCODING_BINARY_LZ4};
use crate::kv3::value::{Flag, Value};
use std::collections::HashMap;

/// The compression method to encode a synthetic document with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    None = 0,
    Lz4 = 1,
    Zstd = 2,
}

/// A richer node tree than [`Value`], used only to steer the test writer into emitting specific
/// binary node types (e.g. `INT16` vs `INT64`) that all collapse to the same [`Value`] on read.
#[derive(Debug, Clone)]
pub(crate) enum TestNode {
    Null,
    Bool(bool),
    Int64(i64),
    UInt64(u64),
    Int32(i32),
    UInt32(u32),
    Int16(i16),
    UInt16(u16),
    Int32AsByte(u8),
    Float(f32),
    Double(f64),
    Str(String),
    Blob(Vec<u8>),
    Array(Vec<TestNode>),
    /// Node type 10.
    ArrayTyped(Vec<TestNode>),
    /// Node type 24 (array length as a single byte).
    ArrayByteLen(Vec<TestNode>),
    /// Node type 25 (v5 only): values live in the other buffer set.
    ArrayAux(Vec<TestNode>),
    Object(Vec<(String, TestNode)>),
    Flagged(Flag, Box<TestNode>),
}

/// The [`Value`] that [`super::binary::parse_binary`] should produce for a given [`TestNode`].
pub(crate) fn expected_value(node: &TestNode) -> Value {
    match node {
        TestNode::Flagged(flag, inner) => Value::with_flag(*flag, expected_value(inner)),
        TestNode::Null => Value::Null,
        TestNode::Bool(b) => Value::Bool(*b),
        TestNode::Int64(i) => Value::Int(*i),
        TestNode::UInt64(u) => Value::UInt(*u),
        TestNode::Int32(i) => Value::Int(*i as i64),
        TestNode::UInt32(u) => Value::UInt(*u as u64),
        TestNode::Int16(i) => Value::Int(*i as i64),
        TestNode::UInt16(u) => Value::UInt(*u as u64),
        TestNode::Int32AsByte(b) => Value::Int(*b as i64),
        TestNode::Float(f) => Value::Double(*f as f64),
        TestNode::Double(d) => Value::Double(*d),
        TestNode::Str(s) => Value::String(s.clone()),
        TestNode::Blob(b) => Value::Blob(b.clone()),
        TestNode::Array(items)
        | TestNode::ArrayTyped(items)
        | TestNode::ArrayByteLen(items)
        | TestNode::ArrayAux(items) => Value::Array(items.iter().map(expected_value).collect()),
        TestNode::Object(entries) => {
            let mut obj = crate::kv3::Object::with_capacity(entries.len());
            for (k, v) in entries {
                obj.push(k.clone(), expected_value(v));
            }
            Value::Object(obj)
        }
    }
}

fn node_flag(node: &TestNode) -> Flag {
    match node {
        TestNode::Flagged(f, _) => *f,
        _ => Flag::None,
    }
}

fn unwrap_node(node: &TestNode) -> &TestNode {
    match node {
        TestNode::Flagged(_, inner) => unwrap_node(inner),
        other => other,
    }
}

fn type_byte(node: &TestNode) -> u8 {
    match node {
        TestNode::Flagged(_, inner) => type_byte(inner),
        TestNode::Null => 1,
        TestNode::Bool(b) => {
            if *b {
                13
            } else {
                14
            }
        }
        TestNode::Int64(0) => 15,
        TestNode::Int64(1) => 16,
        TestNode::Int64(_) => 3,
        TestNode::UInt64(_) => 4,
        TestNode::Double(d) if *d == 0.0 && d.is_sign_positive() => 17,
        TestNode::Double(d) if *d == 1.0 => 18,
        TestNode::Double(_) => 5,
        TestNode::Int32(_) => 11,
        TestNode::UInt32(_) => 12,
        TestNode::Float(_) => 19,
        TestNode::Int16(_) => 20,
        TestNode::UInt16(_) => 21,
        TestNode::Int32AsByte(_) => 23,
        TestNode::Str(_) => 6,
        TestNode::Blob(_) => 7,
        TestNode::Array(_) => 8,
        TestNode::ArrayTyped(_) => 10,
        TestNode::ArrayByteLen(_) => 24,
        TestNode::ArrayAux(_) => 25,
        TestNode::Object(_) => 9,
    }
}

/// Like [`type_byte`], but never picks the `INT64_ZERO`/`INT64_ONE`/`DOUBLE_ZERO`/`DOUBLE_ONE`
/// shortcuts. Typed arrays (10/24/25) share a single type byte for every element, so if the
/// first element happened to be 0 or 1 the shortcut would silently corrupt every other element
/// (whose payload is written in full but never read back, since the shared type carries no
/// payload at all).
fn type_byte_forced(node: &TestNode) -> u8 {
    match node {
        TestNode::Flagged(_, inner) => type_byte_forced(inner),
        TestNode::Int64(_) => 3,
        TestNode::Double(_) => 5,
        other => type_byte(other),
    }
}

fn bitmask_flag(flag: Flag) -> u8 {
    match flag {
        Flag::None => 0,
        Flag::Resource => 1,
        Flag::ResourceName => 2,
        Flag::Panorama => 8,
        Flag::SoundEvent => 16,
        Flag::SubClass => 32,
        Flag::EntityName => panic!("EntityName flag requires binary KV3 version >= 3"),
    }
}

fn enum_flag(flag: Flag) -> u8 {
    match flag {
        Flag::None => 0,
        Flag::Resource => 1,
        Flag::ResourceName => 2,
        Flag::Panorama => 3,
        Flag::SoundEvent => 4,
        Flag::SubClass => 5,
        Flag::EntityName => 6,
    }
}

fn align_to(buf: &mut Vec<u8>, n: usize) {
    while buf.len() % n != 0 {
        buf.push(0);
    }
}

fn zstd_compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ruzstd::encoding::compress(
        std::io::Cursor::new(input),
        &mut out,
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    out
}

/// Chains raw LZ4 blocks across the blob buffer, restarting the 16384-byte frame boundary at
/// each individual blob (mirroring VRF's writer, `BinaryKV3.Serialization.cs` `CompressLz4BinaryBlobs`
/// ~336-350: `for (var segmentOffset = 0; segmentOffset < segmentLength; segmentOffset +=
/// CompressionFrameSize)` iterating per blob segment), while the LZ4 dictionary still carries
/// over continuously across the whole blob stream (a real encoder's internal state persists
/// across segments; here that's emulated by always keying off the trailing 64 KiB of plaintext
/// produced so far, regardless of blob boundaries). This means the last frame of a blob whose
/// length isn't a multiple of 16384 is short, which is exactly the "frame ends early at a blob
/// boundary" case real compiled files rely on (see `binary.rs`'s `Lz4ChainDecoder::decode_block`
/// doc comment) -- generating it here gives that path unit-test coverage instead of relying
/// solely on real fixtures to exercise it.
fn compress_lz4_chain(blobs: &[u8], blob_lengths: &[i32]) -> (Vec<u8>, Vec<u16>) {
    let mut compressed = Vec::new();
    let mut sizes = Vec::new();
    let mut input_offset = 0usize;
    for &len in blob_lengths {
        let segment_len = usize::try_from(len).unwrap_or(0);
        let mut segment_offset = 0usize;
        while segment_offset < segment_len {
            let chunk_len = 16384.min(segment_len - segment_offset);
            let abs_offset = input_offset + segment_offset;
            let chunk = &blobs[abs_offset..abs_offset + chunk_len];
            let dict_start = abs_offset.saturating_sub(64 * 1024);
            let dict = &blobs[dict_start..abs_offset];
            let c = lz4_flex::block::compress_with_dict(chunk, dict);
            assert!(c.len() <= u16::MAX as usize, "lz4 chain frame too large");
            sizes.push(c.len() as u16);
            compressed.extend_from_slice(&c);
            segment_offset += chunk_len;
        }
        input_offset += segment_len;
    }
    (compressed, sizes)
}

fn compress_buffer(buf: &[u8], compression: Compression) -> (Vec<u8>, usize) {
    match compression {
        Compression::None => (buf.to_vec(), 0),
        Compression::Lz4 => {
            let c = lz4_flex::block::compress(buf);
            let len = c.len();
            (c, len)
        }
        Compression::Zstd => {
            let c = zstd_compress(buf);
            let len = c.len();
            (c, len)
        }
    }
}

struct Ctx {
    version: u8,
    strings: Vec<String>,
    string_ids: HashMap<String, i32>,
    bytes1: Vec<u8>,
    bytes2: Vec<u8>,
    bytes4: Vec<u8>,
    bytes8: Vec<u8>,
    aux1: Vec<u8>,
    aux2: Vec<u8>,
    aux4: Vec<u8>,
    aux8: Vec<u8>,
    swapped: bool,
    types: Vec<u8>,
    object_lengths: Vec<u8>,
    blobs: Vec<u8>,
    blob_lengths: Vec<i32>,
}

impl Ctx {
    fn new(version: u8) -> Self {
        Ctx {
            version,
            strings: Vec::new(),
            string_ids: HashMap::new(),
            bytes1: Vec::new(),
            bytes2: Vec::new(),
            bytes4: Vec::new(),
            bytes8: Vec::new(),
            aux1: Vec::new(),
            aux2: Vec::new(),
            aux4: Vec::new(),
            aux8: Vec::new(),
            swapped: false,
            types: Vec::new(),
            object_lengths: Vec::new(),
            blobs: Vec::new(),
            blob_lengths: Vec::new(),
        }
    }

    fn b1(&mut self) -> &mut Vec<u8> {
        if self.swapped {
            &mut self.aux1
        } else {
            &mut self.bytes1
        }
    }
    fn b2(&mut self) -> &mut Vec<u8> {
        if self.swapped {
            &mut self.aux2
        } else {
            &mut self.bytes2
        }
    }
    fn b4(&mut self) -> &mut Vec<u8> {
        if self.swapped {
            &mut self.aux4
        } else {
            &mut self.bytes4
        }
    }
    fn b8(&mut self) -> &mut Vec<u8> {
        if self.swapped {
            &mut self.aux8
        } else {
            &mut self.bytes8
        }
    }

    fn get_string_id(&mut self, s: &str) -> i32 {
        if s.is_empty() {
            return -1;
        }
        if let Some(&id) = self.string_ids.get(s) {
            return id;
        }
        let id = self.strings.len() as i32;
        self.strings.push(s.to_string());
        self.string_ids.insert(s.to_string(), id);
        id
    }

    fn write_type(&mut self, node_type: u8, flag: Flag) {
        if flag == Flag::None {
            self.types.push(node_type);
            return;
        }
        if self.version >= 3 {
            self.types.push(node_type | 0x80);
            self.types.push(enum_flag(flag));
        } else {
            self.types.push(node_type | 0x80);
            self.types.push(bitmask_flag(flag));
        }
    }

    fn write_value(&mut self, node: &TestNode) {
        let flag = node_flag(node);
        let inner = unwrap_node(node);
        self.write_type(type_byte(inner), flag);
        self.write_payload(inner);
    }

    fn write_payload(&mut self, node: &TestNode) {
        match node {
            TestNode::Flagged(_, inner) => self.write_payload(inner),
            TestNode::Null | TestNode::Bool(_) => {}
            TestNode::Int64(0) | TestNode::Int64(1) => {}
            TestNode::Int64(i) => {
                let b = i.to_le_bytes();
                self.b8().extend_from_slice(&b);
            }
            TestNode::UInt64(u) => {
                let b = u.to_le_bytes();
                self.b8().extend_from_slice(&b);
            }
            TestNode::Double(d) if *d == 0.0 && d.is_sign_positive() => {}
            TestNode::Double(d) if *d == 1.0 => {}
            TestNode::Double(d) => {
                let b = d.to_le_bytes();
                self.b8().extend_from_slice(&b);
            }
            TestNode::Int32(i) => {
                let b = i.to_le_bytes();
                self.b4().extend_from_slice(&b);
            }
            TestNode::UInt32(u) => {
                let b = u.to_le_bytes();
                self.b4().extend_from_slice(&b);
            }
            TestNode::Float(f) => {
                let b = f.to_le_bytes();
                self.b4().extend_from_slice(&b);
            }
            TestNode::Int16(i) => {
                let b = i.to_le_bytes();
                self.b2().extend_from_slice(&b);
            }
            TestNode::UInt16(u) => {
                let b = u.to_le_bytes();
                self.b2().extend_from_slice(&b);
            }
            TestNode::Int32AsByte(b) => {
                self.b1().push(*b);
            }
            TestNode::Str(s) => {
                let id = self.get_string_id(s);
                self.b4().extend_from_slice(&id.to_le_bytes());
            }
            TestNode::Blob(bytes) => {
                if self.version < 2 {
                    let len = bytes.len() as i32;
                    self.b4().extend_from_slice(&len.to_le_bytes());
                    if !bytes.is_empty() {
                        self.b1().extend_from_slice(bytes);
                    }
                } else {
                    self.blob_lengths.push(bytes.len() as i32);
                    self.blobs.extend_from_slice(bytes);
                }
            }
            TestNode::Array(items) => {
                let n = items.len() as i32;
                self.b4().extend_from_slice(&n.to_le_bytes());
                for item in items {
                    self.write_value(item);
                }
            }
            TestNode::ArrayTyped(items) => self.write_typed_items(items, false),
            TestNode::ArrayByteLen(items) => self.write_typed_items(items, true),
            TestNode::ArrayAux(items) => self.write_aux_items(items),
            TestNode::Object(entries) => {
                let n = entries.len() as i32;
                if self.version >= 5 {
                    self.object_lengths.extend_from_slice(&n.to_le_bytes());
                } else {
                    self.b4().extend_from_slice(&n.to_le_bytes());
                }
                for (key, val) in entries {
                    let flag = node_flag(val);
                    let inner = unwrap_node(val);
                    // (v1+) member type precedes the key id, which precedes the value payload.
                    self.write_type(type_byte(inner), flag);
                    let key_id = self.get_string_id(key);
                    self.b4().extend_from_slice(&key_id.to_le_bytes());
                    self.write_payload(inner);
                }
            }
        }
    }

    /// Writes the payload matching [`type_byte_forced`]: always the full 8 bytes for `Int64`/
    /// `Double`, regardless of whether the value happens to be 0 or 1 (see its doc comment).
    fn write_payload_forced(&mut self, node: &TestNode) {
        match node {
            TestNode::Flagged(_, inner) => self.write_payload_forced(inner),
            TestNode::Int64(i) => {
                let b = i.to_le_bytes();
                self.b8().extend_from_slice(&b);
            }
            TestNode::Double(d) => {
                let b = d.to_le_bytes();
                self.b8().extend_from_slice(&b);
            }
            other => self.write_payload(other),
        }
    }

    fn write_typed_items(&mut self, items: &[TestNode], byte_len: bool) {
        if byte_len {
            self.b1().push(items.len() as u8);
        } else {
            self.b4()
                .extend_from_slice(&(items.len() as i32).to_le_bytes());
        }
        let (sub_flag, sub_type) = match items.first() {
            Some(first) => (node_flag(first), type_byte_forced(unwrap_node(first))),
            None => (Flag::None, 1), // NULL; arbitrary sub-type for an empty typed array.
        };
        self.write_type(sub_type, sub_flag);
        for item in items {
            self.write_payload_forced(unwrap_node(item));
        }
    }

    fn write_aux_items(&mut self, items: &[TestNode]) {
        assert!(self.version >= 5, "ARRAY_TYPE_AUXILIARY_BUFFER requires v5");
        self.b1().push(items.len() as u8);
        let (sub_flag, sub_type) = match items.first() {
            Some(first) => (node_flag(first), type_byte_forced(unwrap_node(first))),
            None => (Flag::None, 1),
        };
        self.write_type(sub_type, sub_flag);
        self.swapped = !self.swapped;
        for item in items {
            self.write_payload_forced(unwrap_node(item));
        }
        self.swapped = !self.swapped;
    }
}

/// Builds a binary KV3 v1-v5 document for `node`.
pub(crate) fn build(
    node: &TestNode,
    version: u8,
    compression: Compression,
    format: Guid,
) -> Vec<u8> {
    assert!((1..=5).contains(&version));
    let mut ctx = Ctx::new(version);
    ctx.write_value(node);

    if version >= 5 {
        build_v5(&mut ctx, compression, format)
    } else {
        build_v1_4(&mut ctx, version, compression, format)
    }
}

fn build_v1_4(ctx: &mut Ctx, version: u8, compression: Compression, format: Guid) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&ctx.bytes1);
    if version >= 4 && !ctx.bytes2.is_empty() {
        // The reader's `take_lane` only aligns when the lane is non-empty (count > 0); an
        // unconditional align here would insert padding the reader never skips over.
        align_to(&mut buf, 2);
        buf.extend_from_slice(&ctx.bytes2);
    }
    align_to(&mut buf, 4);
    buf.extend_from_slice(&(ctx.strings.len() as i32).to_le_bytes());
    buf.extend_from_slice(&ctx.bytes4);
    // FORMATS.md 3.8: v<5 aligns to 8 even when Bytes8 is empty.
    align_to(&mut buf, 8);
    buf.extend_from_slice(&ctx.bytes8);

    let strings_start = buf.len();
    for s in &ctx.strings {
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }
    buf.extend_from_slice(&ctx.types);
    let count_types = (buf.len() - strings_start) as i32;

    let count_blocks = ctx.blob_lengths.len();
    if count_blocks == 0 {
        buf.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    } else {
        for len in &ctx.blob_lengths {
            buf.extend_from_slice(&len.to_le_bytes());
        }
        buf.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    }

    let mut block_compressed_sizes: Vec<u16> = Vec::new();
    let compressed_blobs: Vec<u8> = if count_blocks == 0 {
        Vec::new()
    } else {
        match compression {
            Compression::None => ctx.blobs.clone(),
            Compression::Lz4 => {
                let (c, sizes) = compress_lz4_chain(&ctx.blobs, &ctx.blob_lengths);
                block_compressed_sizes = sizes;
                c
            }
            Compression::Zstd => Vec::new(), // handled below: blobs ride along with `buf`.
        }
    };
    if count_blocks > 0 && matches!(compression, Compression::Lz4) {
        for size in &block_compressed_sizes {
            buf.extend_from_slice(&size.to_le_bytes());
        }
    }

    let size_uncompressed_total = buf.len();
    let (compressed_buffer, size_compressed_total) = match compression {
        Compression::Zstd if count_blocks > 0 => {
            // v<5 zstd: buffer1 and blobs are compressed together as one frame.
            let mut combined = buf.clone();
            combined.extend_from_slice(&ctx.blobs);
            let c = zstd_compress(&combined);
            let len = c.len();
            (c, len)
        }
        _ => compress_buffer(&buf, compression),
    };

    let mut out = Vec::new();
    let magic = 0x4B56_3300u32 | version as u32;
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&format.0);
    out.extend_from_slice(&(compression as u32).to_le_bytes());

    if version == 1 {
        out.extend_from_slice(&(ctx.bytes1.len() as i32).to_le_bytes());
        out.extend_from_slice(&(ctx.bytes4.len() as i32 / 4 + 1).to_le_bytes());
        out.extend_from_slice(&(ctx.bytes8.len() as i32 / 8).to_le_bytes());
        out.extend_from_slice(&(size_uncompressed_total as i32).to_le_bytes());
    } else {
        out.extend_from_slice(&0u16.to_le_bytes());
        let frame_size: u16 = if matches!(compression, Compression::Lz4) {
            16384
        } else {
            0
        };
        out.extend_from_slice(&frame_size.to_le_bytes());
        out.extend_from_slice(&(ctx.bytes1.len() as i32).to_le_bytes());
        out.extend_from_slice(&(ctx.bytes4.len() as i32 / 4 + 1).to_le_bytes());
        out.extend_from_slice(&(ctx.bytes8.len() as i32 / 8).to_le_bytes());
        out.extend_from_slice(&count_types.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(size_uncompressed_total as i32).to_le_bytes());
        out.extend_from_slice(&(size_compressed_total as i32).to_le_bytes());
        out.extend_from_slice(&(count_blocks as i32).to_le_bytes());
        out.extend_from_slice(&(ctx.blobs.len() as i32).to_le_bytes());
        if version >= 4 {
            out.extend_from_slice(&(ctx.bytes2.len() as i32 / 2).to_le_bytes());
            out.extend_from_slice(&((block_compressed_sizes.len() * 2) as i32).to_le_bytes());
        }
    }

    out.extend_from_slice(&compressed_buffer);
    if count_blocks > 0 {
        if !matches!(compression, Compression::Zstd) {
            out.extend_from_slice(&compressed_blobs);
        }
        out.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    }
    out
}

fn build_v5(ctx: &mut Ctx, compression: Compression, format: Guid) -> Vec<u8> {
    let string_bytes_total: usize = ctx.strings.iter().map(|s| s.len() + 1).sum();

    let mut buffer1 = Vec::new();
    for s in &ctx.strings {
        buffer1.extend_from_slice(s.as_bytes());
        buffer1.push(0);
    }
    buffer1.extend_from_slice(&ctx.aux1);
    if !ctx.aux2.is_empty() {
        align_to(&mut buffer1, 2);
        buffer1.extend_from_slice(&ctx.aux2);
    }
    align_to(&mut buffer1, 4);
    buffer1.extend_from_slice(&(ctx.strings.len() as i32).to_le_bytes());
    buffer1.extend_from_slice(&ctx.aux4);
    if !ctx.aux8.is_empty() {
        align_to(&mut buffer1, 8);
        buffer1.extend_from_slice(&ctx.aux8);
    }

    // Buffer 2 has no "align even when empty" quirk (that only applies to buffer1/v<5's single
    // buffer): every lane here is skipped, alignment included, when its count is zero.
    let mut buffer2 = Vec::new();
    buffer2.extend_from_slice(&ctx.object_lengths);
    buffer2.extend_from_slice(&ctx.bytes1);
    if !ctx.bytes2.is_empty() {
        align_to(&mut buffer2, 2);
        buffer2.extend_from_slice(&ctx.bytes2);
    }
    if !ctx.bytes4.is_empty() {
        align_to(&mut buffer2, 4);
        buffer2.extend_from_slice(&ctx.bytes4);
    }
    if !ctx.bytes8.is_empty() {
        align_to(&mut buffer2, 8);
        buffer2.extend_from_slice(&ctx.bytes8);
    }
    buffer2.extend_from_slice(&ctx.types);

    let count_blocks = ctx.blob_lengths.len();
    if count_blocks == 0 {
        buffer2.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    } else {
        for len in &ctx.blob_lengths {
            buffer2.extend_from_slice(&len.to_le_bytes());
        }
        buffer2.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    }

    let mut block_compressed_sizes: Vec<u16> = Vec::new();
    let compressed_blobs: Vec<u8> = if count_blocks == 0 {
        Vec::new()
    } else {
        match compression {
            Compression::None => ctx.blobs.clone(),
            Compression::Lz4 => {
                let (c, sizes) = compress_lz4_chain(&ctx.blobs, &ctx.blob_lengths);
                block_compressed_sizes = sizes;
                c
            }
            Compression::Zstd => zstd_compress(&ctx.blobs),
        }
    };
    if count_blocks > 0 && matches!(compression, Compression::Lz4) {
        for size in &block_compressed_sizes {
            buffer2.extend_from_slice(&size.to_le_bytes());
        }
    }

    let (compressed_buffer1, size_compressed_buffer1) = compress_buffer(&buffer1, compression);
    let (compressed_buffer2, size_compressed_buffer2) = compress_buffer(&buffer2, compression);

    let size_uncompressed_buffer1 = buffer1.len();
    let size_uncompressed_buffer2 = buffer2.len();
    let size_uncompressed_total = size_uncompressed_buffer1 + size_uncompressed_buffer2;
    let size_compressed_total =
        compressed_buffer1.len() + compressed_buffer2.len() + compressed_blobs.len();

    let count_bytes1_aux = string_bytes_total + ctx.aux1.len();
    let count_bytes2_aux = ctx.aux2.len() / 2;
    let count_bytes4_aux = 1 + ctx.aux4.len() / 4;
    let count_bytes8_aux = ctx.aux8.len() / 8;

    let count_bytes1_b2 = ctx.bytes1.len();
    let count_bytes2_b2 = ctx.bytes2.len() / 2;
    let count_bytes4_b2 = ctx.bytes4.len() / 4;
    let count_bytes8_b2 = ctx.bytes8.len() / 8;
    let count_objects_b2 = ctx.object_lengths.len() / 4;

    let mut out = Vec::new();
    out.extend_from_slice(&0x4B56_3305u32.to_le_bytes());
    out.extend_from_slice(&format.0);
    out.extend_from_slice(&(compression as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    let frame_size: u16 = if matches!(compression, Compression::Lz4) {
        16384
    } else {
        0
    };
    out.extend_from_slice(&frame_size.to_le_bytes());
    out.extend_from_slice(&(count_bytes1_aux as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes4_aux as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes8_aux as i32).to_le_bytes());
    out.extend_from_slice(&(ctx.types.len() as i32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(size_uncompressed_total as i32).to_le_bytes());
    out.extend_from_slice(&(size_compressed_total as i32).to_le_bytes());
    out.extend_from_slice(&(count_blocks as i32).to_le_bytes());
    out.extend_from_slice(&(ctx.blobs.len() as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes2_aux as i32).to_le_bytes());
    out.extend_from_slice(&((block_compressed_sizes.len() * 2) as i32).to_le_bytes());
    out.extend_from_slice(&(size_uncompressed_buffer1 as i32).to_le_bytes());
    out.extend_from_slice(&(size_compressed_buffer1 as i32).to_le_bytes());
    out.extend_from_slice(&(size_uncompressed_buffer2 as i32).to_le_bytes());
    out.extend_from_slice(&(size_compressed_buffer2 as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes1_b2 as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes2_b2 as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes4_b2 as i32).to_le_bytes());
    out.extend_from_slice(&(count_bytes8_b2 as i32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&(count_objects_b2 as i32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());

    out.extend_from_slice(&compressed_buffer1);
    out.extend_from_slice(&compressed_buffer2);
    out.extend_from_slice(&compressed_blobs);
    if count_blocks > 0 {
        out.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes());
    }
    out
}

/// v0 (legacy) encodings that this test writer can produce. `binary_bc` is not generated: there
/// is no BlockCompress encoder in `compress.rs` (only the decoder, per spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum V0Encoding {
    Binary,
    Lz4,
}

fn write_v0_type(out: &mut Vec<u8>, node_type: u8, flag: Flag) {
    if flag == Flag::None {
        out.push(node_type);
    } else {
        out.push(node_type | 0x80);
        out.push(bitmask_flag(flag));
    }
}

fn get_or_insert(strings: &mut Vec<String>, ids: &mut HashMap<String, i32>, s: &str) -> i32 {
    if s.is_empty() {
        return -1;
    }
    if let Some(&id) = ids.get(s) {
        return id;
    }
    let id = strings.len() as i32;
    strings.push(s.to_string());
    ids.insert(s.to_string(), id);
    id
}

fn write_v0_value(
    out: &mut Vec<u8>,
    node: &TestNode,
    strings: &mut Vec<String>,
    ids: &mut HashMap<String, i32>,
) {
    let flag = node_flag(node);
    let inner = unwrap_node(node);
    write_v0_type(out, type_byte(inner), flag);
    write_v0_payload(out, inner, strings, ids);
}

fn write_v0_payload_forced(
    out: &mut Vec<u8>,
    node: &TestNode,
    strings: &mut Vec<String>,
    ids: &mut HashMap<String, i32>,
) {
    match node {
        TestNode::Flagged(_, inner) => write_v0_payload_forced(out, inner, strings, ids),
        TestNode::Int64(i) => out.extend_from_slice(&i.to_le_bytes()),
        TestNode::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        other => write_v0_payload(out, other, strings, ids),
    }
}

fn write_v0_payload(
    out: &mut Vec<u8>,
    node: &TestNode,
    strings: &mut Vec<String>,
    ids: &mut HashMap<String, i32>,
) {
    match node {
        TestNode::Flagged(_, inner) => write_v0_payload(out, inner, strings, ids),
        TestNode::Null | TestNode::Bool(_) => {}
        TestNode::Int64(0) | TestNode::Int64(1) => {}
        TestNode::Int64(i) => out.extend_from_slice(&i.to_le_bytes()),
        TestNode::UInt64(u) => out.extend_from_slice(&u.to_le_bytes()),
        TestNode::Double(d) if *d == 0.0 && d.is_sign_positive() => {}
        TestNode::Double(d) if *d == 1.0 => {}
        TestNode::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        TestNode::Int32(i) => out.extend_from_slice(&i.to_le_bytes()),
        TestNode::UInt32(u) => out.extend_from_slice(&u.to_le_bytes()),
        TestNode::Str(s) => {
            let id = get_or_insert(strings, ids, s);
            out.extend_from_slice(&id.to_le_bytes());
        }
        TestNode::Blob(bytes) => {
            out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        TestNode::Array(items) => {
            out.extend_from_slice(&(items.len() as i32).to_le_bytes());
            for item in items {
                write_v0_value(out, item, strings, ids);
            }
        }
        TestNode::ArrayTyped(items) => {
            out.extend_from_slice(&(items.len() as i32).to_le_bytes());
            let (sf, st) = match items.first() {
                Some(f) => (node_flag(f), type_byte_forced(unwrap_node(f))),
                None => (Flag::None, 1),
            };
            write_v0_type(out, st, sf);
            for item in items {
                write_v0_payload_forced(out, unwrap_node(item), strings, ids);
            }
        }
        TestNode::Object(entries) => {
            out.extend_from_slice(&(entries.len() as i32).to_le_bytes());
            for (key, val) in entries {
                // (!) FORMATS.md 3.5: in v0, the member's key id precedes its type byte.
                let id = get_or_insert(strings, ids, key);
                out.extend_from_slice(&id.to_le_bytes());
                write_v0_value(out, val, strings, ids);
            }
        }
        other => panic!("node kind {other:?} is not representable in binary KV3 v0"),
    }
}

/// Builds a legacy (v0) binary KV3 document for `node`.
pub(crate) fn build_v0(node: &TestNode, encoding: V0Encoding, format: Guid) -> Vec<u8> {
    let mut strings = Vec::new();
    let mut ids = HashMap::new();
    let mut body = Vec::new();
    write_v0_value(&mut body, node, &mut strings, &mut ids);

    let mut inner = Vec::new();
    inner.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    for s in &strings {
        inner.extend_from_slice(s.as_bytes());
        inner.push(0);
    }
    inner.extend_from_slice(&body);
    inner.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

    let mut file = Vec::new();
    file.extend_from_slice(&0x0356_4B56u32.to_le_bytes());
    let encoding_guid: Guid = match encoding {
        V0Encoding::Binary => ENCODING_BINARY,
        V0Encoding::Lz4 => ENCODING_BINARY_LZ4,
    };
    file.extend_from_slice(&encoding_guid.0);
    file.extend_from_slice(&format.0);

    match encoding {
        V0Encoding::Binary => file.extend_from_slice(&inner),
        V0Encoding::Lz4 => {
            let compressed = lz4_flex::block::compress(&inner);
            file.extend_from_slice(&(inner.len() as i32).to_le_bytes());
            file.extend_from_slice(&compressed);
        }
    }
    file
}
