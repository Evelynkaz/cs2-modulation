//! Binary KV3 reader (versions v0-v5). See `docs/FORMATS.md` section 3 and
//! `ValveResourceFormat/Resource/ResourceTypes/BinaryKV3*.cs` for the reference implementation
//! this mirrors.

use crate::compress::{Lz4ChainDecoder, block_compress, lz4_block, zstd_frame};
use crate::kv3::guid::{ENCODING_BINARY, ENCODING_BINARY_BC, ENCODING_BINARY_LZ4};
use crate::kv3::{Document, Flag, Guid, Kv3Error, Kv3Version, Value};
use crate::util::{OutOfBounds, ReadError, Reader};

const MAGIC0: u32 = 0x0356_4B56;
const LZ4_FRAME_SIZE: u32 = 16384;
const BLOB_MARKER: u32 = 0xFFEE_DD00;
const LEGACY_TRAILER: u32 = 0xFFFF_FFFF;
/// Maximum nesting depth for arrays/objects. Lowered from an initially-planned 512: even after
/// splitting each composite-value case into its own `#[inline(never)]` function (so a giant
/// shared match arm doesn't force every level of recursion to reserve stack for the union of all
/// arms' locals), unoptimized builds still use several KiB of stack per nesting level (Windows
/// opt-level 0: ~5-6 KiB, ~176 levels fit in 1 MiB; Linux debug overflowed 1 MiB below 128). 128
/// plus the 4 MiB thread requirement below leaves a wide margin. See
/// `tests::depth_at_limit_does_not_overflow_a_4mib_stack`.
///
/// 128 is also generous relative to real documents: the deepest nesting seen in
/// `de_mirage.vpk` (2,806 KV3 blocks) is 9, and the deepest across every KV3 block in
/// `pak01_dir.vpk` (242,048 blocks) is 32 (a Panorama layout-compiled `LaCo` block).
///
/// Callers must parse on a thread with at least 4 MiB of stack (the CLI uses 16 MiB; the
/// server configures its worker threads likewise).
const RECURSION_LIMIT: u32 = 128;

/// True if `bytes` starts with a recognised binary KV3 magic (v0 legacy or v1..v5).
pub fn is_binary_kv3(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic == MAGIC0 {
        return true;
    }
    let version = magic & 0xFF;
    magic & 0xFFFF_FF00 == 0x4B56_3300 && (1..=5).contains(&version)
}

/// A binary KV3 block's version and (for v1+) compression method, peeked from its header without
/// fully decoding the block (`docs/FORMATS.md` sections 3.1 and 3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryHeaderInfo {
    pub version: u8,
    /// `None` for v0 (compression is encoding-GUID based, not a method enum); for v1-v5, the raw
    /// `u32` at offset 20 (0 = none, 1 = LZ4, 2 = ZSTD).
    pub compression: Option<u32>,
}

/// Peeks `bytes`' binary KV3 version and compression method, without fully decoding the block.
/// Returns `None` if `bytes` isn't recognised as binary KV3 (see [`is_binary_kv3`]) or is too
/// short to contain the compression field. Layout: `docs/FORMATS.md` sections 3.1 and 3.6.
pub fn binary_header_info(bytes: &[u8]) -> Option<BinaryHeaderInfo> {
    if bytes.len() < 4 {
        return None;
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic == MAGIC0 {
        return Some(BinaryHeaderInfo {
            version: 0,
            compression: None,
        });
    }
    if magic & 0xFFFF_FF00 != 0x4B56_3300 {
        return None;
    }
    let version = (magic & 0xFF) as u8;
    if !(1..=5).contains(&version) || bytes.len() < 24 {
        return None;
    }
    let compression = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    Some(BinaryHeaderInfo {
        version,
        compression: Some(compression),
    })
}

/// Parses a binary KV3 document. `bytes` must be the exact block contents, starting at the
/// magic number.
pub fn parse_binary(bytes: &[u8]) -> Result<Document, Kv3Error> {
    let mut r = Reader::new(bytes);
    let magic = r.u32().map_err(|source| trunc("magic", source))?;

    if magic == MAGIC0 {
        return parse_v0(&mut r);
    }

    let version = magic & 0xFF;
    if magic & 0xFFFF_FF00 != 0x4B56_3300 {
        return Err(Kv3Error::BadMagic { magic });
    }
    if !(1..=5).contains(&version) {
        return Err(Kv3Error::UnsupportedVersion { version });
    }

    parse_v1_5(version as u8, &mut r)
}

fn trunc(context: &'static str, source: ReadError) -> Kv3Error {
    Kv3Error::Truncated { context, source }
}

fn bad_marker(context: &'static str, expected: u32, actual: u32) -> Kv3Error {
    Kv3Error::BadMarker {
        context,
        expected,
        actual,
    }
}

/// Converts a header count/size field to a non-negative `usize`, rejecting negative values
/// instead of letting `as usize` turn them into a huge value that could overflow arithmetic or
/// trigger a giant allocation downstream.
fn nn(v: i32, context: &'static str) -> Result<usize, Kv3Error> {
    usize::try_from(v).map_err(|_| Kv3Error::UnexpectedValue {
        context,
        value: v as i64,
    })
}

// ---------------------------------------------------------------------------------------------
// v0 (legacy)
// ---------------------------------------------------------------------------------------------

fn parse_v0(r: &mut Reader) -> Result<Document, Kv3Error> {
    let mut encoding_raw = [0u8; 16];
    encoding_raw.copy_from_slice(r.bytes(16).map_err(|e| trunc("v0 encoding guid", e))?);
    let encoding = Guid(encoding_raw);

    let mut format_raw = [0u8; 16];
    format_raw.copy_from_slice(r.bytes(16).map_err(|e| trunc("v0 format guid", e))?);
    let format = Guid(format_raw);

    let decompressed: Vec<u8> = if encoding == ENCODING_BINARY_BC {
        let rest = &r.bytes(r.remaining()).map_err(|e| trunc("v0 bc body", e))?;
        let (out, _consumed) = block_compress(rest).map_err(|source| Kv3Error::Decompress {
            context: "v0 binary_bc",
            source,
        })?;
        out
    } else if encoding == ENCODING_BINARY_LZ4 {
        let uncompressed_size = nn(r.i32().map_err(|e| trunc("v0 lz4 size", e))?, "v0 lz4 size")?;
        let rest = r
            .bytes(r.remaining())
            .map_err(|e| trunc("v0 lz4 body", e))?;
        lz4_block(rest, uncompressed_size).map_err(|source| Kv3Error::Decompress {
            context: "v0 binary_lz4",
            source,
        })?
    } else if encoding == ENCODING_BINARY {
        r.bytes(r.remaining())
            .map_err(|e| trunc("v0 body", e))?
            .to_vec()
    } else {
        return Err(Kv3Error::UnknownEncoding { guid: encoding });
    };

    let mut br = Reader::new(&decompressed);
    // Bound for lengths that would otherwise let a corrupt/hostile document request a huge
    // allocation from a tiny input (e.g. a bogus array/object/string count read as `i32::MAX`).
    let max_elems = br.len().saturating_mul(8);
    let string_count = br.u32().map_err(|e| trunc("v0 string count", e))? as usize;
    let mut strings = Vec::with_capacity(string_count.min(1024));
    for _ in 0..string_count {
        strings.push(br.cstr().map_err(|e| trunc("v0 strings", e))?.to_string());
    }

    let mut depth = 0u32;
    let mut payloadless_budget = max_elems;
    let (root_type, root_flag) = legacy_read_type(&mut br)?;
    let root = legacy_read_value(
        &mut br,
        &strings,
        root_type,
        root_flag,
        &mut depth,
        &mut payloadless_budget,
    )?;

    let trailer = br.u32().map_err(|e| trunc("v0 trailer", e))?;
    if trailer != LEGACY_TRAILER {
        return Err(bad_marker("v0 trailer", LEGACY_TRAILER, trailer));
    }
    if br.remaining() != 0 {
        return Err(Kv3Error::TrailingData {
            lane: "v0 body",
            remaining: br.remaining(),
        });
    }

    Ok(Document {
        format,
        encoding: Some(encoding),
        version: Kv3Version::Binary(0),
        root,
    })
}

fn legacy_read_type(r: &mut Reader) -> Result<(NodeType, Flag), Kv3Error> {
    let mut databyte = r.u8().map_err(|e| trunc("v0 type", e))?;
    let mut flag = Flag::None;

    if databyte & 0x80 != 0 {
        databyte &= 0x7F;
        let mut raw = r.u8().map_err(|e| trunc("v0 flag", e))? as u32;
        if raw & 4 != 0 {
            raw ^= 4;
        }
        flag = decode_legacy_flag(raw)?;
    }

    let node_type = NodeType::try_from(databyte)?;
    Ok((node_type, flag))
}

fn decode_legacy_flag(raw: u32) -> Result<Flag, Kv3Error> {
    Ok(match raw {
        0 => Flag::None,
        1 => Flag::Resource,
        2 => Flag::ResourceName,
        8 => Flag::Panorama,
        16 => Flag::SoundEvent,
        32 => Flag::SubClass,
        _ => {
            return Err(Kv3Error::BadFlag {
                value: raw,
                context: "v0-v2 bitmask flag",
            });
        }
    })
}

fn legacy_read_value(
    r: &mut Reader,
    strings: &[String],
    node_type: NodeType,
    flag: Flag,
    depth: &mut u32,
    payloadless_budget: &mut usize,
) -> Result<Value, Kv3Error> {
    let value = legacy_read_value_inner(r, strings, node_type, depth, payloadless_budget)?;
    Ok(Value::with_flag(flag, value))
}

fn legacy_read_value_inner(
    r: &mut Reader,
    strings: &[String],
    node_type: NodeType,
    depth: &mut u32,
    payloadless_budget: &mut usize,
) -> Result<Value, Kv3Error> {
    use NodeType::*;
    Ok(match node_type {
        Null => Value::Null,
        Boolean => Value::Bool(r.u8().map_err(|e| trunc("v0 bool", e))? != 0),
        BooleanTrue => Value::Bool(true),
        BooleanFalse => Value::Bool(false),
        Int64Zero => Value::Int(0),
        Int64One => Value::Int(1),
        Int64 => Value::Int(r.i64().map_err(|e| trunc("v0 int64", e))?),
        UInt64 => Value::UInt(r.u64().map_err(|e| trunc("v0 uint64", e))?),
        Int32 => Value::Int(r.i32().map_err(|e| trunc("v0 int32", e))? as i64),
        UInt32 => Value::UInt(r.u32().map_err(|e| trunc("v0 uint32", e))? as u64),
        Double => Value::Double(r.f64().map_err(|e| trunc("v0 double", e))?),
        DoubleZero => Value::Double(0.0),
        DoubleOne => Value::Double(1.0),
        String => {
            let id = r.i32().map_err(|e| trunc("v0 string id", e))?;
            resolve_string(id, strings)?
        }
        BinaryBlob => {
            let len = r.i32().map_err(|e| trunc("v0 blob len", e))?;
            let len = usize::try_from(len).unwrap_or(0);
            Value::Blob(r.bytes(len).map_err(|e| trunc("v0 blob", e))?.to_vec())
        }
        Array => legacy_read_array(r, strings, depth, payloadless_budget)?,
        ArrayTyped => legacy_read_array_typed(r, strings, depth, payloadless_budget)?,
        Object => legacy_read_object(r, strings, depth, payloadless_budget)?,
        other => {
            return Err(Kv3Error::UnknownNodeType {
                value: other as u8,
                context: "v0 value",
            });
        }
    })
}

/// Split out of `legacy_read_value_inner` (rather than inlined as a match arm) so that the
/// recursive call chain through composite values uses a smaller stack frame per level: keeping
/// every arm's locals in one shared function forces the compiler to reserve stack for the union
/// of all of them at every level of recursion, which wastes stack per level in debug builds (see
/// `RECURSION_LIMIT`).
#[inline(never)]
fn legacy_read_array(
    r: &mut Reader,
    strings: &[String],
    depth: &mut u32,
    payloadless_budget: &mut usize,
) -> Result<Value, Kv3Error> {
    *depth += 1;
    if *depth > RECURSION_LIMIT {
        return Err(Kv3Error::RecursionLimit {
            limit: RECURSION_LIMIT,
        });
    }
    let n = r.i32().map_err(|e| trunc("v0 array len", e))?;
    let n = usize::try_from(n).unwrap_or(0);
    let mut items = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let (t, f) = legacy_read_type(r)?;
        items.push(legacy_read_value(
            r,
            strings,
            t,
            f,
            depth,
            payloadless_budget,
        )?);
    }
    *depth -= 1;
    Ok(Value::Array(items))
}

