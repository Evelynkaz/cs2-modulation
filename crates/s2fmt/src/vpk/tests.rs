use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::test_writer::{TestEntry, write_vpk};
use super::{Vpk, VpkError};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A `std::env::temp_dir()` subdirectory unique to one test, removed on
/// drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "s2fmt-vpk-test-{name}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample_data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
}

#[test]
fn v1_dir_only_archive() {
    let dir = TempDir::new("v1-dir-only");
    let path = dir.join("test.vpk");
    let a = sample_data(100, 1);
    let b = sample_data(50, 2);
    write_vpk(
        &path,
        1,
        &[
            TestEntry::new("models/foo.vmdl", a.clone()),
            TestEntry::new("materials/bar.vmat", b.clone()),
        ],
    );

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.version(), 1);
    assert_eq!(vpk.len(), 2);
    assert!(!vpk.is_empty());

    assert_eq!(
        vpk.read_verified(vpk.find("models/foo.vmdl").unwrap())
            .unwrap(),
        a
    );
    assert_eq!(
        vpk.read_verified(vpk.find("materials/bar.vmat").unwrap())
            .unwrap(),
        b
    );
}

#[test]
fn v2_dir_only_archive() {
    let dir = TempDir::new("v2-dir-only");
    let path = dir.join("test.vpk");
    let data = sample_data(4096, 7);
    write_vpk(&path, 2, &[TestEntry::new("a/b/c.txt", data.clone())]);

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.version(), 2);
    assert_eq!(
        vpk.read_verified(vpk.find("a/b/c.txt").unwrap()).unwrap(),
        data
    );
}

#[test]
fn multi_archive() {
    let dir = TempDir::new("multi-archive");
    let path = dir.join("pak01_dir.vpk");
    let in_dir = sample_data(30, 3);
    let in_000 = sample_data(1000, 4);
    let in_001 = sample_data(2000, 5);
    write_vpk(
        &path,
        2,
        &[
            TestEntry::new("dir_file.dat", in_dir.clone()),
            TestEntry::new("chunk0.dat", in_000.clone()).archive(0),
            TestEntry::new("chunk1.dat", in_001.clone()).archive(1),
        ],
    );

    assert!(dir.join("pak01_000.vpk").exists());
    assert!(dir.join("pak01_001.vpk").exists());

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(
        vpk.read_verified(vpk.find("dir_file.dat").unwrap())
            .unwrap(),
        in_dir
    );
    assert_eq!(
        vpk.read_verified(vpk.find("chunk0.dat").unwrap()).unwrap(),
        in_000
    );
    assert_eq!(
        vpk.read_verified(vpk.find("chunk1.dat").unwrap()).unwrap(),
        in_001
    );

    let mut archive_paths = vpk.archive_paths();
    archive_paths.sort();
    let mut expected = vec![
        path.clone(),
        dir.join("pak01_000.vpk"),
        dir.join("pak01_001.vpk"),
    ];
    expected.sort();
    assert_eq!(archive_paths, expected);
}

#[test]
fn preload_only_entries() {
    let dir = TempDir::new("preload-only");
    let path = dir.join("test.vpk");
    let data = sample_data(16, 9);
    write_vpk(
        &path,
        2,
        &[TestEntry::new("small.txt", data.clone()).preload(16)],
    );

    let vpk = Vpk::open(&path).unwrap();
    let entry = vpk.find("small.txt").unwrap();
    assert_eq!(entry.length, 0);
    assert_eq!(entry.preload.len(), 16);
    assert_eq!(vpk.read_verified(entry).unwrap(), data);
}

#[test]
fn preload_plus_archive_data() {
    let dir = TempDir::new("preload-plus-archive");
    let path = dir.join("test.vpk");
    let data = sample_data(200, 11);
    write_vpk(
        &path,
        2,
        &[TestEntry::new("mixed.bin", data.clone()).preload(37)],
    );

    let vpk = Vpk::open(&path).unwrap();
    let entry = vpk.find("mixed.bin").unwrap();
    assert_eq!(entry.preload.len(), 37);
    assert_eq!(entry.length as usize, 200 - 37);
    assert_eq!(vpk.read_verified(entry).unwrap(), data);
}

