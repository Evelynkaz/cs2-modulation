//! Real game: set `CS2_GAME_DIR` to `...\game\csgo`, e.g.
//! `set CS2_GAME_DIR=D:\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo`
//! `cargo test -p s2render --release --test mesh_real_game -- --ignored --nocapture`
//!
//! Collects every model de_mirage's world nodes and entities reference (`m_renderableModel` on
//! scene objects/aggregates, `worldnode.rs`; the `model` entity key) and parses each one, then
//! prints totals and error/format breakdowns.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use s2fmt::resource::Resource;
use s2fmt::vpk::Vpk;
use s2fmt::worldnode;
use s2render::buffer::Buffer;
use s2render::mesh::Mesh;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!(
            "CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test, e.g. \
             `set CS2_GAME_DIR=D:\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo`"
        ),
    }
}

/// KV3 parsing needs a deep stack for nested documents (`stage2_common.md`); matches the other
/// crates' real-game tests.
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(f)
        .expect("spawn 16 MiB thread")
        .join()
        .expect("thread panicked");
}

fn find_in<'a>(vpks: &[&'a Vpk], path: &str) -> Option<(&'a Vpk, s2fmt::vpk::VpkEntry)> {
    for &vpk in vpks {
        if let Some(entry) = vpk.find(path) {
            return Some((vpk, entry.clone()));
        }
    }
    None
}

fn read_resource(vpks: &[&Vpk], path: &str) -> Result<Resource, String> {
    let (vpk, entry) = find_in(vpks, path).ok_or_else(|| "not found".to_string())?;
    let bytes = vpk.read(&entry).map_err(|e| e.to_string())?;
    Resource::parse(bytes).map_err(|e| e.to_string())
}

/// `world.vwrld_c` sits under `maps/<map>/`, not at the tree root, so this looks for an entry
/// whose path ends with `suffix` rather than matching it exactly (`extract::build`'s own
/// `find_entry_ending_with`).
fn read_resource_ending_with(vpk: &Vpk, suffix: &str) -> Result<Resource, String> {
    let entry = vpk
        .entries()
        .find(|e| e.path.to_ascii_lowercase().ends_with(suffix))
        .ok_or_else(|| format!("no entry ending with {suffix}"))?;
    let bytes = vpk.read(entry).map_err(|e| e.to_string())?;
    Resource::parse(bytes).map_err(|e| e.to_string())
}

/// Strips a trailing `_c` (world node `m_renderableModel` values carry it; entity `model` values
/// don't), then re-adds it: both dialects end up at the same compiled path.
fn compiled_path(model_path: &str) -> String {
    let stripped = model_path.strip_suffix("_c").unwrap_or(model_path);
    format!("{stripped}_c")
}

#[derive(Default)]
struct Stats {
    models_ok: usize,
    models_failed: usize,
    model_errors: HashMap<String, usize>,
    ref_meshes_ok: usize,
    ref_meshes_failed: usize,
    ref_mesh_errors: HashMap<String, usize>,
    meshes: usize,
    draw_calls: usize,
    non_triangle_draw_calls: usize,
    vertices: u64,
    triangles: u64,
    format_histogram: HashMap<(String, String), usize>,
    meshopt_header_histogram: HashMap<u8, usize>,
    zstd_buffers: usize,
    total_buffers: usize,
    /// Triangles per material basename (no directory, no `.vmat` extension), to compare against
    /// the VRF reference summary's `per_material_triangles` (keyed the same way).
    material_triangles: HashMap<String, u64>,
}

/// `materials/de_mirage/foo.vmat` -> `foo`, matching VRF's own
/// `Path.GetFileNameWithoutExtension(materialPath)` (`GltfModelExporter.Mesh.cs:433`).
fn material_basename(material_path: &str) -> String {
    let file = material_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(material_path);
    file.strip_suffix(".vmat").unwrap_or(file).to_string()
}

fn error_kind(e: &s2render::MeshError) -> String {
    // The enum's variant name, not the full (path-carrying) message, so errors group sensibly.
    let s = format!("{e:?}");
    s.split(['{', '(']).next().unwrap_or(&s).trim().to_string()
}

fn bump<K: std::hash::Hash + Eq>(map: &mut HashMap<K, usize>, key: K) {
    *map.entry(key).or_insert(0) += 1;
}

