//! Source 2 resource container (`*_c` files). See `docs/FORMATS.md` section
//! 2 and `VRF/Resource/Resource.cs`, `Resource/Enums/BlockType.cs`.

mod blocks;
mod manifest;
#[cfg(test)]
mod tests;
mod types;

use crate::kv3;
use crate::util::{ReadError, Reader};

pub use blocks::{Block, FourCC};
pub use manifest::parse_resource_manifest;
pub use types::{ResourceType, resource_type_from_path};

/// Header version this parser understands (`docs/FORMATS.md` section 2.1).
const HEADER_VERSION: u16 = 12;
/// The first 4 bytes of a VPK archive (`docs/FORMATS.md` section 1.1):
/// rejected early, since `*_c` and `*_dir.vpk` are easy to confuse.
const VPK_MAGIC_BYTES: [u8; 4] = 0x55AA_1234u32.to_le_bytes();
/// The first 4 bytes of a compiled shader file, also rejected early.
const SHADER_MAGIC_BYTES: [u8; 4] = *b"vcs2";

/// Errors parsing or querying a [`Resource`].
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("resource container too short/truncated: {detail}")]
    Truncated { detail: String },
    #[error("this is a VPK archive (magic 0x55AA1234), not a resource container")]
    IsVpk,
    #[error("this is a compiled shader file (magic 'vcs2'), not a resource container")]
    IsShader,
    #[error("unsupported resource header version {actual} (expected {HEADER_VERSION})")]
    UnsupportedHeaderVersion { actual: u16 },
    #[error(
        "block {fourcc} (raw index {index}) at offset {offset} size {size} exceeds buffer of {total} bytes"
    )]
    BlockOutOfRange {
        fourcc: FourCC,
        index: usize,
        offset: usize,
        size: usize,
        total: usize,
    },
    #[error("block {fourcc} (raw index {index}) is not KV3 (magic not recognised)")]
    NotKv3 { fourcc: FourCC, index: usize },
    #[error("resource has no {fourcc} block")]
    MissingBlock { fourcc: FourCC },
    #[error("KV3 error in block {fourcc} (raw index {index}): {source}")]
    Kv3 {
        fourcc: FourCC,
        index: usize,
        #[source]
        source: kv3::Kv3Error,
    },
    #[error(
        "embedded_physics.phys_data_block index {index} does not resolve to a PHYS block (filtered: {filtered:?}, raw: {raw:?})"
    )]
    EmbeddedPhysIndex {
        index: i64,
        filtered: Option<FourCC>,
        raw: Option<FourCC>,
    },
    #[error("resource manifest has unexpected version {version}")]
    ManifestBadVersion { version: i32 },
    /// A malformed value that isn't specifically a truncation (e.g. a
    /// negative count/length field): `docs/FORMATS.md` section 2.5.
    #[error("invalid resource data: {detail}")]
    Invalid { detail: String },
}

pub(super) fn trunc(context: &'static str, source: ReadError) -> ResourceError {
    ResourceError::Truncated {
        detail: format!("{context}: {source}"),
    }
}

/// Which index space [`Resource::embedded_phys`] resolved `phys_data_block`
/// through (`docs/FORMATS.md` section 2.2 "(!)"; `Model.cs:363-365`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScheme {
    /// Resolved via the size-0-filtered block list (VRF's own behaviour).
    Filtered,
    /// Fallback: resolved via the raw (unfiltered) block table.
    Raw,
}

/// The `PHYS` block a model's `CTRL` block points at, plus which index space
/// resolved it.
#[derive(Debug)]
pub struct EmbeddedPhys<'a> {
    /// The resolved `PHYS` block.
    pub block: &'a Block,
    /// Which index space (`filtered` or `raw`) resolved [`Self::block`].
    pub index_scheme: IndexScheme,
}

/// A parsed Source 2 resource container (`*_c` file). Owns the raw bytes;
/// blocks are views into them.
#[derive(Debug)]
pub struct Resource {
    bytes: Vec<u8>,
    file_size: u32,
    header_version: u16,
    version: u16,
    blocks: Vec<Block>,
    /// `Some((header_file_size, actual_len))` when they disagree. Not an
    /// error: some legitimate resources (sounds/textures) have a stale
    /// `FileSize` (`docs/FORMATS.md` section 2.1).
    file_size_mismatch: Option<(u32, usize)>,
}

