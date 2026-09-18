//! Tests against a real CS2 install and VRF's test fixtures. Both ignored by default.
//!
//! Real game: set `CS2_GAME_DIR` to `...\game\csgo`, e.g.
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2fmt --test entities_real_game -- --ignored --nocapture`
//!
//! VRF fixtures: set `VRF_TEST_FILES` to `<ValveResourceFormat checkout>/Tests/Files`.
//!
//! KV3 parsing recurses and needs a stack bigger than the 2 MiB cargo gives test threads, so
//! every test here runs its body on a 4+ MiB thread.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use s2fmt::entities;
use s2fmt::kv3;
use s2fmt::resource::{Resource, ResourceError};
use s2fmt::vpk::Vpk;
use s2fmt::worldnode;

/// Runs `f` on a thread with a 4 MiB stack (KV3 parsing requirement; see `stage2_common.md`).
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(4 << 20)
        .spawn(f)
        .expect("spawn 4 MiB test thread")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
}

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
fn de_mirage_default_ents() {
    on_big_stack(|| {
        let path = game_dir().join("maps").join("de_mirage.vpk");
        let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
        let bytes = vpk
            .read_path("maps/de_mirage/entities/default_ents.vents_c")
            .expect("default_ents.vents_c");
        let res = Resource::parse(bytes).expect("Resource::parse");
        let doc = res.data_kv3().expect("DATA as kv3");
        let lump = entities::decode_entity_lump(&doc.root).expect("decode_entity_lump");

        println!(
            "default_ents: {} entities (raw m_entityKeyValues may include classname-less ones)",
            lump.entities.len()
        );

        let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
        for e in &lump.entities {
            *by_class.entry(e.classname()).or_insert(0) += 1;
        }
        for (class, count) in &by_class {
            println!("{class:32} {count}");
        }

        assert_eq!(
            lump.entities.len(),
            561,
            "de_mirage default_ents.vents_c entity count"
        );
        assert_eq!(by_class.get("func_brush"), Some(&43));
        assert_eq!(by_class.get("func_clip_vphysics"), Some(&11));

        for e in &lump.entities {
            let class = e.classname();
            let interesting = matches!(
                class,
                "func_brush" | "func_clip_vphysics" | "func_breakable"
            ) || class.starts_with("prop_dynamic");
            if !interesting {
                continue;
            }
            println!(
                "{class:24} targetname={:?} model={:?} origin={:?} angles={:?} scales={:?} \
                 startdisabled={:?}",
                e.targetname(),
                e.get_str("model"),
                e.origin(),
                e.angles(),
                e.scales(),
                e.get_bool("startdisabled"),
            );
            if class == "prop_dynamic" {
                println!("  solid={:?} health={:?}", e.get("solid"), e.get("health"));
            }
        }
    });
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_world_and_node() {
    on_big_stack(|| {
        let path = game_dir().join("maps").join("de_mirage.vpk");
        let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

        let world_bytes = vpk
            .read_path("maps/de_mirage/world.vwrld_c")
            .expect("world.vwrld_c");
        let world_res = Resource::parse(world_bytes).expect("Resource::parse world");
        let world_doc = world_res.data_kv3().expect("world DATA as kv3");
        let world = worldnode::decode_world(&world_doc.root).expect("decode_world");
        println!(
            "world: {} entity lumps, {} node prefixes",
            world.entity_lumps.len(),
            world.world_node_prefixes.len()
        );
        assert_eq!(
            world.world_node_prefixes.len(),
            1,
            "de_mirage node prefixes"
        );

        let node_bytes = vpk
            .read_path("maps/de_mirage/worldnodes/n0.vwnod_c")
            .expect("n0.vwnod_c");
        let node_res = Resource::parse(node_bytes).expect("Resource::parse n0");
        let node_doc = node_res.data_kv3().expect("n0 DATA as kv3");
        let node = worldnode::decode_world_node(&node_doc.root).expect("decode_world_node");

        println!(
            "n0: {} scene objects, {} aggregate scene objects",
            node.scene_objects.len(),
            node.aggregate_scene_objects.len()
        );
        assert_eq!(node.scene_objects.len(), 335, "de_mirage n0 scene objects");
        assert_eq!(
            node.aggregate_scene_objects.len(),
            260,
            "de_mirage n0 aggregate scene objects"
        );

        let mut prefix_histogram: BTreeMap<String, usize> = BTreeMap::new();
        let mut flags_histogram: BTreeMap<i64, usize> = BTreeMap::new();
        for so in &node.scene_objects {
            if let Some(model) = &so.renderable_model
                && let Some(prefix) = model.rsplit_once('/').map(|(dir, _)| dir)
            {
                *prefix_histogram.entry(prefix.to_string()).or_insert(0) += 1;
            }
            *flags_histogram.entry(so.object_type_flags).or_insert(0) += 1;
        }
        println!("renderable model directory prefixes: {prefix_histogram:#?}");
        println!("object_type_flags histogram: {flags_histogram:#?}");
    });
}

fn vrf_test_files_dir() -> PathBuf {
    let dir = std::env::var("VRF_TEST_FILES")
        .expect("set VRF_TEST_FILES=<ValveResourceFormat checkout>/Tests/Files to run this test");
    PathBuf::from(dir)
}

/// Recursively finds the first file named `name` under `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().and_then(|f| f.to_str()) == Some(name) {
            return Some(path);
        }
    }
    subdirs.into_iter().find_map(|d| find_file(&d, name))
}

