//! Synthetic-geometry tests for `radar::render`/`radar::callouts` (`specs/s6b_radar.md`
//! §tests); the real de_mirage test is in `radar_real.rs`, ignored by default.

use extract::report::EntityRecord;
use geom::math::V3;
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use radar::{RadarOptions, callouts, render};

fn quad_mesh(quads: &[[[f32; 3]; 4]]) -> CollisionMesh {
    let mut mesh = CollisionMesh::new();
    let attr = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let obj = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldMesh,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    for q in quads {
        let _ = mesh.push_triangles(
            q,
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        );
    }
    mesh
}

fn floor_quad(z: f32, half: f32) -> [[f32; 3]; 4] {
    [
        [-half, -half, z],
        [half, -half, z],
        [half, half, z],
        [-half, half, z],
    ]
}

/// A thin vertical wall at `x`, spanning `[-half_y, half_y]` on y and `[z0, z1]` on z. No
/// horizontal cap, unlike a closed box: a vertical ray never crosses a vertical plane, so this
/// never disturbs the floor probe's own downward ray the way a capped box would.
fn wall_quad(x: f32, half_y: f32, z0: f32, z1: f32) -> [[f32; 3]; 4] {
    [
        [x, -half_y, z0],
        [x, half_y, z0],
        [x, half_y, z1],
        [x, -half_y, z1],
    ]
}

/// A horizontal cap over `[x0, x1] x [y0, y1]` at `z`, e.g. the roof of a raised platform - a
/// downward ray hits this, unlike `wall_quad`.
fn platform_quad(x0: f32, x1: f32, y0: f32, y1: f32, z: f32) -> [[f32; 3]; 4] {
    [[x0, y0, z], [x1, y0, z], [x1, y1, z], [x0, y1, z]]
}

/// The pixel covering world point `(wx, wy)`, given the region's `x0`/`y1` and the pixel size
/// (`ViewerDataCommand.cs:135-137`'s north-edge-first, Y-inverted layout).
fn pixel_at(
    img: &radar::RadarImage,
    x0: f32,
    y1: f32,
    pixel_size: f32,
    wx: f32,
    wy: f32,
) -> [u8; 4] {
    let px = ((wx - x0) / pixel_size).floor() as u32;
    let py = ((y1 - wy) / pixel_size).floor() as u32;
    let i = (py * img.width + px) as usize * 4;
    img.rgba[i..i + 4].try_into().unwrap()
}

fn square_area(min: (f32, f32), max: (f32, f32), z: f32) -> Vec<V3> {
    vec![
        V3::new(min.0, min.1, z),
        V3::new(max.0, min.1, z),
        V3::new(max.0, max.1, z),
        V3::new(min.0, max.1, z),
    ]
}

