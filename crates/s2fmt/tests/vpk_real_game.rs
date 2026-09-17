//! Tests against a real CS2 install. Ignored by default; run with
//! `CS2_GAME_DIR` pointing at `...\game\csgo`, e.g.:
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2fmt --test vpk_real_game -- --ignored --nocapture`

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use s2fmt::vpk::Vpk;

/// Returns the `...\game\csgo` directory, panicking with a clear message if
/// `CS2_GAME_DIR` is unset (per spec, since these tests only run when
/// explicitly un-ignored).
fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test, e.g. \
             `set CS2_GAME_DIR=D:\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo`"
        ),
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_reads_and_verifies() {
    let path = game_dir().join("maps").join("de_mirage.vpk");

    let open_start = Instant::now();
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let open_elapsed = open_start.elapsed();

    assert_eq!(vpk.version(), 2);
    assert_eq!(vpk.len(), 757);

    for expected in [
        "maps/de_mirage/world_physics.vmdl_c",
        "maps/de_mirage.nav",
        "maps/de_mirage/entities/default_ents.vents_c",
    ] {
        assert!(
            vpk.find(expected).is_some(),
            "missing expected entry {expected}"
        );
    }

    let crc_start = Instant::now();
    for entry in vpk.entries() {
        vpk.read_verified(entry)
            .unwrap_or_else(|e| panic!("read_verified failed for {}: {e}", entry.path));
    }
    let crc_elapsed = crc_start.elapsed();

    println!(
        "de_mirage.vpk: open {open_elapsed:?}, full CRC pass over {} entries {crc_elapsed:?}",
        vpk.len()
    );
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn pak01_dir_opens_and_spot_checks() {
    let path = game_dir().join("pak01_dir.vpk");

    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    assert!(!vpk.is_empty());

    // archive_paths() includes the dir file itself alongside every numbered
    // archive it references, so the numbered-archive count is one less.
    let archive_paths = vpk.archive_paths();
    let numbered_archive_count = archive_paths.len() - 1;
    println!(
        "pak01_dir.vpk: {} entries across {numbered_archive_count} numbered archives",
        vpk.len()
    );

    // Group by archive index, then pick one entry per distinct archive
    // first (so as many archives as possible are covered), and only after
    // that fill remaining slots with leftover entries, up to 200 total.
    let mut by_archive: BTreeMap<u16, Vec<_>> = BTreeMap::new();
    for entry in vpk.entries() {
        by_archive
            .entry(entry.archive_index)
            .or_default()
            .push(entry);
    }

    let want = 200usize.min(vpk.len());
    let mut selected = Vec::with_capacity(want);
    let mut distinct_archives = BTreeSet::new();

    for (&archive_index, entries) in &by_archive {
        if selected.len() >= want {
            break;
        }
        if let Some(&first) = entries.first() {
            selected.push(first);
            distinct_archives.insert(archive_index);
        }
    }
    'fill: for entries in by_archive.values() {
        for entry in entries.iter().skip(1) {
            if selected.len() >= want {
                break 'fill;
            }
            selected.push(entry);
        }
    }

    for entry in &selected {
        vpk.read_verified(entry)
            .unwrap_or_else(|e| panic!("read_verified failed for {}: {e}", entry.path));
    }

    println!(
        "pak01_dir.vpk: verified {} entries spread across {} distinct archives",
        selected.len(),
        distinct_archives.len()
    );
}
