//! VPK v1/v2 archives (`*_dir.vpk` + `*_NNN.vpk`, or a single non-split
//! `*.vpk`), read-only.
//!
//! Layout: `docs/FORMATS.md` §1; ValvePak `Package.Read.cs`, `Package.cs`,
//! `PackageEntry.cs` (MIT, see `D:\porject\refs\ValvePak`).

mod platform;
#[cfg(test)]
mod test_writer;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::util::{ReadError, Reader};

/// VPK header magic (`docs/FORMATS.md` §1.1; `Package.cs:16`).
const MAGIC: u32 = 0x55AA_1234;
/// Respawn's customized VPK format (Apex/Titanfall), explicitly rejected
/// (`Package.Read.cs:65-68`).
const RESPAWN_VERSION: u32 = 0x0003_0002;
/// Marks an entry whose bytes live in the `_dir` file itself, at
/// `HeaderSize + TreeSize + EntryOffset` (`Package.Read.cs:150,356,367`).
const ARCHIVE_INDEX_DIR: u16 = 0x7FFF;
/// Every 18-byte tree entry struct ends with this sentinel
/// (`Package.Read.cs:222-226`).
const ENTRY_TERMINATOR: u16 = 0xFFFF;

/// Headroom added to the declared `TreeSize` when retrying a too-small tree buffer (see
/// [`Vpk::open`]). Shrunk to 1 MiB under `cfg(test)` so the test proving the retry is actually
/// capped (`damaged_final_terminator_with_large_dir_data_fails_without_reading_whole_archive`)
/// doesn't need an 80 MiB payload to exceed it.
#[cfg(not(test))]
const RETRY_HEADROOM: u64 = 64 * 1024 * 1024;
#[cfg(test)]
const RETRY_HEADROOM: u64 = 1024 * 1024;

/// Errors from opening or reading a [`Vpk`]. Every variant that names a file
/// carries its path, and entry-specific errors carry the entry's path, per
/// spec.
#[derive(Debug, thiserror::Error)]
pub enum VpkError {
    /// The underlying OS call failed, before any entry was involved (header
    /// or tree I/O).
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The underlying OS call failed while reading a specific entry's data.
    #[error("I/O error reading entry {entry} from {path}: {source}")]
    ReadIo {
        path: PathBuf,
        entry: String,
        #[source]
        source: std::io::Error,
    },
    /// The first 4 bytes were not `0x55AA1234`.
    #[error("bad VPK magic in {path}: expected 0x{expected:08X}, got 0x{actual:08X}")]
    BadMagic {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },
    /// Version is neither 1 nor 2 (or is the Respawn variant `0x00030002`).
    #[error("unsupported VPK version {version} in {path}")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    /// The header or directory tree ran past the end of the file.
    #[error("truncated VPK {path}: {detail}")]
    Truncated { path: PathBuf, detail: String },
    /// An entry's 18-byte struct did not end in `0xFFFF`.
    #[error(
        "bad terminator for entry {entry:?} in {path}: expected 0x{ENTRY_TERMINATOR:04X}, got 0x{actual:04X}"
    )]
    BadTerminator {
        path: PathBuf,
        entry: String,
        actual: u16,
    },
    /// [`Vpk::read_path`] found no entry for the given path.
    #[error("entry not found: {0}")]
    NotFound(String),
    /// An entry references a numbered archive that could not be opened.
    #[error("archive {index} ({path}) referenced by entry {entry} is missing")]
    MissingArchive {
        index: u16,
        path: PathBuf,
        entry: String,
    },
    /// [`Vpk::read_verified`] found the CRC-32 of the read bytes did not
    /// match the entry's recorded checksum.
    #[error(
        "CRC32 mismatch for entry {entry} in {archive}: expected 0x{expected:08X}, got 0x{actual:08X}"
    )]
    CrcMismatch {
        entry: String,
        archive: PathBuf,
        expected: u32,
        actual: u32,
    },
    /// An entry's offset/length falls outside the archive file's bounds.
    #[error(
        "data out of range for entry {entry} in {path}: offset {offset} + length {length} exceeds file size {size}"
    )]
    DataOutOfRange {
        path: PathBuf,
        entry: String,
        offset: u64,
        length: u64,
        size: u64,
    },
}

fn truncated(err: ReadError, path: &Path) -> VpkError {
    VpkError::Truncated {
        path: path.to_path_buf(),
        detail: err.to_string(),
    }
}

