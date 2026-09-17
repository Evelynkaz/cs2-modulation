//! Tests against a real CS2 install. Ignored by default; run with
//! `CS2_GAME_DIR` pointing at `...\game\csgo`, e.g.:
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2fmt --test resource_real_game -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use s2fmt::kv3::{self, Value};
use s2fmt::resource::{FourCC, Resource, parse_resource_manifest, resource_type_from_path};
use s2fmt::vpk::Vpk;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test, e.g. \
             `set CS2_GAME_DIR=D:\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo`"
        ),
    }
}

/// Every block whose bytes look like KV3 (`kv3::is_binary_kv3`), parsed
/// strictly; panics with context on the first failure.
fn assert_every_kv3_block_parses(
    path: &str,
    res: &Resource,
    kv3_counts: &mut BTreeMap<(String, u32, i64), usize>,
) {
    for block in res.blocks() {
        if block.size == 0 {
            continue;
        }
        let bytes = res.block_bytes(block);
        if !kv3::is_binary_kv3(bytes) {
            continue;
        }
        kv3::parse_binary(bytes).unwrap_or_else(|e| {
            panic!(
                "KV3 parse failed for {path} block {} (raw index {}): {e}",
                block.fourcc, block.raw_index
            )
        });
        let kv3::BinaryHeaderInfo {
            version,
            compression,
        } = kv3::binary_header_info(bytes).unwrap();
        *kv3_counts
            .entry((
                block.fourcc.to_string(),
                version as u32,
                compression.map(|c| c as i64).unwrap_or(-1),
            ))
            .or_insert(0) += 1;
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_every_resource_parses() {
    let path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut kv3_counts: BTreeMap<(String, u32, i64), usize> = BTreeMap::new();
    let mut zero_size_blocks = Vec::new();
    let mut parsed = 0usize;

    let start = Instant::now();
    for entry in vpk.entries() {
        if !entry.path.ends_with("_c") {
            continue;
        }
        let bytes = vpk
            .read(entry)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", entry.path));
        let res = Resource::parse(bytes)
            .unwrap_or_else(|e| panic!("Resource::parse failed for {}: {e}", entry.path));

        let type_name = format!("{:?}", resource_type_from_path(&entry.path));
        *by_type.entry(type_name).or_insert(0) += 1;

        for block in res.blocks() {
            if block.size == 0 {
                zero_size_blocks.push((entry.path.clone(), block.fourcc, block.raw_index));
            }
        }

        assert_every_kv3_block_parses(&entry.path, &res, &mut kv3_counts);
        parsed += 1;
    }
    let elapsed = start.elapsed();

    println!("=== de_mirage.vpk: {parsed} '*_c' resources parsed in {elapsed:?} ===");
    println!("by resource type:");
    for (ty, count) in &by_type {
        println!("  {ty}: {count}");
    }
    println!("KV3 blocks by (4CC, version, compression):");
    for ((fourcc, version, compression), count) in &kv3_counts {
        let compression_label = if *compression < 0 {
            "n/a".to_string()
        } else {
            compression.to_string()
        };
        println!("  ({fourcc}, v{version}, c={compression_label}): {count}");
    }
    println!("zero-size blocks ({}):", zero_size_blocks.len());
    for (path, fourcc, index) in zero_size_blocks.iter().take(50) {
        println!("  {path} {fourcc} raw_index={index}");
    }
    if zero_size_blocks.len() > 50 {
        println!("  ... and {} more", zero_size_blocks.len() - 50);
    }
}

fn get_str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn world_physics_vmdl_blocks_and_embedded_phys() {
    let path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/world_physics.vmdl_c")
        .unwrap_or_else(|e| panic!("failed to read world_physics.vmdl_c: {e}"));
    assert_eq!(bytes.len(), 3_491_176);

    let res = Resource::parse(bytes).expect("Resource::parse");

    let expected: &[(FourCC, usize)] = &[
        (FourCC::PHYS, 3_488_560),
        (FourCC::CTRL, 183),
        (FourCC::RED2, 1_490),
        (FourCC::DATA, 856),
    ];
    let actual: Vec<(FourCC, usize)> = res.blocks().iter().map(|b| (b.fourcc, b.size)).collect();
    assert_eq!(
        actual, expected,
        "world_physics.vmdl_c's full block list (order, 4CCs and sizes) must match exactly"
    );

    let phys = res
        .embedded_phys()
        .expect("embedded_phys")
        .expect("world_physics.vmdl_c must have embedded PHYS");
    assert_eq!(phys.block.fourcc, FourCC::PHYS);
    println!("embedded_phys index_scheme = {:?}", phys.index_scheme);

    let doc = res.kv3(phys.block).expect("PHYS block is KV3");
    let m_parts = doc
        .root
        .get("m_parts")
        .and_then(|v| v.as_array())
        .expect("m_parts");
    let m_collision_attributes = doc
        .root
        .get("m_collisionAttributes")
        .and_then(|v| v.as_array())
        .expect("m_collisionAttributes");

    println!("=== world_physics.vmdl_c PHYS ===");
    println!("m_parts.len() = {}", m_parts.len());

    let part0_shape = m_parts.first().and_then(|p| p.get("m_rnShape"));
    let hulls = part0_shape
        .and_then(|s| s.get("m_hulls"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let meshes = part0_shape
        .and_then(|s| s.get("m_meshes"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!("part 0: {hulls} hulls, {meshes} meshes");

    println!("m_collisionAttributes ({}):", m_collision_attributes.len());
    for (i, attr) in m_collision_attributes.iter().enumerate() {
        let group = attr
            .get("m_CollisionGroupString")
            .and_then(|v| v.as_str())
            .unwrap_or("Default");
        let interact_as = get_str_list(attr, "m_InteractAsStrings");
        let interact_exclude = get_str_list(attr, "m_InteractExcludeStrings");
        println!(
            "  [{i}] group={group:?} interact_as={interact_as:?} interact_exclude={interact_exclude:?}"
        );
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn world_physics_vrman_manifest_parses() {
    let path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/world_physics.vrman_c")
        .unwrap_or_else(|e| panic!("failed to read world_physics.vrman_c: {e}"));
    let res = Resource::parse(bytes).expect("Resource::parse");

    let expected: &[(FourCC, usize)] =
        &[(FourCC::RERL, 62), (FourCC::RED2, 710), (FourCC::DATA, 54)];
    for (fourcc, size) in expected {
        let block = res
            .block(*fourcc)
            .unwrap_or_else(|| panic!("missing block {fourcc}"));
        assert_eq!(block.size, *size, "size mismatch for block {fourcc}");
    }

    let data_block = res.block(FourCC::DATA).unwrap();
    let lists = parse_resource_manifest(res.block_bytes(data_block))
        .expect("world_physics.vrman_c DATA parses as a resource manifest");

    println!("=== world_physics.vrman_c manifest ===");
    for (i, list) in lists.iter().enumerate() {
        println!("  [{i}]: {list:?}");
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn world_vwrld_data_lists_nodes() {
    let path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage/world.vwrld_c")
        .unwrap_or_else(|e| panic!("failed to read world.vwrld_c: {e}"));
    let res = Resource::parse(bytes).expect("Resource::parse");
    let doc = res.data_kv3().expect("world.vwrld_c DATA is KV3");

    let entity_lumps = get_str_list(&doc.root, "m_entityLumps");
    let world_nodes = doc
        .root
        .get("m_worldNodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| {
                    n.get("m_worldNodePrefix")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    println!("=== world.vwrld_c ===");
    println!("m_entityLumps: {entity_lumps:?}");
    println!(
        "m_worldNodes[*].m_worldNodePrefix ({}): {world_nodes:?}",
        world_nodes.len()
    );
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn pak01_spot_check_300_entries() {
    let path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

    // Deterministic spread: every Nth `*_c` entry in tree order, where N is
    // chosen so ~300 entries are selected across the whole archive.
    let c_entries: Vec<_> = vpk.entries().filter(|e| e.path.ends_with("_c")).collect();
    let want = 300usize.min(c_entries.len());
    let stride = (c_entries.len() / want).max(1);
    let selected: Vec<_> = c_entries.iter().step_by(stride).take(want).collect();

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for entry in &selected {
        let bytes = match vpk.read(entry) {
            Ok(b) => b,
            Err(e) => {
                failures.push((entry.path.clone(), e.to_string()));
                continue;
            }
        };
        let res = match Resource::parse(bytes) {
            Ok(r) => r,
            Err(e) => {
                failures.push((entry.path.clone(), e.to_string()));
                continue;
            }
        };
        for block in res.blocks() {
            if block.size == 0 {
                continue;
            }
            let block_bytes = res.block_bytes(block);
            if !kv3::is_binary_kv3(block_bytes) {
                continue;
            }
            if let Err(e) = kv3::parse_binary(block_bytes) {
                failures.push((
                    format!(
                        "{} block {} raw_index={}",
                        entry.path, block.fourcc, block.raw_index
                    ),
                    e.to_string(),
                ));
            }
        }
        checked += 1;
    }

    println!("=== pak01_dir.vpk: spot-checked {checked} '*_c' entries ===");
    println!("failures ({}):", failures.len());
    for (path, err) in &failures {
        println!("  {path}: {err}");
    }
    assert!(
        failures.is_empty(),
        "{} entries in pak01_dir.vpk failed to parse; see stdout for details",
        failures.len()
    );
}

/// Item 1 of the final KV3 hardening review's reproducer: pak01
/// `animation/graphs/viewmodel/viewmodel_inspects.vnmgraph_c` DATA is a 7452-byte block holding
/// the same v5 LZ4 document twice back-to-back (3726 bytes each). Every KV3 block of this
/// specific resource must still parse now that a whole-block trailing-bytes check is no longer
/// applied (see `binary.rs`'s `tests::block_with_trailing_bytes_after_document_parses`).
#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn vnmgraph_document_repeated_in_data_block_parses() {
    let path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let entry_path = "animation/graphs/viewmodel/viewmodel_inspects.vnmgraph_c";
    let bytes = vpk
        .read_path(entry_path)
        .unwrap_or_else(|e| panic!("failed to read {entry_path}: {e}"));
    let res = Resource::parse(bytes)
        .unwrap_or_else(|e| panic!("Resource::parse failed for {entry_path}: {e}"));

    let mut kv3_counts: BTreeMap<(String, u32, i64), usize> = BTreeMap::new();
    assert_every_kv3_block_parses(entry_path, &res, &mut kv3_counts);
    assert!(
        !kv3_counts.is_empty(),
        "expected at least one KV3 block in {entry_path}"
    );
}

/// Item 1 of the final KV3 hardening review: parses every KV3 block of every `*_c` entry in
/// `pak01_dir.vpk` (242,048 blocks as of this writing; the reviewer's scan found exactly one
/// failure, the `vnmgraph_document_repeated_in_data_block_parses` case above). This is slow --
/// run it with `cargo test -p s2fmt --release --test resource_real_game -- --ignored --nocapture
/// pak01_every_kv3_block_parses`.
#[test]
#[ignore = "needs CS2_GAME_DIR; slow, run with --release"]
fn pak01_every_kv3_block_parses() {
    let path = game_dir().join("pak01_dir.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

    let mut kv3_counts: BTreeMap<(String, u32, i64), usize> = BTreeMap::new();
    let mut checked = 0usize;
    let mut failures = Vec::new();

    let start = Instant::now();
    for entry in vpk.entries() {
        if !entry.path.ends_with("_c") {
            continue;
        }
        let bytes = match vpk.read(entry) {
            Ok(b) => b,
            Err(e) => {
                failures.push((entry.path.clone(), e.to_string()));
                continue;
            }
        };
        let res = match Resource::parse(bytes) {
            Ok(r) => r,
            Err(e) => {
                failures.push((entry.path.clone(), e.to_string()));
                continue;
            }
        };
        for block in res.blocks() {
            if block.size == 0 {
                continue;
            }
            let block_bytes = res.block_bytes(block);
            if !kv3::is_binary_kv3(block_bytes) {
                continue;
            }
            match kv3::parse_binary(block_bytes) {
                Ok(_) => {
                    let kv3::BinaryHeaderInfo {
                        version,
                        compression,
                    } = kv3::binary_header_info(block_bytes).unwrap();
                    *kv3_counts
                        .entry((
                            block.fourcc.to_string(),
                            version as u32,
                            compression.map(|c| c as i64).unwrap_or(-1),
                        ))
                        .or_insert(0) += 1;
                }
                Err(e) => failures.push((
                    format!(
                        "{} block {} raw_index={}",
                        entry.path, block.fourcc, block.raw_index
                    ),
                    e.to_string(),
                )),
            }
        }
        checked += 1;
    }
    let elapsed = start.elapsed();

    let total_blocks: usize = kv3_counts.values().sum();
    println!(
        "=== pak01_dir.vpk: {checked} '*_c' entries, {total_blocks} KV3 blocks parsed in {elapsed:?} ==="
    );
    println!("failures ({}):", failures.len());
    for (path, err) in &failures {
        println!("  {path}: {err}");
    }
    assert!(
        failures.is_empty(),
        "{} KV3 blocks in pak01_dir.vpk failed to parse; see stdout for details",
        failures.len()
    );
}
