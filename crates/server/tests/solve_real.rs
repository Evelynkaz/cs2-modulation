//! Real de_mirage `POST /api/lineup`/`GET /api/trajectory` tests over the router
//! (`tower::ServiceExt::oneshot`, no network port); needs `CS2_GAME_DIR` plus a pre-populated
//! `cache/` for de_mirage. `s6d_solve_api.md`'s "real, ignored" test.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use extract::cache;
use extract::game::GameInstall;
use geom::filter::names_mask;
use server::AppState;
use server::config::AppConfig;
use sim::ThrowConstants;
use solver::rank;
use solver::target::{MapData, SolveHooks, SolveQuery, StandSpotOrigin, solve_for_target};
use std::sync::atomic::AtomicBool;

const SINGLE_TARGET_DEFAULT_ATTRS: [&str; 3] = ["Default", "default", "EntitySolid"];

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

fn find_de_mirage_dir() -> PathBuf {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    cache::find_cached(&cache_root(), &install, "de_mirage")
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for de_mirage; run `cs2mod extract de_mirage` first"))
}

fn router() -> Router {
    find_de_mirage_dir();
    let config_path = std::env::temp_dir().join(format!(
        "cs2mod-server-solve-real-config-{}.json",
        std::process::id()
    ));
    let viewer_dir = std::env::temp_dir().join(format!(
        "cs2mod-server-solve-real-viewer-{}",
        std::process::id()
    ));
    let state = Arc::new(AppState::new(
        config_path,
        AppConfig::default(),
        cache_root(),
        viewer_dir,
    ));
    server::routes::router(state)
}

/// The same `MapData`/`SolveQuery` `cmd_solver.rs::solve` builds for
/// `cs2mod solve de_mirage --target -1227.0001,-1071.9951,-168 --tolerance 80`, used here to
/// check the server's `POST /api/lineup` against a direct `solve_for_target` call rather than
/// shelling out to the CLI.
fn direct_solve_first_ranked() -> rank::RankedLineup {
    let dir = find_de_mirage_dir();
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");
    let nav_text = std::fs::read_to_string(dir.join("nav.json")).expect("nav.json");
    let nav: extract::report::NavAreasDump = serde_json::from_str(&nav_text).expect("nav.json");
    let nav_areas = nav
        .areas
        .iter()
        .filter(|a| a.hull_index == 0)
        .map(|a| {
            a.corners
                .iter()
                .map(|c| geom::math::V3::from_array(*c))
                .collect()
        })
        .collect();

    let stand_spots = std::fs::read_to_string(dir.join("standspots.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("spots").cloned())
        .and_then(|spots| spots.as_array().cloned())
        .map(|spots| {
            spots
                .iter()
                .map(|s| {
                    let feet = s["feet"].as_array().unwrap();
                    StandSpotOrigin {
                        feet: geom::math::V3::new(
                            feet[0].as_f64().unwrap() as f32,
                            feet[1].as_f64().unwrap() as f32,
                            feet[2].as_f64().unwrap() as f32,
                        ),
                        crouched: s["stance"].as_str() == Some("Crouching"),
                    }
                })
                .collect::<Vec<_>>()
        });

    // `crates/cli/src/cmd_solver.rs:279-293`: spawns loaded from `entities.json`, one
    // representative "front" point per side, all spawns as `map_data.spawns`; no `--spawns`
    // given here, so `spawn_points` stays empty and `origin_reach` defaults to 3100 (no origin
    // click).
    let spawns = extract::load_spawns(&dir).expect("entities.json");
    let mut all_spawns = spawns.t.clone();
    all_spawns.extend(spawns.ct.iter().copied());
    let mut spawn_fronts = Vec::new();
    if !spawns.t.is_empty() {
        spawn_fronts.push(spawns.t[spawns.t.len() / 2]);
    }
    if !spawns.ct.is_empty() {
        spawn_fronts.push(spawns.ct[spawns.ct.len() / 2]);
    }

    let attribute_filter = Some(names_mask(&mesh, &SINGLE_TARGET_DEFAULT_ATTRS));
    let map_data = MapData {
        mesh,
        nav_areas,
        stand_spots,
        spawns: all_spawns,
        attribute_filter,
    };
    let query = SolveQuery {
        target: geom::math::V3::new(-1227.0001, -1071.9951, -168.0),
        has_target_z: true,
        tolerance: 80.0,
        origin_reach: 3100.0,
        min_stability: 0.4,
        spawn_fronts,
        ..Default::default()
    };
    let constants = ThrowConstants::default();
    let cancel = AtomicBool::new(false);
    let hooks = SolveHooks {
        progress: &|_, _| {},
        on_origin: None,
        on_candidate: None,
    };
    let solve = solve_for_target(&map_data, &query, &constants, &hooks, &cancel);
    let ranked = rank::ranked(&solve, None);
    ranked.into_iter().next().expect("at least one lineup")
}

#[tokio::test]
#[ignore = "needs CS2_GAME_DIR and a de_mirage cache entry"]
async fn lineup_matches_direct_solve_and_trajectory_agrees_with_rest() {
    // A leftover cached answer from a previous run of this test would collapse the stream to one
    // `result` line with no phases, failing the assertions below - each run must solve fresh.
    let _ = std::fs::remove_dir_all(cache_root().join("solves"));
    let router = router();
    let direct = direct_solve_first_ranked();

    let body = json!({
        "map": "de_mirage",
        "target": [-1227.0001, -1071.9951, -168.0],
        "tolerance": 80.0,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lineup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();

    let mut phases: Vec<String> = Vec::new();
    let mut result: Option<Value> = None;
    for line in &lines {
        let v: Value = serde_json::from_str(line).unwrap();
        if let Some(p) = v.get("phase").and_then(|p| p.as_str()) {
            phases.push(p.to_string());
        }
        if let Some(r) = v.get("result") {
            result = Some(r.clone());
        }
    }
    let result = result.expect("stream must end with a result line");
    for expected in [
        "prepare",
        "colliders",
        "origins",
        "sweep",
        "verify",
        "sightline",
        "pins",
    ] {
        assert!(
            phases.iter().any(|p| p == expected),
            "expected phase {expected:?} in {phases:?}"
        );
    }

    let first = &result["lineups"][0];
    assert_eq!(first["id"], json!(direct.id));
    assert_eq!(first["console"], json!(direct.console));

    // `/api/trajectory` for that same lineup: the last point must agree with `rest`.
    let l = &direct.lineup;
    let traj_uri = format!(
        "/api/trajectory?map=de_mirage&x={}&y={}&z={}&type={:?}&pitch={}&yaw={}&strength={}&runDeg={}",
        l.feet.x,
        l.feet.y,
        l.feet.z,
        l.throw_type,
        l.pitch_deg,
        l.yaw_deg,
        l.strength,
        l.run_yaw_offset_deg
    );
    let resp = router
        .oneshot(
            Request::builder()
                .uri(traj_uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let points = value["points"].as_array().unwrap();
    let last = points.last().expect("at least one trajectory point");
    let lx = last[0].as_f64().unwrap() as f32;
    let ly = last[1].as_f64().unwrap() as f32;
    let lz = last[2].as_f64().unwrap() as f32;
    let dist = ((lx - l.rest_point.x).powi(2)
        + (ly - l.rest_point.y).powi(2)
        + (lz - l.rest_point.z).powi(2))
    .sqrt();
    assert!(
        dist <= 1.0,
        "trajectory's last point {last:?} vs rest {:?}",
        l.rest_point
    );
}
