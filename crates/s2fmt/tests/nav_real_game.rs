//! Tests against a real CS2 install. Ignored by default; run with
//! `CS2_GAME_DIR` pointing at `...\game\csgo`, e.g.:
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2fmt --test nav_real_game -- --ignored --nocapture`
//!
//! Embedded KV3 data means these tests must run on a thread with >= 4 MiB of stack (stage 2
//! common context); `parse_nav` itself is spawned onto one rather than relying on the test
//! harness's default.

use std::collections::BTreeMap;
use std::path::PathBuf;

use s2fmt::nav::{self, NavError, NavMesh, flags};
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

/// Parses `bytes` on a thread with a 4 MiB stack (KV3 parsing needs it; see stage 2 common
/// context and `kv3::binary`'s `RECURSION_LIMIT` doc comment).
fn parse_nav_threaded(bytes: Vec<u8>) -> Result<NavMesh, NavError> {
    std::thread::Builder::new()
        .stack_size(4 << 20)
        .spawn(move || nav::parse_nav(&bytes))
        .expect("spawn 4 MiB parser thread")
        .join()
        .expect("nav parser thread must not panic")
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_nav_parses_and_reports_facts() {
    let path = game_dir().join("maps").join("de_mirage.vpk");
    let vpk = Vpk::open(&path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"));
    let bytes = vpk
        .read_path("maps/de_mirage.nav")
        .unwrap_or_else(|e| panic!("failed to read maps/de_mirage.nav: {e}"));
    assert_eq!(bytes.len(), 469_448);

    let nav = parse_nav_threaded(bytes).expect("de_mirage.nav must parse");
    assert_eq!(nav.version, 36);

    let mut areas_by_hull: BTreeMap<u8, usize> = BTreeMap::new();
    let mut corner_counts: BTreeMap<usize, usize> = BTreeMap::new();
    let mut crouch = 0usize;
    let mut jump = 0usize;
    let mut stairs = 0usize;
    for area in &nav.areas {
        *areas_by_hull.entry(area.hull_index).or_insert(0) += 1;
        *corner_counts.entry(area.corners.len()).or_insert(0) += 1;
        if area.attribute_flags & flags::CROUCH_HEIGHT != 0 {
            crouch += 1;
        }
        if area.attribute_flags & flags::JUMP != 0 {
            jump += 1;
        }
        if area.attribute_flags & flags::STAIRS != 0 {
            stairs += 1;
        }
    }

    let mut dangling = 0usize;
    let mut checked = 0usize;
    for area in &nav.areas {
        for edge in &area.connections {
            for conn in edge {
                checked += 1;
                if nav.area(conn.area_id).is_none() {
                    dangling += 1;
                }
            }
        }
    }

    let mut hull0_min = [f32::MAX; 3];
    let mut hull0_max = [f32::MIN; 3];
    for area in nav.hull_areas(0) {
        for corner in &area.corners {
            for i in 0..3 {
                hull0_min[i] = hull0_min[i].min(corner[i]);
                hull0_max[i] = hull0_max[i].max(corner[i]);
            }
        }
    }

    println!("=== de_mirage.nav ===");
    println!(
        "version={} sub_version={} is_analyzed={}",
        nav.version, nav.sub_version, nav.is_analyzed
    );
    println!("areas={} ladders={}", nav.areas.len(), nav.ladders.len());
    println!("areas by hull index: {areas_by_hull:?}");
    println!("corner count distribution: {corner_counts:?}");
    println!("areas with CROUCH_HEIGHT={crouch} JUMP={jump} STAIRS={stairs}");
    println!("hull-0 bounds: min={hull0_min:?} max={hull0_max:?}");
    println!("connection area ids checked={checked} dangling={dangling}");

    if let Some(gp) = &nav.generation_params {
        println!(
            "generation params: nav_gen_version={} hull_preset_name={:?} hull_definitions_file={:?}",
            gp.nav_gen_version, gp.hull_preset_name, gp.hull_definitions_file
        );
        for (i, hull) in gp.hulls.iter().enumerate() {
            println!(
                "  hull[{i}]: enabled={} radius={} height={} max_climb={} max_slope={} \
                 max_jump_down_dist={} max_jump_horiz_dist_base={} max_jump_up_dist={}",
                hull.enabled,
                hull.radius,
                hull.height,
                hull.max_climb,
                hull.max_slope,
                hull.max_jump_down_dist,
                hull.max_jump_horiz_dist_base,
                hull.max_jump_up_dist
            );
        }
    }

    assert_eq!(dangling, 0, "connections must resolve to existing area ids");
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn every_map_vpk_nav_parses() {
    let maps_dir = game_dir().join("maps");
    let entries =
        std::fs::read_dir(&maps_dir).unwrap_or_else(|e| panic!("failed to read {maps_dir:?}: {e}"));

    let mut map_vpks: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "vpk"))
        .collect();
    map_vpks.sort();

    let mut checked = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();

    for vpk_path in &map_vpks {
        let vpk = match Vpk::open(vpk_path) {
            Ok(v) => v,
            Err(e) => {
                failures.push((vpk_path.display().to_string(), e.to_string()));
                continue;
            }
        };

        for entry in vpk.entries() {
            if !entry.path.ends_with(".nav") {
                continue;
            }
            let bytes = match vpk.read(entry) {
                Ok(b) => b,
                Err(e) => {
                    failures.push((
                        format!("{}!{}", vpk_path.display(), entry.path),
                        e.to_string(),
                    ));
                    continue;
                }
            };
            match parse_nav_threaded(bytes) {
                Ok(nav) => {
                    checked += 1;
                    println!(
                        "{}!{}: version={} areas={}",
                        vpk_path.display(),
                        entry.path,
                        nav.version,
                        nav.areas.len()
                    );
                }
                Err(e) => failures.push((
                    format!("{}!{}", vpk_path.display(), entry.path),
                    e.to_string(),
                )),
            }
        }
    }

    println!(
        "=== checked {checked} .nav file(s) across {} map vpk(s) ===",
        map_vpks.len()
    );
    println!("failures ({}):", failures.len());
    for (path, err) in &failures {
        println!("  {path}: {err}");
    }
    assert!(
        failures.is_empty(),
        "{} .nav file(s) failed to parse; see stdout for details",
        failures.len()
    );
}