fn record_buffer(stats: &mut Stats, buffer: &Buffer, is_vertex: bool) {
    stats.total_buffers += 1;
    if buffer.compression.zstd {
        stats.zstd_buffers += 1;
    }
    if let Some(header) = buffer.compression.meshopt_header {
        bump(&mut stats.meshopt_header_histogram, header);
    }
    if is_vertex {
        for field in &buffer.fields {
            bump(
                &mut stats.format_histogram,
                (field.semantic_name.clone(), format!("{:?}", field.format)),
            );
        }
    }
}

fn record_mesh(stats: &mut Stats, mesh: &Mesh) {
    stats.meshes += 1;
    for b in &mesh.vertex_buffers {
        record_buffer(stats, b, true);
    }
    for b in &mesh.index_buffers {
        record_buffer(stats, b, false);
    }
    for scene_object in &mesh.scene_objects {
        for dc in &scene_object.draw_calls {
            stats.draw_calls += 1;
            if !dc.is_triangle_list {
                stats.non_triangle_draw_calls += 1;
                continue;
            }
            let triangles = (dc.index_count.max(0) as u64) / 3;
            stats.vertices += dc.vertex_count.max(0) as u64;
            stats.triangles += triangles;
            if let Some(m) = &dc.material_path {
                *stats
                    .material_triangles
                    .entry(material_basename(m))
                    .or_insert(0) += triangles;
            }
        }
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_meshes_and_models_parse_without_errors() {
    on_big_stack(|| {
        let csgo_dir = game_dir();
        let map_vpk_path = csgo_dir.join("maps").join("de_mirage.vpk");
        let shared_vpk_path = csgo_dir.join("pak01_dir.vpk");
        let map_vpk = Vpk::open(&map_vpk_path).expect("open map VPK");
        let shared_vpk = Vpk::open(&shared_vpk_path).expect("open shared VPK");
        let vpks: Vec<&Vpk> = vec![&map_vpk, &shared_vpk];

        // 1. World -> world nodes -> every scene object's/aggregate's renderable model.
        let world_doc =
            read_resource_ending_with(&map_vpk, "world.vwrld_c").expect("read world.vwrld_c");
        let world = worldnode::decode_world(&world_doc.data_kv3().unwrap().root)
            .expect("WorldError is uninhabited");

        let mut model_paths: HashSet<String> = HashSet::new();
        for prefix in &world.world_node_prefixes {
            let node_path = worldnode::world_node_path(prefix);
            let doc = match read_resource(&vpks, &node_path) {
                Ok(r) => r.data_kv3().expect("world node DATA is KV3"),
                Err(e) => {
                    println!("world node {node_path}: {e}");
                    continue;
                }
            };
            let node = worldnode::decode_world_node(&doc.root).expect("WorldError is uninhabited");
            for so in &node.scene_objects {
                if let Some(m) = &so.renderable_model {
                    model_paths.insert(compiled_path(m));
                }
            }
            for agg in &node.aggregate_scene_objects {
                if let Some(m) = &agg.renderable_model {
                    model_paths.insert(compiled_path(m));
                }
            }
        }
        println!("world node model refs: {}", model_paths.len());

        // 2. Every *.vents_c entity lump's `model` key (docs/FORMATS.md §7's own fallback: every
        // vents_c in the map VPK, rather than re-deriving `m_entityLumps`/`m_childLumps`
        // resolution here).
        let mut entity_model_count = 0usize;
        for entry in map_vpk.entries() {
            if !entry.extension().eq_ignore_ascii_case("vents_c") {
                continue;
            }
            let bytes = map_vpk.read(entry).expect("read vents_c");
            let resource = Resource::parse(bytes).expect("parse vents_c");
            let doc = resource.data_kv3().expect("vents_c DATA is KV3");
            let lump = s2fmt::entities::decode_entity_lump(&doc.root).expect("decode entity lump");
            for e in &lump.entities {
                if let Some(m) = e.get_str("model")
                    && !m.is_empty()
                {
                    model_paths.insert(compiled_path(m));
                    entity_model_count += 1;
                }
            }
        }
        println!("entity `model` refs seen: {entity_model_count}");
        println!("distinct compiled model paths: {}", model_paths.len());

        // 3. Parse every referenced model, and every mesh it embeds or refers to externally.
        let mut stats = Stats::default();
        let mut unsupported: HashSet<String> = HashSet::new();
        for path in &model_paths {
            let resource = match read_resource(&vpks, path) {
                Ok(r) => r,
                Err(e) => {
                    stats.models_failed += 1;
                    bump(&mut stats.model_errors, format!("resource: {e}"));
                    continue;
                }
            };
            let model = match s2render::model::decode_model(&resource) {
                Ok(m) => m,
                Err(e) => {
                    stats.models_failed += 1;
                    bump(&mut stats.model_errors, error_kind(&e));
                    unsupported.insert(format!("{path}: {e}"));
                    continue;
                }
            };
            stats.models_ok += 1;
            // Only the most detailed populated LOD level (change item 4): a model with several
            // LOD levels stores each as its own embedded/ref mesh, and counting every level would
            // multiply the triangle total by however many LODs it has.
            let level = model.lod.lowest_level;
            for em in &model.embedded_meshes {
                if !model.lod.is_mesh_in_level(em.mesh_index, level) {
                    continue;
                }
                record_mesh(&mut stats, &em.mesh);
            }
            for rm in &model.ref_meshes {
                if !model.lod.is_mesh_in_level(rm.mesh_index, level) {
                    continue;
                }
                let ref_path = compiled_path(&rm.mesh_path);
                let resource = match read_resource(&vpks, &ref_path) {
                    Ok(r) => r,
                    Err(e) => {
                        stats.ref_meshes_failed += 1;
                        bump(&mut stats.ref_mesh_errors, format!("resource: {e}"));
                        continue;
                    }
                };
                match s2render::mesh::decode_mesh_resource(&resource, &ref_path) {
                    Ok(mesh) => {
                        stats.ref_meshes_ok += 1;
                        record_mesh(&mut stats, &mesh);
                    }
                    Err(e) => {
                        stats.ref_meshes_failed += 1;
                        bump(&mut stats.ref_mesh_errors, error_kind(&e));
                        unsupported.insert(format!("{ref_path}: {e}"));
                    }
                }
            }
        }

        println!("=== de_mirage mesh/model parse ===");
        println!(
            "models ok: {}, failed: {}",
            stats.models_ok, stats.models_failed
        );
        println!(
            "ref meshes ok: {}, failed: {}",
            stats.ref_meshes_ok, stats.ref_meshes_failed
        );
        println!("meshes: {}", stats.meshes);
        println!(
            "draw calls: {} ({} non-triangle, skipped)",
            stats.draw_calls, stats.non_triangle_draw_calls
        );
        println!("vertices (triangle draw calls): {}", stats.vertices);
        println!("triangles: {}", stats.triangles);
        println!(
            "total buffers: {}, zstd-compressed: {}",
            stats.total_buffers, stats.zstd_buffers
        );
        println!(
            "meshopt stream header histogram: {:#?}",
            stats.meshopt_header_histogram
        );
        println!("attribute format histogram: {:#?}", stats.format_histogram);
        println!("model error kinds: {:#?}", stats.model_errors);
        println!("ref mesh error kinds: {:#?}", stats.ref_mesh_errors);
        println!("distinct materials: {}", stats.material_triangles.len());
        let mut by_triangles: Vec<(&String, &u64)> = stats.material_triangles.iter().collect();
        by_triangles.sort_by(|a, b| b.1.cmp(a.1));
        println!("top 20 materials by triangle count:");
        for (name, triangles) in by_triangles.iter().take(20) {
            println!("  {name}: {triangles}");
        }
        if !unsupported.is_empty() {
            println!("unsupported forms (up to 20):");
            for line in unsupported.iter().take(20) {
                println!("  {line}");
            }
        }

        assert_eq!(
            stats.models_failed, 0,
            "every referenced model must parse: {:#?}",
            stats.model_errors
        );
        assert_eq!(
            stats.ref_meshes_failed, 0,
            "every referenced external mesh must parse: {:#?}",
            stats.ref_mesh_errors
        );
        assert!(stats.models_ok > 0, "expected at least one model to parse");
        assert!(stats.triangles > 0, "expected at least one triangle");
    });
}
