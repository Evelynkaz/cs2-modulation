//! The 3D skybox (`s6f3a4_lighting.md` change item 8): a *separate* VPK, found via the main map's
//! own `skybox_reference` entity, exported through the exact same [`crate::export::export_map`]
//! pipeline (its own world walk, materials, lightmaps, probes) into its own `render_sky.glb` plus
//! a `skybox` block in the main map's `render.json`.
//!
//! Ground truth: `Renderer/Renderer/World/WorldLoader.cs:1456-1560` (own lighting, `p_main =
//! ((p_sky - sky_camera.origin)*scale)*skyboxReference`), `Renderer/Renderer/Renderer.cs:34-40,
//! 662-756` (draw order); facts (`skybox_reference`/`sky_camera` field names, per-map scale/origin)
//! cross-checked against `f3_survey/REPORT.md` §3 on real VPKs (Mirage/Dust2/Inferno/Nuke/Ancient).

use s2fmt::entities::Entity;
use serde_json::{Value, json};

use crate::entity::entity_num;
use crate::export::ExportResult;

/// The main map's own `skybox_reference` entity: which VPK holds the 3D skybox, and its own
/// (on every surveyed map, identity) placement transform.
#[derive(Debug, Clone)]
pub struct SkyboxReferenceInfo {
    /// `targetmapname`, e.g. `maps/prefabs/de_mirage/3dskybox_mirage_legacy.vmap`.
    pub target_map: String,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub scales: [f32; 3],
}

/// Reads the main map's `skybox_reference` entity, `None` if this map has no 3D skybox.
pub fn parse_skybox_reference(e: &Entity) -> Option<SkyboxReferenceInfo> {
    if !e.classname().eq_ignore_ascii_case("skybox_reference") {
        return None;
    }
    let target_map = e.get_str("targetmapname")?.to_string();
    Some(SkyboxReferenceInfo {
        target_map,
        origin: e.origin(),
        angles: e.angles(),
        scales: e.scales(),
    })
}

/// `targetmapname` (a `.vmap` resource path) to the map key `GameInstall::map_vpk` expects: strips
/// a leading `maps/` and a trailing `.vmap`, case-insensitively (`f3_survey/REPORT.md` §3: the VPK
/// sits at `<csgo>/maps/<that key>.vpk`, exactly like any other map).
pub fn map_key_from_target(target_map: &str) -> String {
    let normalized = target_map.replace('\\', "/");
    let stripped = normalized
        .strip_prefix("maps/")
        .or_else(|| normalized.strip_prefix("Maps/"))
        .unwrap_or(&normalized);
    stripped
        .strip_suffix(".vmap")
        .or_else(|| stripped.strip_suffix(".vmap_c"))
        .unwrap_or(stripped)
        .to_string()
}

/// The skybox's own `sky_camera` entity (lives only in the skybox VPK's own entity lump, never the
/// main map's -- `f3_survey/REPORT.md` §3): the scale/origin the viewer places its skybox camera
/// with (`export_map`'s `place_entities` detects this the same way it detects `skybox_reference`,
/// so it shows up in *every* export's own report -- null on a plain map, populated on a skybox
/// one).
#[derive(Debug, Clone)]
pub struct SkyCameraInfo {
    pub scale: f32,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
}

/// Reads a `sky_camera` entity, `None` if this isn't one or its (single-float) `scale` is absent.
pub fn parse_sky_camera(e: &Entity) -> Option<SkyCameraInfo> {
    if !e.classname().eq_ignore_ascii_case("sky_camera") {
        return None;
    }
    let scale = entity_num(e.get("scale"))? as f32;
    Some(SkyCameraInfo {
        scale,
        origin: e.origin(),
        angles: e.angles(),
    })
}