#[inline(never)]
fn legacy_read_array_typed(
    r: &mut Reader,
    strings: &[String],
    depth: &mut u32,
    payloadless_budget: &mut usize,
) -> Result<Value, Kv3Error> {
    *depth += 1;
    if *depth > RECURSION_LIMIT {
        return Err(Kv3Error::RecursionLimit {
            limit: RECURSION_LIMIT,
        });
    }
    let n = r.i32().map_err(|e| trunc("v0 typed array len", e))?;
    let n = usize::try_from(n).unwrap_or(0);
    let (sub_type, sub_flag) = legacy_read_type(r)?;
    if is_payloadless(sub_type) {
        *payloadless_budget =
            payloadless_budget
                .checked_sub(n)
                .ok_or(Kv3Error::UnexpectedValue {
                    context: "typed array length exceeds input",
                    value: n as i64,
                })?;
    }
    let mut items = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        items.push(legacy_read_value(
            r,
            strings,
            sub_type,
            sub_flag,
            depth,
            payloadless_budget,
        )?);
    }
    *depth -= 1;
    Ok(Value::Array(items))
}

#[inline(never)]
fn legacy_read_object(
    r: &mut Reader,
    strings: &[String],
    depth: &mut u32,
    payloadless_budget: &mut usize,
) -> Result<Value, Kv3Error> {
    *depth += 1;
    if *depth > RECURSION_LIMIT {
        return Err(Kv3Error::RecursionLimit {
            limit: RECURSION_LIMIT,
        });
    }
    let n = r.i32().map_err(|e| trunc("v0 object len", e))?;
    let n = usize::try_from(n).unwrap_or(0);
    let mut obj = crate::kv3::Object::with_capacity(n.min(1024));
    for _ in 0..n {
        // (!) In v0, the member's key id precedes its type byte (FORMATS.md 3.5).
        let key_id = r.i32().map_err(|e| trunc("v0 key id", e))?;
        let name = resolve_string_key(key_id, strings)?;
        let (t, f) = legacy_read_type(r)?;
        obj.push(
            name,
            legacy_read_value(r, strings, t, f, depth, payloadless_budget)?,
        );
    }
    *depth -= 1;
    Ok(Value::Object(obj))
}

fn resolve_string(id: i32, strings: &[String]) -> Result<Value, Kv3Error> {
    if id == -1 {
        return Ok(Value::String(std::string::String::new()));
    }
    let idx = usize::try_from(id).map_err(|_| Kv3Error::StringIdOutOfRange {
        id,
        count: strings.len(),
    })?;
    strings
        .get(idx)
        .map(|s| Value::String(s.clone()))
        .ok_or(Kv3Error::StringIdOutOfRange {
            id,
            count: strings.len(),
        })
}

fn resolve_string_key(id: i32, strings: &[String]) -> Result<std::string::String, Kv3Error> {
    if id == -1 {
        return Ok(std::string::String::new());
    }
    let idx = usize::try_from(id).map_err(|_| Kv3Error::StringIdOutOfRange {
        id,
        count: strings.len(),
    })?;
    strings
        .get(idx)
        .cloned()
        .ok_or(Kv3Error::StringIdOutOfRange {
            id,
            count: strings.len(),
        })
}

// ---------------------------------------------------------------------------------------------
// Node type
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeType {
    Null = 1,
    Boolean = 2,
    Int64 = 3,
    UInt64 = 4,
    Double = 5,
    String = 6,
    BinaryBlob = 7,
    Array = 8,
    Object = 9,
    ArrayTyped = 10,
    Int32 = 11,
    UInt32 = 12,
    BooleanTrue = 13,
    BooleanFalse = 14,
    Int64Zero = 15,
    Int64One = 16,
    DoubleZero = 17,
    DoubleOne = 18,
    Float = 19,
    Int16 = 20,
    UInt16 = 21,
    Unknown22 = 22,
    Int32AsByte = 23,
    ArrayTypeByteLength = 24,
    ArrayTypeAuxiliaryBuffer = 25,
}

impl TryFrom<u8> for NodeType {
    type Error = Kv3Error;

    fn try_from(v: u8) -> Result<Self, Kv3Error> {
        use NodeType::*;
        Ok(match v {
            1 => Null,
            2 => Boolean,
            3 => Int64,
            4 => UInt64,
            5 => Double,
            6 => String,
            7 => BinaryBlob,
            8 => Array,
            9 => Object,
            10 => ArrayTyped,
            11 => Int32,
            12 => UInt32,
            13 => BooleanTrue,
            14 => BooleanFalse,
            15 => Int64Zero,
            16 => Int64One,
            17 => DoubleZero,
            18 => DoubleOne,
            19 => Float,
            20 => Int16,
            21 => UInt16,
            22 => Unknown22,
            23 => Int32AsByte,
            24 => ArrayTypeByteLength,
            25 => ArrayTypeAuxiliaryBuffer,
            other => {
                return Err(Kv3Error::UnknownNodeType {
                    value: other,
                    context: "type stream",
                });
            }
        })
    }
}

/// True for node types with no payload bytes at all (the value is fully implied by the type
/// byte). A typed array (10/24/25) whose element type is one of these can specify an element
/// count with no corresponding data to bound it against, so callers must check `n` against some
/// other bound (see `max_elems` / item 3 in the KV3 hardening review) instead of relying on
/// running out of input.
fn is_payloadless(t: NodeType) -> bool {
    matches!(
        t,
        NodeType::Null
            | NodeType::BooleanTrue
            | NodeType::BooleanFalse
            | NodeType::Int64Zero
            | NodeType::Int64One
            | NodeType::DoubleZero
            | NodeType::DoubleOne
    )
}

