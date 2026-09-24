//! Check 3 (`s6f3a3_map.md` "Проверки"): visual-geometry vs. collision-geometry parity.
//!
//! Needs `CS2_GAME_DIR` and an extraction cache for the map (`cs2mod extract <map>`, for the
//! collision mesh and nav data). The visual side is exported fresh in-process via `export_map`
//! rather than read from a `render.glb` already sitting in the cache -- so this test doesn't
//! depend on (or go stale against) a prior `cs2mod export-glb` run, and never touches a
//! `render.glb` file another process (e.g. the viewer server) may have open. Casts a ray straight
//! down through both the collision mesh (`world.cgeo`) and the exported geometry at every nav
//! area's centroid, and reports the height gap between the two hits (median, p95, and any large
//! outlier with its coordinates).
//!
//! Parses the exported `.glb` itself (not a general glTF reader -- only the shapes this crate's
//! own `gltf.rs` writes: `POSITION`/indices accessors as tightly-packed bufferViews into the
//! single BIN chunk, node `matrix` as a flat column-major 16-float array).
//!
//! `cargo test -p s2render --release --test collision_parity_real_game -- --ignored --nocapture`

use std::path::PathBuf;

use extract::cache;
use extract::game::GameInstall;
use geom::bvh::Bvh;
use geom::collider::Collider;
use geom::filter::{AttributeMask, from_fn};
use geom::math::V3;
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use s2render::export::{ExportOptions, export_map};
use s2render::source::Sources;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!("CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test"),
    }
}

fn cache_dir(map: &str) -> PathBuf {
    let install = GameInstall::new(game_dir()).expect("valid CS2 install");
    let cache_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache");
    cache::find_cached(&cache_root, &install, map)
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for {map}; run `cs2mod extract {map}` first"))
}

const IDENTITY: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

struct GlbDoc {
    json: serde_json::Value,
    bin: Vec<u8>,
}

fn parse_glb(bytes: &[u8]) -> GlbDoc {
    assert_eq!(&bytes[0..4], b"glTF", "not a GLB file");
    let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    assert_eq!(&bytes[16..20], b"JSON");
    let json: serde_json::Value =
        serde_json::from_slice(&bytes[20..20 + json_len]).expect("glTF JSON chunk");
    let bin_start = 20 + json_len;
    let bin_len = u32::from_le_bytes(bytes[bin_start..bin_start + 4].try_into().unwrap()) as usize;
    assert_eq!(&bytes[bin_start + 4..bin_start + 8], b"BIN\0");
    let bin = bytes[bin_start + 8..bin_start + 8 + bin_len].to_vec();
    GlbDoc { json, bin }
}

fn buffer_view_offset(doc: &GlbDoc, view_index: usize) -> usize {
    doc.json["bufferViews"][view_index]["byteOffset"]
        .as_u64()
        .unwrap_or(0) as usize
}

/// `POSITION` (or any tightly-packed `VEC3` `FLOAT` accessor this crate's own writer produces).
fn accessor_positions(doc: &GlbDoc, accessor_index: usize) -> Vec<[f32; 3]> {
    let acc = &doc.json["accessors"][accessor_index];
    let view = acc["bufferView"].as_u64().unwrap() as usize;
    let count = acc["count"].as_u64().unwrap() as usize;
    assert_eq!(
        acc["componentType"].as_u64().unwrap(),
        5126,
        "expected FLOAT"
    );
    let offset = buffer_view_offset(doc, view);
    (0..count)
        .map(|i| {
            let s = offset + i * 12;
            [
                f32::from_le_bytes(doc.bin[s..s + 4].try_into().unwrap()),
                f32::from_le_bytes(doc.bin[s + 4..s + 8].try_into().unwrap()),
                f32::from_le_bytes(doc.bin[s + 8..s + 12].try_into().unwrap()),
            ]
        })
        .collect()
}

fn accessor_indices(doc: &GlbDoc, accessor_index: usize) -> Vec<u32> {
    let acc = &doc.json["accessors"][accessor_index];
    let view = acc["bufferView"].as_u64().unwrap() as usize;
    let count = acc["count"].as_u64().unwrap() as usize;
    let component_type = acc["componentType"].as_u64().unwrap();
    let offset = buffer_view_offset(doc, view);
    match component_type {
        5123 => (0..count)
            .map(|i| {
                let s = offset + i * 2;
                u32::from(u16::from_le_bytes(doc.bin[s..s + 2].try_into().unwrap()))
            })
            .collect(),
        5125 => (0..count)
            .map(|i| {
                let s = offset + i * 4;
                u32::from_le_bytes(doc.bin[s..s + 4].try_into().unwrap())
            })
            .collect(),
        other => panic!("unexpected index componentType {other}"),
    }
}