/// Extra files from the skybox's own export that the main map's `render.json`/cache directory must
/// not receive: its own 2D sky cube, cube fog and post-processing LUT. The 3D skybox reuses the
/// *outer* map's own 2D sky, fog and tonemapping (REPORT.md §5's "Skybox fog uses the main fog
/// constants at ×16 positions"; the 2D sky and post-processing are likewise drawn only once, from
/// the main map's own data) -- decoding this map's own copies of them inside `export_map` is
/// harmless (they're cheap and every map has them) but shipping the files would be dead weight.
fn is_dropped_extra_file(name: &str) -> bool {
    name == "render_sky_cube.bin"
        || name == "render_fog_cube.bin"
        || name == "render_lut.bin"
        || name.starts_with("render_sky_face_")
}

/// `render_lm_<name>_<mip>.bin` / `render_lm_<name>_fallback.png` -> `render_sky_lm_...` (avoids
/// colliding with the main map's own identically-named lightmap files in the same cache
/// directory -- both maps compile an `irradiance.vtex` etc).
fn rename_lightmap_file(name: &str) -> String {
    match name.strip_prefix("render_lm_") {
        Some(rest) => format!("render_sky_lm_{rest}"),
        None => name.to_string(),
    }
}

/// What [`merge_skybox_export`] produces: the skybox's own glb (under its own file name), the
/// extra files it needs (renamed/filtered), and the `skybox` value for the main map's
/// `render.json`.
pub struct SkyboxMerge {
    pub glb_file_name: String,
    pub glb: Vec<u8>,
    pub extra_files: Vec<(String, Vec<u8>)>,
    pub json: Value,
}

/// An error merging a skybox export -- never fatal to the main map's own export (the caller logs
/// this into the main report and ships without a `skybox` key, `s6f3a4_lighting.md`'s change item
/// 8 is additive).
#[derive(Debug, thiserror::Error)]
pub enum SkyboxMergeError {
    #[error("skybox VPK's own entity lump has no sky_camera entity")]
    NoSkyCamera,
}

