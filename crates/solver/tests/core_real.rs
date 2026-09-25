//! Real de_mirage sweep + verify smoke test; needs `CS2_GAME_DIR` (to
//! resolve the current build/cache). `specs/s5b_core.md` §Tests (real,
//! ignored).

use std::path::PathBuf;
use std::time::Instant;

use extract::cache;
use extract::game::GameInstall;
use geom::filter::grenade_mask;
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::voxel::VoxelGrid;
use sim::{ThrowConstants, ThrowType};
use solver::{nav_ground, sweep, verify, zone};

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_bombsite_a_sweep_and_verify() {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    let cache_root = cache_root();
    let dir = cache::find_cached(&cache_root, &install, "de_mirage")
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for de_mirage; run `cs2mod extract de_mirage` first"));
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");

    let entities_text = std::fs::read_to_string(dir.join("entities.json")).expect("entities.json");
    let entities: Vec<extract::EntityRecord> =
        serde_json::from_str(&entities_text).expect("parse entities.json");
    let bombsite_a = entities
        .iter()
        .find(|e| {
            e.classname == "env_cs_place"
                && e.properties.get("place_name").and_then(|v| v.as_str()) == Some("BombsiteA")
        })
        .expect("de_mirage should have a BombsiteA env_cs_place entity");
    let target_xy = V3::from_array(bombsite_a.origin);

    let nav_text = std::fs::read_to_string(dir.join("nav.json")).expect("nav.json");
    let nav: extract::report::NavAreasDump =
        serde_json::from_str(&nav_text).expect("parse nav.json");
    let nav_areas: Vec<Vec<V3>> = nav
        .areas
        .iter()
        .filter(|a| a.hull_index == 0)
        .map(|a| a.corners.iter().map(|c| V3::from_array(*c)).collect())
        .collect();
    let target_z = nav_ground::nav_ground_z_nearby(&nav_areas, target_xy.x, target_xy.y)
        .unwrap_or(target_xy.z);
    let target = V3::new(target_xy.x, target_xy.y, target_z);
    println!(
        "target (BombsiteA env_cs_place, ground-snapped): ({:.0}, {:.0}, {:.0})",
        target.x, target.y, target.z
    );

    let grenade_solid = grenade_mask(&mesh);
    let Some((mesh_min, mesh_max)) = mesh.bounds() else {
        panic!("de_mirage mesh has no triangles");
    };
    let (mesh_min, mesh_max) = (V3::from_array(mesh_min), V3::from_array(mesh_max));
    let region_min = V3::new(target.x - 2000.0, target.y - 2000.0, mesh_min.z);
    let region_max = V3::new(
        target.x + 2000.0,
        target.y + 2000.0,
        (target.z + 900.0).min(mesh_max.z + 64.0),
    );

    let collider = UniformGrid::build(&mesh, &grenade_solid, None, 128.0).expect("build collider");
    let grid = VoxelGrid::build(
        &mesh,
        &grenade_solid,
        16.0,
        Aabb {
            min: region_min,
            max: region_max,
        },
    )
    .expect("build voxel grid");

    let zone_crossings = zone::point_target_zone(&grid, target, 32.0);
    assert!(
        !zone_crossings.is_empty(),
        "target should have reachable landing cells"
    );

    // Origins: a 64u lattice of nav-area centroids within 1500u of the
    // target. Part A's `origins::origins_from_nav_areas` was still under
    // concurrent development while this test was written; the spec
    // explicitly allows this fallback ("else a simple 64u lattice of nav
    // centroids -- note which").
    const LATTICE_STEP: f32 = 64.0;
    let mut seen = std::collections::HashSet::new();
    let mut origins: Vec<V3> = Vec::new();
    for corners in &nav_areas {
        let min_x = corners.iter().map(|c| c.x).fold(f32::INFINITY, f32::min);
        let max_x = corners
            .iter()
            .map(|c| c.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = corners.iter().map(|c| c.y).fold(f32::INFINITY, f32::min);
        let max_y = corners
            .iter()
            .map(|c| c.y)
            .fold(f32::NEG_INFINITY, f32::max);
        let z = corners.iter().map(|c| c.z).sum::<f32>() / corners.len() as f32;
        if (min_x - target.x).max(target.x - max_x) > 1500.0
            || (min_y - target.y).max(target.y - max_y) > 1500.0
        {
            continue;
        }
        let mut x = (min_x / LATTICE_STEP).ceil() * LATTICE_STEP;
        while x <= max_x {
            let mut y = (min_y / LATTICE_STEP).ceil() * LATTICE_STEP;
            while y <= max_y {
                let dx = x - target.x;
                let dy = y - target.y;
                if (dx * dx + dy * dy).sqrt() <= 1500.0
                    && solver::standspots::point_in_polygon(corners, x, y)
                    && seen.insert((x as i32, y as i32))
                {
                    origins.push(V3::new(x, y, z));
                }
                y += LATTICE_STEP;
            }
            x += LATTICE_STEP;
        }
    }
    println!("origins: {}", origins.len());
    assert!(
        !origins.is_empty(),
        "expected at least one nav-lattice origin within 1500u"
    );

    let types = [
        ThrowType::Stand,
        ThrowType::Crouch,
        ThrowType::JumpThrow,
        ThrowType::CrouchJumpThrow,
        ThrowType::RunJumpThrow,
    ];
    let k = ThrowConstants::default();
    let opts = sweep::SweepOptions {
        constants: Some(&k),
        target: Some(target),
        ..Default::default()
    };

    let start = Instant::now();
    let candidates = sweep::solve(&grid, &zone_crossings, &types, &origins, &opts);
    let sweep_elapsed = start.elapsed();
    println!(
        "sweep: {} candidates in {:.1}s",
        candidates.len(),
        sweep_elapsed.as_secs_f64()
    );
    assert!(
        !candidates.is_empty(),
        "expected at least one coarse candidate near BombsiteA"
    );

    let verify_opts: verify::VerifyOptions<UniformGrid> = verify::VerifyOptions {
        min_stability: 0.4,
        constants: Some(&k),
        aim_target: Some(target),
        tolerance: Some(32.0),
        area_accept: None,
        collider_glass_gone: None,
        on_candidate: None,
        cancel: None,
    };
    let start = Instant::now();
    let verified =
        verify::verify_exact(&grid, &collider, &zone_crossings, &candidates, &verify_opts);
    let verify_elapsed = start.elapsed();
    println!(
        "verify: {} verified in {:.1}s",
        verified.len(),
        verify_elapsed.as_secs_f64()
    );

    println!("top {} (of {}):", verified.len().min(10), verified.len());
    for l in verified.iter().take(10) {
        println!(
            "  feet ({:.0},{:.0},{:.0}) yaw {:.1} pitch {:.1} {:?} click {:.2} rest ({:.0},{:.0},{:.0}) stability {:.2} scatter {:.1}",
            l.feet.x,
            l.feet.y,
            l.feet.z,
            l.yaw_deg,
            l.pitch_deg,
            l.throw_type,
            l.strength,
            l.rest_point.x,
            l.rest_point.y,
            l.rest_point.z,
            l.stability,
            l.rest_scatter
        );
    }

    for l in &verified {
        assert!(
            verify::within_tolerance(l.rest_point, target, 32.0, grid.voxel_size()),
            "verified lineup {l:?} should re-simulate within tolerance of the target"
        );
    }
}