/// A single file entry from a VPK directory tree.
#[derive(Debug, Clone)]
pub struct VpkEntry {
    /// Full `/`-separated path as stored in the tree (`dir/name.ext`, with
    /// `" "` placeholders for missing directory/extension dropped, per
    /// `docs/FORMATS.md` §1.2).
    pub path: String,
    pub crc32: u32,
    pub archive_index: u16,
    pub offset: u32,
    pub length: u32,
    /// Bytes stored inline in the tree, immediately preceding the archive
    /// data. Read first when reconstructing the entry's contents.
    pub preload: Vec<u8>,
}

impl VpkEntry {
    /// Total reconstructed length: preload bytes plus archive bytes.
    pub fn total_len(&self) -> u64 {
        self.preload.len() as u64 + u64::from(self.length)
    }

    /// The file extension (without the leading dot), or `""` if none.
    /// Derived from [`VpkEntry::path`], not the raw tree field.
    pub fn extension(&self) -> &str {
        let file_name = self.path.rsplit('/').next().unwrap_or(&self.path);
        match file_name.rsplit_once('.') {
            Some((_, ext)) => ext,
            None => "",
        }
    }
}

/// A read-only, thread-safe view over a VPK archive: the `_dir.vpk` (or a
/// single non-split `.vpk`) plus any numbered archives its entries
/// reference. Archive file handles are opened lazily and cached.
#[derive(Debug)]
pub struct Vpk {
    version: u32,
    /// `HeaderSize + TreeSize` (actual, not the possibly-stale header
    /// field; see `Package.Read.cs:257`) — the base offset of entries in
    /// the dir file's data section.
    data_offset: u64,
    dir_path: PathBuf,
    dir_file: Arc<File>,
    /// The dir path's file name with a trailing `.vpk` and `_dir` stripped
    /// (`Package.Read.cs:434`), kept as an `OsString` (not `String`/
    /// `to_string_lossy`) so a non-Unicode file name round-trips exactly
    /// instead of getting corrupted by lossy re-encoding.
    archive_stem_name: OsString,
    entries: Vec<VpkEntry>,
    /// Lowercased, `/`-normalized full path -> index into `entries`.
    index: HashMap<String, usize>,
    /// Numbered archive files, opened on first use. A read lock is held
    /// only long enough to clone the cached `Arc<File>`; the actual
    /// positional read (`platform::read_exact_at`) happens lock-free.
    archives: RwLock<HashMap<u16, Arc<File>>>,
}

impl Vpk {
    /// Opens a VPK. `path` may be a `..._dir.vpk` (multi-archive) or a
    /// single non-split `....vpk` (e.g. a map VPK). Reads only the header
    /// and directory tree; numbered archives are opened lazily on
    /// [`Vpk::read`].
    pub fn open(path: impl AsRef<Path>) -> Result<Vpk, VpkError> {
        let dir_path = path.as_ref().to_path_buf();
        let file = File::open(&dir_path).map_err(|source| VpkError::Io {
            path: dir_path.clone(),
            source,
        })?;

        // Fixed 12-byte v1 header: magic, version, TreeSize
        // (`docs/FORMATS.md` §1.1; `Package.Read.cs:46-52`).
        let mut header = [0u8; 12];
        read_exact_at_mapped(&file, &mut header, 0, &dir_path)?;
        let mut r = Reader::new(&header);
        let magic = r.u32().expect("fixed-size buffer");
        if magic != MAGIC {
            return Err(VpkError::BadMagic {
                path: dir_path,
                expected: MAGIC,
                actual: magic,
            });
        }
        let version = r.u32().expect("fixed-size buffer");
        let declared_tree_size = u64::from(r.u32().expect("fixed-size buffer"));

        if version == RESPAWN_VERSION || (version != 1 && version != 2) {
            return Err(VpkError::UnsupportedVersion {
                path: dir_path,
                version,
            });
        }

        // v2 has 4 extra u32 section sizes right after TreeSize
        // (`docs/FORMATS.md` §1.1; `Package.Read.cs:55-59`). We only need
        // to know they exist to compute HeaderSize; verify/signature
        // sections are out of scope (constraint: CRC verification is
        // per-entry, `Package.Read.cs:157-166`).
        let header_size: u64 = if version == 2 { 28 } else { 12 };

        let file_len = file
            .metadata()
            .map_err(|source| VpkError::Io {
                path: dir_path.clone(),
                source,
            })?
            .len();

        // The header's TreeSize can be tampered with (too small or too
        // large) and Valve's own reader doesn't actually trust it — it just
        // parses sequentially until it hits the tree's own double-NUL
        // terminator (`Package.Read.cs:174-257`). We honor it as a sizing
        // hint but retry with a bigger buffer if parsing runs off the end of
        // it, so a wrong TreeSize doesn't turn into a spurious failure.
        //
        // The retry cap is bounded well below "rest of the file": a real
        // tree is at most a few tens of MB (`docs/FORMATS.md` §1, `pak01_dir`
        // note), so a damaged tree shouldn't be able to force reading a
        // whole multi-GB archive into memory just to fail anyway.
        let max_tree_buf = file_len.saturating_sub(header_size);
        let retry_cap = max_tree_buf.min(
            declared_tree_size
                .saturating_mul(2)
                .max(declared_tree_size.saturating_add(RETRY_HEADROOM)),
        );
        let mut buf_size = declared_tree_size.min(max_tree_buf);
        let (entries, actual_tree_size) = loop {
            let mut tree_buf = vec![0u8; buf_size as usize];
            read_exact_at_mapped(&file, &mut tree_buf, header_size, &dir_path)?;
            match parse_tree(&tree_buf, &dir_path) {
                Ok(result) => break result,
                // Only a buffer that ran out early is worth retrying with
                // more bytes; any other error (e.g. a bad terminator) is a
                // real format problem no amount of extra buffer fixes.
                Err(VpkError::Truncated { .. }) if buf_size < retry_cap => {
                    buf_size = (buf_size.max(1) * 2).min(retry_cap);
                }
                Err(err) => return Err(err),
            }
        };

        // TreeSize is replaced by the actually-consumed byte count, since
        // that (not the header field) is what data offsets are based on
        // (`docs/FORMATS.md` §1.2 "(!)"; `Package.Read.cs:257`).
        let data_offset = header_size + actual_tree_size as u64;

        // First entry wins on a duplicate lookup key (rather than the tree's
        // last one silently shadowing it), so `find` is deterministic
        // regardless of iteration/hash order.
        let mut index = HashMap::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            index.entry(normalize_lookup(&entry.path)).or_insert(i);
        }

