//! Check 1 (`s6f3a3_map.md` "Проверки") + item 12's real-map numbers: exports de_mirage
//! in-process and checks the reference-parity triangle count plus the tools/skin/materialExtras/
//! overlay numbers this receipt reports. Writes `render.glb`/`render.json` into a scratch temp
//! directory (guarded, removed at the end), not the shared repo `cache/`, which a running viewer
//! server may have `render.glb` open -- this test never fights that file for a Windows rename.
//!
//! `cargo test -p s2render --release --test export_mirage_real_game -- --ignored --nocapture`

use std::path::PathBuf;

use extract::game::GameInstall;
use s2render::export::{ExportOptions, export_map};
use s2render::source::Sources;

fn game_dir() -> PathBuf {
    match std::env::var_os("CS2_GAME_DIR") {
        Some(v) => PathBuf::from(v),
        None => panic!("CS2_GAME_DIR is not set; point it at '...\\game\\csgo' to run this test"),
    }
}

struct TempDir(PathBuf);
impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
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
        "s2render-export-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    TempDir(dir)
}

fn glb_json(bytes: &[u8]) -> serde_json::Value {
    let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    serde_json::from_slice(&bytes[20..20 + json_len]).expect("glTF JSON chunk")
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_export_matches_reference_numbers() {
    let install = GameInstall::new(game_dir()).expect("valid CS2 install");
    let sources = Sources::open(&install.map_vpk("de_mirage"), &install.csgo_dir)
        .expect("open de_mirage sources");

    let result = export_map(&sources, &ExportOptions::default()).expect("export de_mirage");

    // Written to a scratch cache dir (not the shared repo `cache/`) purely to also exercise the
    // same bytes `cs2mod export-glb` would write; the assertions below all read the in-memory
    // `result`, not this copy.
    let dir = temp_dir("de-mirage");
    std::fs::write(dir.join("render.glb"), &result.glb).expect("write render.glb");
    std::fs::write(
        dir.join("render.json"),
        serde_json::to_vec_pretty(&result.report).expect("serialize render.json"),
    )
    .expect("write render.json");

    let counts = &result.report["counts"];
    assert_eq!(
        counts["triangles"], 1_293_293,
        "reference parity: 1294008 - 217 (tools blocklight) - 498 (startdisabled entities)"
    );

    let tools = result.report["dropped"]["toolsMaterials"]
        .as_array()
        .expect("toolsMaterials array");
    let find_tool = |suffix: &str| {
        tools
            .iter()
            .find(|t| t["material"].as_str().unwrap().ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} not in dropped.toolsMaterials: {tools:?}"))
    };
    let blocklight = find_tool("toolsblocklight.vmat");
    assert_eq!(blocklight["drawCalls"], 2);
    assert_eq!(blocklight["triangles"], 7);
    let solidblocklight = find_tool("toolssolidblocklight.vmat");
    assert_eq!(solidblocklight["drawCalls"], 7);
    assert_eq!(solidblocklight["triangles"], 210);

    let per_material = result.report["perMaterialTriangles"]
        .as_array()
        .expect("perMaterialTriangles array");
    let material_triangles = |suffix: &str| {
        per_material
            .iter()
            .find(|m| m["material"].as_str().unwrap().ends_with(suffix))
            .map(|m| m["triangles"].as_u64().unwrap())
    };
    assert_eq!(
        material_triangles("tvebstest.vmat"),
        Some(164),
        "TV skin split: tvebstest draws"
    );
    assert_eq!(
        material_triangles("tvanimstatic.vmat"),
        Some(164),
        "TV skin split: tvanimstatic draws"
    );
    assert_eq!(
        material_triangles("tvstatic.vmat"),
        None,
        "tvstatic must be fully remapped away by the by-index skin swap (§3)"
    );

    let extras = result.report["materialExtras"]["byMaterial"]
        .as_object()
        .expect("materialExtras.byMaterial object");
    let layer_count = extras
        .values()
        .filter(|v| v.get("layers").is_some())
        .count();
    let tint_mask_count = extras
        .values()
        .filter(|v| v.get("tintMask").is_some())
        .count();
    let mod2x_count = extras
        .values()
        .filter(|v| v.get("blendMode").and_then(|b| b.as_str()) == Some("mod2x"))
        .count();
    assert_eq!(layer_count, 11, "layer-blended materials");
    assert_eq!(tint_mask_count, 2, "F_TINT_MASK materials");
    assert_eq!(mod2x_count, 1, "mod2x materials");

    let doc = glb_json(&result.glb);
    let materials = doc["materials"].as_array().expect("materials array");
    let meshes = doc["meshes"].as_array().expect("meshes array");
    let nodes = doc["nodes"].as_array().expect("nodes array");
    let qualifying: std::collections::HashSet<u64> = materials
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m["alphaMode"] == "BLEND"
                && m["pbrMetallicRoughness"]["baseColorFactor"][3]
                    .as_f64()
                    .is_some_and(|a| a < 1.0)
        })
        .map(|(i, _)| i as u64)
        .collect();
    let mut counts: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for n in nodes {
        let Some(mesh_idx) = n["mesh"].as_u64() else {
            continue;
        };
        let mesh = &meshes[mesh_idx as usize];
        for prim in mesh["primitives"].as_array().unwrap() {
            let mat_idx = prim["material"].as_u64().unwrap();
            if qualifying.contains(&mat_idx) {
                *counts.entry(mat_idx).or_insert(0) += 1;
            }
        }
    }
    for (idx, count) in &counts {
        println!(
            "BLEND<1 material {}: alpha={} instances={}",
            materials[*idx as usize]["name"],
            materials[*idx as usize]["pbrMetallicRoughness"]["baseColorFactor"][3],
            count
        );
    }
    let blend_alpha_below_one: u64 = counts.values().sum();
    assert_eq!(
        blend_alpha_below_one, 7,
        "win_square x2, decalstain002a, drainage_stain_01, debris_concrete001a, \
         train_cement_stain_01 x2"
    );

    let overlay_order_nodes = nodes
        .iter()
        .filter(|n| n["extras"]["overlayOrder"].is_number())
        .count();
    assert_eq!(
        overlay_order_nodes, 4,
        "one overlay object at order 1, three at order 2"
    );
}