fn node_matrix_transform(node: &serde_json::Value) -> [[f32; 4]; 3] {
    let Some(m) = node.get("matrix").and_then(|v| v.as_array()) else {
        return IDENTITY;
    };
    let m: Vec<f32> = m.iter().map(|v| v.as_f64().unwrap() as f32).collect();
    [
        [m[0], m[4], m[8], m[12]],
        [m[1], m[5], m[9], m[13]],
        [m[2], m[6], m[10], m[14]],
    ]
}

fn transform_point(t: &[[f32; 4]; 3], p: [f32; 3]) -> [f32; 3] {
    [
        t[0][0] * p[0] + t[0][1] * p[1] + t[0][2] * p[2] + t[0][3],
        t[1][0] * p[0] + t[1][1] * p[1] + t[1][2] * p[2] + t[1][3],
        t[2][0] * p[0] + t[2][1] * p[1] + t[2][2] * p[2] + t[2][3],
    ]
}

/// Walks the glTF node graph from `scene.nodes`, appending every mesh primitive's triangles
/// (world-space) to `mesh`.
fn collect_visual_mesh(doc: &GlbDoc) -> CollisionMesh {
    let mut mesh = CollisionMesh::new();
    let attr = mesh
        .add_attribute(CollisionAttribute {
            name: "Visual".to_string(),
            interact_as: Vec::new(),
            interact_with: Vec::new(),
            interact_exclude: Vec::new(),
            synthetic: true,
        })
        .unwrap();
    let object = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldMesh,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });

    let nodes = doc.json["nodes"].as_array().expect("nodes array");
    let meshes = doc.json["meshes"].as_array().expect("meshes array");

    #[allow(clippy::too_many_arguments)]
    fn walk(
        doc: &GlbDoc,
        nodes: &[serde_json::Value],
        meshes: &[serde_json::Value],
        index: usize,
        parent: [[f32; 4]; 3],
        mesh_out: &mut CollisionMesh,
        attr: u16,
        object: u32,
    ) {
        let node = &nodes[index];
        let local = node_matrix_transform(node);
        let world = s2render::gltf::compose(&parent, &local);

        if let Some(mesh_index) = node.get("mesh").and_then(|v| v.as_u64()) {
            let gltf_mesh = &meshes[mesh_index as usize];
            for prim in gltf_mesh["primitives"].as_array().unwrap() {
                let pos_accessor = prim["attributes"]["POSITION"].as_u64().unwrap() as usize;
                let idx_accessor = prim["indices"].as_u64().unwrap() as usize;
                let positions = accessor_positions(doc, pos_accessor);
                let indices = accessor_indices(doc, idx_accessor);
                let world_positions: Vec<[f32; 3]> = positions
                    .iter()
                    .map(|&p| transform_point(&world, p))
                    .collect();
                let tris: Vec<[u32; 3]> = indices.as_chunks::<3>().0.to_vec();
                let _ = mesh_out.push_triangles(
                    &world_positions,
                    &tris,
                    attr,
                    |_| SurfaceProperty::NONE,
                    object,
                );
            }
        }

        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            for child in children {
                walk(
                    doc,
                    nodes,
                    meshes,
                    child.as_u64().unwrap() as usize,
                    world,
                    mesh_out,
                    attr,
                    object,
                );
            }
        }
    }

    let scene_index = doc.json["scene"].as_u64().unwrap_or(0) as usize;
    let roots = doc.json["scenes"][scene_index]["nodes"]
        .as_array()
        .expect("scene roots");
    for root in roots {
        walk(
            doc,
            nodes,
            meshes,
            root.as_u64().unwrap() as usize,
            IDENTITY,
            &mut mesh,
            attr,
            object,
        );
    }

    mesh
}

/// Excludes invisible collision-only volumes (sky/player-clip/NPC-clip/grenade-clip brushes,
/// `EntityPhysicsClip`) that block physics but have no render mesh, so the collision side of the
/// comparison doesn't hit a ceiling-height sky/clip boundary the visual mesh was never going to
/// have (`geom::filter::grenade_mask`/`player_mask`'s own docs: neither excludes every one of
/// these on its own, since grenade/player movement care about different subsets than "is this
/// rendered").
fn visible_ish_mask(mesh: &CollisionMesh) -> AttributeMask {
    const INVISIBLE_LAYERS: [&str; 4] = ["playerclip", "npcclip", "sky", "csgo_grenadeclip"];
    from_fn(mesh, |a| {
        !INVISIBLE_LAYERS
            .iter()
            .any(|bad| a.interact_as.iter().any(|x| x.eq_ignore_ascii_case(bad)))
            && a.name != "EntityPhysicsClip"
    })
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let idx = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[idx]
}

