//! Tests against real Valve resource files. Ignored by default: set `VRF_TEST_FILES` to
//! `<ValveResourceFormat checkout>/Tests/Files` to run them. These files are not copied into
//! this repository.
//!
//! This test file must not depend on `s2fmt::resource`; it walks the `*_c` resource container's
//! block table itself (FORMATS.md section 2).

use s2fmt::kv3::{Object, Value, is_binary_kv3, parse_binary, parse_text, to_text};
use std::fs;
use std::path::{Path, PathBuf};

fn test_files_dir() -> PathBuf {
    let dir = std::env::var("VRF_TEST_FILES")
        .expect("set VRF_TEST_FILES=<ValveResourceFormat checkout>/Tests/Files to run this test");
    PathBuf::from(dir)
}

/// Recursively collects every regular file under `dir`.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files);
        } else {
            files.push(path);
        }
    }
}

/// Walks the resource container's block table (header version 12; FORMATS.md 2.1-2.2) and
/// returns `(FourCC, block bytes)` for every block with a non-zero size. Since this test now
/// walks every file under the test directory (not just filenames containing "kv3"), most files
/// visited are not resource containers at all (images, `.cs` sources, etc); this returns an
/// empty list rather than panicking whenever the header or any table entry doesn't check out.
fn resource_blocks(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    if data.len() < 16 {
        return Vec::new();
    }
    let header_version = u16::from_le_bytes([data[4], data[5]]);
    if header_version != 12 {
        return Vec::new();
    }
    let block_offset = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let block_count = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
    let Some(table_pos) = 8usize.checked_add(block_offset) else {
        return Vec::new();
    };

    let mut blocks = Vec::with_capacity(block_count.min(1024));
    for i in 0..block_count {
        let Some(entry) = table_pos.checked_add(i * 12) else {
            break;
        };
        if entry + 12 > data.len() {
            break;
        }
        let fourcc = [
            data[entry],
            data[entry + 1],
            data[entry + 2],
            data[entry + 3],
        ];
        let rel_offset_pos = entry + 4;
        let rel_offset =
            u32::from_le_bytes(data[rel_offset_pos..rel_offset_pos + 4].try_into().unwrap())
                as usize;
        let size = u32::from_le_bytes(data[entry + 8..entry + 12].try_into().unwrap()) as usize;
        if size == 0 {
            // FORMATS.md 2.2: VRF drops zero-size block entries.
            continue;
        }
        let Some(abs_offset) = rel_offset_pos.checked_add(rel_offset) else {
            continue;
        };
        let Some(end) = abs_offset.checked_add(size) else {
            continue;
        };
        if end > data.len() {
            continue;
        }
        blocks.push((fourcc, &data[abs_offset..end]));
    }
    blocks
}

fn find_block<'a>(data: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    resource_blocks(data)
        .into_iter()
        .find(|(fcc, _)| fcc == fourcc)
        .map(|(_, block)| block)
}

fn count_blobs(v: &Value) -> (usize, usize) {
    match v.unflagged() {
        Value::Blob(b) => (1, b.len()),
        Value::Array(items) => items
            .iter()
            .map(count_blobs)
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        Value::Object(o) => o
            .iter()
            .map(|(_, v)| count_blobs(v))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        _ => (0, 0),
    }
}

