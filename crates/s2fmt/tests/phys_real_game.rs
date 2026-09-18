//! Tests against a real CS2 install. Ignored by default; run with `CS2_GAME_DIR` pointing at
//! `...\game\csgo`, e.g.:
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2fmt --release --test phys_real_game -- --ignored --nocapture`
//!
//! KV3 parsing needs a >= 4 MiB stack (`stage2_common.md`); every test here does its real work on
//! a spawned thread with 16 MiB, matching the CLI.

use std::collections::BTreeMap;
use std::path::PathBuf;

use s2fmt::phys::{self, Hull, Mesh, PhysAggregate};
use s2fmt::resource::Resource;
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

/// Runs `f` on a freshly spawned 16 MiB thread (KV3 parsing needs >= 4 MiB; cargo test threads
/// are 2 MiB), matching the CLI's own thread stack size.
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(f)
        .expect("spawn 16 MiB thread")
        .join()
        .expect("thread panicked");
}

fn validate_and_count(agg: &PhysAggregate) -> (usize, usize, usize) {
    let mut hull_triangles = 0usize;
    let mut mesh_triangles = 0usize;
    let mut hull_flag_histogram: BTreeMap<u32, usize> = BTreeMap::new();
    for part in &agg.parts {
        for hull_desc in &part.shape.hulls {
            let hull: &Hull = &hull_desc.shape;
            hull.validate()
                .unwrap_or_else(|e| panic!("hull failed to validate: {e}"));
            let tris = hull
                .triangles()
                .unwrap_or_else(|e| panic!("hull triangulation failed: {e}"));
            for tri in &tris {
                for &idx in tri {
                    assert!(
                        (idx as usize) < hull.positions().len(),
                        "hull triangle index {idx} out of range (have {})",
                        hull.positions().len()
                    );
                }
            }
            hull_triangles += tris.len();
            *hull_flag_histogram.entry(hull.flags).or_insert(0) += 1;
        }
        for mesh_desc in &part.shape.meshes {
            let mesh: &Mesh = &mesh_desc.shape;
            for tri in &mesh.triangles {
                for &idx in tri {
                    assert!(
                        (idx as usize) < mesh.vertices.len(),
                        "mesh triangle index {idx} out of range (have {})",
                        mesh.vertices.len()
                    );
                }
            }
            mesh_triangles += mesh.triangles.len();
        }
    }
    println!("hull flags histogram: {hull_flag_histogram:?}");
    (hull_triangles, mesh_triangles, hull_flag_histogram.len())
}

fn overall_bounds(agg: &PhysAggregate) -> Option<([f32; 3], [f32; 3])> {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for part in &agg.parts {
        for hull_desc in &part.shape.hulls {
            any = true;
            for axis in 0..3 {
                min[axis] = min[axis].min(hull_desc.shape.bounds_min[axis]);
                max[axis] = max[axis].max(hull_desc.shape.bounds_max[axis]);
            }
        }
        for mesh_desc in &part.shape.meshes {
            any = true;
            for axis in 0..3 {
                min[axis] = min[axis].min(mesh_desc.shape.min[axis]);
                max[axis] = max[axis].max(mesh_desc.shape.max[axis]);
            }
        }
    }
    any.then_some((min, max))
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_world_physics_decodes_and_validates() {
    on_big_stack(|| {
        let path = game_dir().join("maps").join("de_mirage.vpk");
        let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
        let bytes = vpk
            .read_path("maps/de_mirage/world_physics.vmdl_c")
            .unwrap_or_else(|e| panic!("failed to read world_physics.vmdl_c: {e}"));
        let res = Resource::parse(bytes).expect("Resource::parse");
        let embedded = res
            .embedded_phys()
            .expect("embedded_phys")
            .expect("world_physics.vmdl_c must have embedded PHYS");
        let doc = res.kv3(embedded.block).expect("PHYS block is KV3");

        let agg = phys::decode(&doc.root).expect("phys::decode");

        assert_eq!(agg.parts.len(), 1, "expected 1 part");
        assert_eq!(agg.parts[0].shape.hulls.len(), 1933, "expected 1933 hulls");
        assert_eq!(agg.parts[0].shape.meshes.len(), 8, "expected 8 meshes");
        assert_eq!(
            agg.collision_attributes.len(),
            9,
            "expected 9 collision attributes"
        );
        assert_eq!(
            agg.surface_property_hashes.len(),
            40,
            "expected 40 surface property hashes"
        );

        let (hull_triangles, mesh_triangles, distinct_flags) = validate_and_count(&agg);
        println!("=== de_mirage world_physics.vmdl_c PHYS ===");
        println!("parts: {}", agg.parts.len());
        println!("hulls: {}", agg.parts[0].shape.hulls.len());
        println!("meshes: {}", agg.parts[0].shape.meshes.len());
        println!("collision attributes: {}", agg.collision_attributes.len());
        println!(
            "surface property hashes: {}",
            agg.surface_property_hashes.len()
        );
        println!("total hull triangle count: {hull_triangles}");
        println!("total mesh triangle count: {mesh_triangles}");
        println!("distinct hull flag values: {distinct_flags}");
        if let Some((min, max)) = overall_bounds(&agg) {
            println!("overall bounds: min={min:?} max={max:?}");
        }
    });
}

/// Decodes the PHYS of 20 deterministic `*.vmdl_c` (embedded PHYS) entries from `pak01_dir.vpk`
/// -- all that have embedded PHYS must decode and validate. `pak01_dir.vpk` itself carries no
/// standalone `*.vphys_c` entries in this build (all its physics data is embedded in `*.vmdl_c`);
/// see `standalone_vphys_files_decode_and_validate` (below) for real `*.vphys_c` coverage.
#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn pak01_embedded_phys_decode_and_validate() {
    on_big_stack(|| {
        let path = game_dir().join("pak01_dir.vpk");
        let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));

        let vmdl_entries: Vec<_> = vpk
            .entries()
            .filter(|e| e.path.ends_with(".vmdl_c"))
            .collect();

        let mut spheres_seen = 0usize;
        let mut capsules_seen = 0usize;
        let mut bind_poses_seen = 0usize;
        let mut vmdl_decoded = 0usize;

        let want = 20usize.min(vmdl_entries.len());
        let stride = vmdl_entries.len().checked_div(want).unwrap_or(1).max(1);
        for entry in vmdl_entries.iter().step_by(stride).take(want) {
            let bytes = vpk
                .read(entry)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", entry.path));
            let res = Resource::parse(bytes)
                .unwrap_or_else(|e| panic!("Resource::parse failed for {}: {e}", entry.path));
            let Some(embedded) = res
                .embedded_phys()
                .unwrap_or_else(|e| panic!("embedded_phys failed for {}: {e}", entry.path))
            else {
                continue;
            };
            let doc = res
                .kv3(embedded.block)
                .unwrap_or_else(|e| panic!("PHYS block not KV3 for {}: {e}", entry.path));
            let agg = phys::decode(&doc.root)
                .unwrap_or_else(|e| panic!("phys::decode failed for {}: {e}", entry.path));
            validate_and_count(&agg);
            for part in &agg.parts {
                spheres_seen += part.shape.spheres.len();
                capsules_seen += part.shape.capsules.len();
            }
            bind_poses_seen += agg.bind_pose.len();
            vmdl_decoded += 1;
        }

        println!("=== pak01_dir.vpk embedded phys spot check ===");
        println!(
            "vmdl_c with embedded PHYS decoded: {vmdl_decoded} (of {} *.vmdl_c entries seen)",
            vmdl_entries.len()
        );
        println!("spheres seen: {spheres_seen}");
        println!("capsules seen: {capsules_seen}");
        println!("bind poses seen: {bind_poses_seen}");
        assert!(
            vmdl_decoded > 0,
            "expected to decode at least one *.vmdl_c PHYS"
        );
    });
}

/// Decodes every standalone `*.vphys_c` entry in `maps/cs_italy.vpk` and
/// `maps/lobby_mapveto.vpk` (real standalone PHYS resources, per `docs/FORMATS.md` 6.1's
/// `world_physics.vphys_c` fallback path) -- all must decode, validate and triangulate.
#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn standalone_vphys_files_decode_and_validate() {
    on_big_stack(|| {
        let mut total_decoded = 0usize;
        for map in ["cs_italy", "lobby_mapveto"] {
            let path = game_dir().join("maps").join(format!("{map}.vpk"));
            let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
            let vphys_entries: Vec<_> = vpk
                .entries()
                .filter(|e| e.path.ends_with(".vphys_c"))
                .collect();
            println!(
                "=== {map}.vpk: {} *.vphys_c entries ===",
                vphys_entries.len()
            );

            for entry in &vphys_entries {
                let bytes = vpk
                    .read(entry)
                    .unwrap_or_else(|e| panic!("failed to read {}: {e}", entry.path));
                let res = Resource::parse(bytes)
                    .unwrap_or_else(|e| panic!("Resource::parse failed for {}: {e}", entry.path));
                let doc = res
                    .data_kv3()
                    .unwrap_or_else(|e| panic!("DATA block not KV3 for {}: {e}", entry.path));
                let agg = phys::decode(&doc.root)
                    .unwrap_or_else(|e| panic!("phys::decode failed for {}: {e}", entry.path));
                let hulls: usize = agg.parts.iter().map(|p| p.shape.hulls.len()).sum();
                let meshes: usize = agg.parts.iter().map(|p| p.shape.meshes.len()).sum();
                println!("  {}: {hulls} hulls, {meshes} meshes", entry.path);
                validate_and_count(&agg);
                total_decoded += 1;
            }
        }
        assert!(
            total_decoded > 0,
            "expected to decode at least one standalone *.vphys_c entry"
        );
    });
}