// ---------------------------------------------------------------------------------------------
// Lane: a bounds-checked forward cursor over one already-decompressed byte lane.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Lane<'a> {
    name: &'static str,
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Lane<'a> {
    fn new(name: &'static str, buf: &'a [u8]) -> Self {
        Lane { name, buf, pos: 0 }
    }

    fn empty(name: &'static str) -> Self {
        Lane {
            name,
            buf: &[],
            pos: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn check_exhausted(&self) -> Result<(), Kv3Error> {
        if self.remaining() != 0 {
            return Err(Kv3Error::TrailingData {
                lane: self.name,
                remaining: self.remaining(),
            });
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Kv3Error> {
        if self.pos + n > self.buf.len() {
            return Err(trunc(
                self.name,
                ReadError::OutOfBounds(OutOfBounds {
                    offset: self.pos,
                    len: n,
                    size: self.buf.len(),
                }),
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Kv3Error> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, Kv3Error> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u16(&mut self) -> Result<u16, Kv3Error> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, Kv3Error> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, Kv3Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, Kv3Error> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, Kv3Error> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Kv3Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64, Kv3Error> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn cstr(&mut self) -> Result<std::string::String, Kv3Error> {
        let nul = self.buf[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| {
                trunc(
                    self.name,
                    ReadError::OutOfBounds(OutOfBounds {
                        offset: self.pos,
                        len: 1,
                        size: self.buf.len(),
                    }),
                )
            })?;
        let bytes = &self.buf[self.pos..self.pos + nul];
        self.pos += nul + 1;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| Kv3Error::InvalidUtf8 { context: self.name })
    }
}

fn align_up(x: usize, n: usize) -> usize {
    x.div_ceil(n) * n
}

/// Advances `offset` to `align`, then slices `count * elem_size` bytes from `buf` at that
/// offset. Returns an empty lane (with no alignment applied) if `count == 0`.
fn take_lane<'a>(
    buf: &'a [u8],
    offset: &mut usize,
    count: usize,
    elem_size: usize,
    align: usize,
    name: &'static str,
) -> Result<Lane<'a>, Kv3Error> {
    if count == 0 {
        return Ok(Lane::empty(name));
    }
    *offset = align_up(*offset, align);
    let len = count.checked_mul(elem_size).ok_or_else(|| {
        trunc(
            name,
            ReadError::OutOfBounds(OutOfBounds {
                offset: *offset,
                len: count,
                size: buf.len(),
            }),
        )
    })?;
    let end = offset.checked_add(len).ok_or_else(|| {
        trunc(
            name,
            ReadError::OutOfBounds(OutOfBounds {
                offset: *offset,
                len,
                size: buf.len(),
            }),
        )
    })?;
    if end > buf.len() {
        return Err(trunc(
            name,
            ReadError::OutOfBounds(OutOfBounds {
                offset: *offset,
                len,
                size: buf.len(),
            }),
        ));
    }
    let lane = Lane::new(name, &buf[*offset..end]);
    *offset = end;
    Ok(lane)
}

// ---------------------------------------------------------------------------------------------
// v1-v5
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Buffers<'a> {
    bytes1: Lane<'a>,
    bytes2: Lane<'a>,
    bytes4: Lane<'a>,
    bytes8: Lane<'a>,
}

impl<'a> Default for Lane<'a> {
    fn default() -> Self {
        Lane::empty("uninitialised")
    }
}

impl<'a> Buffers<'a> {
    fn check_exhausted(&self) -> Result<(), Kv3Error> {
        self.bytes1.check_exhausted()?;
        self.bytes2.check_exhausted()?;
        self.bytes4.check_exhausted()?;
        self.bytes8.check_exhausted()?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompressionMethod {
    Uncompressed,
    Lz4,
    Zstd,
}

impl CompressionMethod {
    fn from_u32(v: u32, version: u8) -> Result<Self, Kv3Error> {
        Ok(match v {
            0 => CompressionMethod::Uncompressed,
            1 => CompressionMethod::Lz4,
            2 => CompressionMethod::Zstd,
            other => {
                return Err(Kv3Error::UnknownCompression {
                    method: other,
                    version: version as u32,
                });
            }
        })
    }
}

struct Context<'a> {
    version: u8,
    strings: Vec<std::string::String>,
    types: Lane<'a>,
    object_lengths: Lane<'a>,
    blob_lengths: Lane<'a>,
    blobs: Lane<'a>,
    buffer: Buffers<'a>,
    /// The v5 auxiliary buffer. Always present (empty for v<5); `has_aux` gates whether type 25
    /// is legal. Kept as a plain `Buffers` rather than `Option<Buffers>` so that entering and
    /// leaving an AUX array is a single symmetric `mem::swap` with `buffer` (matching VRF's
    /// `(AuxiliaryBuffer, Buffer) = (Buffer, AuxiliaryBuffer)` tuple swap exactly), which stays
    /// correct even for a type 25 nested inside another type 25.
    aux: Buffers<'a>,
    has_aux: bool,
    depth: u32,
    /// Remaining budget for payloadless typed-array elements, shared across the whole document
    /// (initialised to `max_elems`, decremented by `n` each time a payloadless typed array is
    /// read). Checking `n > max_elems` per array (as `max_elems` alone did) bounds one array, but
    /// nesting payloadless typed arrays inside each other lets each level multiply the total
    /// element count while every individual `n` still stays under `max_elems` -- e.g. 1000 arrays
    /// of 30000 nulls each, from a few KB of input, produced 30 million `Value`s. A single
    /// document-wide budget bounds the sum instead.
    payloadless_budget: usize,
}

fn read_type(types: &mut Lane, version: u8) -> Result<(NodeType, Flag), Kv3Error> {
    let mut databyte = types.u8()?;
    let mut flag = Flag::None;

    if version >= 3 {
        if databyte & 0x80 != 0 {
            databyte &= 0x3F;
            let raw = types.u8()? as u32;
            flag = match raw {
                0 => Flag::None,
                1 => Flag::Resource,
                2 => Flag::ResourceName,
                3 => Flag::Panorama,
                4 => Flag::SoundEvent,
                5 => Flag::SubClass,
                6 => Flag::EntityName,
                _ => {
                    return Err(Kv3Error::BadFlag {
                        value: raw,
                        context: "v3+ enum flag",
                    });
                }
            };
        }
    } else if databyte & 0x80 != 0 {
        databyte &= 0x7F;
        let mut raw = types.u8()? as u32;
        if raw & 4 != 0 {
            raw ^= 4;
        }
        flag = decode_legacy_flag(raw)?;
    }

    let node_type = NodeType::try_from(databyte)?;
    Ok((node_type, flag))
}

fn parse_v1_5(version: u8, r: &mut Reader) -> Result<Document, Kv3Error> {
    // Bound for lengths that would otherwise let a corrupt/hostile document request a huge
    // allocation or iteration count from a tiny input.
    let max_elems = r.len().saturating_mul(8);

    let mut format_raw = [0u8; 16];
    format_raw.copy_from_slice(r.bytes(16).map_err(|e| trunc("format guid", e))?);
    let format = Guid(format_raw);

    let compression_raw = r.u32().map_err(|e| trunc("compression method", e))?;
    let compression = CompressionMethod::from_u32(compression_raw, version)?;

    let (
        compression_dictionary_id,
        compression_frame_size,
        count_bytes1,
        count_bytes4,
        count_bytes8,
        count_types,
        size_uncompressed_total,
        size_compressed_total,
        count_blocks,
        size_binary_blobs_bytes,
    );

    if version == 1 {
        compression_dictionary_id = 0u16;
        compression_frame_size = 0u16;
        count_bytes1 = nn(
            r.i32().map_err(|e| trunc("v1 countBytes1", e))?,
            "v1 countBytes1",
        )?;
        count_bytes4 = nn(
            r.i32().map_err(|e| trunc("v1 countBytes4", e))?,
            "v1 countBytes4",
        )?;
        count_bytes8 = nn(
            r.i32().map_err(|e| trunc("v1 countBytes8", e))?,
            "v1 countBytes8",
        )?;
        count_types = 0;
        size_uncompressed_total = nn(
            r.i32().map_err(|e| trunc("v1 sizeUncompressedTotal", e))?,
            "v1 sizeUncompressedTotal",
        )?;
        // Version 1 has no explicit compressed size field: it is the rest of the block.
        size_compressed_total = r.remaining();
        count_blocks = 0;
        size_binary_blobs_bytes = 0;
    } else {
        compression_dictionary_id = r.u16().map_err(|e| trunc("compressionDictionaryId", e))?;
        compression_frame_size = r.u16().map_err(|e| trunc("compressionFrameSize", e))?;
        count_bytes1 = nn(r.i32().map_err(|e| trunc("countBytes1", e))?, "countBytes1")?;
        count_bytes4 = nn(r.i32().map_err(|e| trunc("countBytes4", e))?, "countBytes4")?;
        count_bytes8 = nn(r.i32().map_err(|e| trunc("countBytes8", e))?, "countBytes8")?;
        count_types = nn(r.i32().map_err(|e| trunc("countTypes", e))?, "countTypes")?;
        let _count_objects = r.u16().map_err(|e| trunc("countObjects", e))?;
        let _count_arrays = r.u16().map_err(|e| trunc("countArrays", e))?;
        size_uncompressed_total = nn(
            r.i32().map_err(|e| trunc("sizeUncompressedTotal", e))?,
            "sizeUncompressedTotal",
        )?;
        size_compressed_total = nn(
            r.i32().map_err(|e| trunc("sizeCompressedTotal", e))?,
            "sizeCompressedTotal",
        )?;
        count_blocks = nn(r.i32().map_err(|e| trunc("countBlocks", e))?, "countBlocks")?;
        size_binary_blobs_bytes = nn(
            r.i32().map_err(|e| trunc("sizeBinaryBlobsBytes", e))?,
            "sizeBinaryBlobsBytes",
        )?;
    }
    let mut count_bytes2 = 0usize;
    if version >= 4 {
        count_bytes2 = nn(r.i32().map_err(|e| trunc("countBytes2", e))?, "countBytes2")?;
        let _size_block_compressed_sizes_bytes = r
            .i32()
            .map_err(|e| trunc("sizeBlockCompressedSizesBytes", e))?;
    }

    let mut size_uncompressed_buffer1 = size_uncompressed_total;
    let mut size_compressed_buffer1 = size_compressed_total;
    let mut size_uncompressed_buffer2 = 0usize;
    let mut size_compressed_buffer2 = 0usize;
    let mut count_bytes1_b2 = 0usize;
    let mut count_bytes2_b2 = 0usize;
    let mut count_bytes4_b2 = 0usize;
    let mut count_bytes8_b2 = 0usize;
    let mut count_objects_b2 = 0usize;

    if version >= 5 {
        size_uncompressed_buffer1 = nn(
            r.i32().map_err(|e| trunc("sizeUncompressedBuffer1", e))?,
            "sizeUncompressedBuffer1",
        )?;
        size_compressed_buffer1 = nn(
            r.i32().map_err(|e| trunc("sizeCompressedBuffer1", e))?,
            "sizeCompressedBuffer1",
        )?;
        size_uncompressed_buffer2 = nn(
            r.i32().map_err(|e| trunc("sizeUncompressedBuffer2", e))?,
            "sizeUncompressedBuffer2",
        )?;
        size_compressed_buffer2 = nn(
            r.i32().map_err(|e| trunc("sizeCompressedBuffer2", e))?,
            "sizeCompressedBuffer2",
        )?;
        count_bytes1_b2 = nn(
            r.i32().map_err(|e| trunc("countBytes1_buffer2", e))?,
            "countBytes1_buffer2",
        )?;
        count_bytes2_b2 = nn(
            r.i32().map_err(|e| trunc("countBytes2_buffer2", e))?,
            "countBytes2_buffer2",
        )?;
        count_bytes4_b2 = nn(
            r.i32().map_err(|e| trunc("countBytes4_buffer2", e))?,
            "countBytes4_buffer2",
        )?;
        count_bytes8_b2 = nn(
            r.i32().map_err(|e| trunc("countBytes8_buffer2", e))?,
            "countBytes8_buffer2",
        )?;
        let _unk13 = r.i32().map_err(|e| trunc("unk13", e))?;
        count_objects_b2 = nn(
            r.i32().map_err(|e| trunc("countObjects_buffer2", e))?,
            "countObjects_buffer2",
        )?;
        let _count_arrays_b2 = r.i32().map_err(|e| trunc("countArrays_buffer2", e))?;
        let _unk16 = r.i32().map_err(|e| trunc("unk16", e))?;
    }

    if compression_dictionary_id != 0 {
        return Err(Kv3Error::UnknownCompression {
            method: compression_dictionary_id as u32 + 1000,
            version: version as u32,
        });
    }
    match compression {
        CompressionMethod::Lz4 if version >= 2 => {
            if compression_frame_size as u32 != LZ4_FRAME_SIZE {
                return Err(bad_marker(
                    "compressionFrameSize",
                    LZ4_FRAME_SIZE,
                    compression_frame_size as u32,
                ));
            }
        }
        CompressionMethod::Lz4 => {}
        _ => {
            if compression_frame_size != 0 {
                return Err(bad_marker(
                    "compressionFrameSize",
                    0,
                    compression_frame_size as u32,
                ));
            }
        }
    }

    // Buffer 1 (aux for v5, main otherwise).
    let mut zstd_v_lt5_blob_tail: Option<Vec<u8>> = None;
    let buffer1_raw: Vec<u8> = match compression {
        CompressionMethod::Uncompressed => r
            .bytes(size_uncompressed_buffer1)
            .map_err(|e| trunc("buffer1 (uncompressed)", e))?
            .to_vec(),
        CompressionMethod::Lz4 => {
            let compressed = r
                .bytes(size_compressed_buffer1)
                .map_err(|e| trunc("buffer1 (lz4)", e))?;
            lz4_block(compressed, size_uncompressed_buffer1).map_err(|source| {
                Kv3Error::Decompress {
                    context: "buffer1 lz4",
                    source,
                }
            })?
        }
        CompressionMethod::Zstd => {
            let out_len = if version < 5 {
                size_uncompressed_buffer1 + size_binary_blobs_bytes
            } else {
                size_uncompressed_buffer1
            };
            let compressed = r
                .bytes(size_compressed_buffer1)
                .map_err(|e| trunc("buffer1 (zstd)", e))?;
            let decoded =
                zstd_frame(compressed, out_len).map_err(|source| Kv3Error::Decompress {
                    context: "buffer1 zstd",
                    source,
                })?;
            if version < 5 {
                zstd_v_lt5_blob_tail = Some(decoded[size_uncompressed_buffer1..].to_vec());
                decoded[..size_uncompressed_buffer1].to_vec()
            } else {
                decoded
            }
        }
    };

    let mut offset1 = 0usize;
    let mut b1_bytes1 = take_lane(
        &buffer1_raw,
        &mut offset1,
        count_bytes1,
        1,
        1,
        "buffer1.bytes1",
    )?;
    let b1_bytes2 = take_lane(
        &buffer1_raw,
        &mut offset1,
        count_bytes2,
        2,
        2,
        "buffer1.bytes2",
    )?;
    let mut b1_bytes4 = take_lane(
        &buffer1_raw,
        &mut offset1,
        count_bytes4,
        4,
        4,
        "buffer1.bytes4",
    )?;
    let b1_bytes8 = take_lane(
        &buffer1_raw,
        &mut offset1,
        count_bytes8,
        8,
        8,
        "buffer1.bytes8",
    )?;
    if count_bytes8 == 0 && version < 5 {
        // (!) v<5 aligns to 8 even when Bytes8 is empty; v5 does not (FORMATS.md 3.8/3.9).
        offset1 = align_up(offset1, 8);
        if offset1 > buffer1_raw.len() {
            return Err(trunc(
                "buffer1 (align8 on empty bytes8)",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: offset1,
                    len: 0,
                    size: buffer1_raw.len(),
                }),
            ));
        }
    }

    let string_count = b1_bytes4.i32()? as usize;
    let mut strings = Vec::with_capacity(string_count.min(1 << 16));

    let mut types_lane = Lane::empty("types");
    let mut tail_for_blocks: Option<Lane> = None;
    let mut aux_buffers = Buffers::default();
    let has_aux = version >= 5;
    let mut main_buffers = Buffers {
        bytes1: Lane::empty("buffer.bytes1"),
        bytes2: Lane::empty("buffer.bytes2"),
        bytes4: Lane::empty("buffer.bytes4"),
        bytes8: Lane::empty("buffer.bytes8"),
    };

    if version >= 5 {
        // VRF asserts the whole buffer1 span was accounted for by the bytes1/2/4/8 lanes
        // (BinaryKV3.cs:459) before it starts consuming strings out of bytes1.
        if offset1 != buffer1_raw.len() {
            return Err(Kv3Error::TrailingData {
                lane: "buffer1",
                remaining: buffer1_raw.len() - offset1,
            });
        }
        for _ in 0..string_count {
            strings.push(b1_bytes1.cstr()?);
        }
        aux_buffers = Buffers {
            bytes1: b1_bytes1,
            bytes2: b1_bytes2,
            bytes4: b1_bytes4,
            bytes8: b1_bytes8,
        };
    } else {
        let strings_start = offset1;
        for _ in 0..string_count {
            let nul = buffer1_raw
                .get(offset1..)
                .ok_or_else(|| {
                    trunc(
                        "buffer1 strings",
                        ReadError::OutOfBounds(OutOfBounds {
                            offset: offset1,
                            len: 1,
                            size: buffer1_raw.len(),
                        }),
                    )
                })?
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| {
                    trunc(
                        "buffer1 strings",
                        ReadError::OutOfBounds(OutOfBounds {
                            offset: offset1,
                            len: 1,
                            size: buffer1_raw.len(),
                        }),
                    )
                })?;
            let s = std::str::from_utf8(&buffer1_raw[offset1..offset1 + nul]).map_err(|_| {
                Kv3Error::InvalidUtf8 {
                    context: "buffer1 strings",
                }
            })?;
            strings.push(s.to_string());
            offset1 += nul + 1;
        }

        let types_len = if version == 1 {
            size_uncompressed_total
                .checked_sub(offset1)
                .and_then(|v| v.checked_sub(4))
                .ok_or_else(|| {
                    trunc(
                        "v1 types length",
                        ReadError::OutOfBounds(OutOfBounds {
                            offset: offset1,
                            len: 4,
                            size: size_uncompressed_total,
                        }),
                    )
                })?
        } else {
            count_types
                .checked_sub(offset1 - strings_start)
                .ok_or_else(|| {
                    trunc(
                        "types length",
                        ReadError::OutOfBounds(OutOfBounds {
                            offset: offset1 - strings_start,
                            len: 0,
                            size: count_types,
                        }),
                    )
                })?
        };

        let types_end = offset1.checked_add(types_len).ok_or_else(|| {
            trunc(
                "buffer1 types",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: offset1,
                    len: types_len,
                    size: buffer1_raw.len(),
                }),
            )
        })?;
        if types_end > buffer1_raw.len() {
            return Err(trunc(
                "buffer1 types",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: offset1,
                    len: types_len,
                    size: buffer1_raw.len(),
                }),
            ));
        }
        types_lane = Lane::new("types", &buffer1_raw[offset1..types_end]);
        offset1 = types_end;

        if count_blocks == 0 {
            let marker_bytes = buffer1_raw.get(offset1..offset1 + 4).ok_or_else(|| {
                trunc(
                    "buffer1 trailer",
                    ReadError::OutOfBounds(OutOfBounds {
                        offset: offset1,
                        len: 4,
                        size: buffer1_raw.len(),
                    }),
                )
            })?;
            let marker = u32::from_le_bytes(marker_bytes.try_into().unwrap());
            offset1 += 4;
            if marker != BLOB_MARKER {
                return Err(bad_marker("buffer1 trailer", BLOB_MARKER, marker));
            }
            if offset1 != buffer1_raw.len() {
                return Err(Kv3Error::TrailingData {
                    lane: "buffer1",
                    remaining: buffer1_raw.len() - offset1,
                });
            }
        } else {
            tail_for_blocks = Some(Lane::new("buffer1 tail", &buffer1_raw[offset1..]));
        }

        // For v<5 there is no auxiliary buffer: buffer1's lanes are the main buffer directly.
        main_buffers = Buffers {
            bytes1: b1_bytes1,
            bytes2: b1_bytes2,
            bytes4: b1_bytes4,
            bytes8: b1_bytes8,
        };
    }

    // Buffer 2 (main, v5 only). Declared here (rather than inside the `if`) so that lanes
    // borrowed from it below can outlive the `if` block.
    let mut object_lengths = Lane::empty("objectLengths");
    let buffer2_raw: Vec<u8>;

    if version >= 5 {
        buffer2_raw = match compression {
            CompressionMethod::Uncompressed => r
                .bytes(size_uncompressed_buffer2)
                .map_err(|e| trunc("buffer2 (uncompressed)", e))?
                .to_vec(),
            CompressionMethod::Lz4 => {
                let compressed = r
                    .bytes(size_compressed_buffer2)
                    .map_err(|e| trunc("buffer2 (lz4)", e))?;
                lz4_block(compressed, size_uncompressed_buffer2).map_err(|source| {
                    Kv3Error::Decompress {
                        context: "buffer2 lz4",
                        source,
                    }
                })?
            }
            CompressionMethod::Zstd => {
                let compressed = r
                    .bytes(size_compressed_buffer2)
                    .map_err(|e| trunc("buffer2 (zstd)", e))?;
                zstd_frame(compressed, size_uncompressed_buffer2).map_err(|source| {
                    Kv3Error::Decompress {
                        context: "buffer2 zstd",
                        source,
                    }
                })?
            }
        };

        let mut offset2 = count_objects_b2.checked_mul(4).ok_or_else(|| {
            trunc(
                "buffer2 objectLengths",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: 0,
                    len: count_objects_b2,
                    size: buffer2_raw.len(),
                }),
            )
        })?;
        if offset2 > buffer2_raw.len() {
            return Err(trunc(
                "buffer2 objectLengths",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: 0,
                    len: offset2,
                    size: buffer2_raw.len(),
                }),
            ));
        }
        object_lengths = Lane::new("objectLengths", &buffer2_raw[..offset2]);

        main_buffers.bytes1 = take_lane(
            &buffer2_raw,
            &mut offset2,
            count_bytes1_b2,
            1,
            1,
            "buffer2.bytes1",
        )?;
        main_buffers.bytes2 = take_lane(
            &buffer2_raw,
            &mut offset2,
            count_bytes2_b2,
            2,
            2,
            "buffer2.bytes2",
        )?;
        main_buffers.bytes4 = take_lane(
            &buffer2_raw,
            &mut offset2,
            count_bytes4_b2,
            4,
            4,
            "buffer2.bytes4",
        )?;
        main_buffers.bytes8 = take_lane(
            &buffer2_raw,
            &mut offset2,
            count_bytes8_b2,
            8,
            8,
            "buffer2.bytes8",
        )?;

        let types_end = offset2.checked_add(count_types).ok_or_else(|| {
            trunc(
                "buffer2 types",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: offset2,
                    len: count_types,
                    size: buffer2_raw.len(),
                }),
            )
        })?;
        if types_end > buffer2_raw.len() {
            return Err(trunc(
                "buffer2 types",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: offset2,
                    len: count_types,
                    size: buffer2_raw.len(),
                }),
            ));
        }
        types_lane = Lane::new("types", &buffer2_raw[offset2..types_end]);
        offset2 = types_end;

        if count_blocks == 0 {
            let marker_bytes = buffer2_raw.get(offset2..offset2 + 4).ok_or_else(|| {
                trunc(
                    "buffer2 trailer",
                    ReadError::OutOfBounds(OutOfBounds {
                        offset: offset2,
                        len: 4,
                        size: buffer2_raw.len(),
                    }),
                )
            })?;
            let marker = u32::from_le_bytes(marker_bytes.try_into().unwrap());
            offset2 += 4;
            if marker != BLOB_MARKER {
                return Err(bad_marker("buffer2 trailer", BLOB_MARKER, marker));
            }
            if offset2 != buffer2_raw.len() {
                return Err(Kv3Error::TrailingData {
                    lane: "buffer2",
                    remaining: buffer2_raw.len() - offset2,
                });
            }
        } else {
            tail_for_blocks = Some(Lane::new("buffer2 tail", &buffer2_raw[offset2..]));
        }
    }

    let mut blob_lengths = Lane::empty("blobLengths");
    let blobs: Vec<u8>;

    if count_blocks > 0 {
        let mut tail = tail_for_blocks.ok_or(Kv3Error::TrailingData {
            lane: "blob table",
            remaining: 0,
        })?;
        let blob_lengths_bytes = count_blocks.checked_mul(4).ok_or_else(|| {
            trunc(
                "blobLengths",
                ReadError::OutOfBounds(OutOfBounds {
                    offset: 0,
                    len: count_blocks,
                    size: 0,
                }),
            )
        })?;
        let lengths_bytes = tail.take(blob_lengths_bytes)?;
        blob_lengths = Lane::new("blobLengths", lengths_bytes);
        let marker = tail.u32()?;
        if marker != BLOB_MARKER {
            return Err(bad_marker("blob table trailer", BLOB_MARKER, marker));
        }

        blobs = match compression {
            CompressionMethod::Uncompressed => {
                // Unlike Lz4 (which drains `tail` via its frame-size loop), there is nothing
                // else expected after the marker for uncompressed/zstd blobs.
                tail.check_exhausted()?;
                r.bytes(size_binary_blobs_bytes)
                    .map_err(|e| trunc("blobs (uncompressed)", e))?
                    .to_vec()
            }
            CompressionMethod::Lz4 => {
                let mut chain = Lz4ChainDecoder::new(size_binary_blobs_bytes);
                let mut decoded_offset = 0usize;
                while tail.remaining() > 0 {
                    let frame_len = tail.u16()? as usize;
                    let compressed = r
                        .bytes(frame_len)
                        .map_err(|e| trunc("blobs (lz4 frame)", e))?;
                    let want = (compression_frame_size as usize)
                        .min(size_binary_blobs_bytes.saturating_sub(decoded_offset));
                    let n = chain.decode_block(compressed, want).map_err(|source| {
                        Kv3Error::Decompress {
                            context: "blobs lz4 chain",
                            source,
                        }
                    })?;
                    decoded_offset += n;
                }
                chain.finish().map_err(|source| Kv3Error::Decompress {
                    context: "blobs lz4 chain finish",
                    source,
                })?
            }
            CompressionMethod::Zstd => {
                tail.check_exhausted()?;
                if version >= 5 {
                    let size_compressed_blobs = size_compressed_total
                        .checked_sub(size_compressed_buffer1)
                        .and_then(|v| v.checked_sub(size_compressed_buffer2))
                        .ok_or(Kv3Error::UnexpectedValue {
                            context: "sizeCompressedTotal too small for blobs",
                            value: size_compressed_total as i64,
                        })?;
                    let compressed = r
                        .bytes(size_compressed_blobs)
                        .map_err(|e| trunc("blobs (zstd)", e))?;
                    zstd_frame(compressed, size_binary_blobs_bytes).map_err(|source| {
                        Kv3Error::Decompress {
                            context: "blobs zstd",
                            source,
                        }
                    })?
                } else {
                    zstd_v_lt5_blob_tail
                        .take()
                        .ok_or(Kv3Error::UnexpectedValue {
                            context: "zstd blob tail missing",
                            value: 0,
                        })?
                }
            }
        };

        let trailer = r.u32().map_err(|e| trunc("file trailer", e))?;
        if trailer != BLOB_MARKER {
            return Err(bad_marker("file trailer", BLOB_MARKER, trailer));
        }
    } else {
        // v2-v4 zstd decodes buffer1 and the blobs in one frame (see above); if there's no blob
        // table (no blocks) but the frame decoded a non-empty tail past buffer1 anyway, that tail
        // doesn't actually correspond to anything the rest of the format describes, so surface it
        // instead of silently dropping it.
        if let Some(tail) = zstd_v_lt5_blob_tail.take()
            && !tail.is_empty()
        {
            return Err(Kv3Error::TrailingData {
                lane: "zstd blob tail",
                remaining: tail.len(),
            });
        }
        blobs = Vec::new();
    }

    // Item 14 of the KV3 hardening review originally added a whole-block `r.remaining() != 0`
    // check here, matching what held for every fixture then available. That doesn't hold in
    // general: a real CS2 file, pak01 `animation/graphs/viewmodel/viewmodel_inspects.vnmgraph_c`
    // DATA, is a 7452-byte block whose declared sizes describe only the first of two back-to-back
    // copies of the same 3726-byte v5 LZ4 document (see
    // `tests::block_with_trailing_bytes_after_document_parses`,
    // `resource_real_game::vnmgraph_document_repeated_in_data_block_parses`). VRF doesn't check
    // for trailing bytes after the document either, so we don't. The per-lane checks just below
    // (types/object_lengths/blob_lengths/blobs/buffer/aux) still catch the cases where a lane's
    // own declared size doesn't match what was actually consumed from it.
    let blobs_lane = Lane::new("blobs", &blobs);

    let mut ctx = Context {
        version,
        strings,
        types: types_lane,
        object_lengths,
        blob_lengths,
        blobs: blobs_lane,
        buffer: main_buffers,
        aux: aux_buffers,
        has_aux,
        depth: 0,
        payloadless_budget: max_elems,
    };

    let (root_type, root_flag) = read_type(&mut ctx.types, ctx.version)?;
    let root = read_value(&mut ctx, root_type)?;
    let root = Value::with_flag(root_flag, root);

    ctx.types.check_exhausted()?;
    ctx.object_lengths.check_exhausted()?;
    ctx.blob_lengths.check_exhausted()?;
    ctx.blobs.check_exhausted()?;
    ctx.buffer.check_exhausted()?;
    if ctx.has_aux {
        ctx.aux.check_exhausted()?;
    }

    Ok(Document {
        format,
        encoding: None,
        version: Kv3Version::Binary(version),
        root,
    })
}