fn run_parity_check(map: &str) {
    let dir = cache_dir(map);
    let install = GameInstall::new(game_dir()).expect("valid CS2 install");
    let sources = Sources::open(&install.map_vpk(map), &install.csgo_dir)
        .unwrap_or_else(|e| panic!("open sources for {map}: {e}"));
    let result = export_map(&sources, &ExportOptions::default())
        .unwrap_or_else(|e| panic!("export {map}: {e}"));
    let doc = parse_glb(&result.glb);
    let visual_mesh = collect_visual_mesh(&doc);
    println!(
        "{map}: visual mesh {} vertices, {} triangles",
        visual_mesh.vertices.len(),
        visual_mesh.triangle_count()
    );

    let collision_mesh = cache::load_mesh(&dir).expect("load world.cgeo");
    let nav_corners = extract::mapdata::load_nav_areas(&dir).expect("load nav.json");
    println!("{map}: {} nav areas", nav_corners.len());

    let visual_bvh =
        Bvh::build(&visual_mesh, &visible_ish_mask(&visual_mesh), None).expect("build visual BVH");
    let collision_bvh = Bvh::build(&collision_mesh, &visible_ish_mask(&collision_mesh), None)
        .expect("build collision BVH");

    let mut centroids: Vec<V3> = nav_corners
        .iter()
        .filter(|c| !c.is_empty())
        .map(|corners| {
            let sum = corners.iter().fold(V3::ZERO, |a, &b| a + b);
            sum / corners.len() as f32
        })
        .collect();
    centroids.truncate(2000);

    // A window around each area's own (nav-corner-averaged) height, not one global top-to-bottom
    // ray: on a multi-story map (e.g. nuke's silo/ramp/upper yard) a ray from far above the whole
    // map hits whatever story is topmost at that (x,y), not the story the nav area is actually
    // on, which would flag every lower-level area as a huge false "gap".
    const ABOVE: f32 = 128.0;
    const BELOW: f32 = 512.0;

    let mut diffs: Vec<f32> = Vec::new();
    let mut large: Vec<(V3, f32, Option<f32>, Option<f32>)> = Vec::new();
    let mut visual_miss = 0usize;
    let mut collision_miss = 0usize;

    for c in &centroids {
        let high = c.z + ABOVE;
        let low = c.z - BELOW;
        let from = V3::new(c.x, c.y, high);
        let to = V3::new(c.x, c.y, low);
        let visual_hit = visual_bvh
            .first_hit_ray(from, to)
            .map(|h| high + h.t * (low - high));
        let collision_hit = collision_bvh
            .first_hit_ray(from, to)
            .map(|h| high + h.t * (low - high));
        match (visual_hit, collision_hit) {
            (Some(v), Some(cz)) => {
                let d = (v - cz).abs();
                diffs.push(d);
                if d > 32.0 {
                    large.push((*c, d, Some(v), Some(cz)));
                }
            }
            (None, Some(_)) => visual_miss += 1,
            (Some(_), None) => collision_miss += 1,
            (None, None) => {}
        }
    }

    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = percentile(&diffs, 0.5);
    let p95 = percentile(&diffs, 0.95);
    println!(
        "{map}: {} points compared, {} visual misses, {} collision misses",
        diffs.len(),
        visual_miss,
        collision_miss
    );
    println!(
        "{map}: height gap median={median:.3} p95={p95:.3} max={:.3}",
        diffs.last().copied().unwrap_or(f32::NAN)
    );
    println!("{map}: {} points with gap > 32 units:", large.len());
    for (c, d, v, cz) in large.iter().take(20) {
        println!(
            "  ({:.1},{:.1}) gap={d:.2} visual_z={v:?} collision_z={cz:?}",
            c.x, c.y
        );
    }

    assert!(
        diffs.len() > 100,
        "expected many comparable points, got {}",
        diffs.len()
    );
    assert!(
        median < 8.0,
        "median visual/collision height gap too large: {median}"
    );
}

#[test]
#[ignore = "needs CS2_GAME_DIR and a prior `cs2mod export-glb de_mirage`"]
fn de_mirage_visual_matches_collision() {
    run_parity_check("de_mirage");
}

#[test]
#[ignore = "needs CS2_GAME_DIR and a prior `cs2mod export-glb de_nuke`"]
fn de_nuke_visual_matches_collision() {
    run_parity_check("de_nuke");
}
