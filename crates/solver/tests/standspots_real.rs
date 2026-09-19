//! Real-map stand-spot checks. Set `CS2_GAME_DIR` to `...\game\csgo` and run
//! with `cargo test -p solver --release --test standspots_real -- --ignored --nocapture`.

use std::path::PathBuf;

use extract::cache;
use extract::game::GameInstall;
use extract::report::{EntityRecord, NavAreasDump};
use geom::filter::player_mask;
use geom::grid::UniformGrid;
use geom::math::V3;
use solver::origins;
use solver::standspots::{self, Stance};

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test, e.g. \
             `set CS2_GAME_DIR=D:\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo`"
        ),
    }
}

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

struct MapData {
    nav_areas: Vec<Vec<V3>>,
    entities: Vec<EntityRecord>,
    collider: UniformGrid,
    min: V3,
    max: V3,
}

fn load_map(map: &str) -> MapData {
    let install = GameInstall::new(game_dir()).expect("game install");
    let dir = cache::find_cached(&cache_root(), &install, map)
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for {map}; run `cs2mod extract {map}` first"));
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");

    let nav_text = std::fs::read_to_string(dir.join("nav.json")).expect("read nav.json");
    let nav: NavAreasDump = serde_json::from_str(&nav_text).expect("parse nav.json");
    let nav_areas: Vec<Vec<V3>> = nav
        .areas
        .iter()
        .filter(|a| a.hull_index == 0)
        .map(|a| a.corners.iter().map(|c| V3::from_array(*c)).collect())
        .collect();

    let entities_text =
        std::fs::read_to_string(dir.join("entities.json")).expect("read entities.json");
    let entities: Vec<EntityRecord> =
        serde_json::from_str(&entities_text).expect("parse entities.json");

    let (min, max) = mesh.bounds().expect("mesh has triangles");
    let (min, max) = (V3::from_array(min), V3::from_array(max));
    let mask = player_mask(&mesh);
    let collider = UniformGrid::build(&mesh, &mask, None, 128.0).expect("build player collider");

    MapData {
        nav_areas,
        entities,
        collider,
        min,
        max,
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_stand_spots() {
    let data = load_map("de_mirage");
    let start = std::time::Instant::now();
    let spots = standspots::compute(
        &data.collider,
        &data.nav_areas,
        data.min,
        data.max,
        16.0,
        None,
    );
    let elapsed = start.elapsed();

    let nav_covered = spots.iter().filter(|s| s.nav_covered).count();
    let recovered = spots.len() - nav_covered;
    println!(
        "de_mirage: {} stand spots in {:.1}s ({nav_covered} nav-covered, {recovered} recovered)",
        spots.len(),
        elapsed.as_secs_f64()
    );

    // Every player spawn has a stand spot close by. NOTE: on real de_mirage
    // data, `info_player_terrorist`/`counterterrorist` entity `origin.z` can
    // sit up to ~34u above the actual player-solid collision floor at that
    // XY (confirmed against both the player and grenade attribute masks, so
    // this is not an attribute-filtering bug in this port) - see the s5a
    // receipt's notes.
    for e in &data.entities {
        if e.classname != "info_player_terrorist" && e.classname != "info_player_counterterrorist" {
            continue;
        }
        let raw_spawn = V3::from_array(e.origin);
        let spawn = match origins::floor_under_hull(
            &data.collider,
            raw_spawn,
            sim::STAND_EYE_HEIGHT,
            512.0,
        ) {
            Some(z) => V3::new(raw_spawn.x, raw_spawn.y, z),
            None => raw_spawn,
        };
        let found = spots.iter().any(|s| {
            let dx = s.feet.x - spawn.x;
            let dy = s.feet.y - spawn.y;
            let horizontal = (dx * dx + dy * dy).sqrt();
            horizontal <= 32.0 && (s.feet.z - spawn.z).abs() <= 24.0
        });
        if !found {
            let mut nearest = spots[0];
            let mut nearest_d = f32::MAX;
            for s in &spots {
                let dx = s.feet.x - spawn.x;
                let dy = s.feet.y - spawn.y;
                let d = (dx * dx + dy * dy).sqrt() + (s.feet.z - spawn.z).abs();
                if d < nearest_d {
                    nearest_d = d;
                    nearest = *s;
                }
            }
            eprintln!(
                "nearest to {spawn:?}: {:?} (nav_covered={})",
                nearest.feet, nearest.nav_covered
            );
        }
        assert!(
            found,
            "{} at {:?} has no stand spot within 32u horizontally / 24u vertically",
            e.classname, spawn
        );
    }

    // At least one recovered (non-nav-covered) spot sits well above the
    // nearest nav-covered ground - i.e. on top of a crate-like elevated
    // surface the nav mesh does not cover (StandSpots.cs's doc comment;
    // `technical_debt.md`'s "Heights: every position we hand out was low").
    let elevated_recovered = spots.iter().any(|s| {
        if s.nav_covered {
            return false;
        }
        spots.iter().any(|nav| {
            if !nav.nav_covered {
                return false;
            }
            let dx = s.feet.x - nav.feet.x;
            let dy = s.feet.y - nav.feet.y;
            (dx * dx + dy * dy).sqrt() <= 64.0 && s.feet.z - nav.feet.z >= 20.0
        })
    });
    assert!(
        elevated_recovered,
        "expected at least one recovered spot above nearby nav ground"
    );
    assert!(spots.iter().any(|s| s.stance == Stance::Crouching));
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_dust2_stand_spot_count() {
    let data = load_map("de_dust2");
    let start = std::time::Instant::now();
    let spots = standspots::compute(
        &data.collider,
        &data.nav_areas,
        data.min,
        data.max,
        16.0,
        None,
    );
    println!(
        "de_dust2: {} stand spots in {:.1}s",
        spots.len(),
        start.elapsed().as_secs_f64()
    );
    assert!(!spots.is_empty());
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_nuke_stand_spot_count() {
    let data = load_map("de_nuke");
    let start = std::time::Instant::now();
    let spots = standspots::compute(
        &data.collider,
        &data.nav_areas,
        data.min,
        data.max,
        16.0,
        None,
    );
    println!(
        "de_nuke: {} stand spots in {:.1}s",
        spots.len(),
        start.elapsed().as_secs_f64()
    );
    assert!(!spots.is_empty());
}