        Ok(Vpk {
            version,
            data_offset,
            archive_stem_name: archive_stem_name(&dir_path),
            dir_file: Arc::new(file),
            dir_path,
            entries,
            index,
            archives: RwLock::new(HashMap::new()),
        })
    }

    /// VPK format version: 1 or 2.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Number of entries in the directory tree.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in tree order (grouped by extension, then directory, then
    /// name — the order Valve's own writer produces them in).
    pub fn entries(&self) -> impl Iterator<Item = &VpkEntry> {
        self.entries.iter()
    }

    /// Looks up an entry by path. Case-insensitive; accepts `\` or `/` as
    /// the separator; leading/trailing separators are trimmed.
    ///
    /// Lookup is by an `O(1)` hash map keyed on the normalized path, built
    /// once in [`Vpk::open`]. Because tree strings with invalid UTF-8 are
    /// decoded lossily (see `read_tree_cstr`), two distinct raw byte
    /// sequences can normalize to the same key; when that happens, the
    /// first entry in tree order wins here and the other is unreachable
    /// through `find`/`read_path`, though it's still present in
    /// [`Vpk::entries`], which is the complete, uncollapsed list.
    pub fn find(&self, path: &str) -> Option<&VpkEntry> {
        let key = normalize_lookup(path);
        self.index.get(&key).map(|&i| &self.entries[i])
    }

    /// Reconstructs an entry's contents: preload bytes followed by the
    /// archive bytes at `entry.offset..entry.offset + entry.length`
    /// (`docs/FORMATS.md` §1.3; `Package.Read.cs:121-160`).
    pub fn read(&self, entry: &VpkEntry) -> Result<Vec<u8>, VpkError> {
        let mut out = entry.preload.clone();
        if entry.length > 0 {
            let (file, base_offset, archive_path) = self.locate(entry)?;
            let file_len = file
                .metadata()
                .map_err(|source| VpkError::ReadIo {
                    path: archive_path.clone(),
                    entry: entry.path.clone(),
                    source,
                })?
                .len();
            let start = base_offset + u64::from(entry.offset);
            let length = u64::from(entry.length);
            let end = start.checked_add(length);
            if end.is_none_or(|end| end > file_len) {
                return Err(VpkError::DataOutOfRange {
                    path: archive_path,
                    entry: entry.path.clone(),
                    offset: start,
                    length,
                    size: file_len,
                });
            }
            let mut buf = vec![0u8; entry.length as usize];
            platform::read_exact_at(&file, &mut buf, start).map_err(|source| VpkError::ReadIo {
                path: archive_path,
                entry: entry.path.clone(),
                source,
            })?;
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }

    /// [`Vpk::read`] plus CRC-32 (IEEE) verification of the reconstructed
    /// bytes (`docs/FORMATS.md` §1.3 "CRC-32 ... считается по всему
    /// восстановленному файлу").
    pub fn read_verified(&self, entry: &VpkEntry) -> Result<Vec<u8>, VpkError> {
        let data = self.read(entry)?;
        let actual = crc32fast::hash(&data);
        if actual != entry.crc32 {
            let archive = if entry.archive_index == ARCHIVE_INDEX_DIR {
                self.dir_path.clone()
            } else {
                self.archive_path(entry.archive_index)
            };
            return Err(VpkError::CrcMismatch {
                entry: entry.path.clone(),
                archive,
                expected: entry.crc32,
                actual,
            });
        }
        Ok(data)
    }

    /// [`Vpk::find`] plus [`Vpk::read`], failing with [`VpkError::NotFound`]
    /// if there is no such entry.
    pub fn read_path(&self, path: &str) -> Result<Vec<u8>, VpkError> {
        let entry = self
            .find(path)
            .ok_or_else(|| VpkError::NotFound(path.to_string()))?;
        self.read(entry)
    }

    /// Every archive file this VPK's entries reference, including the dir
    /// file itself, sorted for determinism.
    pub fn archive_paths(&self) -> Vec<PathBuf> {
        let mut paths = std::collections::BTreeSet::new();
        paths.insert(self.dir_path.clone());
        for entry in &self.entries {
            if entry.archive_index != ARCHIVE_INDEX_DIR {
                paths.insert(self.archive_path(entry.archive_index));
            }
        }
        paths.into_iter().collect()
    }

    /// Resolves an entry to the file to read it from, the base byte offset
    /// within that file, and the file's path (for error messages).
    fn locate(&self, entry: &VpkEntry) -> Result<(Arc<File>, u64, PathBuf), VpkError> {
        if entry.archive_index == ARCHIVE_INDEX_DIR {
            Ok((
                self.dir_file.clone(),
                self.data_offset,
                self.dir_path.clone(),
            ))
        } else {
            let path = self.archive_path(entry.archive_index);
            let file = self.open_archive(entry.archive_index, &path, &entry.path)?;
            Ok((file, 0, path))
        }
    }

    fn archive_path(&self, index: u16) -> PathBuf {
        // `{base}_{index:03}.vpk`, base = dir path minus `.vpk`/`_dir`
        // (`docs/FORMATS.md` §1.3; `Package.Read.cs:434-437`). Built with
        // `with_file_name` off the original dir path so the parent
        // directory is never round-tripped through a `String`.
        let mut name = self.archive_stem_name.clone();
        name.push(format!("_{index:03}.vpk"));
        self.dir_path.with_file_name(name)
    }

    fn open_archive(
        &self,
        index: u16,
        path: &Path,
        entry_path: &str,
    ) -> Result<Arc<File>, VpkError> {
        if let Some(file) = self
            .archives
            .read()
            .expect("archives lock poisoned")
            .get(&index)
        {
            return Ok(file.clone());
        }
        let file = File::open(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                VpkError::MissingArchive {
                    index,
                    path: path.to_path_buf(),
                    entry: entry_path.to_string(),
                }
            } else {
                VpkError::ReadIo {
                    path: path.to_path_buf(),
                    entry: entry_path.to_string(),
                    source,
                }
            }
        })?;
        let file = Arc::new(file);
        self.archives
            .write()
            .expect("archives lock poisoned")
            .insert(index, file.clone());
        Ok(file)
    }
}

