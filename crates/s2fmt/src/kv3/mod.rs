//! KeyValues3: binary (v0-v5) and text formats. See `docs/FORMATS.md` sections 3 and 4.

mod binary;
mod guid;
#[cfg(test)]
pub(crate) mod test_writer;
mod text;
mod value;

pub use binary::{BinaryHeaderInfo, binary_header_info, is_binary_kv3, parse_binary};
pub use guid::{
    ENCODING_BINARY, ENCODING_BINARY_BC, ENCODING_BINARY_LZ4, ENCODING_TEXT, FORMAT_GENERIC, Guid,
};
pub use text::{parse_text, to_text};
pub use value::{Flag, Object, Value};

/// The KV3 version of a document: a binary version (1-5, or 0 for legacy) or text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kv3Version {
    Binary(u8),
    Text,
}

/// A parsed KV3 document.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub format: Guid,
    /// The v0 encoding GUID (`binary_bc`/`binary_lz4`/`binary`), if this was a v0 document.
    pub encoding: Option<Guid>,
    pub version: Kv3Version,
    pub root: Value,
}

/// An error parsing a binary or text KV3 document.
#[derive(Debug, thiserror::Error)]
pub enum Kv3Error {
    #[error("not a binary KV3 document: bad magic {magic:#010x}")]
    BadMagic { magic: u32 },
    #[error("unsupported binary KV3 version {version}")]
    UnsupportedVersion { version: u32 },
    #[error("unknown v0 KV3 encoding GUID {guid}")]
    UnknownEncoding { guid: Guid },
    #[error("unknown compression method {method} (version {version})")]
    UnknownCompression { method: u32, version: u32 },
    #[error("unexpected value for {context}: {value}")]
    UnexpectedValue { context: &'static str, value: i64 },
    #[error("unexpected KV3 flag value {value} ({context})")]
    BadFlag { value: u32, context: &'static str },
    #[error("unknown KV3 node type {value} at {context}")]
    UnknownNodeType { value: u8, context: &'static str },
    #[error("KV3 node type 22 is reserved/unsupported")]
    ReservedNodeType22,
    #[error("string id {id} out of range (have {count} strings)")]
    StringIdOutOfRange { id: i32, count: usize },
    #[error("KV3 nesting exceeds the recursion limit of {limit}")]
    RecursionLimit { limit: u32 },
    #[error("lane '{lane}' has {remaining} unconsumed bytes after parsing")]
    TrailingData {
        lane: &'static str,
        remaining: usize,
    },
    #[error("bad marker at {context}: expected {expected:#010x}, got {actual:#010x}")]
    BadMarker {
        context: &'static str,
        expected: u32,
        actual: u32,
    },
    #[error("truncated binary KV3 data at {context}: {source}")]
    Truncated {
        context: &'static str,
        #[source]
        source: crate::util::ReadError,
    },
    #[error("decompression failed for {context}: {source}")]
    Decompress {
        context: &'static str,
        #[source]
        source: crate::compress::DecompressError,
    },
    #[error("invalid UTF-8 string at {context}")]
    InvalidUtf8 { context: &'static str },
    #[error("text KV3 parse error at {pos}: {message}")]
    TextParse { pos: usize, message: String },
}