fn read_value(ctx: &mut Context, node_type: NodeType) -> Result<Value, Kv3Error> {
    use NodeType::*;
    Ok(match node_type {
        Null => Value::Null,
        BooleanTrue => Value::Bool(true),
        BooleanFalse => Value::Bool(false),
        Int64Zero => Value::Int(0),
        Int64One => Value::Int(1),
        DoubleZero => Value::Double(0.0),
        DoubleOne => Value::Double(1.0),
        Boolean => Value::Bool(ctx.buffer.bytes1.u8()? != 0),
        Int32AsByte => Value::Int(ctx.buffer.bytes1.u8()? as i64),
        Int16 => Value::Int(ctx.buffer.bytes2.i16()? as i64),
        UInt16 => Value::UInt(ctx.buffer.bytes2.u16()? as u64),
        Int32 => Value::Int(ctx.buffer.bytes4.i32()? as i64),
        UInt32 => Value::UInt(ctx.buffer.bytes4.u32()? as u64),
        Float => Value::Double(ctx.buffer.bytes4.f32()? as f64),
        Int64 => Value::Int(ctx.buffer.bytes8.i64()?),
        UInt64 => Value::UInt(ctx.buffer.bytes8.u64()?),
        Double => Value::Double(ctx.buffer.bytes8.f64()?),
        String => {
            let id = ctx.buffer.bytes4.i32()?;
            if id == -1 {
                Value::String(std::string::String::new())
            } else {
                let idx = usize::try_from(id).map_err(|_| Kv3Error::StringIdOutOfRange {
                    id,
                    count: ctx.strings.len(),
                })?;
                let s = ctx.strings.get(idx).ok_or(Kv3Error::StringIdOutOfRange {
                    id,
                    count: ctx.strings.len(),
                })?;
                Value::String(s.clone())
            }
        }
        BinaryBlob if ctx.version < 2 => {
            let len = ctx.buffer.bytes4.i32()?;
            let len = usize::try_from(len).unwrap_or(0);
            Value::Blob(ctx.buffer.bytes1.take(len)?.to_vec())
        }
        BinaryBlob => {
            let len = ctx.blob_lengths.i32()?;
            let len = usize::try_from(len).unwrap_or(0);
            Value::Blob(ctx.blobs.take(len)?.to_vec())
        }
        Array => read_array(ctx)?,
        ArrayTyped | ArrayTypeByteLength => {
            read_array_typed(ctx, node_type == ArrayTypeByteLength)?
        }
        ArrayTypeAuxiliaryBuffer => read_array_aux(ctx)?,
        Object => read_object(ctx)?,
        Unknown22 => return Err(Kv3Error::ReservedNodeType22),
    })
}