/// Reads `buf.len()` bytes at `offset`, mapping a short/EOF read to
/// [`VpkError::Truncated`] and any other I/O failure to [`VpkError::Io`].
fn read_exact_at_mapped(
    file: &File,
    buf: &mut [u8],
    offset: u64,
    path: &Path,
) -> Result<(), VpkError> {
    platform::read_exact_at(file, buf, offset).map_err(|source| {
        if source.kind() == std::io::ErrorKind::UnexpectedEof {
            VpkError::Truncated {
                path: path.to_path_buf(),
                detail: format!("expected {} bytes at offset {offset}", buf.len()),
            }
        } else {
            VpkError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

/// Strips a trailing `.vpk` and then a trailing `_dir` (both
/// case-insensitively) from the dir path's file name, producing the base
/// name numbered archives are derived from (`Package.cs:196-210`,
/// `StripDirVpkSuffixes`). Operates at the code-unit/byte level
/// (`platform::strip_ascii_suffix`) rather than through `to_string_lossy`,
/// so a non-Unicode file name isn't corrupted just to check its extension.
fn archive_stem_name(path: &Path) -> OsString {
    let file_name = path.file_name().unwrap_or_default();
    let mut stem = file_name.to_os_string();
    if let Some(stripped) = platform::strip_ascii_suffix(&stem, ".vpk") {
        stem = stripped;
    }
    if let Some(stripped) = platform::strip_ascii_suffix(&stem, "_dir") {
        stem = stripped;
    }
    stem
}

/// Normalizes a lookup path: `\` -> `/`, trim leading/trailing `/`,
/// lowercase (Valve's own paths are already lowercase; case-insensitivity
/// is a spec requirement, not a format fact).
fn normalize_lookup(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_lowercase()
}

/// Builds the full `/`-separated path from the three tree strings,
/// dropping `" "` placeholders for a missing directory or extension
/// (`docs/FORMATS.md` §1.2).
fn build_full_path(ext: &str, dir: &str, name: &str) -> String {
    let file_name = if ext == " " {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    };
    if dir == " " {
        file_name
    } else {
        format!("{dir}/{file_name}")
    }
}

/// Reads one of the tree's NUL-terminated strings leniently: bytes that
/// aren't valid UTF-8 are replaced (`String::from_utf8_lossy`) rather than
/// treated as a parse failure. Valve's own paths are ASCII, but nothing in
/// `docs/FORMATS.md` §1.2 guarantees that for every producer, and a
/// mis-encoded name shouldn't make the rest of the archive unreadable.
/// `Reader::cstr` validates UTF-8 strictly, so this re-implements the NUL
/// scan with only `Reader`'s public, bounds-checked primitives.
fn read_tree_cstr(r: &mut Reader<'_>, dir_path: &Path) -> Result<String, VpkError> {
    let start = r.pos();
    let mut len = 0usize;
    loop {
        if r.u8().map_err(|e| truncated(e, dir_path))? == 0 {
            break;
        }
        len += 1;
    }
    r.set_pos(start);
    let bytes = r.bytes(len).map_err(|e| truncated(e, dir_path))?;
    r.set_pos(start + len + 1); // skip the NUL
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Parses the directory tree: three nested NUL-terminated-string loops
/// (extension, directory, file name), each ended by an empty string, then
/// the fixed 18-byte entry struct plus its preload bytes
/// (`docs/FORMATS.md` §1.2; `Package.Read.cs:174-258`).
///
/// Returns the parsed entries and the number of bytes actually consumed
/// (used to replace the header's possibly-stale TreeSize).
fn parse_tree(buf: &[u8], dir_path: &Path) -> Result<(Vec<VpkEntry>, usize), VpkError> {
    let mut r = Reader::new(buf);
    let mut entries = Vec::new();

    loop {
        let ext = read_tree_cstr(&mut r, dir_path)?;
        if ext.is_empty() {
            break;
        }
        loop {
            let dir = read_tree_cstr(&mut r, dir_path)?;
            if dir.is_empty() {
                break;
            }
            loop {
                let name = read_tree_cstr(&mut r, dir_path)?;
                if name.is_empty() {
                    break;
                }
                let full_path = build_full_path(&ext, &dir, &name);

                let crc32 = r.u32().map_err(|e| truncated(e, dir_path))?;
                let preload_len = r.u16().map_err(|e| truncated(e, dir_path))? as usize;
                let archive_index = r.u16().map_err(|e| truncated(e, dir_path))?;
                let offset = r.u32().map_err(|e| truncated(e, dir_path))?;
                let length = r.u32().map_err(|e| truncated(e, dir_path))?;
                let terminator = r.u16().map_err(|e| truncated(e, dir_path))?;
                if terminator != ENTRY_TERMINATOR {
                    return Err(VpkError::BadTerminator {
                        path: dir_path.to_path_buf(),
                        entry: full_path,
                        actual: terminator,
                    });
                }
                let preload = r
                    .bytes(preload_len)
                    .map_err(|e| truncated(e, dir_path))?
                    .to_vec();

                entries.push(VpkEntry {
                    path: full_path,
                    crc32,
                    archive_index,
                    offset,
                    length,
                    preload,
                });
            }
        }
    }

    Ok((entries, r.pos()))
}
