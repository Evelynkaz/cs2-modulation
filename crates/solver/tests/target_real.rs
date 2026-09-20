//! Real de_mirage target-solve smoke tests; needs `CS2_GAME_DIR` (to resolve
//! the current build/cache) or a pre-populated `cache/`. `specs/s5c_target.md`
//! §Tests (real, ignored) / §Verification items 3 (exactness) and 4 (timing).
//!
//! Query coordinates are queries `c`/`c3` from the parity harness
//! (`modulator-work/scratch/cs_target/data/query_c.txt`,`query_c3.txt`),
//! bit-exact against the reference on those two, plus a reach-300 variant of
//! query `a` (the CLI's own default reach once `--from`/`--getpos` is given).
//! The C# parity harness itself (Verification 1) and the recall-corpus
//! comparison (Verification 2) are a separate pass; this file only exercises
//! our own port end to end and checks its own promise (every returned
//! lineup re-simulates within tolerance of the resolved target).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use extract::cache;
use extract::game::GameInstall;
use geom::filter::names_mask;
use geom::math::V3;
use sim::{ThrowConstants, ThrowSpec, Trace, eye_height, simulate_exact};
use solver::target::{
    MapData, Phase, SolveHooks, SolveQuery, StandSpotOrigin, TargetSolve, solve_for_target,
};
use solver::verify::within_tolerance;

/// The target solver's own hardcoded voxel cell size (`TargetSolver.cs:142`,
/// `target::VOXEL_SIZE`), needed here only for `within_tolerance`'s z-band.
const VOXEL_SIZE: f32 = 16.0;

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

fn load_map(map: &str) -> MapData {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    let dir = cache::find_cached(&cache_root(), &install, map)
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for {map}; run `cs2mod extract {map}` first"));
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");

    let nav_text = std::fs::read_to_string(dir.join("nav.json")).expect("nav.json");
    let nav: extract::report::NavAreasDump =
        serde_json::from_str(&nav_text).expect("parse nav.json");
    let nav_areas: Vec<Vec<V3>> = nav
        .areas
        .iter()
        .filter(|a| a.hull_index == 0)
        .map(|a| a.corners.iter().map(|c| V3::from_array(*c)).collect())
        .collect();

    let stand_spots = std::fs::read_to_string(dir.join("standspots.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v.get("spots").cloned())
        .and_then(|spots| spots.as_array().cloned())
        .map(|spots| {
            spots
                .iter()
                .map(|s| {
                    let feet = s["feet"].as_array().unwrap();
                    StandSpotOrigin {
                        feet: V3::new(
                            feet[0].as_f64().unwrap() as f32,
                            feet[1].as_f64().unwrap() as f32,
                            feet[2].as_f64().unwrap() as f32,
                        ),
                        crouched: s["stance"].as_str() == Some("Crouching"),
                    }
                })
                .collect::<Vec<_>>()
        });

    // `MeshSetup.cs:18` (`SingleTargetDefaultAttrs`), matching `attrs=default`
    // in the parity harness's own query files.
    let attribute_filter = Some(names_mask(&mesh, &["Default", "default", "EntitySolid"]));

    MapData {
        mesh,
        nav_areas,
        stand_spots,
        spawns: Vec::new(),
        attribute_filter,
    }
}

fn assert_exact(solve: &TargetSolve, target: V3, tolerance: f32, k: &ThrowConstants) {
    assert!(!solve.lineups.is_empty(), "expected at least one lineup");
    for l in &solve.lineups {
        let eye = l.feet + V3::new(0.0, 0.0, eye_height(l.throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: l.yaw_deg,
            pitch_deg: l.pitch_deg,
            throw_type: l.throw_type,
            strength: l.strength,
            run_yaw_offset_deg: l.run_yaw_offset_deg,
        };
        let result = simulate_exact(&solve.collider, &spec, k, Trace::default());
        assert!(!result.lost, "lineup {l:?} re-simulated as lost");
        assert!(
            within_tolerance(result.rest, target, tolerance, VOXEL_SIZE),
            "lineup {l:?} re-simulated rest {:?} outside tolerance {tolerance}u of target {target:?}",
            result.rest
        );
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_three_target_queries() {
    let map = load_map("de_mirage");
    let k = ThrowConstants::default();
    let cancel = AtomicBool::new(false);

    // query `a`, but with the CLI's own reach-300 default for a click solve
    // (the harness's own query `a` uses reach 600).
    {
        let q = SolveQuery {
            target: V3::new(-662.5, -1612.5, 0.0),
            has_target_z: false,
            origin_click: Some([-104.5, -1796.0]),
            origin_reach: 300.0,
            tolerance: 32.0,
            ..Default::default()
        };
        let start = Instant::now();
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        println!(
            "(a, reach 300) {} origins, {} lineups in {:.2}s",
            solve.origins,
            solve.lineups.len(),
            start.elapsed().as_secs_f64()
        );
        assert_exact(&solve, solve.target, q.tolerance, &k);
    }

    // query `c`: exact origin, originz given explicitly.
    {
        let q = SolveQuery {
            target: V3::new(-662.5, -1612.5, 0.0),
            has_target_z: false,
            origin_click: Some([32.0, -1696.0]),
            origin_z: Some(-168.0),
            origin_reach: 600.0,
            tolerance: 32.0,
            exact_origin: true,
            ..Default::default()
        };
        let start = Instant::now();
        let hooks = SolveHooks {
            progress: &|_, _| {},
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        println!(
            "(c) {} origins, {} lineups in {:.2}s",
            solve.origins,
            solve.lineups.len(),
            start.elapsed().as_secs_f64()
        );
        assert_exact(&solve, solve.target, q.tolerance, &k);
    }

    // query `c3`: exact origin, 3D target (no nav-derived Z).
    {
        let q = SolveQuery {
            target: V3::new(-662.5, -1612.5, -120.0),
            has_target_z: true,
            origin_click: Some([32.0, -1696.0]),
            origin_reach: 600.0,
            tolerance: 32.0,
            exact_origin: true,
            ..Default::default()
        };
        let start = Instant::now();
        let hooks = SolveHooks {
            progress: &|phase: Phase, count: usize| {
                println!("    phase {phase:?} ({count})");
            },
            on_origin: None,
            on_candidate: None,
        };
        let solve = solve_for_target(&map, &q, &k, &hooks, &cancel);
        println!(
            "(c3) {} origins, {} lineups in {:.2}s",
            solve.origins,
            solve.lineups.len(),
            start.elapsed().as_secs_f64()
        );
        assert_exact(&solve, solve.target, q.tolerance, &k);
    }
}