#[test]
fn empty_extension_and_directory_placeholders() {
    let dir = TempDir::new("empty-placeholders");
    let path = dir.join("test.vpk");
    let data = sample_data(10, 13);
    write_vpk(&path, 2, &[TestEntry::new("noext", data.clone())]);

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.entries().next().unwrap().path, "noext");
    assert_eq!(vpk.read_verified(vpk.find("noext").unwrap()).unwrap(), data);
}

#[test]
fn files_with_no_extension() {
    let dir = TempDir::new("no-extension");
    let path = dir.join("test.vpk");
    write_vpk(
        &path,
        2,
        &[TestEntry::new("some/dir/readme", vec![1, 2, 3])],
    );

    let vpk = Vpk::open(&path).unwrap();
    let entry = vpk.find("some/dir/readme").unwrap();
    assert_eq!(entry.extension(), "");
    assert_eq!(entry.path, "some/dir/readme");
}

#[test]
fn case_insensitive_find_with_backslashes() {
    let dir = TempDir::new("case-insensitive-find");
    let path = dir.join("test.vpk");
    write_vpk(
        &path,
        2,
        &[TestEntry::new("maps/de_mirage.vpk_info", vec![9])],
    );

    let vpk = Vpk::open(&path).unwrap();
    assert!(vpk.find("MAPS\\DE_MIRAGE.VPK_INFO").is_some());
    assert!(vpk.find("/maps/de_mirage.vpk_info/").is_some());
    assert!(vpk.find("maps\\de_mirage.vpk_info").is_some());
    assert!(vpk.find("nope").is_none());
}

#[test]
fn crc_mismatch_detection() {
    let dir = TempDir::new("crc-mismatch");
    let path = dir.join("test.vpk");
    let data = sample_data(64, 21);
    write_vpk(&path, 2, &[TestEntry::new("corrupt.bin", data)]);

    // Corrupt one byte of the dir file's data section (well past the
    // header/tree, inside the entry's archive bytes).
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let vpk = Vpk::open(&path).unwrap();
    let entry = vpk.find("corrupt.bin").unwrap();
    match vpk.read_verified(entry) {
        Err(VpkError::CrcMismatch { .. }) => {}
        other => panic!("expected CrcMismatch, got {other:?}"),
    }
}

#[test]
fn missing_numbered_archive() {
    let dir = TempDir::new("missing-archive");
    let path = dir.join("pak01_dir.vpk");
    write_vpk(
        &path,
        2,
        &[TestEntry::new("chunk.dat", sample_data(10, 1)).archive(0)],
    );

    // Delete the archive chunk that was just written.
    std::fs::remove_file(dir.join("pak01_000.vpk")).unwrap();

    let vpk = Vpk::open(&path).unwrap();
    let entry = vpk.find("chunk.dat").unwrap();
    match vpk.read(entry) {
        Err(VpkError::MissingArchive { index: 0, .. }) => {}
        other => panic!("expected MissingArchive, got {other:?}"),
    }
}

#[test]
fn bad_terminator() {
    let dir = TempDir::new("bad-terminator");
    let path = dir.join("test.vpk");
    write_vpk(&path, 2, &[TestEntry::new("a.txt", sample_data(4, 1))]);

    let mut bytes = std::fs::read(&path).unwrap();
    // Locate the terminator: header (28) + "txt\0" + " \0" (empty dir
    // placeholder) + "a\0" + crc(4) + preload_len(2) + archive(2) +
    // offset(4) + length(4) = 16 bytes, then the 2-byte terminator.
    let header_size = 28usize;
    let terminator_offset = header_size + "txt\0".len() + " \0".len() + "a\0".len() + 16;
    assert_eq!(
        u16::from_le_bytes([bytes[terminator_offset], bytes[terminator_offset + 1]]),
        0xFFFF
    );
    bytes[terminator_offset] = 0x00;
    bytes[terminator_offset + 1] = 0x00;
    std::fs::write(&path, &bytes).unwrap();

    match Vpk::open(&path) {
        Err(VpkError::BadTerminator { actual: 0, .. }) => {}
        other => panic!("expected BadTerminator, got {other:?}"),
    }
}