#[test]
#[ignore = "needs VRF_TEST_FILES=<ValveResourceFormat>/Tests/Files"]
fn every_kv3_block_parses() {
    let dir = test_files_dir();
    let mut files = Vec::new();
    walk(&dir, &mut files);

    let mut checked = 0usize;
    let mut failures = Vec::new();

    for path in files {
        if !path.is_file() {
            continue;
        }
        let name = path.display().to_string();
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.len() < 16 {
            continue;
        }

        // A whole-file KV3 blob (not wrapped in a resource container) also needs to parse; real
        // fixtures include both forms.
        if is_binary_kv3(&data) {
            checked += 1;
            if let Err(e) = parse_binary(&data) {
                failures.push(format!("{name} (raw): {e}"));
            }
        }

        let mut file_blocks = 0usize;
        let mut file_ok = 0usize;
        for (fourcc, block) in resource_blocks(&data) {
            if block.len() >= 4 && is_binary_kv3(block) {
                checked += 1;
                file_blocks += 1;
                match parse_binary(block) {
                    Ok(_) => file_ok += 1,
                    Err(e) => failures.push(format!("{name} block {fourcc:?}: {e}")),
                }
            }
        }
        if file_blocks > 0 {
            eprintln!("{name}: {file_ok}/{file_blocks} KV3 blocks parsed");
        }
    }

    assert!(
        checked > 0,
        "expected at least one KV3 block under {}",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "{} KV3 blocks failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "needs VRF_TEST_FILES=<ValveResourceFormat>/Tests/Files"]
fn default_ents_kv3_versions_agree() {
    let dir = test_files_dir();
    let files = [
        "default_ents_kv3_v0.vents_c",
        "default_ents_kv3_v1.vents_c",
        "default_ents_kv3_v4_zstd.vents_c",
    ];

    let mut roots = Vec::new();
    for f in files {
        let data = fs::read(dir.join(f)).unwrap_or_else(|e| panic!("read {f}: {e}"));
        let block = find_block(&data, b"DATA").unwrap_or_else(|| panic!("{f}: no DATA block"));
        let doc = parse_binary(block).unwrap_or_else(|e| panic!("{f}: {e}"));
        roots.push((f, doc.root));
    }

    // Normalisation (documented deviation from a naive deep-equality check; see notes in the
    // task receipt): these three fixtures are NOT the same entity data re-encoded per version
    // -- e.g. v0's `m_entityKeyValues` has 22 entries vs v1's 11 -- so a full deep comparison of
    // the roots is not meaningful. Root schemas also differ (v0 lacks `m_hammerUniqueId`, v4
    // lacks `m_flags`). We instead check the fields that are orthogonal to the specific entity
    // content for equality, and separately sanity-check the shape of `m_entityKeyValues`. This
    // still exercises every binary version's decoder (including v4's zstd path) against real
    // data and catches any decoder-introduced divergence in the shared fields.
    for pair in roots.windows(2) {
        for key in ["m_name", "m_manifestName", "m_childLumps"] {
            let (Some(a), Some(b)) = (pair[0].1.get(key), pair[1].1.get(key)) else {
                continue;
            };
            assert_eq!(a, b, "{} vs {}: .{key} differs", pair[0].0, pair[1].0);
        }
    }

    for (name, root) in &roots {
        let entities = root
            .get("m_entityKeyValues")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{name}: missing m_entityKeyValues array"));
        assert!(!entities.is_empty(), "{name}: m_entityKeyValues is empty");
        for entity in entities {
            assert!(
                entity
                    .get("m_keyValuesData")
                    .and_then(Value::as_blob)
                    .is_some(),
                "{name}: entity missing m_keyValuesData blob"
            );
            assert!(
                entity
                    .get("m_connections")
                    .and_then(Value::as_array)
                    .is_some(),
                "{name}: entity missing m_connections array"
            );
        }
    }
}

/// Normalises away the one documented lossy spot in the text grammar (FORMATS.md 4 /
/// `text.rs`'s `roundtrips_scalars_and_containers` note): a non-negative `Value::Int` is
/// indistinguishable from a `Value::UInt` once written as plain digits and re-parsed, since the
/// grammar tries `ulong` before `long` for non-negative tokens.
fn norm(v: &Value) -> Value {
    match v {
        Value::Int(i) if *i >= 0 => Value::UInt(*i as u64),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), norm(v)))
                .collect::<Object>(),
        ),
        Value::Flagged(f, b) => Value::Flagged(*f, Box::new(norm(b))),
        other => other.clone(),
    }
}

#[test]
#[ignore = "needs VRF_TEST_FILES=<ValveResourceFormat>/Tests/Files"]
fn every_text_kv3_file_round_trips() {
    let dir = test_files_dir();
    let mut files = Vec::new();
    walk(&dir, &mut files);

    let bom: &[u8] = &[0xEF, 0xBB, 0xBF];
    let mut checked = 0usize;
    let mut failures = Vec::new();

    for path in files {
        let Ok(mut data) = fs::read(&path) else {
            continue;
        };
        if data.starts_with(bom) {
            data.drain(..3);
        }
        if !data.starts_with(b"<!-- kv3") {
            continue;
        }
        let name = path.display().to_string();
        let Ok(src) = std::str::from_utf8(&data) else {
            failures.push(format!("{name}: not valid UTF-8"));
            continue;
        };

        let doc = match parse_text(src) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{name}: parse_text failed: {e}"));
                continue;
            }
        };
        checked += 1;

        match parse_text(&to_text(&doc)) {
            Ok(back) => {
                if norm(&back.root) != norm(&doc.root) {
                    failures.push(format!("{name}: round-trip root differs"));
                }
            }
            Err(e) => failures.push(format!("{name}: to_text output failed to re-parse: {e}")),
        }
    }

    assert!(
        checked > 0,
        "expected at least one text KV3 file under {}",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "{} text KV3 files failed to round-trip:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "needs VRF_TEST_FILES=<ValveResourceFormat>/Tests/Files"]
fn known_blob_counts_match_vrf_fixture() {
    // Expected values from ValveResourceFormat's KeyValuesTest.cs:405-406.
    let dir = test_files_dir();

    let data = fs::read(dir.join("piece_kv3_v4.vmdl_c")).expect("read piece_kv3_v4.vmdl_c");
    let phys = find_block(&data, b"PHYS").expect("PHYS block");
    let doc = parse_binary(phys).expect("parse PHYS block");
    let (count, total) = count_blobs(&doc.root);
    assert_eq!(count, 7, "piece_kv3_v4.vmdl_c PHYS blob count");
    assert_eq!(total, 1378, "piece_kv3_v4.vmdl_c PHYS total blob bytes");

    let data = fs::read(dir.join("basepostprocess_kv3_v4_uncompressed.vpost_c"))
        .expect("read basepostprocess_kv3_v4_uncompressed.vpost_c");
    let block = find_block(&data, b"DATA").expect("DATA block");
    let doc = parse_binary(block).expect("parse DATA block");
    let (count, total) = count_blobs(&doc.root);
    assert_eq!(count, 1, "basepostprocess DATA blob count");
    assert_eq!(total, 131_072, "basepostprocess DATA blob bytes");
}