/// Split out of `read_value` (rather than inlined as a match arm) so that the recursive call
/// chain through composite values uses a smaller stack frame per level: keeping every arm's
/// locals in one shared function forces the compiler to reserve stack for the union of all of
/// them at every level of recursion, which wastes stack per level in debug builds (see
/// `RECURSION_LIMIT`).
#[inline(never)]
fn read_array(ctx: &mut Context) -> Result<Value, Kv3Error> {
    enter_depth(ctx)?;
    let n = ctx.buffer.bytes4.i32()?;
    let n = usize::try_from(n).unwrap_or(0);
    let mut items = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let (t, f) = read_type(&mut ctx.types, ctx.version)?;
        let v = read_value(ctx, t)?;
        items.push(Value::with_flag(f, v));
    }
    leave_depth(ctx);
    Ok(Value::Array(items))
}

#[inline(never)]
fn read_array_typed(ctx: &mut Context, byte_len: bool) -> Result<Value, Kv3Error> {
    enter_depth(ctx)?;
    let n = if byte_len {
        ctx.buffer.bytes1.u8()? as usize
    } else {
        let n = ctx.buffer.bytes4.i32()?;
        usize::try_from(n).unwrap_or(0)
    };
    let (sub_type, sub_flag) = read_type(&mut ctx.types, ctx.version)?;
    if is_payloadless(sub_type) {
        ctx.payloadless_budget =
            ctx.payloadless_budget
                .checked_sub(n)
                .ok_or(Kv3Error::UnexpectedValue {
                    context: "typed array length exceeds input",
                    value: n as i64,
                })?;
    }
    let mut items = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let v = read_value(ctx, sub_type)?;
        items.push(Value::with_flag(sub_flag, v));
    }
    leave_depth(ctx);
    Ok(Value::Array(items))
}