impl Resource {
    /// Parses a resource container's header and block table
    /// (`docs/FORMATS.md` section 2.1-2.2). Does not decode block contents
    /// (KV3 or otherwise); use [`Resource::kv3`]/[`Resource::data_kv3`] for
    /// that.
    pub fn parse(bytes: Vec<u8>) -> Result<Resource, ResourceError> {
        if bytes.len() < 16 {
            return Err(ResourceError::Truncated {
                detail: format!("header requires 16 bytes, have {}", bytes.len()),
            });
        }
        let magic4: [u8; 4] = bytes[0..4].try_into().expect("checked length");
        if magic4 == VPK_MAGIC_BYTES {
            return Err(ResourceError::IsVpk);
        }
        if magic4 == SHADER_MAGIC_BYTES {
            return Err(ResourceError::IsShader);
        }

        let mut r = Reader::new(&bytes);
        let file_size = r.u32().map_err(|e| trunc("file size", e))?;
        let header_version = r.u16().map_err(|e| trunc("header version", e))?;
        if header_version != HEADER_VERSION {
            return Err(ResourceError::UnsupportedHeaderVersion {
                actual: header_version,
            });
        }
        let version = r.u16().map_err(|e| trunc("version", e))?;
        let block_offset_field_pos = r.pos();
        let block_offset = r.u32().map_err(|e| trunc("block offset", e))? as usize;
        let block_count = r.u32().map_err(|e| trunc("block count", e))? as usize;

        let table_pos = block_offset_field_pos + block_offset;
        let mut tr = Reader::new(&bytes);
        tr.set_pos(table_pos);

        let mut blocks = Vec::with_capacity(block_count.min(1 << 16));
        let mut filtered_i = 0usize;
        for i in 0..block_count {
            let fourcc_bytes = tr.bytes(4).map_err(|e| trunc("block fourcc", e))?;
            let fourcc = FourCC([
                fourcc_bytes[0],
                fourcc_bytes[1],
                fourcc_bytes[2],
                fourcc_bytes[3],
            ]);
            let rel_offset_field_pos = tr.pos();
            let rel_offset = tr.u32().map_err(|e| trunc("block rel offset", e))? as usize;
            let size = tr.u32().map_err(|e| trunc("block size", e))? as usize;
            let offset = rel_offset_field_pos + rel_offset;

            if size > 0 {
                let end = offset.checked_add(size);
                if end.is_none_or(|end| end > bytes.len()) {
                    return Err(ResourceError::BlockOutOfRange {
                        fourcc,
                        index: i,
                        offset,
                        size,
                        total: bytes.len(),
                    });
                }
            }

            let filtered_index = if size > 0 {
                let idx = filtered_i;
                filtered_i += 1;
                Some(idx)
            } else {
                None
            };

            blocks.push(Block {
                fourcc,
                raw_index: i,
                filtered_index,
                offset,
                size,
            });
        }

        let file_size_mismatch = if file_size as usize != bytes.len() {
            Some((file_size, bytes.len()))
        } else {
            None
        };

        Ok(Resource {
            bytes,
            file_size,
            header_version,
            version,
            blocks,
            file_size_mismatch,
        })
    }

    /// The header's `FileSize` field. May disagree with the actual byte
    /// length; see [`Resource::file_size_mismatch`].
    pub fn file_size(&self) -> u32 {
        self.file_size
    }

    /// The container's header version. Always [`HEADER_VERSION`] (12) for a
    /// successfully-[`parse`](Resource::parse)d resource.
    pub fn header_version(&self) -> u16 {
        self.header_version
    }

    /// The resource's own type-specific version (`docs/FORMATS.md`
    /// section 2.1), distinct from the header version.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// `Some((header FileSize, actual byte length))` if they disagree; not
    /// an error (`docs/FORMATS.md` section 2.1).
    pub fn file_size_mismatch(&self) -> Option<(u32, usize)> {
        self.file_size_mismatch
    }

