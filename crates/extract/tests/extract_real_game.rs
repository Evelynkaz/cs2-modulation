//! Real game: set `CS2_GAME_DIR` to `...\game\csgo`, e.g.
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p extract --release --test extract_real_game -- --ignored --nocapture`
//!
//! KV3 parsing needs a >= 4 MiB stack (`stage2_common.md`); every test here does its real work on
//! a spawned thread with 16 MiB, matching the CLI.

use std::path::{Path, PathBuf};

use extract::build::{ExtractOptions, extract_map};
use extract::cache;
use extract::game::GameInstall;
use geom::{filter, obj};

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

/// A `std::env::temp_dir()` subdirectory unique to one test, removed (recursively) on drop, even
/// on panic.
struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!(
        "cs2mod_extract_real_test_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

/// Möller-Trumbore ray/triangle intersection: returns the hit distance along `dir` (from
/// `origin`), if any, ignoring backface/culling (either winding counts).
fn ray_triangle(
    origin: [f32; 3],
    dir: [f32; 3],
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
) -> Option<f32> {
    const EPS: f32 = 1e-6;
    fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }
    fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }
    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    let edge1 = sub(b, a);
    let edge2 = sub(c, a);
    let h = cross(dir, edge2);
    let det = dot(edge1, h);
    if det.abs() < EPS {
        return None;
    }
    let inv_det = 1.0 / det;
    let s = sub(origin, a);
    let u = dot(s, h) * inv_det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, edge1);
    let v = dot(dir, q) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = dot(edge2, q) * inv_det;
    if t > EPS { Some(t) } else { None }
}

/// True if a grenade-solid triangle exists within `max_dist` units straight down from `origin`
/// (brute force over every triangle -- `O(triangles)` per spawn, fine for a one-off test).
fn grenade_solid_triangle_below(
    mesh: &geom::mesh::CollisionMesh,
    mask: &filter::AttributeMask,
    origin: [f32; 3],
    max_dist: f32,
) -> bool {
    let dir = [0.0, 0.0, -1.0];
    for (i, tri) in mesh.triangles.iter().enumerate() {
        if !mask.is_solid(mesh.tri_attribute[i]) {
            continue;
        }
        let a = mesh.vertices[tri[0] as usize];
        let b = mesh.vertices[tri[1] as usize];
        let c = mesh.vertices[tri[2] as usize];
        if let Some(t) = ray_triangle(origin, dir, a, b, c)
            && t <= max_dist
        {
            return true;
        }
    }
    false
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_extracts_matches_policy_and_spawns_are_over_solid_floor() {
    on_big_stack(|| {
        let install = GameInstall::new(game_dir()).expect("valid CS2 install");
        let started = std::time::Instant::now();
        let extraction = extract_map(&install, "de_mirage", &ExtractOptions::default())
            .expect("extract_map de_mirage");
        let elapsed = started.elapsed();
        println!("=== de_mirage extraction took {elapsed:?} ===");
        println!("triangles: {}", extraction.mesh.triangle_count());
        println!("attributes: {:#?}", extraction.mesh.attributes);
        println!("report: {:#?}", extraction.report);

        assert!(
            extraction.mesh.triangle_count() > 0,
            "world hull/mesh triangles must be > 0"
        );
        extraction.mesh.validate().expect("mesh must validate");

        let attr_names: Vec<&str> = extraction
            .mesh
            .attributes
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let world_attr_count = extraction
            .mesh
            .attributes
            .iter()
            .filter(|a| !a.synthetic)
            .count();
        assert_eq!(
            world_attr_count, 9,
            "de_mirage's world_physics PHYS has 9 collision attributes; expected all referenced. Got: {attr_names:?}"
        );
        assert!(
            attr_names.contains(&"EntitySolid"),
            "missing EntitySolid: {attr_names:?}"
        );
        assert!(
            attr_names.contains(&"EntityPhysicsClip"),
            "missing EntityPhysicsClip: {attr_names:?}"
        );

        // Every skipped func_brush for "retake" must equal the number of func_brush entities
        // whose targetname contains "retake" or whose model contains "/retake_" (case-insensitive).
        let retake_skips = extraction
            .report
            .skipped_entities
            .iter()
            .filter(|s| s.classname == "func_brush" && s.reason == "retake")
            .count();
        let retake_func_brush = extraction
            .entities
            .iter()
            .filter(|e| {
                e.classname == "func_brush"
                    && (e
                        .targetname
                        .as_deref()
                        .is_some_and(|t| t.to_ascii_lowercase().contains("retake"))
                        || e.model
                            .as_deref()
                            .is_some_and(|m| m.to_ascii_lowercase().contains("/retake_")))
            })
            .count();
        assert_eq!(
            retake_skips, retake_func_brush,
            "retake-skip count must equal retake-named/pathed func_brush entities"
        );

        // Every info_player_terrorist/counterterrorist spawn must be inside the mesh's XY bounds
        // and have a grenade-solid triangle within 128 units straight down.
        let (min, max) = extraction.mesh.bounds().expect("mesh has vertices");
        let grenade_mask = filter::grenade_mask(&extraction.mesh);
        let mut spawn_count = 0usize;
        for e in &extraction.entities {
            if e.classname != "info_player_terrorist"
                && e.classname != "info_player_counterterrorist"
            {
                continue;
            }
            spawn_count += 1;
            let o = e.origin;
            assert!(
                o[0] >= min[0] && o[0] <= max[0] && o[1] >= min[1] && o[1] <= max[1],
                "{} spawn at {o:?} is outside mesh XY bounds {min:?}..{max:?}",
                e.classname
            );
            assert!(
                grenade_solid_triangle_below(&extraction.mesh, &grenade_mask, o, 128.0),
                "{} spawn at {o:?} has no grenade-solid triangle within 128 units below it",
                e.classname
            );
        }
        assert!(spawn_count > 0, "expected at least one spawn point");
        println!("checked {spawn_count} spawn points for a solid floor below");

        // cgeo round trip.
        let dir = temp_dir("de_mirage_cache");
        let cache_dir = cache::save_extraction(&dir, &extraction, false).expect("save_extraction");
        let reloaded = cache::load_mesh(&cache_dir).expect("load_mesh");
        assert_eq!(reloaded.vertices, extraction.mesh.vertices);
        assert_eq!(reloaded.triangles, extraction.mesh.triangles);
        assert_eq!(reloaded.tri_attribute, extraction.mesh.tri_attribute);
        assert_eq!(reloaded.tri_surface, extraction.mesh.tri_surface);
        assert_eq!(reloaded.attributes, extraction.mesh.attributes);
        assert_eq!(reloaded.surfaces, extraction.mesh.surfaces);

        // export-obj of the grenade filter.
        let obj_path = dir.join("de_mirage_grenade.obj");
        let mtl_path = dir.join("de_mirage_grenade.mtl");
        let mut obj_file = std::fs::File::create(&obj_path).unwrap();
        let mut mtl_file = std::fs::File::create(&mtl_path).unwrap();
        let stats = obj::write_obj(
            &extraction.mesh,
            Some(&grenade_mask),
            None,
            &mut obj_file,
            Some((&mut mtl_file, "de_mirage_grenade.mtl")),
            obj::ObjOptions::default(),
        )
        .expect("write_obj");
        println!(
            "export-obj (grenade filter): {} triangles, {} vertices -> {}",
            stats.triangles_written,
            stats.vertices_written,
            obj_path.display()
        );
        assert!(stats.triangles_written > 0);
    });
}

/// Report-only smoke test: extract a map and just check it doesn't crash, has triangles, and
/// every player spawn has a grenade-solid triangle beneath it (no strict attribute-table
/// assertions, since these maps weren't the ones policy was calibrated against).
fn extract_report_only(map: &str) {
    on_big_stack({
        let map = map.to_string();
        move || {
            let install = GameInstall::new(game_dir()).expect("valid CS2 install");
            let extraction = extract_map(&install, &map, &ExtractOptions::default())
                .unwrap_or_else(|e| panic!("extract_map {map}: {e}"));
            println!("=== {map} report ===");
            println!("triangles: {}", extraction.mesh.triangle_count());
            println!("{:#?}", extraction.report);
            assert!(extraction.mesh.triangle_count() > 0);
            extraction.mesh.validate().expect("mesh must validate");

            let grenade_mask = filter::grenade_mask(&extraction.mesh);
            let mut checked = 0usize;
            for e in &extraction.entities {
                if e.classname != "info_player_terrorist"
                    && e.classname != "info_player_counterterrorist"
                {
                    continue;
                }
                checked += 1;
                assert!(
                    grenade_solid_triangle_below(&extraction.mesh, &grenade_mask, e.origin, 128.0),
                    "{} spawn at {:?} has no grenade-solid triangle within 128 units below it",
                    e.classname,
                    e.origin
                );
            }
            println!("checked {checked} spawn points for a solid floor below");
        }
    });
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_dust2_extracts_and_spawns_are_over_floor() {
    extract_report_only("de_dust2");
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_nuke_extracts_and_spawns_are_over_floor() {
    extract_report_only("de_nuke");
}