/// Like [`find_file`], but panics if `name` isn't present anywhere under `dir`: the VRF fixture
/// tests name specific files they expect to exist, not files that are merely optional.
fn find_file_or_panic(dir: &Path, name: &str) -> PathBuf {
    find_file(dir, name)
        .unwrap_or_else(|| panic!("{name} not found anywhere under VRF_TEST_FILES ({dir:?})"))
}

/// `res.data_kv3()`, or `None` with a printed skip message if the DATA block isn't KV3 at all
/// (some older fixtures compile entity lumps as NTRO structs instead; decoding that format is
/// out of this module's scope, which only covers the KV3-tree encoding FORMATS.md section 7
/// describes).
fn data_kv3_or_skip(res: &Resource, path: &Path, name: &str) -> Option<kv3::Document> {
    match res.data_kv3() {
        Ok(doc) => Some(doc),
        Err(ResourceError::NotKv3 { .. }) => {
            println!(
                "skipping {name}: DATA block is not KV3 (likely an older NTRO-encoded resource)"
            );
            None
        }
        Err(e) => panic!("{path:?} DATA as kv3: {e}"),
    }
}

/// Decodes one VRF entity-lump fixture. Panics if the file itself is missing; returns `true` if
/// it decoded (KV3 `DATA`), `false` if it was an NTRO-skip (still allowed, but counted
/// separately — see [`vrf_fixture_entity_lumps`]).
fn decode_vrf_ents_fixture(dir: &Path, name: &str) -> bool {
    let path = find_file_or_panic(dir, name);
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let res = Resource::parse(bytes).unwrap_or_else(|e| panic!("Resource::parse {path:?}: {e}"));
    let Some(doc) = data_kv3_or_skip(&res, &path, name) else {
        return false;
    };
    let lump = entities::decode_entity_lump(&doc.root)
        .unwrap_or_else(|e| panic!("decode_entity_lump {path:?}: {e}"));

    // decode_entity_lump already drops any entity with no classname, so every survivor has one
    // by construction. entities::KNOWN_KEYS resolves classname/etc. when the compiler stored
    // them as unnamed hashed legacy fields (`docs/FORMATS.md` section 7), so these older
    // fixtures should still yield entities, not zero.
    for e in &lump.entities {
        assert!(
            !e.classname().is_empty(),
            "{path:?}: entity with no classname"
        );
    }
    assert!(
        !lump.entities.is_empty(),
        "{path:?}: expected at least one decoded entity"
    );

    let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &lump.entities {
        *by_class.entry(e.classname()).or_insert(0) += 1;
    }
    println!(
        "{name}: {} entities, classes: {by_class:?}",
        lump.entities.len()
    );
    true
}

#[test]
#[ignore = "needs VRF_TEST_FILES"]
fn vrf_fixture_entity_lumps() {
    on_big_stack(|| {
        let dir = vrf_test_files_dir();
        let names = [
            "default_ents_kv3_v0.vents_c",
            "default_ents_kv3_v1.vents_c",
            "default_ents_kv3_v4_zstd.vents_c",
            "dune_test_ents.vents_c",
            "graphics_settings_ents.vents_c",
            "ascent_speedup_switch_template_ents.vents_c",
        ];
        let mut decoded = 0;
        let mut ntro_skipped = 0;
        for name in names {
            if decode_vrf_ents_fixture(&dir, name) {
                decoded += 1;
            } else {
                ntro_skipped += 1;
            }
        }
        println!("vrf_fixture_entity_lumps: {decoded} decoded, {ntro_skipped} NTRO-skipped");
        assert!(
            decoded >= 4,
            "expected at least 4 fixtures to decode via KV3, got {decoded} (+ {ntro_skipped} NTRO-skipped)"
        );
    });
}

#[test]
#[ignore = "needs VRF_TEST_FILES"]
fn vrf_fixture_world_node_and_world() {
    on_big_stack(|| {
        let dir = vrf_test_files_dir();

        let node_name = "node000_kv3_v2_zstd.vwnod_c";
        let node_path = find_file_or_panic(&dir, node_name);
        let bytes = fs::read(&node_path).unwrap_or_else(|e| panic!("read {node_path:?}: {e}"));
        let res =
            Resource::parse(bytes).unwrap_or_else(|e| panic!("Resource::parse {node_path:?}: {e}"));
        if let Some(doc) = data_kv3_or_skip(&res, &node_path, node_name) {
            let node = worldnode::decode_world_node(&doc.root)
                .unwrap_or_else(|e| panic!("decode_world_node {node_path:?}: {e}"));
            println!(
                "{node_name}: {} scene objects, {} aggregates",
                node.scene_objects.len(),
                node.aggregate_scene_objects.len()
            );
        }

        let world_name = "world.vwrld_c";
        let world_path = find_file_or_panic(&dir, world_name);
        let bytes = fs::read(&world_path).unwrap_or_else(|e| panic!("read {world_path:?}: {e}"));
        let res = Resource::parse(bytes)
            .unwrap_or_else(|e| panic!("Resource::parse {world_path:?}: {e}"));
        if let Some(doc) = data_kv3_or_skip(&res, &world_path, world_name) {
            let world = worldnode::decode_world(&doc.root)
                .unwrap_or_else(|e| panic!("decode_world {world_path:?}: {e}"));
            println!(
                "{world_name}: {} entity lumps, {} node prefixes",
                world.entity_lumps.len(),
                world.world_node_prefixes.len()
            );
        }
    });
}
