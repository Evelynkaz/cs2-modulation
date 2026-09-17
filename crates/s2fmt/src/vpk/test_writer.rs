//! Test-only VPK writer, mirroring `Package.Save.cs`'s tree layout, used to
//! build synthetic archives for the unit tests below. Not part of the
//! public API.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// One file to place into a synthetic VPK.
pub(crate) struct TestEntry {
    pub path: String,
    pub data: Vec<u8>,
    preload_len: usize,
    archive: Option<u16>,
}

impl TestEntry {
    pub fn new(path: &str, data: Vec<u8>) -> Self {
        TestEntry {
            path: path.to_string(),
            data,
            preload_len: 0,
            archive: None,
        }
    }

    /// Puts the first `n` bytes of `data` inline in the tree as preload.
    pub fn preload(mut self, n: usize) -> Self {
        self.preload_len = n;
        self
    }

    /// Places the non-preload bytes in numbered archive `index` instead of
    /// the dir file.
    pub fn archive(mut self, index: u16) -> Self {
        self.archive = Some(index);
        self
    }
}

/// Splits `path` into `(ext, dir, name)` tree strings, using `" "` for a
/// missing extension or directory, mirroring `AddFile` in
/// `Package.Save.cs`.
fn split_path(path: &str) -> (String, String, String) {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let (name, ext) = match file.rfind('.') {
        Some(i) => (&file[..i], &file[i + 1..]),
        None => (file, ""),
    };
    let dir = if dir.is_empty() {
        " ".to_string()
    } else {
        dir.to_string()
    };
    let ext = if ext.is_empty() {
        " ".to_string()
    } else {
        ext.to_string()
    };
    (ext, dir, name.to_string())
}

/// Writes a synthetic `version` 1 or 2 VPK to `dir_path`, plus any numbered
/// archives its entries reference (next to `dir_path`, named from its
/// stem).
pub(crate) fn write_vpk(dir_path: &Path, version: u32, entries: &[TestEntry]) {
    // ext -> dir -> entries, grouped as the tree's nested loops require.
    let mut tree: BTreeMap<String, BTreeMap<String, Vec<&TestEntry>>> = BTreeMap::new();
    for entry in entries {
        let (ext, dir, _name) = split_path(&entry.path);
        tree.entry(ext)
            .or_default()
            .entry(dir)
            .or_default()
            .push(entry);
    }

    let mut archive_data: HashMap<u16, Vec<u8>> = HashMap::new();
    let mut dir_data: Vec<u8> = Vec::new();
    let mut tree_bytes: Vec<u8> = Vec::new();

    for (ext, dirs) in &tree {
        tree_bytes.extend_from_slice(ext.as_bytes());
        tree_bytes.push(0);
        for (dir, es) in dirs {
            tree_bytes.extend_from_slice(dir.as_bytes());
            tree_bytes.push(0);
            for entry in es {
                let (_, _, name) = split_path(&entry.path);
                tree_bytes.extend_from_slice(name.as_bytes());
                tree_bytes.push(0);

                let crc = crc32fast::hash(&entry.data);
                let preload_len = entry.preload_len.min(entry.data.len());
                let (preload, rest) = entry.data.split_at(preload_len);
                let archive_index = entry.archive.unwrap_or(0x7FFF);

                let offset = if archive_index == 0x7FFF {
                    dir_data.len() as u32
                } else {
                    archive_data.entry(archive_index).or_default().len() as u32
                };

                tree_bytes.extend_from_slice(&crc.to_le_bytes());
                tree_bytes.extend_from_slice(&(preload_len as u16).to_le_bytes());
                tree_bytes.extend_from_slice(&archive_index.to_le_bytes());
                tree_bytes.extend_from_slice(&offset.to_le_bytes());
                tree_bytes.extend_from_slice(&(rest.len() as u32).to_le_bytes());
                tree_bytes.extend_from_slice(&0xFFFFu16.to_le_bytes());
                tree_bytes.extend_from_slice(preload);

                if archive_index == 0x7FFF {
                    dir_data.extend_from_slice(rest);
                } else {
                    archive_data
                        .entry(archive_index)
                        .or_default()
                        .extend_from_slice(rest);
                }
            }
            tree_bytes.push(0); // end of file-name loop for this directory
        }
        tree_bytes.push(0); // end of directory loop for this extension
    }
    tree_bytes.push(0); // end of extension loop

    // The VPK magic (`docs/FORMATS.md` §1.1); a literal here, not
    // `super::MAGIC`, so these tests don't depend on the reader they're
    // exercising.
    const MAGIC: u32 = 0x55AA_1234;

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(tree_bytes.len() as u32).to_le_bytes());
    if version == 2 {
        out.extend_from_slice(&(dir_data.len() as u32).to_le_bytes()); // FileDataSectionSize
        out.extend_from_slice(&0u32.to_le_bytes()); // ArchiveMD5SectionSize
        out.extend_from_slice(&0u32.to_le_bytes()); // OtherMD5SectionSize
        out.extend_from_slice(&0u32.to_le_bytes()); // SignatureSectionSize
    }
    out.extend_from_slice(&tree_bytes);
    out.extend_from_slice(&dir_data);

    std::fs::write(dir_path, &out).expect("write synthetic vpk");

    for (index, data) in &archive_data {
        let chunk_path = archive_chunk_path(dir_path, *index);
        std::fs::write(chunk_path, data).expect("write synthetic archive chunk");
    }
}

/// Own copy of the `{base}_{index:03}.vpk` naming rule (base = dir path's
/// file name minus `.vpk`/`_dir`, case-insensitively), deliberately not
/// shared with `super::archive_stem_name` so these tests catch the reader
/// disagreeing with the format rather than agreeing with itself. Test paths
/// are always valid Unicode, so plain `String`s are fine here (unlike in
/// the reader, which must also handle non-Unicode paths).
fn archive_chunk_path(dir_path: &Path, index: u16) -> PathBuf {
    let file_name = dir_path
        .file_name()
        .expect("dir path has a file name")
        .to_string_lossy()
        .into_owned();
    let mut stem = file_name.as_str();
    if let Some(s) = strip_suffix_ci(stem, ".vpk") {
        stem = s;
    }
    if let Some(s) = strip_suffix_ci(stem, "_dir") {
        stem = s;
    }
    dir_path.with_file_name(format!("{stem}_{index:03}.vpk"))
}

/// Case-insensitive ASCII suffix strip, using `str::get` (which returns
/// `None` rather than panicking) instead of direct byte-index slicing, so a
/// multi-byte character sitting right where the suffix would start can
/// never cause a boundary panic. Deliberately a separate implementation
/// from `platform::strip_ascii_suffix`, not shared, per this module's doc
/// comment above.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() < suffix.len() {
        return None;
    }
    let split = s.len() - suffix.len();
    let tail = s.get(split..)?;
    if tail.eq_ignore_ascii_case(suffix) {
        s.get(..split)
    } else {
        None
    }
}