#[test]
fn truncated_tree() {
    let dir = TempDir::new("truncated-tree");
    let path = dir.join("test.vpk");
    write_vpk(&path, 2, &[TestEntry::new("a.txt", sample_data(4, 1))]);

    let bytes = std::fs::read(&path).unwrap();
    // Cut the file off in the middle of the tree.
    let truncated = &bytes[..bytes.len() - 6];
    std::fs::write(&path, truncated).unwrap();

    match Vpk::open(&path) {
        Err(VpkError::Truncated { .. }) => {}
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn bad_magic() {
    let dir = TempDir::new("bad-magic");
    let path = dir.join("test.vpk");
    write_vpk(&path, 2, &[TestEntry::new("a.txt", sample_data(4, 1))]);

    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    match Vpk::open(&path) {
        Err(VpkError::BadMagic { .. }) => {}
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn respawn_version_rejected() {
    let dir = TempDir::new("respawn-version");
    let path = dir.join("test.vpk");
    write_vpk(&path, 2, &[TestEntry::new("a.txt", sample_data(4, 1))]);

    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&0x0003_0002u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    match Vpk::open(&path) {
        Err(VpkError::UnsupportedVersion {
            version: 0x0003_0002,
            ..
        }) => {}
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn tampered_tree_size_larger_than_actual_still_reads() {
    let dir = TempDir::new("tree-size-larger");
    let path = dir.join("test.vpk");
    let data = sample_data(64, 5);
    write_vpk(&path, 1, &[TestEntry::new("a.bin", data.clone())]);

    // TreeSize is a u32 at offset 8 in both v1 and v2 headers. Inflate it;
    // the dir file has plenty of trailing data-section bytes so this stays
    // within the file.
    let mut bytes = std::fs::read(&path).unwrap();
    let declared = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    bytes[8..12].copy_from_slice(&(declared + 5).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.read_verified(vpk.find("a.bin").unwrap()).unwrap(), data);
}

#[test]
fn tampered_tree_size_smaller_than_actual_still_reads() {
    let dir = TempDir::new("tree-size-smaller");
    let path = dir.join("test.vpk");
    let data = sample_data(64, 6);
    write_vpk(&path, 1, &[TestEntry::new("a.bin", data.clone())]);

    let mut bytes = std::fs::read(&path).unwrap();
    let declared = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert!(declared > 5, "test tree too small to shrink");
    bytes[8..12].copy_from_slice(&(declared - 5).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    // The initial (too-small) buffer read must fail and be retried larger
    // for this to succeed.
    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.read_verified(vpk.find("a.bin").unwrap()).unwrap(), data);
}

#[test]
fn damaged_final_terminator_with_large_dir_data_fails_without_reading_whole_archive() {
    let dir = TempDir::new("damaged-final-terminator");
    let path = dir.join("test.vpk");
    // Large dir-stored payload with no zero byte anywhere, so a runaway
    // tree scan (triggered by the damaged terminator below) can never
    // stumble onto a spurious NUL and must run all the way to the retry
    // cap before giving up — this is what proves the cap actually bounds
    // the retry, rather than it happening to find a terminator early.
    // `RETRY_HEADROOM` is 1 MiB under `cfg(test)` (vs. 64 MiB otherwise), so 3 MiB is well past
    // the cap here without making this test slow.
    let big_len = 3 * 1024 * 1024; // well past the (test-only) retry cap's 1 MiB headroom
    let data = vec![0xABu8; big_len];
    write_vpk(&path, 2, &[TestEntry::new("big.bin", data)]);

    let mut bytes = std::fs::read(&path).unwrap();
    // The tree's very last byte is the empty-extension string that closes
    // the outermost loop (`docs/FORMATS.md` §1.2). Damage it so the outer
    // loop never sees an empty string and keeps scanning into the (NUL-free)
    // archive data that follows.
    let header_size = 28usize;
    let declared_tree_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let last_tree_byte = header_size + declared_tree_size - 1;
    assert_eq!(bytes[last_tree_byte], 0);
    bytes[last_tree_byte] = 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    match Vpk::open(&path) {
        Err(VpkError::Truncated { .. }) => {}
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn uppercase_dir_filename_resolves_numbered_archive() {
    let dir = TempDir::new("uppercase-name");
    let path = dir.join("PAK01_DIR.VPK");
    let data = sample_data(40, 8);
    write_vpk(
        &path,
        2,
        &[TestEntry::new("chunk.dat", data.clone()).archive(0)],
    );

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(
        vpk.read_verified(vpk.find("chunk.dat").unwrap()).unwrap(),
        data
    );
}

#[test]
fn non_ascii_dir_filename() {
    let dir = TempDir::new("non-ascii-name");
    let path = dir.join("地图.vpk");
    let data = sample_data(20, 9);
    write_vpk(&path, 2, &[TestEntry::new("a.bin", data.clone())]);

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(vpk.read_verified(vpk.find("a.bin").unwrap()).unwrap(), data);
}

#[test]
fn non_ascii_dir_filename_with_numbered_archive() {
    let dir = TempDir::new("non-ascii-name-numbered");
    let path = dir.join("地图_dir.vpk");
    let data = sample_data(40, 10);
    write_vpk(
        &path,
        2,
        &[TestEntry::new("chunk.dat", data.clone()).archive(0)],
    );

    assert!(dir.join("地图_000.vpk").exists());

    let vpk = Vpk::open(&path).unwrap();
    assert_eq!(
        vpk.read_verified(vpk.find("chunk.dat").unwrap()).unwrap(),
        data
    );
}

#[test]
fn concurrent_lazy_archive_open() {
    let dir = TempDir::new("concurrent-lazy-open");
    let path = dir.join("pak01_dir.vpk");
    let mut entries = Vec::new();
    let mut expected = Vec::new();
    for i in 0..8u8 {
        let data = sample_data(2000, i);
        entries.push(TestEntry::new(&format!("f{i}.bin"), data.clone()).archive(u16::from(i) % 3));
        expected.push(data);
    }
    write_vpk(&path, 2, &entries);

    let vpk = Vpk::open(&path).unwrap();
    std::thread::scope(|scope| {
        for i in 0..8u8 {
            let vpk = &vpk;
            let expected = &expected[i as usize];
            scope.spawn(move || {
                let entry = vpk.find(&format!("f{i}.bin")).unwrap();
                let data = vpk.read_verified(entry).unwrap();
                assert_eq!(&data, expected);
            });
        }
    });
}

#[test]
fn fuzz_truncations_and_bit_flips_never_panic() {
    let dir = TempDir::new("fuzz");
    let path = dir.join("test.vpk");
    write_vpk(
        &path,
        2,
        &[
            TestEntry::new("a/b.txt", sample_data(50, 1)).preload(10),
            TestEntry::new("c.dat", sample_data(30, 2)).archive(0),
        ],
    );
    let original = std::fs::read(&path).unwrap();

    for cut in 0..original.len() {
        std::fs::write(&path, &original[..cut]).unwrap();
        let _ = Vpk::open(&path); // must not panic
    }

    for bit in 0..original.len() {
        let mut bytes = original.clone();
        bytes[bit] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        if let Ok(vpk) = Vpk::open(&path) {
            for entry in vpk.entries() {
                let _ = vpk.read_verified(entry); // must not panic
            }
        }
    }

    std::fs::write(&path, &original).unwrap();
}

#[test]
fn thread_safety_concurrent_reads() {
    let dir = TempDir::new("thread-safety");
    let path = dir.join("test.vpk");
    let mut entries = Vec::new();
    for i in 0..8u8 {
        entries.push(TestEntry::new(
            &format!("file{i}.bin"),
            sample_data(5000, i),
        ));
    }
    write_vpk(&path, 2, &entries);

    let vpk = Vpk::open(&path).unwrap();
    std::thread::scope(|scope| {
        for i in 0..8u8 {
            let vpk = &vpk;
            scope.spawn(move || {
                let expected = sample_data(5000, i);
                for _ in 0..20 {
                    let entry = vpk.find(&format!("file{i}.bin")).unwrap();
                    let data = vpk.read_verified(entry).unwrap();
                    assert_eq!(data, expected);
                }
            });
        }
    });
}