/// Filters/renames `sky_result`'s own extra files and folds its report into the `skybox` block for
/// the main map's `render.json`. `reference` is the main map's own `skybox_reference` entity, kept
/// verbatim for the viewer/for debugging.
pub fn merge_skybox_export(
    reference: &SkyboxReferenceInfo,
    mut sky_result: ExportResult,
) -> Result<SkyboxMerge, SkyboxMergeError> {
    let sky_camera = sky_result.report.get("skyCamera").cloned();
    let sky_camera = match sky_camera {
        Some(v) if !v.is_null() => v,
        _ => return Err(SkyboxMergeError::NoSkyCamera),
    };

    let mut extra_files = Vec::with_capacity(sky_result.extra_files.len());
    for (name, bytes) in std::mem::take(&mut sky_result.extra_files) {
        if is_dropped_extra_file(&name) {
            continue;
        }
        extra_files.push((rename_lightmap_file(&name), bytes));
    }

    let mut report = sky_result.report;
    for key in ["lightmaps", "lightmapFallbacks"] {
        if let Some(arr) = report["lighting"][key].as_array_mut() {
            for entry in arr {
                if let Some(file) = entry.get("file").and_then(Value::as_str) {
                    let renamed = rename_lightmap_file(file);
                    entry["file"] = json!(renamed);
                }
            }
        }
    }
    report["sky"] = Value::Null;
    report["fog"] = Value::Null;
    report["postProcessing"] = Value::Null;
    report["skyFilesNote"] = json!(
        "this skybox map's own env_sky/env_cubemap_fog/post_processing_volume were decoded (cheap, \
         every map has them) but their files were dropped and these three keys nulled out: the 3D \
         skybox reuses the OUTER map's own 2D sky cube, fog and tonemapping/LUT (REPORT.md §5 \
         'Skybox fog uses the main fog constants at ×scale positions', §6) -- only this \
         map's own lightmaps/probes/materials/textures are kept, its render_lm_* files renamed to \
         render_sky_lm_* to avoid colliding with the outer map's own"
    );

    let json = json!({
        "file": "render_sky.glb",
        "fileMeaning": "a second, self-contained render.glb/render.json pair for the 3D skybox VPK found via the main map's skybox_reference.targetmapname (f3_survey/REPORT.md §3); this key's own 'report' is exactly that skybox's render.json, in the skybox's OWN local coordinates -- do not apply 'reference'/'origin'/'scale' below to its geometry, only to the viewer's own skybox camera",
        "scale": sky_camera["scale"],
        "origin": sky_camera["origin"],
        "originMeaning": "sky_camera's own origin/scale (read from the skybox VPK's own entity lump, never the main map's) -- see this key's own cameraFormula for how it combines with reference.origin",
        "reference": {
            "targetMap": reference.target_map,
            "origin": reference.origin,
            "angles": reference.angles,
            "scales": reference.scales,
        },
        "referenceMeaning": "the main map's own skybox_reference entity transform (skyboxReference in the p_main formula above); angles/scales are identity on every map surveyed (f3_survey/REPORT.md §3), but origin is not always zero (ar_shoots (0.733,575.999,0), ar_shoots_night, ar_baggage (-16,-1,27.575), de_overpass (0,0,4)) -- the viewer's camera-placement formula composes reference.origin in (see cameraFormula), the rotation/scale still assumed identity",
        "cameraFormula": "VRF composes p_main = (p_sky - origin)*scale + reference.origin (WorldLoader.cs:1536-1548); inverted for camera placement: skyCameraPos = origin + (mainCameraPos - reference.origin)/scale, same orientation as the main camera; far plane must also cover the skybox's own geometry bounds, not just mainCamera.far/scale (a small far/scale can clip real background scenery on some maps), near/far otherwise scaled by 1/scale",
        "renderOrder": "main scene opaque -> this skybox's own opaque geometry behind it with its own depth (clear depth in between) -> 2D sky -> this skybox's own translucents -> main scene translucents (REPORT.md §5, RR.cs:34-40,662-756)",
        "report": report,
    });

    Ok(SkyboxMerge {
        glb_file_name: "render_sky.glb".to_string(),
        glb: sky_result.glb,
        extra_files,
        json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_key_from_target_strips_maps_prefix_and_vmap_suffix() {
        assert_eq!(
            map_key_from_target("maps/prefabs/de_mirage/3dskybox_mirage_legacy.vmap"),
            "prefabs/de_mirage/3dskybox_mirage_legacy"
        );
        assert_eq!(
            map_key_from_target("prefabs/de_dust2_skybox.vmap"),
            "prefabs/de_dust2_skybox"
        );
        assert_eq!(
            map_key_from_target("de_ancient_skybox"),
            "de_ancient_skybox"
        );
    }

    #[test]
    fn is_dropped_extra_file_matches_only_2d_sky_fog_and_lut() {
        assert!(is_dropped_extra_file("render_sky_cube.bin"));
        assert!(is_dropped_extra_file("render_fog_cube.bin"));
        assert!(is_dropped_extra_file("render_lut.bin"));
        assert!(is_dropped_extra_file("render_sky_face_posx.png"));
        assert!(!is_dropped_extra_file("render_lm_irradiance_1.bin"));
        assert!(!is_dropped_extra_file("render_tex/abc123.bin"));
    }

    #[test]
    fn rename_lightmap_file_only_touches_render_lm_prefix() {
        assert_eq!(
            rename_lightmap_file("render_lm_irradiance_1.bin"),
            "render_sky_lm_irradiance_1.bin"
        );
        assert_eq!(
            rename_lightmap_file("render_lm_irradiance_fallback.png"),
            "render_sky_lm_irradiance_fallback.png"
        );
        assert_eq!(
            rename_lightmap_file("render_tex/abc.bin"),
            "render_tex/abc.bin"
        );
    }

    #[test]
    fn merge_fails_without_a_sky_camera_in_the_nested_report() {
        let reference = SkyboxReferenceInfo {
            target_map: "maps/prefabs/de_mirage/3dskybox_mirage_legacy.vmap".to_string(),
            origin: [0.0, 0.0, 0.0],
            angles: [0.0, 0.0, 0.0],
            scales: [1.0, 1.0, 1.0],
        };
        let sky_result = ExportResult {
            glb: Vec::new(),
            report: json!({ "skyCamera": null }),
            extra_files: Vec::new(),
        };
        assert!(matches!(
            merge_skybox_export(&reference, sky_result),
            Err(SkyboxMergeError::NoSkyCamera)
        ));
    }
}