#[test]
fn floor_with_no_obstacles_is_all_class_zero_and_alpha_matches_nav_coverage() {
    let mesh = quad_mesh(&[floor_quad(0.0, 1000.0)]);
    let nav_areas = vec![square_area((-100.0, -100.0), (100.0, 100.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 10.0,
        region: Some([-400.0, -400.0, 400.0, 400.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    // Center: inside the nav polygon.
    let center = pixel_at(&img, -400.0, 400.0, 10.0, 0.0, 0.0);
    assert_eq!(center[0], 0, "center class");
    assert_eq!(center[3], 255, "center alpha");
    // Far corner: well beyond the polygon plus its gap-bridging reach (96u), so uncovered.
    let corner = pixel_at(&img, -400.0, 400.0, 10.0, -390.0, -390.0);
    assert_eq!(corner[3], 0, "far-corner alpha");
}

#[test]
fn tall_box_renders_as_wall() {
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(40.0, 200.0, 0.0, 80.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![square_area((-200.0, -200.0), (200.0, 200.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 5.0,
        region: Some([-200.0, -200.0, 200.0, 200.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let p = pixel_at(&img, -200.0, 200.0, 5.0, 40.0, 50.0);
    assert_eq!(p[0], 255, "expected wall class over the tall box");
}

#[test]
fn short_box_renders_as_low_cover() {
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(40.0, 200.0, 0.0, 30.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![square_area((-200.0, -200.0), (200.0, 200.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 5.0,
        region: Some([-200.0, -200.0, 200.0, 200.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let p = pixel_at(&img, -200.0, 200.0, 5.0, 40.0, 50.0);
    assert_eq!(p[0], 128, "expected low-cover class over the short box");
}

#[test]
fn low_cover_beside_a_taller_platform_still_renders_as_low_cover() {
    // The floor probe is a single straight-down ray at the pixel center
    // (`ViewerDataCommand.cs:148-151`), not a 5-point player-hull footprint probe: a hull probe
    // takes the max height across its 5 samples, so a pixel right next to (but not under) a
    // taller raised platform would have one of its corner samples land on the platform roof and
    // report that height as "the floor" here too - pushing the low wall's own 0..30 span below
    // the probe's classification window and misclassifying it as open floor.
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(0.0, 50.0, 0.0, 30.0));
    // A taller raised platform (roof at z=40) whose footprint reaches under this pixel's
    // player-hull-width corner (16 units away) but not its own center.
    quads.push(platform_quad(10.0, 25.0, -50.0, 50.0, 40.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![square_area((-100.0, -100.0), (100.0, 100.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 5.0,
        region: Some([-100.0, -100.0, 100.0, 100.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let p = pixel_at(&img, -100.0, 100.0, 5.0, 0.0, 0.0);
    assert_eq!(
        p[0], 128,
        "the wall stub's own low-cover class must not be swallowed by the neighboring platform"
    );
}

#[test]
fn stacked_nav_areas_use_the_lower_level_for_ground_height() {
    // Real geometry only exists at the lower level (a wall at z=0..80). A decoy nav area at
    // z=1000 covers the exact same footprint; if the ground grid picked the decoy's height, the
    // floor probe would look for geometry 976..1040 and find none, so the wall pixel would come
    // back as floor (class 0) instead of wall (class 255).
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(40.0, 200.0, 0.0, 80.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![
        square_area((-200.0, -200.0), (200.0, 200.0), 0.0),
        square_area((-200.0, -200.0), (200.0, 200.0), 1000.0),
    ];
    let opts = RadarOptions {
        pixel_size: 5.0,
        region: Some([-200.0, -200.0, 200.0, 200.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let p = pixel_at(&img, -200.0, 200.0, 5.0, 40.0, 50.0);
    assert_eq!(p[0], 255, "the lower level's geometry must win");
}

#[test]
fn boundary_and_thickening_passes_extend_the_outline_two_pixels_beyond_nav_coverage() {
    // Nav coverage is a 200x200 square that ends well short of the wall below; the boundary
    // pass paints the wall pixel directly, and it takes both thickening passes
    // (`ViewerDataCommand.cs:214-250`) to reach two pixels further out - a third pixel out is
    // never reached by either pass.
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(195.0, 300.0, 0.0, 80.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![square_area((-100.0, -100.0), (100.0, 100.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 10.0,
        region: Some([-400.0, -400.0, 400.0, 400.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let over_wall = pixel_at(&img, -400.0, 400.0, 10.0, 195.0, 0.0);
    assert_eq!(
        over_wall,
        [255, 0, 0, 255],
        "boundary pass must paint the wall pixel"
    );
    let two_steps_out = pixel_at(&img, -400.0, 400.0, 10.0, 215.0, 0.0);
    assert_eq!(
        two_steps_out,
        [255, 0, 0, 255],
        "reaching two pixels further out needs both thickening passes"
    );
    let three_steps_out = pixel_at(&img, -400.0, 400.0, 10.0, 225.0, 0.0);
    assert_eq!(
        three_steps_out,
        [0, 0, 0, 0],
        "three pixels out is beyond both thickening passes' reach"
    );
}

#[test]
fn ground_height_tint_differs_between_nav_levels() {
    let mesh = quad_mesh(&[floor_quad(0.0, 1000.0)]);
    let nav_areas = vec![
        square_area((-100.0, -100.0), (-10.0, 100.0), 0.0),
        square_area((10.0, -100.0), (100.0, 100.0), 100.0),
    ];
    let opts = RadarOptions {
        pixel_size: 10.0,
        region: Some([-150.0, -150.0, 150.0, 150.0]),
    };
    let img = render(&mesh, &nav_areas, &opts).unwrap();
    let low = pixel_at(&img, -150.0, 150.0, 10.0, -50.0, 0.0);
    let high = pixel_at(&img, -150.0, 150.0, 10.0, 50.0, 0.0);
    assert_ne!(
        low[1], high[1],
        "tint (G channel) must differ between nav levels"
    );
}

#[test]
fn region_without_nav_coverage_errors_instead_of_panicking() {
    let mesh = quad_mesh(&[floor_quad(0.0, 1000.0)]);
    let nav_areas = vec![square_area((10_000.0, 10_000.0), (10_100.0, 10_100.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 10.0,
        region: Some([-200.0, -200.0, 200.0, 200.0]),
    };
    let err = render(&mesh, &nav_areas, &opts).unwrap_err();
    assert!(matches!(err, radar::RadarError::NoNavCoverage));
}

#[test]
fn render_is_deterministic_across_runs() {
    let mut quads = vec![floor_quad(0.0, 1000.0)];
    quads.push(wall_quad(40.0, 200.0, 0.0, 80.0));
    let mesh = quad_mesh(&quads);
    let nav_areas = vec![square_area((-200.0, -200.0), (200.0, 200.0), 0.0)];
    let opts = RadarOptions {
        pixel_size: 4.0,
        region: Some([-200.0, -200.0, 200.0, 200.0]),
    };
    let a = render(&mesh, &nav_areas, &opts).unwrap();
    let b = render(&mesh, &nav_areas, &opts).unwrap();
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    assert_eq!(a.rgba, b.rgba);
}

fn place_entity(name: &str, x: f32, y: f32) -> EntityRecord {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "place_name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    EntityRecord {
        classname: "env_cs_place".to_string(),
        targetname: None,
        origin: [x, y, 0.0],
        angles: [0.0, 0.0, 0.0],
        model: None,
        hammer_id: None,
        properties,
        from_child_lump: None,
    }
}

#[test]
fn callouts_group_case_insensitively_and_average_coordinates() {
    let entities = vec![
        place_entity("BombsiteA", 0.0, 0.0),
        place_entity("bombsitea", 100.0, 100.0),
        place_entity("Mid", -50.0, -50.0),
    ];
    let out = callouts(&entities, [-1000, -1000, 1000, 1000]);
    assert_eq!(
        out,
        vec![
            ("BombsiteA".to_string(), 50, 50),
            ("Mid".to_string(), -50, -50),
        ]
    );
}
