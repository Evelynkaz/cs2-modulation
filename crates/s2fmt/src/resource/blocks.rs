//! The resource container's block table: `FourCC` tags and `Block` entries
//! (`docs/FORMATS.md` section 2.2; `VRF/Resource/Resource.cs`).

use std::fmt;

/// A 4-byte block tag, stored and compared as the raw bytes that appear in
/// the file (not a little-endian integer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    /// Main payload block (KV3 or NTRO).
    pub const DATA: FourCC = FourCC(*b"DATA");
    /// `PhysAggregateData` (KV3).
    pub const PHYS: FourCC = FourCC(*b"PHYS");
    /// Model control block: `embedded_meshes`, `embedded_physics`, ... (KV3).
    pub const CTRL: FourCC = FourCC(*b"CTRL");
    /// External resource references (`RERL`), used by NTRO (out of scope here).
    pub const RERL: FourCC = FourCC(*b"RERL");
    /// Legacy binary edit info.
    pub const REDI: FourCC = FourCC(*b"REDI");
    /// KV3 edit info (compiler identifier, used to disambiguate resource type).
    pub const RED2: FourCC = FourCC(*b"RED2");
    /// Schema for a binary `DATA` block (legacy fallback, not decoded here).
    pub const NTRO: FourCC = FourCC(*b"NTRO");
    /// Legacy struct data block.
    pub const MDAT: FourCC = FourCC(*b"MDAT");
}

impl fmt::Display for FourCC {
    /// ASCII, with each non-printable byte shown as `\xNN`
    /// (`docs/FORMATS.md` section 2.2).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02X}")?;
            }
        }
        Ok(())
    }
}

/// One entry from the block table, in table (raw) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    /// The block's 4-byte tag (`docs/FORMATS.md` section 2.2/2.3).
    pub fourcc: FourCC,
    /// Index into the raw table (all entries, including size-0 ones).
    pub raw_index: usize,
    /// Index into the size-0-filtered list, `None` when `size == 0`
    /// (`docs/FORMATS.md` section 2.2 "(!)"; `Resource.cs:225-228`).
    pub filtered_index: Option<usize>,
    /// Absolute byte offset into the resource's bytes. For a zero-size
    /// block this is computed from the table's `RelOffset` but never
    /// bounds-checked (see [`super::Resource::block_bytes`]).
    pub offset: usize,
    /// The block's byte length, `0` for an empty table entry.
    pub size: usize,
}