    /// All table entries, in table order, including zero-size ones.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The first block with the given 4CC and `size > 0`.
    pub fn block(&self, fourcc: FourCC) -> Option<&Block> {
        self.blocks
            .iter()
            .find(|b| b.fourcc == fourcc && b.size > 0)
    }

    /// The raw bytes of a block. Non-empty blocks were bounds-checked
    /// against these same bytes in [`Resource::parse`]; a zero-size block's
    /// `offset`/`RelOffset` is *not* validated (nothing reads it), so an
    /// out-of-range zero-size block yields `&[]` rather than panicking.
    pub fn block_bytes(&self, block: &Block) -> &[u8] {
        self.bytes
            .get(block.offset..block.offset.saturating_add(block.size))
            .unwrap_or(&[])
    }

    /// The block whose `filtered_index` (size-0 entries removed) equals `i`
    /// (VRF's own indexing semantics, `docs/FORMATS.md` section 2.2 "(!)").
    pub fn block_by_filtered_index(&self, i: usize) -> Option<&Block> {
        self.blocks.iter().find(|b| b.filtered_index == Some(i))
    }

    /// The block whose `raw_index` (position in the unfiltered table) equals
    /// `i`.
    pub fn block_by_raw_index(&self, i: usize) -> Option<&Block> {
        self.blocks.iter().find(|b| b.raw_index == i)
    }

    /// Parses a block's bytes as KV3. Errors with [`ResourceError::NotKv3`]
    /// if the bytes don't start with a recognised KV3 magic.
    pub fn kv3(&self, block: &Block) -> Result<kv3::Document, ResourceError> {
        let bytes = self.block_bytes(block);
        if !kv3::is_binary_kv3(bytes) {
            return Err(ResourceError::NotKv3 {
                fourcc: block.fourcc,
                index: block.raw_index,
            });
        }
        kv3::parse_binary(bytes).map_err(|source| ResourceError::Kv3 {
            fourcc: block.fourcc,
            index: block.raw_index,
            source,
        })
    }

    /// [`Resource::kv3`] on the (first, non-empty) `DATA` block.
    pub fn data_kv3(&self) -> Result<kv3::Document, ResourceError> {
        let block = self
            .block(FourCC::DATA)
            .ok_or(ResourceError::MissingBlock {
                fourcc: FourCC::DATA,
            })?;
        self.kv3(block)
    }

    /// Resolves a model's embedded physics block via its `CTRL` block's
    /// `embedded_physics.phys_data_block` index (`docs/FORMATS.md` section
    /// 2.2 "(!)", 6.1; `Model.cs:353-365`).
    ///
    /// Tries the size-0-filtered index first (VRF's own behaviour), then
    /// falls back to the raw index if that doesn't resolve to a `PHYS`
    /// block. Returns `Ok(None)` if there is no `CTRL` block, or no such
    /// key in it.
    pub fn embedded_phys(&self) -> Result<Option<EmbeddedPhys<'_>>, ResourceError> {
        let ctrl = match self.block(FourCC::CTRL) {
            Some(b) => b,
            None => return Ok(None),
        };
        let doc = self.kv3(ctrl)?;
        let index = match doc
            .root
            .get("embedded_physics")
            .and_then(|v| v.get("phys_data_block"))
            .and_then(|v| v.as_i64())
        {
            Some(i) => i,
            None => return Ok(None),
        };

        let filtered_block = usize::try_from(index)
            .ok()
            .and_then(|i| self.block_by_filtered_index(i));
        if let Some(b) = filtered_block {
            if b.fourcc == FourCC::PHYS {
                return Ok(Some(EmbeddedPhys {
                    block: b,
                    index_scheme: IndexScheme::Filtered,
                }));
            }
        }

        let raw_block = usize::try_from(index)
            .ok()
            .and_then(|i| self.block_by_raw_index(i));
        if let Some(b) = raw_block {
            if b.fourcc == FourCC::PHYS {
                return Ok(Some(EmbeddedPhys {
                    block: b,
                    index_scheme: IndexScheme::Raw,
                }));
            }
        }

        Err(ResourceError::EmbeddedPhysIndex {
            index,
            filtered: filtered_block.map(|b| b.fourcc),
            raw: raw_block.map(|b| b.fourcc),
        })
    }
}