#[inline(never)]
fn read_array_aux(ctx: &mut Context) -> Result<Value, Kv3Error> {
    if !ctx.has_aux {
        return Err(Kv3Error::UnknownNodeType {
            value: 25,
            context: "type 25 requires v5 auxiliary buffer",
        });
    }
    enter_depth(ctx)?;
    let n = ctx.buffer.bytes1.u8()? as usize;
    let (sub_type, sub_flag) = read_type(&mut ctx.types, ctx.version)?;
    if is_payloadless(sub_type) {
        ctx.payloadless_budget =
            ctx.payloadless_budget
                .checked_sub(n)
                .ok_or(Kv3Error::UnexpectedValue {
                    context: "typed array length exceeds input",
                    value: n as i64,
                })?;
    }

    // Swap the current buffer with the auxiliary one for the duration of the array, exactly
    // mirroring VRF's symmetric tuple swap (BinaryKV3.cs:997: `(AuxiliaryBuffer, Buffer) =
    // (Buffer, AuxiliaryBuffer)`), including on the error path, so that a type 25 nested inside
    // another type 25 sees the buffers in the right state instead of an already-emptied aux.
    std::mem::swap(&mut ctx.buffer, &mut ctx.aux);

    let mut items = Vec::with_capacity(n.min(1024));
    let mut err = None;
    for _ in 0..n {
        match read_value(ctx, sub_type) {
            Ok(v) => items.push(Value::with_flag(sub_flag, v)),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }

    std::mem::swap(&mut ctx.buffer, &mut ctx.aux);

    if let Some(e) = err {
        return Err(e);
    }
    leave_depth(ctx);
    Ok(Value::Array(items))
}

#[inline(never)]
fn read_object(ctx: &mut Context) -> Result<Value, Kv3Error> {
    enter_depth(ctx)?;
    let n = if ctx.version >= 5 {
        let n = ctx.object_lengths.i32()?;
        usize::try_from(n).unwrap_or(0)
    } else {
        let n = ctx.buffer.bytes4.i32()?;
        usize::try_from(n).unwrap_or(0)
    };
    let mut obj = crate::kv3::Object::with_capacity(n.min(1024));
    for _ in 0..n {
        let (t, f) = read_type(&mut ctx.types, ctx.version)?;
        let key_id = ctx.buffer.bytes4.i32()?;
        let name = if key_id == -1 {
            std::string::String::new()
        } else {
            let idx = usize::try_from(key_id).map_err(|_| Kv3Error::StringIdOutOfRange {
                id: key_id,
                count: ctx.strings.len(),
            })?;
            ctx.strings
                .get(idx)
                .cloned()
                .ok_or(Kv3Error::StringIdOutOfRange {
                    id: key_id,
                    count: ctx.strings.len(),
                })?
        };
        let v = read_value(ctx, t)?;
        obj.push(name, Value::with_flag(f, v));
    }
    leave_depth(ctx);
    Ok(Value::Object(obj))
}

#[inline(always)]
fn enter_depth(ctx: &mut Context) -> Result<(), Kv3Error> {
    ctx.depth += 1;
    if ctx.depth > RECURSION_LIMIT {
        return Err(Kv3Error::RecursionLimit {
            limit: RECURSION_LIMIT,
        });
    }
    Ok(())
}

#[inline(always)]
fn leave_depth(ctx: &mut Context) {
    ctx.depth -= 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv3::guid::FORMAT_GENERIC;
    use crate::kv3::test_writer::{
        Compression, TestNode, V0Encoding, build, build_v0, expected_value,
    };

    #[test]
    fn is_binary_kv3_recognises_versions() {
        assert!(!is_binary_kv3(&[]));
        assert!(!is_binary_kv3(&[1, 2, 3]));
        assert!(is_binary_kv3(&MAGIC0.to_le_bytes()));
        for v in 1u32..=5 {
            let magic = 0x4B56_3300 | v;
            assert!(is_binary_kv3(&magic.to_le_bytes()));
        }
        assert!(!is_binary_kv3(&(0x4B56_3306u32).to_le_bytes()));
        assert!(!is_binary_kv3(&(0x4B56_3300u32).to_le_bytes()));
    }

    #[test]
    fn binary_header_info_v0() {
        let bytes = build_v0(&TestNode::Int64(0), V0Encoding::Binary, FORMAT_GENERIC);
        assert_eq!(
            binary_header_info(&bytes),
            Some(BinaryHeaderInfo {
                version: 0,
                compression: None,
            })
        );
    }

    #[test]
    fn binary_header_info_v5() {
        let bytes = build(&TestNode::Int64(0), 5, Compression::Zstd, FORMAT_GENERIC);
        assert_eq!(
            binary_header_info(&bytes),
            Some(BinaryHeaderInfo {
                version: 5,
                compression: Some(2),
            })
        );
    }

    #[test]
    fn binary_header_info_truncated() {
        assert_eq!(binary_header_info(&[]), None);
        assert_eq!(binary_header_info(&[1, 2, 3]), None);
        let magic = 0x4B56_3305u32;
        // Long enough to pass `is_binary_kv3`'s 4-byte check, but short of the offset-20..24
        // compression field.
        assert_eq!(binary_header_info(&magic.to_le_bytes()), None);
    }

    #[test]
    fn binary_header_info_non_kv3() {
        assert_eq!(binary_header_info(&[0u8; 64]), None);
    }

    /// Hand-assembled tiny v5 document, byte by byte, to pin the layout independently of
    /// `test_writer.rs`. Root is `{ a = 42 }` (one Int32 member), uncompressed, no blobs.
    /// Field order/sizes per FORMATS.md 3.6 and 3.9.
    #[test]
    fn hand_assembled_v5_minimal_document() {
        // --- buffer1 ("aux"): strings ("a\0"), padding to align4, then the string count (1). ---
        let mut buffer1 = Vec::new();
        buffer1.extend_from_slice(b"a\0"); // Bytes1: the only string, "a".
        buffer1.extend_from_slice(&[0, 0]); // padding to align offset 2 -> 4.
        buffer1.extend_from_slice(&1i32.to_le_bytes()); // Bytes4: string count = 1.
        assert_eq!(buffer1.len(), 8);

        // --- buffer2 ("main"): ObjectLengths(1), Bytes4 = [key_id=0, value=42], types, marker. ---
        let mut buffer2 = Vec::new();
        buffer2.extend_from_slice(&1i32.to_le_bytes()); // ObjectLengths: root has 1 member.
        buffer2.extend_from_slice(&0i32.to_le_bytes()); // Bytes4: key id of "a" (string 0).
        buffer2.extend_from_slice(&42i32.to_le_bytes()); // Bytes4: the member's Int32 value.
        buffer2.push(9); // types: OBJECT (9), no flag.
        buffer2.push(11); // types: INT32 (11), no flag.
        buffer2.extend_from_slice(&0xFFEE_DD00u32.to_le_bytes()); // no-blocks trailer marker.
        assert_eq!(buffer2.len(), 4 + 8 + 2 + 4);

        let mut file = Vec::new();
        file.extend_from_slice(&0x4B56_3305u32.to_le_bytes()); // magic: KV3\x05.
        file.extend_from_slice(&FORMAT_GENERIC.0); // format guid.
        file.extend_from_slice(&0u32.to_le_bytes()); // compression: 0 = none.
        file.extend_from_slice(&0u16.to_le_bytes()); // compressionDictionaryId.
        file.extend_from_slice(&0u16.to_le_bytes()); // compressionFrameSize.
        file.extend_from_slice(&2i32.to_le_bytes()); // countBytes1 (aux): "a\0".
        file.extend_from_slice(&1i32.to_le_bytes()); // countBytes4 (aux): the string count int.
        file.extend_from_slice(&0i32.to_le_bytes()); // countBytes8 (aux).
        file.extend_from_slice(&2i32.to_le_bytes()); // countTypes (v5: types only) = 2 bytes.
        file.extend_from_slice(&0u16.to_le_bytes()); // countObjects (unused by reader).
        file.extend_from_slice(&0u16.to_le_bytes()); // countArrays (unused by reader).
        file.extend_from_slice(&(buffer1.len() as i32 + buffer2.len() as i32).to_le_bytes()); // sizeUncompressedTotal.
        file.extend_from_slice(&(buffer1.len() as i32 + buffer2.len() as i32).to_le_bytes()); // sizeCompressedTotal (== uncompressed here).
        file.extend_from_slice(&0i32.to_le_bytes()); // countBlocks = 0 (no blobs).
        file.extend_from_slice(&0i32.to_le_bytes()); // sizeBinaryBlobsBytes = 0.
        file.extend_from_slice(&0i32.to_le_bytes()); // countBytes2 (aux).
        file.extend_from_slice(&0i32.to_le_bytes()); // sizeBlockCompressedSizesBytes.
        file.extend_from_slice(&(buffer1.len() as i32).to_le_bytes()); // sizeUncompressedBuffer1.
        file.extend_from_slice(&0i32.to_le_bytes()); // sizeCompressedBuffer1 (0: uncompressed).
        file.extend_from_slice(&(buffer2.len() as i32).to_le_bytes()); // sizeUncompressedBuffer2.
        file.extend_from_slice(&0i32.to_le_bytes()); // sizeCompressedBuffer2 (0: uncompressed).
        file.extend_from_slice(&0i32.to_le_bytes()); // countBytes1_buffer2.
        file.extend_from_slice(&0i32.to_le_bytes()); // countBytes2_buffer2.
        file.extend_from_slice(&2i32.to_le_bytes()); // countBytes4_buffer2: key id + value.
        file.extend_from_slice(&0i32.to_le_bytes()); // countBytes8_buffer2.
        file.extend_from_slice(&0i32.to_le_bytes()); // unk13.
        file.extend_from_slice(&1i32.to_le_bytes()); // countObjects_buffer2 = 1 (the root object).
        file.extend_from_slice(&0i32.to_le_bytes()); // countArrays_buffer2.
        file.extend_from_slice(&0i32.to_le_bytes()); // unk16.
        file.extend_from_slice(&buffer1);
        file.extend_from_slice(&buffer2);
        // No blobs, no trailing file-level marker (only present when countBlocks > 0).

        let doc = parse_binary(&file).expect("hand-assembled v5 document should parse");
        assert_eq!(doc.version, Kv3Version::Binary(5));
        assert_eq!(doc.format, FORMAT_GENERIC);
        let mut expected = crate::kv3::Object::new();
        expected.push("a", Value::Int(42));
        assert_eq!(doc.root, Value::Object(expected));
    }

    fn sample_tree(version: u8) -> TestNode {
        let mut entries = vec![
            ("null_val".to_string(), TestNode::Null),
            ("bool_true".to_string(), TestNode::Bool(true)),
            ("bool_false".to_string(), TestNode::Bool(false)),
            ("int_zero".to_string(), TestNode::Int64(0)),
            ("int_one".to_string(), TestNode::Int64(1)),
            ("int_big".to_string(), TestNode::Int64(-123_456_789)),
            ("uint_big".to_string(), TestNode::UInt64(9_999_999_999)),
            ("double_zero".to_string(), TestNode::Double(0.0)),
            ("double_one".to_string(), TestNode::Double(1.0)),
            ("double_val".to_string(), TestNode::Double(12345.6789)),
            (
                "str_val".to_string(),
                TestNode::Str("hello world".to_string()),
            ),
            (
                "str_unicode".to_string(),
                TestNode::Str("unicode: \u{1F600} \u{4e2d}\u{6587}".to_string()),
            ),
            (
                "str_dup".to_string(),
                TestNode::Str("hello world".to_string()),
            ),
            (
                "blob_small".to_string(),
                TestNode::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            ),
            ("array_empty".to_string(), TestNode::Array(vec![])),
            (
                "array_vals".to_string(),
                TestNode::Array(vec![
                    TestNode::Int64(1),
                    TestNode::Str("x".to_string()),
                    TestNode::Bool(false),
                ]),
            ),
            (
                "array_typed".to_string(),
                TestNode::ArrayTyped(vec![
                    TestNode::Int64(10),
                    TestNode::Int64(20),
                    TestNode::Int64(30),
                ]),
            ),
            (
                "array_bytelen".to_string(),
                TestNode::ArrayByteLen(vec![TestNode::UInt64(1), TestNode::UInt64(2)]),
            ),
            (
                "flagged".to_string(),
                TestNode::Flagged(
                    Flag::Resource,
                    Box::new(TestNode::Str("models/foo.vmdl".to_string())),
                ),
            ),
            (
                "nested_obj".to_string(),
                TestNode::Object(vec![("inner".to_string(), TestNode::Int64(42))]),
            ),
        ];
        if version >= 4 {
            entries.push(("int32".to_string(), TestNode::Int32(-42)));
            entries.push(("uint32".to_string(), TestNode::UInt32(42)));
            entries.push(("int16".to_string(), TestNode::Int16(-7)));
            entries.push(("uint16".to_string(), TestNode::UInt16(7)));
            entries.push(("int32_as_byte".to_string(), TestNode::Int32AsByte(200)));
            entries.push(("float_val".to_string(), TestNode::Float(1.5)));
        }
        if version >= 3 {
            entries.push((
                "flagged_entity".to_string(),
                TestNode::Flagged(
                    Flag::EntityName,
                    Box::new(TestNode::Str("ent1".to_string())),
                ),
            ));
        }
        if version >= 2 {
            entries.push(("blob_big".to_string(), TestNode::Blob(vec![7u8; 20_000])));
        }
        if version >= 5 {
            entries.push((
                "array_aux".to_string(),
                TestNode::ArrayAux(vec![
                    TestNode::Int64(1),
                    TestNode::Int64(2),
                    TestNode::Int64(3),
                ]),
            ));
        }
        TestNode::Object(entries)
    }

    fn sample_tree_v0() -> TestNode {
        TestNode::Object(vec![
            ("null_val".into(), TestNode::Null),
            ("bool_true".into(), TestNode::Bool(true)),
            ("int_val".into(), TestNode::Int64(-42)),
            ("uint_val".into(), TestNode::UInt64(42)),
            ("double_val".into(), TestNode::Double(3.5)),
            ("str_val".into(), TestNode::Str("hi".into())),
            ("blob_val".into(), TestNode::Blob(vec![1, 2, 3])),
            (
                "array_val".into(),
                TestNode::Array(vec![TestNode::Int64(1), TestNode::Int64(2)]),
            ),
            (
                "array_typed".into(),
                TestNode::ArrayTyped(vec![TestNode::Int64(1), TestNode::Int64(2)]),
            ),
            ("int32".into(), TestNode::Int32(-7)),
            ("uint32".into(), TestNode::UInt32(7)),
            (
                "flagged".into(),
                TestNode::Flagged(Flag::Resource, Box::new(TestNode::Str("x".into()))),
            ),
        ])
    }

    fn deep_nest(depth: usize) -> TestNode {
        let mut node = TestNode::Int64(1);
        for i in 0..depth {
            node = TestNode::Object(vec![(format!("d{i}"), node)]);
        }
        node
    }

    fn big_array(n: usize) -> TestNode {
        TestNode::Array((0..n).map(|i| TestNode::Int64(i as i64)).collect())
    }

    #[test]
    fn depth_at_limit_does_not_overflow_a_4mib_stack() {
        let tree = deep_nest(RECURSION_LIMIT as usize);
        let bytes = build(&tree, 5, Compression::None, FORMAT_GENERIC);
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse_binary(&bytes).map(|_| ()))
            .expect("spawn probe thread");
        let result = handle
            .join()
            .expect("parser thread must not overflow the stack");
        assert!(
            result.is_ok(),
            "depth {RECURSION_LIMIT} (== RECURSION_LIMIT) should parse successfully: {result:?}"
        );
    }

    #[test]
    fn depth_over_limit_errors_without_overflow() {
        let tree = deep_nest(RECURSION_LIMIT as usize + 50);
        let bytes = build(&tree, 5, Compression::None, FORMAT_GENERIC);
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || parse_binary(&bytes).err().map(|e| e.to_string()))
            .expect("spawn probe thread");
        let err = handle
            .join()
            .expect("parser thread must not overflow the stack");
        assert!(
            matches!(err, Some(ref msg) if msg.contains("recursion")),
            "expected a RecursionLimit error, got {err:?}"
        );
    }

    #[test]
    fn round_trip_all_versions_and_compressions() {
        for version in 1..=5u8 {
            let tree = sample_tree(version);
            let expected = expected_value(&tree);
            for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
                let bytes = build(&tree, version, compression, FORMAT_GENERIC);
                let doc = parse_binary(&bytes)
                    .unwrap_or_else(|e| panic!("v{version} {compression:?}: {e}"));
                assert_eq!(doc.root, expected, "v{version} {compression:?}");
                assert_eq!(doc.version, Kv3Version::Binary(version));
                assert_eq!(doc.format, FORMAT_GENERIC);
            }
        }
    }

    #[test]
    fn round_trip_edge_cases() {
        let handle = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(move || {
                let cases: Vec<(&str, TestNode)> = vec![
                    ("empty_object", TestNode::Object(vec![])),
                    ("root_scalar", TestNode::Int64(-1)),
                    ("deep_nesting", deep_nest(100)),
                    ("big_array", big_array(20_000)),
                    (
                        "multi_blob_chain",
                        TestNode::Object(vec![
                            ("a".to_string(), TestNode::Blob(vec![1u8; 20_000])),
                            ("b".to_string(), TestNode::Blob(vec![2u8; 20_000])),
                            ("c".to_string(), TestNode::Blob(vec![3u8; 5_000])),
                        ]),
                    ),
                ];
                for (name, node) in &cases {
                    let expected = expected_value(node);
                    for version in 1..=5u8 {
                        for compression in [Compression::None, Compression::Lz4, Compression::Zstd]
                        {
                            let bytes = build(node, version, compression, FORMAT_GENERIC);
                            let doc = parse_binary(&bytes).unwrap_or_else(|e| {
                                panic!("{name} v{version} {compression:?}: {e}")
                            });
                            assert_eq!(doc.root, expected, "{name} v{version} {compression:?}");
                        }
                    }
                }
            })
            .expect("spawn probe thread");
        handle
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p));
    }

    #[test]
    fn round_trip_v0() {
        let tree = sample_tree_v0();
        let expected = expected_value(&tree);
        for encoding in [V0Encoding::Binary, V0Encoding::Lz4] {
            let bytes = build_v0(&tree, encoding, FORMAT_GENERIC);
            let doc = parse_binary(&bytes).unwrap_or_else(|e| panic!("{encoding:?}: {e}"));
            assert_eq!(doc.root, expected);
            assert_eq!(doc.version, Kv3Version::Binary(0));
        }
    }

    #[test]
    fn truncated_and_flipped_inputs_never_panic() {
        let mut docs = Vec::new();
        for version in 1..=5u8 {
            for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
                docs.push(build(
                    &sample_tree(version),
                    version,
                    compression,
                    FORMAT_GENERIC,
                ));
            }
        }
        docs.push(build_v0(
            &sample_tree_v0(),
            V0Encoding::Binary,
            FORMAT_GENERIC,
        ));
        docs.push(build_v0(&sample_tree_v0(), V0Encoding::Lz4, FORMAT_GENERIC));

        for doc in &docs {
            let truncate_step = (doc.len() / 37).max(1);
            for len in (0..doc.len()).step_by(truncate_step) {
                let _ = parse_binary(&doc[..len]);
            }
            let flip_step = (doc.len() / 23).max(1);
            for i in (0..doc.len()).step_by(flip_step) {
                let mut flipped = doc.clone();
                flipped[i] ^= 0xFF;
                let _ = parse_binary(&flipped);
            }
        }
    }

    #[test]
    fn arbitrary_garbage_never_panics() {
        let mut state: u32 = 0x9E37_79B9;
        for len in 0..300 {
            let mut input = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                input.push((state >> 24) as u8);
            }
            let _ = parse_binary(&input);
        }
    }

    /// A real CS2 fixture -- pak01
    /// `animation/graphs/viewmodel/viewmodel_inspects.vnmgraph_c` DATA -- is a 7452-byte block
    /// holding the same v5 LZ4 document twice back-to-back (3726 bytes each), with nothing marking
    /// the boundary. VRF doesn't check for trailing bytes after the document; see the comment
    /// above the per-lane `check_exhausted` calls in `parse_v1_5`.
    #[test]
    fn block_with_trailing_bytes_after_document_parses() {
        let tree = sample_tree(5);
        let expected = expected_value(&tree);
        let doc = build(&tree, 5, Compression::Lz4, FORMAT_GENERIC);
        let doubled = [doc.clone(), doc.clone()].concat();
        let parsed = parse_binary(&doubled)
            .expect("a document followed by a byte-for-byte copy of itself should still parse");
        assert_eq!(parsed.root, expected);
        let solo = parse_binary(&doc).expect("the original document parses on its own");
        assert_eq!(parsed.root, solo.root);
    }

    /// Hand-assembles an uncompressed v2 document whose root is a typed array (10) of `bytes4`
    /// values `[string_count=0, k, n, n, ..., n]` (`k` copies of `n`) and whose types lane is
    /// `[10 (root elem type), 10 (outer array's elem type), 1 (inner arrays' elem type, repeated
    /// k times)]`: an outer typed array of `k` typed arrays, each holding `n` `Null`s.
    fn build_nested_null_array_bomb(k: usize, n: i32) -> Vec<u8> {
        let mut bytes4 = vec![0i32, k as i32];
        bytes4.extend(std::iter::repeat_n(n, k));
        let mut types = vec![10u8, 10u8];
        types.extend(std::iter::repeat_n(1u8, k));

        let mut buf = Vec::new();
        for v in &bytes4 {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        buf.extend_from_slice(&types);
        buf.extend_from_slice(&BLOB_MARKER.to_le_bytes());

        let mut f = Vec::new();
        f.extend_from_slice(&0x4B56_3302u32.to_le_bytes()); // magic: KV3\x02, uncompressed.
        f.extend_from_slice(&FORMAT_GENERIC.0);
        f.extend_from_slice(&0u32.to_le_bytes()); // compression: none.
        f.extend_from_slice(&0u16.to_le_bytes()); // compressionDictionaryId.
        f.extend_from_slice(&0u16.to_le_bytes()); // compressionFrameSize.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBytes1.
        f.extend_from_slice(&(bytes4.len() as i32).to_le_bytes()); // countBytes4.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBytes8.
        f.extend_from_slice(&(types.len() as i32).to_le_bytes()); // countTypes.
        f.extend_from_slice(&0u16.to_le_bytes()); // countObjects.
        f.extend_from_slice(&0u16.to_le_bytes()); // countArrays.
        f.extend_from_slice(&(buf.len() as i32).to_le_bytes()); // sizeUncompressedTotal.
        f.extend_from_slice(&(buf.len() as i32).to_le_bytes()); // sizeCompressedTotal.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBlocks.
        f.extend_from_slice(&0i32.to_le_bytes()); // sizeBinaryBlobsBytes.
        f.extend_from_slice(&buf);
        f
    }

    /// Item 2 of the final KV3 hardening review: before `Context::payloadless_budget`, the
    /// payloadless-typed-array bound (`n > max_elems`) was checked per array, so nesting them
    /// multiplied the effective bound. `k=1000` arrays of `n=30000` `Null`s each, from well under
    /// 5 KB of input, used to build 30 million `Value`s; a shared budget must reject this
    /// promptly instead.
    #[test]
    fn nested_payloadless_typed_arrays_are_bounded_by_a_shared_budget() {
        let doc = build_nested_null_array_bomb(1000, 30_000);
        let start = std::time::Instant::now();
        let result = parse_binary(&doc);
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "expected the nested typed-array bomb to be rejected, got {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "rejection took too long: {elapsed:?}"
        );
    }

    /// Hand-assembles a v2 zstd document whose single frame decodes to a valid, complete
    /// `countBlocks == 0` buffer1 (ending in the no-blocks marker) followed by `tail_len` extra
    /// bytes that `sizeBinaryBlobsBytes` claims but that nothing (no blob table, since
    /// `countBlocks == 0`) ever consumes.
    fn build_v2_zstd_with_dangling_blob_tail(tail_len: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0i32.to_le_bytes()); // Bytes4: string count = 0.
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        buf.extend_from_slice(&BLOB_MARKER.to_le_bytes());

        let mut plain = buf.clone();
        plain.extend(std::iter::repeat_n(0xAAu8, tail_len));
        let mut compressed = Vec::new();
        ruzstd::encoding::compress(
            std::io::Cursor::new(&plain),
            &mut compressed,
            ruzstd::encoding::CompressionLevel::Fastest,
        );

        let mut f = Vec::new();
        f.extend_from_slice(&0x4B56_3302u32.to_le_bytes()); // magic: KV3\x02, zstd.
        f.extend_from_slice(&FORMAT_GENERIC.0);
        f.extend_from_slice(&2u32.to_le_bytes()); // compression: zstd.
        f.extend_from_slice(&0u16.to_le_bytes()); // compressionDictionaryId.
        f.extend_from_slice(&0u16.to_le_bytes()); // compressionFrameSize.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBytes1.
        f.extend_from_slice(&1i32.to_le_bytes()); // countBytes4.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBytes8.
        f.extend_from_slice(&0i32.to_le_bytes()); // countTypes.
        f.extend_from_slice(&0u16.to_le_bytes()); // countObjects.
        f.extend_from_slice(&0u16.to_le_bytes()); // countArrays.
        f.extend_from_slice(&(buf.len() as i32).to_le_bytes()); // sizeUncompressedTotal.
        f.extend_from_slice(&(compressed.len() as i32).to_le_bytes()); // sizeCompressedTotal.
        f.extend_from_slice(&0i32.to_le_bytes()); // countBlocks.
        f.extend_from_slice(&(tail_len as i32).to_le_bytes()); // sizeBinaryBlobsBytes.
        f.extend_from_slice(&compressed);
        f
    }

    /// Item 4 of the final KV3 hardening review: a v2-v4 zstd frame decodes buffer1 and the blobs
    /// together; if `countBlocks == 0` (no blob table) but the frame decoded a non-empty tail past
    /// buffer1 anyway (because `sizeBinaryBlobsBytes` was non-zero), that tail must be reported
    /// rather than silently dropped.
    #[test]
    fn v2_zstd_dangling_blob_tail_with_no_blocks_is_rejected() {
        let doc = build_v2_zstd_with_dangling_blob_tail(16);
        match parse_binary(&doc) {
            Err(Kv3Error::TrailingData {
                lane: "zstd blob tail",
                remaining: 16,
            }) => {}
            other => panic!("expected TrailingData on \"zstd blob tail\", got {other:?}"),
        }
    }
}
