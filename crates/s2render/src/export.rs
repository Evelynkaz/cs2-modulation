//! Top-level map export: walks the world (scene objects + aggregates) and entities, resolves
//! materials/textures, and writes one glTF document -- `cs2mod export-glb`'s actual work (§2-§6).
//!
//! Ground truth: `IO/Gltf/GltfModelExporter.World.cs`, `GltfModelExporter.Mesh.cs`,
//! `GltfModelExporter.Material.cs`; facts cross-checked against `scratch/f3_survey/REPORT.md`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use s2fmt::entities::{self, Entity, EntityLump, entity_transform};
use serde_json::{Value, json};

use crate::buffer::Buffer;
use crate::color_correct;
use crate::entity as ent;
use crate::environment;
use crate::gltf::{self, GltfBuilder};
use crate::lightmaps;
use crate::material::{self, AlphaMode, MetalnessSource, RawMaterial, ResolvedMaterial};
use crate::mesh::{DrawCall, Mesh};
use crate::model::{self, Model};
use crate::native_texture::{ColorSpace, Loaded, TextureCatalog};
use crate::probes::{self, ProbeAtlasTextures, ProbeVolume};
use crate::source::{Sources, compiled_path};
use crate::world::{self, AggregateRaw, SceneObjectRaw, WorldNodeRaw};

/// Fixed encode scale for `_LPV`'s `UNSIGNED_SHORT` normalized channels (`gltf::add_lpv_u16`):
/// not computed per export (that would need a second pass over every baked vertex to find the
/// true max) -- `32` is a constant chosen because the probe irradiance maximum across every one
/// of the 23 installed maps checked is 23.56, on de_train, so it never clips.
const LPV_SCALE: f32 = 32.0;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("failed to read world.vwrld_c: {0}")]
    World(#[from] crate::source::SourceError),
    #[error(
        "no entity lumps could be resolved (world listed none, and no *.vents_c in the map VPK)"
    )]
    NoEntityLumps,
    #[error("failed to write glb: {0}")]
    Glb(#[from] crate::gltf::GlbTooLarge),
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub max_texture: u32,
    /// `--lightmap-quality high` (§1): irradiance at mip 0 (8192², the full-resolution file) in
    /// place of the mip-1 (4096²) default.
    pub lightmap_quality_high: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            max_texture: 1024,
            lightmap_quality_high: false,
        }
    }
}

pub struct ExportResult {
    pub glb: Vec<u8>,
    pub report: serde_json::Value,
    /// Files this export writes alongside `render.glb`/`render.json` (§1/§4/§6): raw lightmap/sky
    /// cube blocks, fallback PNGs, `render_lut.bin`, and (`s6f3a6_native_tex.md`) every material's
    /// `render_tex/<sha12>.bin`. `(file name, bytes)`.
    pub extra_files: Vec<(String, Vec<u8>)>,
}

/// Per-role texture budget (§2 of `s6f3a5_size.md`: "бюджет по ТИПУ, а не один на всё", carried
/// over unchanged by `s6f3a6_native_tex.md`, which only changes *how* a texture at a given budget
/// is stored -- raw BC/RGBA8 mips now, not JPEG/PNG). `Color` (base color, a layer's own color,
/// self-illum) stays at the export's full `--max-texture` budget, since surface albedo is what a
/// player's eye resolves most readily at typical viewing distance. `Normal` (the normal map's own
/// texel) and `Mask` (AO, metalness, blend modulation, the tint mask) both get the smaller
/// secondary budget: the spec's own suggested table allows normals "512–1024" (not just 1024) --
/// `SECONDARY_MAX_SIDE` picks the more conservative half of that same "512 или 256" range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TextureRole {
    Color,
    Normal,
    Mask,
}

/// The `Normal`/`Mask` roles' own cap before clamping to `--max-texture` (`s6f3a5_size.md`'s
/// constraint that the flag "remains the upper bound" for every type, not just color).
const SECONDARY_MAX_SIDE: u32 = 512;

impl TextureRole {
    fn max_side(self, base_max_side: u32) -> u32 {
        match self {
            TextureRole::Color => base_max_side,
            TextureRole::Normal | TextureRole::Mask => base_max_side.min(SECONDARY_MAX_SIDE),
        }
    }
}

/// Every dropped tools-material draw call, bucketed by `vmat` path (§6).
#[derive(Default)]
struct ToolsDrop {
    draw_calls: u32,
    triangles: u64,
}

#[derive(Default)]
struct ClassStats {
    total: u32,
    with_model: u32,
    drawn: u32,
    triangles: u64,
    reasons: HashMap<String, u32>,
}

#[derive(Default)]
struct Report {
    triangles: u64,
    per_material_triangles: HashMap<String, u64>,
    tools_dropped: HashMap<String, ToolsDrop>,
    missing_resources: Vec<String>,
    class_stats: HashMap<String, ClassStats>,
    material_extras: HashMap<u32, serde_json::Value>,
    /// `{classname, targetname, model, reason}` for every entity skipped by a §3 visibility rule
    /// (preview model, or `startdisabled`/`enabled`/`renderamt`/`rendermode`), in encounter order
    /// (§6's `dropped.hiddenEntities`).
    hidden_entities: Vec<serde_json::Value>,
    unlit_used: bool,
    scene_objects_total: u32,
    scene_objects_placed: u32,
    aggregates_total: u32,
    aggregates_fallback: u32,
    fragments_total: u32,
    fragments_placed: u32,
    /// Draw call / triangle split by §0's lighting classification (the checklist's "лайтмапленые/
    /// пробные draw call'ы и треугольники").
    lightmap_draw_calls: u32,
    lightmap_triangles: u64,
    unlit_draw_calls: u32,
    unlit_triangles: u64,
    probe_draw_calls: u32,
    probe_triangles: u64,
    /// Vertices whose `_LPV` bake fell back to zero because the atlas decode failed for that
    /// texel (not fatal -- §2).
    probe_sample_failures: u32,
    /// `env_combined_light_probe_volume` entities that looked like a probe volume but were
    /// missing a required field.
    probe_volumes_skipped: u32,
}

/// `(mesh_key, flat draw-call index, overlay, material index)` -- `get_or_build_mesh`'s glTF-mesh
/// dedup cache key.
type MeshKey = (String, usize, bool, u32);

/// Cached-decode state shared across the whole export: models, materials, textures and the
/// glTF-mesh dedup cache keyed by (geometry source, draw call, overlay, material).
struct Ctx<'a> {
    sources: &'a Sources,
    max_texture: u32,
    builder: GltfBuilder,
    models: HashMap<String, Option<Arc<Model>>>,
    raw_materials: HashMap<String, Option<Arc<RawMaterial>>>,
    resolved_materials: HashMap<String, Arc<ResolvedMaterial>>,
    gltf_materials: HashMap<(String, [u8; 4]), u32>,
    /// Native (raw BC/RGBA8) material textures, written as `render_tex/<sha12>.bin`
    /// (`s6f3a6_native_tex.md`, change item 1) -- replaces the old JPEG/PNG-embedded-in-glb path.
    tex_catalog: TextureCatalog,
    meshes: HashMap<MeshKey, (u32, [[f32; 4]; 3])>,
    /// Shared geometry for probe-lit draw calls, keyed without the material/tint (§2: geometry is
    /// instance-independent, only `_LPV` isn't).
    probe_geometries: HashMap<(String, usize, bool), Arc<ProbeGeometry>>,
    probe_volumes: Vec<ProbeVolume>,
    probe_atlas: Option<ProbeAtlasTextures>,
    /// The sun's baked shadow channel (§1's fix: the first `light_environment`'s
    /// `bakedshadowindex`, fallback `bakelightindex`, `0..=3`), used by every probe sample --
    /// `None` means no baked sun shadow exists, so probe visibility is always `1.0`.
    baked_shadow_channel: Option<usize>,
    report: Report,
}

fn quantize_tint(t: [f32; 4]) -> [u8; 4] {
    t.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl<'a> Ctx<'a> {
    fn get_model(&mut self, path: &str) -> Option<Arc<Model>> {
        if let Some(cached) = self.models.get(path) {
            return cached.clone();
        }
        let result = match self.sources.resource(path) {
            Ok(resource) => match model::decode_model(&resource) {
                Ok(m) => Some(Arc::new(m)),
                Err(e) => {
                    self.report
                        .missing_resources
                        .push(format!("{path}: failed to decode model: {e}"));
                    None
                }
            },
            Err(_) => {
                self.report
                    .missing_resources
                    .push(format!("{path}: not found"));
                None
            }
        };
        self.models.insert(path.to_string(), result.clone());
        result
    }

    fn get_raw_material(&mut self, path: &str) -> Option<Arc<RawMaterial>> {
        if let Some(cached) = self.raw_materials.get(path) {
            return cached.clone();
        }
        let compiled = compiled_path(path);
        let result = match self.sources.resource(&compiled) {
            Ok(resource) => match material::decode_material(&resource, &compiled) {
                Ok(m) => Some(Arc::new(m)),
                Err(e) => {
                    self.report
                        .missing_resources
                        .push(format!("{compiled}: failed to decode material: {e}"));
                    None
                }
            },
            Err(_) => {
                self.report
                    .missing_resources
                    .push(format!("{compiled}: not found"));
                None
            }
        };
        self.raw_materials.insert(path.to_string(), result.clone());
        result
    }

    fn get_resolved_material(&mut self, path: &str) -> Option<Arc<ResolvedMaterial>> {
        if let Some(r) = self.resolved_materials.get(path) {
            return Some(r.clone());
        }
        let raw = self.get_raw_material(path)?;
        let resolved = Arc::new(material::resolve(&raw));
        self.resolved_materials
            .insert(path.to_string(), resolved.clone());
        Some(resolved)
    }

    /// Loads `path` natively (`s6f3a6_native_tex.md`, `TextureCatalog::load`) at `role`'s budget
    /// and `color_space`, reporting (and returning `None` on) any failure -- the one entry point
    /// every material-texture reference below goes through.
    fn get_texture(
        &mut self,
        path: &str,
        role: TextureRole,
        color_space: ColorSpace,
    ) -> Option<Loaded> {
        let compiled = compiled_path(path);
        let max_side = role.max_side(self.max_texture);
        match self
            .tex_catalog
            .load(self.sources, &compiled, max_side, color_space)
        {
            Ok(loaded) => Some(loaded),
            Err(e) => {
                self.report
                    .missing_resources
                    .push(format!("{compiled}: failed to load texture: {e}"));
                None
            }
        }
    }

    /// Review fix item 1: a `csgo_environment` layer's two colour-correction matrices
    /// (`color_correct::layer_color_matrices`), reading `color_texture_path`'s vtex-header
    /// `Reflectivity` as the contrast pivot (`(1,1,1)` when there is no colour texture or the
    /// read fails, matching `RenderMaterial.cs`'s own `Vector3.One` fallback).
    fn layer_color_matrices(
        &mut self,
        color_texture_path: Option<&str>,
        params: &material::EnvHeightParams,
    ) -> color_correct::LayerColorMatrices {
        let reflectivity = color_texture_path
            .and_then(|path| {
                self.tex_catalog
                    .reflectivity(self.sources, &compiled_path(path))
            })
            .map(|r| [r[0], r[1], r[2]])
            .unwrap_or([1.0, 1.0, 1.0]);
        color_correct::layer_color_matrices(params.color_csb, params.color_tint, reflectivity)
    }

    /// Builds (or reuses) the glTF material for `vmat_path` tinted by `tint_rgba`; `None` if the
    /// material failed to load or is a tools material (caller checks the latter separately so it
    /// can also skip the draw call and bucket it in the report).
    fn get_gltf_material(&mut self, vmat_path: &str, tint_rgba: [f32; 4]) -> Option<u32> {
        let key = (vmat_path.to_string(), quantize_tint(tint_rgba));
        if let Some(&idx) = self.gltf_materials.get(&key) {
            return Some(idx);
        }
        let resolved = self.get_resolved_material(vmat_path)?;
        let (base_color_factor, extras_tint) = material::base_color_factor(&resolved, tint_rgba);

        let mut mat = serde_json::Map::new();
        mat.insert("name".into(), json!(vmat_path));

        // §7: standard glTF texture fields (baseColorTexture, normalTexture, the
        // metallic-roughness texture, ...) are never populated -- every material texture lives
        // only in render.json's own `textures[]`, referenced by index from `extras` below (this
        // viewer is the only reader of either file).
        let mut pbr = serde_json::Map::new();
        pbr.insert("baseColorFactor".into(), json!(base_color_factor));
        pbr.insert("metallicFactor".into(), json!(0.0));
        pbr.insert("roughnessFactor".into(), json!(1.0));
        if resolved.constant_black {
            pbr.insert(
                "baseColorFactor".into(),
                json!([0.0, 0.0, 0.0, base_color_factor[3]]),
            );
        }
        mat.insert("pbrMetallicRoughness".into(), Value::Object(pbr));

        mat.insert(
            "alphaMode".into(),
            json!(match resolved.alpha_mode {
                AlphaMode::Opaque => "OPAQUE",
                AlphaMode::Mask => "MASK",
                AlphaMode::Blend => "BLEND",
            }),
        );
        if let Some(cutoff) = resolved.alpha_cutoff {
            mat.insert("alphaCutoff".into(), json!(cutoff));
        }
        if resolved.double_sided {
            mat.insert("doubleSided".into(), json!(true));
        }
        if resolved.unlit {
            mat.insert("extensions".into(), json!({ "KHR_materials_unlit": {} }));
            self.report.unlit_used = true;
        }

        let mut extras = serde_json::Map::new();
        extras.insert(
            "baseColorAlphaMeaning".into(),
            json!(resolved.base_color_alpha_meaning),
        );
        if resolved.mod2x {
            extras.insert("blendMode".into(), json!("mod2x"));
        }
        if let Some(tint) = extras_tint {
            extras.insert("tint".into(), json!(tint));
            if let Some(mask_path) = &resolved.tint_mask_texture
                && let Some(loaded) =
                    self.get_texture(mask_path, TextureRole::Mask, ColorSpace::Linear)
            {
                insert_loaded(&mut extras, "tintMask", loaded, None);
            }
        }

        if !resolved.constant_black
            && let Some(tex_path) = &resolved.base_color_texture
        {
            // §4's mod2x exception: `csgo_static_overlay`/`csgo_unlitgeneric` with
            // `F_BLEND_MODE == 3` read `g_tColor` linear, not sRGB (`REPORT.md`'s "Exception").
            let color_space = if resolved.mod2x {
                ColorSpace::Linear
            } else {
                ColorSpace::Srgb
            };
            if let Some(loaded) = self.get_texture(tex_path, TextureRole::Color, color_space) {
                insert_loaded(&mut extras, "baseColor", loaded, Some(color_space));
            }
        }

        if let Some(normal_path) = &resolved.normal_texture
            && let Some(loaded) =
                self.get_texture(normal_path, TextureRole::Normal, ColorSpace::Linear)
        {
            insert_loaded(&mut extras, "normal", loaded, None);
        }

        if let Some(layers) = &resolved.layers {
            let mut layer_json = serde_json::Map::new();
            if let Some(p) = &layers.layer2_color
                && let Some(loaded) = self.get_texture(p, TextureRole::Color, ColorSpace::Srgb)
            {
                insert_loaded(
                    &mut layer_json,
                    "layer2Color",
                    loaded,
                    Some(ColorSpace::Srgb),
                );
            }
            if let Some(p) = &layers.layer2_normal
                && let Some(loaded) = self.get_texture(p, TextureRole::Normal, ColorSpace::Linear)
            {
                insert_loaded(&mut layer_json, "layer2Normal", loaded, None);
            }
            if let Some(p) = &layers.blend_modulation
                && let Some(loaded) = self.get_texture(p, TextureRole::Mask, ColorSpace::Linear)
            {
                insert_loaded(&mut layer_json, "blendModulation", loaded, None);
            }
            layer_json.insert("formula".into(), json!(material::LAYER_BLEND_FORMULA));
            extras.insert("layers".into(), Value::Object(layer_json));
        }

        // `s6f3a6_native_tex.md` change item 5's fix: `g_tColor2`/`g_tNormal2` are now resolved
        // (previously dropped entirely for every `csgo_environment_blend` material).
        // `s6f3a7_env_materials.md` change item 1: `g_tHeight2` and the height-band blend-weight/
        // roughness-remap/AO-levels/metalness inputs are exported alongside them so the viewer
        // can mix this layer in with the reference's own formula
        // (`csgo_environment.frag.slang:795-897`) instead of dropping it, as before.
        if let Some(env2) = &resolved.env_layer2
            && let Some(p) = &env2.color2
            && let Some(color2) = self.get_texture(p, TextureRole::Color, ColorSpace::Srgb)
            && let Some(p) = &env2.normal2
            && let Some(normal2) = self.get_texture(p, TextureRole::Normal, ColorSpace::Linear)
            && let Some(p) = &env2.height.height_texture
            && let Some(height2) = self.get_texture(p, TextureRole::Mask, ColorSpace::Linear)
        {
            // Requires all three (color/normal/height) to resolve to *something* -- a real
            // texture or a 4x4-constant (`Loaded::Constant`, review fix item 4: the viewer now
            // uploads those as a 1x1 texture instead of treating them as missing) both count.
            // Every one of the 69 csgo_environment_blend materials surveyed carries all three in
            // one form or the other, so falling back to layer-1-only rendering (rather than
            // mixing in a black/flat layer 2) never actually happens on the maps this exporter
            // re-exports; a handful (Ancient/Train/Vertigo) do have a constant `g_tHeight2`.
            let mut env_json = serde_json::Map::new();
            insert_loaded(&mut env_json, "color2", color2, Some(ColorSpace::Srgb));
            insert_loaded(&mut env_json, "normal2", normal2, None);
            insert_loaded(&mut env_json, "height2", height2, None);
            env_json.insert(
                "roughnessContrast2".into(),
                json!(env2.height.roughness_contrast),
            );
            env_json.insert(
                "roughnessBrightness2".into(),
                json!(env2.height.roughness_brightness),
            );
            // review fix item 7.
            env_json.insert("normalContrast2".into(), json!(env2.height.normal_contrast));
            env_json.insert("aoLevels2".into(), json!(env2.height.ao_levels));
            env_json.insert(
                "metalnessEnabled2".into(),
                json!(env2.height.metalness_enabled),
            );
            env_json.insert("heightScale1".into(), json!(env2.height_scale1));
            env_json.insert("heightZeroPoint1".into(), json!(env2.height_zero_point1));
            env_json.insert("heightScale2".into(), json!(env2.height_scale2));
            env_json.insert("heightZeroPoint2".into(), json!(env2.height_zero_point2));
            env_json.insert("blendSoftness2".into(), json!(env2.blend_softness2));
            // review fix item 8: which formula paths this material needs that the legacy
            // GetBlendWeights/mix(layer1,layer2,weight2) implementation above cannot reproduce.
            if !env2.unsupported.is_empty() {
                env_json.insert(
                    "unsupported".into(),
                    json!(
                        env2.unsupported
                            .iter()
                            .map(|(flag, why)| json!({ "flag": flag, "why": why }))
                            .collect::<Vec<_>>()
                    ),
                );
            }
            // review fix item 2: layer 2's own UV transform (`csgo_environment.vert.slang:
            // 175-180`, rotation/center always default on every material surveyed).
            env_json.insert("uvScale2".into(), json!(env2.height.uv_scale));
            env_json.insert("uvOffset2".into(), json!(env2.height.uv_offset));
            env_json.insert("uvRotation2".into(), json!(env2.height.uv_rotation));
            if let Some((dir, min_max)) = env2.facing2 {
                env_json.insert("facingDirection2".into(), json!(dir));
                env_json.insert("facingMinMax2".into(), json!(min_max));
            }
            // review fix item 1: per-layer colour-correction matrices (`RenderMaterial.cs:
            // 671-744`, `color_correct` module).
            let cc2 = self.layer_color_matrices(env2.color2.as_deref(), &env2.height);
            env_json.insert("colorAdjust2".into(), json!(cc2.color_adjust));
            env_json.insert("adjust2".into(), json!(cc2.adjust));
            env_json.insert(
                "colorCorrectionMode2".into(),
                json!(env2.height.color_correction_mode),
            );
            env_json.insert(
                "tintMaskContrast2".into(),
                json!(env2.height.tint_mask_contrast),
            );
            env_json.insert(
                "tintMaskBrightness2".into(),
                json!(env2.height.tint_mask_brightness),
            );
            env_json.insert(
                "formula".into(),
                json!(
                    "weight2 from csgo_environment.frag.slang:348-377 GetBlendWeights (legacy \
                     path only -- see this material's own 'unsupported' key if it needs \
                     F_USE_NEW_BLENDING instead), vertex paint _BLEND = vColorBlendValues.x + \
                     height1/height2.r bands; colour/roughness/AO/metalness/normal each \
                     mix(layer1, layer2, weight2), i.e. the reference's own CombineColor/\
                     CombineRoughness/CombineOcclusion/CombineNormal at their default \
                     overlay=0/replace=1/combine=0 (this material's own values, if it overrides \
                     any of them, are not read)"
                ),
            );
            extras.insert("envLayer2".into(), Value::Object(env_json));
        }

        // `s6f3a7_env_materials.md` change items 1/3: layer 1's own height/roughness-remap/
        // AO-levels/metalness inputs, for both plain `csgo_environment` and the blend shader
        // (`csgo_environment.frag.slang:63-73,769,1046`).
        if let Some(env1) = &resolved.env1
            && let Some(p) = &env1.height_texture
            && let Some(loaded) = self.get_texture(p, TextureRole::Mask, ColorSpace::Linear)
        {
            let mut env1_json = serde_json::Map::new();
            insert_loaded(&mut env1_json, "height1", loaded, None);
            env1_json.insert("roughnessContrast1".into(), json!(env1.roughness_contrast));
            env1_json.insert(
                "roughnessBrightness1".into(),
                json!(env1.roughness_brightness),
            );
            // review fix item 7.
            env1_json.insert("normalContrast1".into(), json!(env1.normal_contrast));
            env1_json.insert("aoLevels1".into(), json!(env1.ao_levels));
            env1_json.insert("metalnessEnabled1".into(), json!(env1.metalness_enabled));
            // review fix item 2: layer 1's own UV transform (exported for completeness; not
            // applied to layer-1 sampling today, see `EnvHeightParams::uv_scale`'s own doc).
            env1_json.insert("uvScale1".into(), json!(env1.uv_scale));
            env1_json.insert("uvOffset1".into(), json!(env1.uv_offset));
            // review fix item 1: layer 1's own colour-correction matrices.
            let cc1 = self.layer_color_matrices(resolved.base_color_texture.as_deref(), env1);
            env1_json.insert("colorAdjust1".into(), json!(cc1.color_adjust));
            env1_json.insert("adjust1".into(), json!(cc1.adjust));
            env1_json.insert(
                "colorCorrectionMode1".into(),
                json!(env1.color_correction_mode),
            );
            env1_json.insert("tintMaskContrast1".into(), json!(env1.tint_mask_contrast));
            env1_json.insert(
                "tintMaskBrightness1".into(),
                json!(env1.tint_mask_brightness),
            );
            extras.insert("env1".into(), Value::Object(env1_json));
        }

        // §7: AO/metalness/roughness aren't part of glTF's metallic-roughness texture (that would
        // need resampling AO/roughness/metalness to one shared resolution first); each stays its
        // own texture reference instead, same as `extras.layers` above.
        if let Some(ao_path) = &resolved.ao_texture
            && let Some(loaded) = self.get_texture(ao_path, TextureRole::Mask, ColorSpace::Linear)
        {
            insert_loaded(&mut extras, "ao", loaded, None);
            extras.insert("aoChannel".into(), json!("r"));
        }
        match &resolved.metalness {
            MetalnessSource::Texture(p) => {
                if let Some(loaded) = self.get_texture(p, TextureRole::Mask, ColorSpace::Linear) {
                    insert_loaded(&mut extras, "metalness", loaded, None);
                    // complex.frag.slang:604: `mat.Metalness = metalnessTexture.g` -- channel G,
                    // not R (unlike `g_tAmbientOcclusion`).
                    extras.insert("metalnessChannel".into(), json!("g"));
                }
            }
            MetalnessSource::Scalar(v) => {
                extras.insert("metalnessValue".into(), json!(v));
            }
        }
        if let Some(si) = &resolved.self_illum
            && let Some(loaded) =
                self.get_texture(&si.texture, TextureRole::Color, ColorSpace::Srgb)
        {
            insert_loaded(&mut extras, "selfIllum", loaded, Some(ColorSpace::Srgb));
            extras.insert("selfIllumScale".into(), json!(si.scale));
            extras.insert("selfIllumBrightness".into(), json!(si.brightness));
            extras.insert("selfIllumTint".into(), json!(si.tint));
            extras.insert("selfIllumAlbedoFactor".into(), json!(si.albedo_factor));
        }
        extras.insert(
            "noSpecularAtFullRoughness".into(),
            json!(resolved.no_specular_at_full_roughness),
        );
        extras.insert("fogEnabled".into(), json!(resolved.fog_enabled));

        if !extras.is_empty() {
            mat.insert("extras".into(), Value::Object(extras.clone()));
        }

        let index = self.builder.add_material(Value::Object(mat));
        if !extras.is_empty() {
            self.report
                .material_extras
                .insert(index, Value::Object(extras));
        }
        self.gltf_materials.insert(key, index);
        Some(index)
    }
}

/// Inserts either `"{key}Texture": <render.json textures[] index>` or, for a 4x4-constant source
/// (`native_texture::Loaded::Constant`), `"{key}Constant": [r,g,b,a]` (the pre-codec bytes a real
/// texture sample would be in) plus `"{key}ConstantCodec"` (and, when `color_space` is given,
/// `"{key}ConstantColorSpace"`) -- so the viewer runs the exact same per-texel decode on a constant
/// as it would on a sampled texel, rather than this exporter duplicating that decode
/// (`native_texture::Loaded`'s own doc comment).
fn insert_loaded(
    extras: &mut serde_json::Map<String, Value>,
    key: &str,
    loaded: Loaded,
    color_space: Option<ColorSpace>,
) {
    match loaded {
        Loaded::Texture(idx) => {
            extras.insert(format!("{key}Texture"), json!(idx));
        }
        Loaded::Constant { raw, codec } => {
            extras.insert(format!("{key}Constant"), json!(raw));
            extras.insert(format!("{key}ConstantCodec"), json!(codec.as_str()));
            if let Some(cs) = color_space {
                extras.insert(format!("{key}ConstantColorSpace"), json!(cs.as_str()));
            }
        }
    }
}

/// A field's decoded full-buffer array, for whichever of POSITION/NORMAL/TEXCOORD/`_BLEND` a
/// draw call carries -- found by searching every vertex buffer the draw call binds, since which
/// physical stream carries which semantic varies (`REPORT.md` §7: overlay UV sometimes sits in
/// the position stream).
fn find_field<'b>(
    mesh: &'b Mesh,
    dc: &DrawCall,
    semantic: &str,
    index: i32,
) -> Option<(&'b Buffer, &'b crate::buffer::InputLayoutField)> {
    for &vb_idx in &dc.vertex_buffers {
        if let Some(buf) = mesh.vertex_buffers.get(vb_idx)
            && let Some(f) = buf.field(semantic, index)
        {
            return Some((buf, f));
        }
    }
    None
}

struct BuiltGeometry {
    position: u32,
    /// The per-mesh KHR_mesh_quantization dequantization placement `add_positions` returned
    /// alongside `position` (`s6f3a5_size.md` change item 1) -- every node that instances this
    /// geometry must compose its own placement transform with this one (`gltf::compose`) so the
    /// quantized POSITION accessor decodes back to world space correctly.
    quantize: [[f32; 4]; 3],
    normal: u32,
    uv0: Option<u32>,
    uv1: Option<u32>,
    blend: Option<u32>,
    indices: u32,
    triangles: u64,
    /// The same positions/normals `add_positions`/`add_normals_quantized` were fed, kept only when
    /// `keep_raw` is set -- probe-lit primitives need the model-space vertices back to
    /// re-transform per instance and bake `_LPV` (§2); lightmap/unlit primitives never ask for
    /// this.
    raw_positions: Option<Vec<[f32; 3]>>,
    raw_normals: Option<Vec<[f32; 3]>>,
}

/// Decodes one draw call's geometry into fresh glTF accessors (positions/normals/UV/`_BLEND`
/// plus a compacted local index buffer), applying the overlay normal offset if requested (§4/§6).
#[allow(clippy::too_many_arguments)]
fn build_geometry(
    builder: &mut GltfBuilder,
    mesh: &Mesh,
    dc: &DrawCall,
    overlay: bool,
    needs_uv1: bool,
    lightmap_uv_scale: [f32; 2],
    needs_blend: bool,
    keep_raw: bool,
) -> Result<BuiltGeometry, crate::error::MeshError> {
    let abs_indices = dc.resolve_indices(mesh)?;

    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut order: Vec<u32> = Vec::new();
    let mut local_indices = Vec::with_capacity(abs_indices.len());
    for &ai in &abs_indices {
        let local = *remap.entry(ai).or_insert_with(|| {
            order.push(ai);
            (order.len() - 1) as u32
        });
        local_indices.push(local);
    }

    let (pos_buf, pos_field) =
        find_field(mesh, dc, "POSITION", 0).ok_or_else(|| crate::error::MeshError::Missing {
            path: "POSITION".to_string(),
        })?;
    let positions_full = crate::attributes::decode_positions(pos_buf, pos_field)?;

    let normals_full = find_field(mesh, dc, "NORMAL", 0)
        .map(|(b, f)| crate::attributes::decode_normals(b, f))
        .transpose()?;
    let uv0_full = find_field(mesh, dc, "TEXCOORD", 0)
        .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
        .transpose()?;
    // Lightmap UV = vertex field semantic `texcoord` (lowercase), index 3 ("vLightmapUV",
    // `VertexAttributeLocations.cs:89-90`) -- not index 1.
    let uv1_full = if needs_uv1 {
        find_field(mesh, dc, "TEXCOORD", 3)
            .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
            .transpose()?
    } else {
        None
    };
    let blend_full = if needs_blend {
        find_field(mesh, dc, "TEXCOORD", 4)
            .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
            .transpose()?
    } else {
        None
    };

    let mut positions = Vec::with_capacity(order.len());
    let mut normals = Vec::with_capacity(order.len());
    let mut uv0 = Vec::with_capacity(order.len());
    let mut uv1 = Vec::with_capacity(order.len());
    let mut blend = Vec::with_capacity(order.len());

    for &ai in &order {
        let i = ai as usize;
        let mut p = *positions_full.get(i).unwrap_or(&[0.0, 0.0, 0.0]);
        let n = normals_full
            .as_ref()
            .and_then(|arr| arr.get(i))
            .map(|n| n.normal)
            .unwrap_or([0.0, 0.0, 1.0]);
        if overlay {
            p = [
                p[0] + n[0] * material::OVERLAY_NORMAL_OFFSET,
                p[1] + n[1] * material::OVERLAY_NORMAL_OFFSET,
                p[2] + n[2] * material::OVERLAY_NORMAL_OFFSET,
            ];
        }
        positions.push(p);
        normals.push(n);
        uv0.push(
            uv0_full
                .as_ref()
                .map(|a| [a.get(i)[0], a.get(i)[1]])
                .unwrap_or([0.0, 0.0]),
        );
        if needs_uv1 {
            let raw = uv1_full
                .as_ref()
                .map(|a| [a.get(i)[0], a.get(i)[1]])
                .unwrap_or([0.0, 0.0]);
            uv1.push([raw[0] * lightmap_uv_scale[0], raw[1] * lightmap_uv_scale[1]]);
        }
        if needs_blend {
            let w = blend_full.as_ref().map(|a| a.get(i)[0]).unwrap_or(0.0);
            blend.push(w.clamp(0.0, 1.0));
        }
    }

    let triangles = (local_indices.len() / 3) as u64;
    let (position, quantize) = builder.add_positions(&positions);
    let normal = builder.add_normals_quantized(&normals);
    let uv0_idx = Some(builder.add_uv0(&uv0));
    let uv1_idx = if needs_uv1 {
        Some(builder.add_uv1_u16(&uv1))
    } else {
        None
    };
    let blend_idx = if needs_blend {
        Some(builder.add_blend_f32(&blend))
    } else {
        None
    };
    let indices = builder.add_indices(&local_indices);

    Ok(BuiltGeometry {
        position,
        quantize,
        normal,
        uv0: uv0_idx,
        uv1: uv1_idx,
        blend: blend_idx,
        indices,
        triangles,
        raw_positions: keep_raw.then(|| positions.clone()),
        raw_normals: keep_raw.then(|| normals.clone()),
    })
}

/// Which of §0's three light sources a primitive gets (`Mesh.cs:202-204`, `RenderableMesh.cs:
/// 314-325`): a draw call with baked lightmap data *and* an actual TEXCOORD index-3 stream draws
/// from the lightmap; otherwise an unlit material has no indirect term at all; otherwise it falls
/// back to probe volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LightingClass {
    Lightmap,
    Unlit,
    Probe,
}

impl LightingClass {
    fn as_str(self) -> &'static str {
        match self {
            LightingClass::Lightmap => "lightmap",
            LightingClass::Unlit => "unlit",
            LightingClass::Probe => "probe",
        }
    }
}

fn classify_lighting(mesh: &Mesh, dc: &DrawCall, material_unlit: bool) -> LightingClass {
    let has_lightmap_uv = find_field(mesh, dc, "TEXCOORD", 3).is_some();
    if dc.has_baked_lighting_from_lightmap && has_lightmap_uv {
        LightingClass::Lightmap
    } else if material_unlit {
        LightingClass::Unlit
    } else {
        LightingClass::Probe
    }
}

/// Geometry shared across every instance of a probe-lit draw call: POSITION/NORMAL/TEXCOORD_0/
/// `_BLEND`/indices are identical regardless of placement, only `_LPV` differs per instance
/// (§2) -- so instances share these accessors, each building its own fresh primitive/mesh with
/// its own `_LPV` accessor (see `bake_and_place_probe_instance`).
struct ProbeGeometry {
    position: u32,
    /// See `BuiltGeometry::quantize` -- every instance of this geometry composes its own placement
    /// transform with this one before instancing the shared POSITION accessor.
    quantize: [[f32; 4]; 3],
    normal: u32,
    uv0: Option<u32>,
    blend: Option<u32>,
    indices: u32,
    local_positions: Vec<[f32; 3]>,
    local_normals: Vec<[f32; 3]>,
}

/// What [`get_or_build_mesh`] produced: a shared mesh a caller can instance directly via
/// [`add_instance`], or probe geometry a caller must still bake `_LPV` for and place itself (via
/// `bake_and_place_probe_instance`) since baking needs the instance's own world transform.
enum MeshBuild {
    Shared {
        mesh_idx: u32,
        /// See `BuiltGeometry::quantize`.
        quantize: [[f32; 4]; 3],
    },
    Probe {
        geometry: Arc<ProbeGeometry>,
        material_index: u32,
    },
}

/// Gets or builds the geometry for `(mesh_key, draw call index into `mesh`'s own flattened
/// `m_sceneObjects[].m_drawCalls[]` list, overlay, material)`. Multiple placements sharing this
/// key instance the same glTF mesh (§6's "instances are nodes with matrices") -- a deliberate
/// simplification vs. the reference's "one glTF mesh per source vmesh, N primitives" grouping:
/// geometrically and in triangle count these are identical, only the JSON grouping granularity
/// differs. Probe-lit draw calls (`LightingClass::Probe`) are the one exception: their geometry
/// (not material) is still shared and cached, but each instance gets its own primitive/mesh so it
/// can carry its own `_LPV` (§2).
#[allow(clippy::too_many_arguments)]
fn get_or_build_mesh(
    ctx: &mut Ctx,
    mesh_key: &str,
    mesh: &Mesh,
    flat_index: usize,
    dc: &DrawCall,
    overlay: bool,
    lightmap_uv_scale: [f32; 2],
    tint_rgba: [f32; 4],
) -> Option<(MeshBuild, u64)> {
    let Some(material_path) = &dc.material_path else {
        ctx.report.missing_resources.push(format!(
            "{mesh_key}#{flat_index}: draw call has no material path"
        ));
        return None;
    };
    let tris = (dc.index_count.max(0) / 3) as u64;
    let raw = ctx.get_raw_material(material_path)?;
    if raw.is_tools_material() {
        let entry = ctx
            .report
            .tools_dropped
            .entry(material_path.clone())
            .or_default();
        entry.draw_calls += 1;
        entry.triangles += tris;
        return None;
    }

    let resolved = ctx.get_resolved_material(material_path)?;
    let lighting = classify_lighting(mesh, dc, resolved.unlit);

    let material_index = ctx.get_gltf_material(material_path, tint_rgba)?;
    // Every instance (cache hit or not) contributes `tris` to the report's "instanced" total,
    // matching VRF's own `triangles_instanced` -- an instance sharing a cached mesh still draws
    // its own copy of the triangles at render time (§6's "so counts the reference").
    ctx.report.triangles += tris;
    *ctx.report
        .per_material_triangles
        .entry(material_path.clone())
        .or_insert(0) += tris;
    match lighting {
        LightingClass::Lightmap => {
            ctx.report.lightmap_draw_calls += 1;
            ctx.report.lightmap_triangles += tris;
        }
        LightingClass::Unlit => {
            ctx.report.unlit_draw_calls += 1;
            ctx.report.unlit_triangles += tris;
        }
        LightingClass::Probe => {
            ctx.report.probe_draw_calls += 1;
            ctx.report.probe_triangles += tris;
        }
    }

    // `s6f3a7_env_materials.md` change item 1: `csgo_environment_blend` reads the exact same
    // TEXCOORD4/`VertexPaintBlendParams` vertex stream as `csgo_lightmappedgeneric`'s `F_LAYERS`
    // blend (`vColorBlendValues.x`, `csgo_environment.vert.slang:26,242` vs.
    // `complex.frag.slang:413`) -- same `_BLEND` accessor, no new vertex attribute needed.
    let needs_blend = resolved.layers.is_some() || resolved.env_layer2.is_some();

    if lighting == LightingClass::Probe {
        let geom_key = (mesh_key.to_string(), flat_index, overlay);
        if let Some(geom) = ctx.probe_geometries.get(&geom_key) {
            let geom = geom.clone();
            return Some((
                MeshBuild::Probe {
                    geometry: geom,
                    material_index,
                },
                tris,
            ));
        }
        let built = match build_geometry(
            &mut ctx.builder,
            mesh,
            dc,
            overlay,
            false,
            lightmap_uv_scale,
            needs_blend,
            true,
        ) {
            Ok(g) => g,
            Err(e) => {
                ctx.report
                    .missing_resources
                    .push(format!("{mesh_key}#{flat_index}: {e}"));
                return None;
            }
        };
        let geom = Arc::new(ProbeGeometry {
            position: built.position,
            quantize: built.quantize,
            normal: built.normal,
            uv0: built.uv0,
            blend: built.blend,
            indices: built.indices,
            local_positions: built.raw_positions.unwrap_or_default(),
            local_normals: built.raw_normals.unwrap_or_default(),
        });
        ctx.probe_geometries.insert(geom_key, geom.clone());
        debug_assert_eq!(built.triangles, tris);
        return Some((
            MeshBuild::Probe {
                geometry: geom,
                material_index,
            },
            tris,
        ));
    }

    let key = (mesh_key.to_string(), flat_index, overlay, material_index);
    if let Some(&(mesh_idx, quantize)) = ctx.meshes.get(&key) {
        return Some((MeshBuild::Shared { mesh_idx, quantize }, tris));
    }

    let needs_uv1 = lighting == LightingClass::Lightmap;
    let geometry = match build_geometry(
        &mut ctx.builder,
        mesh,
        dc,
        overlay,
        needs_uv1,
        lightmap_uv_scale,
        needs_blend,
        false,
    ) {
        Ok(g) => g,
        Err(e) => {
            ctx.report
                .missing_resources
                .push(format!("{mesh_key}#{flat_index}: {e}"));
            return None;
        }
    };

    let mut attributes = serde_json::Map::new();
    attributes.insert("POSITION".into(), json!(geometry.position));
    attributes.insert("NORMAL".into(), json!(geometry.normal));
    if let Some(uv0) = geometry.uv0 {
        attributes.insert("TEXCOORD_0".into(), json!(uv0));
    }
    if let Some(uv1) = geometry.uv1 {
        attributes.insert("TEXCOORD_1".into(), json!(uv1));
    }
    if let Some(blend) = geometry.blend {
        attributes.insert("_BLEND".into(), json!(blend));
    }
    let primitive = json!({
        "attributes": Value::Object(attributes),
        "indices": geometry.indices,
        "material": material_index,
        "extras": { "lighting": lighting.as_str() },
    });
    let mesh_json = json!({ "primitives": [primitive] });
    let mesh_idx = ctx.builder.add_mesh(mesh_json);
    let quantize = geometry.quantize;
    ctx.meshes.insert(key, (mesh_idx, quantize));
    debug_assert_eq!(
        geometry.triangles, tris,
        "resolved index count must match the draw call's own"
    );

    Some((MeshBuild::Shared { mesh_idx, quantize }, tris))
}

/// Bakes `_LPV` for one probe-lit instance (§2) and places it: transforms `geometry`'s
/// model-space vertices to world space with `transform`, binds a probe volume (handshake first,
/// else `fallback_point_override` when given -- one world AABB centre over every draw call of the
/// *placement*, not just this one instance's geometry, REPORT.md §2's binding rule -- else this
/// instance's own world-space bounds centre, else the global volume -- `probes::bind_volume`),
/// samples the ambient cube + sun visibility per vertex, and adds a fresh primitive/mesh/node
/// (never shared -- a different instance samples different world positions).
#[allow(clippy::too_many_arguments)]
fn bake_and_place_probe_instance(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    name: Option<&str>,
    geometry: &ProbeGeometry,
    material_index: u32,
    transform: &[[f32; 4]; 3],
    light_probe_handshake: i64,
    overlay_order: Option<i64>,
    fallback_point_override: Option<[f32; 3]>,
) {
    let n = geometry.local_positions.len();
    let mut world_positions = Vec::with_capacity(n);
    let mut world_normals = Vec::with_capacity(n);
    let mut world_min = [f32::INFINITY; 3];
    let mut world_max = [f32::NEG_INFINITY; 3];
    for i in 0..n {
        let p = probes::apply_affine(transform, geometry.local_positions[i]);
        let raw_n = geometry
            .local_normals
            .get(i)
            .copied()
            .unwrap_or([0.0, 0.0, 1.0]);
        let ln = probes::apply_linear(transform, raw_n);
        let len = (ln[0] * ln[0] + ln[1] * ln[1] + ln[2] * ln[2]).sqrt();
        let wn = if len > 1e-8 {
            [ln[0] / len, ln[1] / len, ln[2] / len]
        } else {
            [0.0, 0.0, 1.0]
        };
        for c in 0..3 {
            world_min[c] = world_min[c].min(p[c]);
            world_max[c] = world_max[c].max(p[c]);
        }
        world_positions.push(p);
        world_normals.push(wn);
    }
    let bounds_center = fallback_point_override.unwrap_or(if n > 0 {
        [
            (world_min[0] + world_max[0]) / 2.0,
            (world_min[1] + world_max[1]) / 2.0,
            (world_min[2] + world_max[2]) / 2.0,
        ]
    } else {
        [0.0, 0.0, 0.0]
    });

    let volume = probes::bind_volume(&ctx.probe_volumes, light_probe_handshake, bounds_center);
    let shadow_channel = ctx.baked_shadow_channel;
    let mut lpv = Vec::with_capacity(n);
    for i in 0..n {
        let sample = volume.and_then(|v| {
            ctx.probe_atlas
                .as_ref()
                .and_then(|a| a.sample(v, world_positions[i], world_normals[i], shadow_channel))
        });
        match sample {
            Some((irr, vis)) => lpv.push([irr[0], irr[1], irr[2], vis]),
            None => {
                ctx.report.probe_sample_failures += 1;
                lpv.push([0.0, 0.0, 0.0, 0.0]);
            }
        }
    }

    let lpv_accessor = ctx.builder.add_lpv_u16(&lpv, LPV_SCALE);

    let mut attributes = serde_json::Map::new();
    attributes.insert("POSITION".into(), json!(geometry.position));
    attributes.insert("NORMAL".into(), json!(geometry.normal));
    if let Some(uv0) = geometry.uv0 {
        attributes.insert("TEXCOORD_0".into(), json!(uv0));
    }
    if let Some(blend) = geometry.blend {
        attributes.insert("_BLEND".into(), json!(blend));
    }
    attributes.insert("_LPV".into(), json!(lpv_accessor));
    let primitive = json!({
        "attributes": Value::Object(attributes),
        "indices": geometry.indices,
        "material": material_index,
        "extras": { "lighting": "probe" },
    });
    let mesh_idx = ctx.builder.add_mesh(json!({ "primitives": [primitive] }));

    add_instance(
        ctx,
        children,
        name,
        mesh_idx,
        &gltf::compose(transform, &geometry.quantize),
        overlay_order,
        light_probe_handshake,
    );
}

fn flatten_draw_calls(mesh: &Mesh) -> Vec<&DrawCall> {
    mesh.scene_objects
        .iter()
        .flat_map(|so| so.draw_calls.iter())
        .collect()
}

/// `overlay_order` is `Some(m_nOverlayRenderOrder)` only for a placement whose *object* is itself
/// flagged overlay (`OBJECT_TYPE_OVERLAY`), never merely because a material sets `F_OVERLAY` (§4);
/// aggregates and entities always pass `None`. Written as `extras.overlayOrder` on the node only
/// when `Some`.
fn add_instance(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    name: Option<&str>,
    mesh_idx: u32,
    transform: &[[f32; 4]; 3],
    overlay_order: Option<i64>,
    light_probe_handshake: i64,
) {
    let mut node = serde_json::Map::new();
    if let Some(n) = name {
        node.insert("name".into(), json!(n));
    }
    node.insert("mesh".into(), json!(mesh_idx));
    if *transform != world::IDENTITY_TRANSFORM {
        node.insert("matrix".into(), json!(gltf::node_matrix(transform)));
    }
    let mut extras = serde_json::Map::new();
    if let Some(order) = overlay_order {
        extras.insert("overlayOrder".into(), json!(order));
    }
    if light_probe_handshake != 0 {
        extras.insert("lightProbeHandshake".into(), json!(light_probe_handshake));
    }
    if !extras.is_empty() {
        node.insert("extras".into(), Value::Object(extras));
    }
    let idx = ctx.builder.add_node(Value::Object(node));
    children.push(idx);
}

/// Every mesh a regular (non-aggregate) placement's model carries, at its lowest (most detailed)
/// LOD level: embedded meshes first, then referenced ones, matching `LoadModelMeshes`
/// (`GltfModelExporter.cs:576-598`).
fn model_meshes_at_lowest_lod(
    ctx: &mut Ctx,
    model_path: &str,
    model: &Model,
) -> Vec<(String, Arc<Mesh>)> {
    let level = model.lod.lowest_level;
    let mut out = Vec::new();
    for em in &model.embedded_meshes {
        if !model.lod.is_mesh_in_level(em.mesh_index, level) {
            continue;
        }
        out.push((
            format!("{model_path}#embedded{}", em.mesh_index),
            Arc::new(em.mesh.clone()),
        ));
    }
    for rm in &model.ref_meshes {
        if !model.lod.is_mesh_in_level(rm.mesh_index, level) {
            continue;
        }
        let ref_path = compiled_path(&rm.mesh_path);
        match ctx.sources.resource(&ref_path) {
            Ok(resource) => match crate::mesh::decode_mesh_resource(&resource, &ref_path) {
                Ok(m) => out.push((
                    format!("{model_path}#ref{}:{ref_path}", rm.mesh_index),
                    Arc::new(m),
                )),
                Err(e) => ctx
                    .report
                    .missing_resources
                    .push(format!("{ref_path}: failed to decode mesh: {e}")),
            },
            Err(_) => ctx
                .report
                .missing_resources
                .push(format!("{ref_path}: not found")),
        }
    }
    out
}

fn combine_tint(outer: [f32; 4], dc: &DrawCall) -> [f32; 4] {
    let rgb = dc.tint_color.unwrap_or([1.0, 1.0, 1.0]);
    let a = dc.alpha.unwrap_or(1.0);
    [
        outer[0] * rgb[0],
        outer[1] * rgb[1],
        outer[2] * rgb[2],
        outer[3] * a,
    ]
}

fn place_scene_object(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    so: &SceneObjectRaw,
    lightmap_uv_scale: [f32; 2],
) {
    // Absent/default `m_nOverlayRenderOrder` (0) means "no explicit order", not "order 0" -- on
    // Mirage most overlay objects have no order set at all, only 4 do (§4).
    let overlay_order = (so.overlay_render_order != 0).then_some(so.overlay_render_order);
    if let Some(renderable_model) = &so.renderable_model {
        let path = compiled_path(renderable_model);
        let Some(model) = ctx.get_model(&path) else {
            return;
        };
        // Overlay world scene objects still go through `place_model`'s non-overlay path when the
        // object flag alone drives it; folded in below by re-resolving materials per draw call
        // instead (kept simple: overlay offset also applies when the *material* says overlay,
        // handled inside `get_or_build_mesh`'s geometry key via the object-level flag OR'd with
        // the material's own -- see the call below).
        place_model_with_overlay(
            ctx,
            children,
            &path,
            &model,
            None,
            &so.transform,
            so.tint,
            None,
            lightmap_uv_scale,
            so.is_overlay,
            overlay_order,
            so.light_probe_volume_handshake,
        );
    } else if let Some(renderable) = &so.renderable {
        let path = compiled_path(renderable);
        let Ok(resource) = ctx.sources.resource(&path) else {
            ctx.report
                .missing_resources
                .push(format!("{path}: not found"));
            return;
        };
        let mesh = match crate::mesh::decode_mesh_resource(&resource, &path) {
            Ok(m) => m,
            Err(e) => {
                ctx.report
                    .missing_resources
                    .push(format!("{path}: failed to decode mesh: {e}"));
                return;
            }
        };
        place_mesh_draw_calls(
            ctx,
            children,
            &path,
            &mesh,
            None,
            &so.transform,
            so.tint,
            None,
            lightmap_uv_scale,
            so.is_overlay,
            overlay_order,
            so.light_probe_volume_handshake,
            None,
        );
    }
}

/// Places every triangle-list draw call in one already-resolved mesh: overlay is `object_overlay
/// || material.material_says_overlay` (§4), with an optional per-draw-call skin remap. Shared by
/// [`place_model_with_overlay`] (looping over a model's meshes) and `place_scene_object`'s
/// `m_renderable` path (already just one mesh), so both `m_renderableModel` and `m_renderable`
/// scene objects place their draw calls the same way (§2/§11). `bounds_center_override`, when
/// `Some`, is the probe-binding centre used for every probe-lit draw call here instead of this
/// mesh's own bounds -- `place_model_with_overlay` passes the union over every mesh in the whole
/// model (REPORT.md's binding rule: one box per model *node*, the union of all its meshes, not
/// per mesh -- `ModelSceneNode.Bones.cs:76-83`, `Scene.cs:2212`, §13); the `m_renderable` path has
/// no sibling meshes to union, so it passes `None` and keeps its own single-mesh centre.
#[allow(clippy::too_many_arguments)]
fn place_mesh_draw_calls(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    mesh_key: &str,
    mesh: &Mesh,
    name: Option<&str>,
    transform: &[[f32; 4]; 3],
    tint_rgba: [f32; 4],
    skin_map: Option<&HashMap<String, String>>,
    lightmap_uv_scale: [f32; 2],
    object_overlay: bool,
    overlay_order: Option<i64>,
    light_probe_handshake: i64,
    bounds_center_override: Option<[f32; 3]>,
) -> u64 {
    let mut triangles = 0u64;
    let flat = flatten_draw_calls(mesh);
    // §13's fix: one world AABB centre over *every* draw call of this placement (not just a
    // single probe-lit draw call's own geometry), so every probe-lit draw call belonging to the
    // same object binds to the same volume instead of a draw-call boundary splitting one object
    // across two volumes.
    let placement_bounds_center = bounds_center_override
        .unwrap_or_else(|| placement_world_bounds_center(mesh, &flat, transform));
    for (i, dc) in flat.iter().enumerate() {
        if !dc.is_triangle_list {
            continue;
        }
        let remapped;
        let dc_ref: &DrawCall = if let (Some(map), Some(orig)) = (skin_map, &dc.material_path) {
            if let Some(new_path) = map.get(orig) {
                let mut cloned = (*dc).clone();
                cloned.material_path = Some(new_path.clone());
                remapped = cloned;
                &remapped
            } else {
                dc
            }
        } else {
            dc
        };
        let Some(material_path) = &dc_ref.material_path else {
            continue;
        };
        let material_overlay = ctx
            .get_resolved_material(material_path)
            .map(|r| r.material_says_overlay)
            .unwrap_or(false);
        let overlay = object_overlay || material_overlay;
        let final_tint = combine_tint(tint_rgba, dc_ref);
        let Some((build, tris)) = get_or_build_mesh(
            ctx,
            mesh_key,
            mesh,
            i,
            dc_ref,
            overlay,
            lightmap_uv_scale,
            final_tint,
        ) else {
            continue;
        };
        triangles += tris;
        // `m_nOverlayRenderOrder` is a scene-object property: only write it when the *object*
        // itself is the overlay (not merely a regular placement whose material happens to set
        // `F_OVERLAY`, which still gets the vertex offset above but has no such order of its own).
        let final_overlay_order = if object_overlay { overlay_order } else { None };
        match build {
            MeshBuild::Shared { mesh_idx, quantize } => add_instance(
                ctx,
                children,
                name,
                mesh_idx,
                &gltf::compose(transform, &quantize),
                final_overlay_order,
                light_probe_handshake,
            ),
            MeshBuild::Probe {
                geometry,
                material_index,
            } => bake_and_place_probe_instance(
                ctx,
                children,
                name,
                &geometry,
                material_index,
                transform,
                light_probe_handshake,
                final_overlay_order,
                Some(placement_bounds_center),
            ),
        }
    }
    triangles
}

/// Accumulates one mesh's triangle-list draw calls' `POSITION` stream into a running world-space
/// AABB, placed by `transform` -- the shared core of [`placement_world_bounds_center`] (one mesh)
/// and [`model_world_bounds_center`] (every mesh of a model, §13).
fn accumulate_world_bounds(
    mesh: &Mesh,
    flat: &[&DrawCall],
    transform: &[[f32; 4]; 3],
    min: &mut [f32; 3],
    max: &mut [f32; 3],
    any: &mut bool,
) {
    for &dc in flat {
        if !dc.is_triangle_list {
            continue;
        }
        let Some((buf, field)) = find_field(mesh, dc, "POSITION", 0) else {
            continue;
        };
        let Ok(positions) = crate::attributes::decode_positions(buf, field) else {
            continue;
        };
        for &p in &positions {
            let w = probes::apply_affine(transform, p);
            for c in 0..3 {
                min[c] = min[c].min(w[c]);
                max[c] = max[c].max(w[c]);
            }
            *any = true;
        }
    }
}

fn bounds_center(min: [f32; 3], max: [f32; 3], any: bool) -> [f32; 3] {
    if !any {
        return [0.0, 0.0, 0.0];
    }
    [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ]
}

/// One world AABB centre over every triangle-list draw call's `POSITION` stream, placed by
/// `transform` (REPORT.md §2's binding rule: the whole *object*'s bounds, not one draw call's) --
/// decodes only `POSITION` (no normals/UV/index remap), so this is cheap to compute once per
/// placement even though [`build_geometry`] later re-decodes some of the same draw calls in full.
/// `[0,0,0]` if nothing decodes (matches the previous per-instance fallback default).
fn placement_world_bounds_center(
    mesh: &Mesh,
    flat: &[&DrawCall],
    transform: &[[f32; 4]; 3],
) -> [f32; 3] {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut any = false;
    accumulate_world_bounds(mesh, flat, transform, &mut min, &mut max, &mut any);
    bounds_center(min, max, any)
}

/// Like [`placement_world_bounds_center`], but unioned over every mesh of a model at its lowest
/// LOD (§13's fix): VRF binds one probe volume per model *node*, the union of all its meshes'
/// bounds (`ModelSceneNode.Bones.cs:76-83`, `Scene.cs:2212`), not a separate box per mesh -- a
/// per-mesh box could bind two meshes of the same placement to two different volumes right at a
/// volume boundary.
fn model_world_bounds_center(
    meshes: &[(String, Arc<Mesh>)],
    transform: &[[f32; 4]; 3],
) -> [f32; 3] {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for (_, mesh) in meshes {
        let flat = flatten_draw_calls(mesh);
        accumulate_world_bounds(mesh, &flat, transform, &mut min, &mut max, &mut any);
    }
    bounds_center(min, max, any)
}

/// Like [`place_model`], but resolves the per-draw-call overlay flag as `object_overlay ||
/// material.material_says_overlay` (§4).
#[allow(clippy::too_many_arguments)]
fn place_model_with_overlay(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    model_path: &str,
    model: &Model,
    name: Option<&str>,
    transform: &[[f32; 4]; 3],
    tint_rgba: [f32; 4],
    skin_map: Option<&HashMap<String, String>>,
    lightmap_uv_scale: [f32; 2],
    object_overlay: bool,
    overlay_order: Option<i64>,
    light_probe_handshake: i64,
) -> u64 {
    let mut triangles = 0u64;
    let meshes = model_meshes_at_lowest_lod(ctx, model_path, model);
    // §13's fix: one binding centre over every mesh of this model, not a fresh one per mesh.
    let bounds_center_override = model_world_bounds_center(&meshes, transform);
    for (mesh_key, mesh) in &meshes {
        triangles += place_mesh_draw_calls(
            ctx,
            children,
            mesh_key,
            mesh,
            name,
            transform,
            tint_rgba,
            skin_map,
            lightmap_uv_scale,
            object_overlay,
            overlay_order,
            light_probe_handshake,
            Some(bounds_center_override),
        );
    }
    triangles
}

fn place_aggregate(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    agg: &AggregateRaw,
    lightmap_uv_scale: [f32; 2],
) {
    let Some(renderable_model) = &agg.renderable_model else {
        return;
    };
    let path = compiled_path(renderable_model);
    let Some(model) = ctx.get_model(&path) else {
        return;
    };

    let has_fragment_data = agg.fragments.iter().any(|f| f.draw_call_index >= 0);
    if agg.fragments.is_empty() || !has_fragment_data {
        // No usable m_nDrawCallIndex data: fall back to a single identity-placed instance
        // (`AggregateCreateFragments` returning `false` -> `LoadModel` fallback).
        ctx.report.aggregates_fallback += 1;
        place_model_with_overlay(
            ctx,
            children,
            &path,
            &model,
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            lightmap_uv_scale,
            false,
            None,
            0,
        );
        return;
    }

    let mesh = if let Some(em) = model.embedded_meshes.first() {
        em.mesh.clone()
    } else if let Some(rm) = model.ref_meshes.first() {
        let ref_path = compiled_path(&rm.mesh_path);
        match ctx.sources.resource(&ref_path).and_then(|r| {
            crate::mesh::decode_mesh_resource(&r, &ref_path).map_err(|e| {
                crate::source::SourceError::Parse {
                    path: ref_path.clone(),
                    source: s2fmt::resource::ResourceError::Invalid {
                        detail: e.to_string(),
                    },
                }
            })
        }) {
            Ok(m) => m,
            Err(_) => {
                ctx.report
                    .missing_resources
                    .push(format!("{ref_path}: not found or failed to decode"));
                return;
            }
        }
    } else {
        ctx.report
            .missing_resources
            .push(format!("{path}: aggregate model has no mesh"));
        return;
    };

    let placements = match world::build_fragment_placements(agg) {
        Ok(p) => p,
        Err(e) => {
            ctx.report.missing_resources.push(format!("{path}: {e}"));
            return;
        }
    };
    let flat = flatten_draw_calls(&mesh);
    for placement in &placements {
        let Some(&dc) = flat.get(placement.draw_call_index) else {
            ctx.report.missing_resources.push(format!(
                "{path}: fragment references draw call {} out of {}",
                placement.draw_call_index,
                flat.len()
            ));
            continue;
        };
        if !dc.is_triangle_list {
            continue;
        }
        let Some(material_path) = &dc.material_path else {
            continue;
        };
        let material_overlay = ctx
            .get_resolved_material(material_path)
            .map(|r| r.material_says_overlay)
            .unwrap_or(false);
        let tint = [placement.tint[0], placement.tint[1], placement.tint[2], 1.0];
        let final_tint = combine_tint(tint, dc);
        let Some((build, _tris)) = get_or_build_mesh(
            ctx,
            &path,
            &mesh,
            placement.draw_call_index,
            dc,
            material_overlay,
            lightmap_uv_scale,
            final_tint,
        ) else {
            continue;
        };
        ctx.report.fragments_placed += 1;
        match build {
            MeshBuild::Shared { mesh_idx, quantize } => add_instance(
                ctx,
                children,
                None,
                mesh_idx,
                &gltf::compose(&placement.transform, &quantize),
                None,
                placement.light_probe_volume_handshake,
            ),
            MeshBuild::Probe {
                geometry,
                material_index,
            } => bake_and_place_probe_instance(
                ctx,
                children,
                None,
                &geometry,
                material_index,
                &placement.transform,
                placement.light_probe_volume_handshake,
                None,
                // No override: an aggregate fragment is already one draw call, i.e. already the
                // whole "placement" REPORT.md §2's binding rule cares about, so the function's own
                // per-instance AABB (this fragment's own geometry) *is* the placement AABB.
                None,
            ),
        }
    }
}

fn bump_class<'r>(report: &'r mut Report, classname: &str, has_model: bool) -> &'r mut ClassStats {
    let stats = report.class_stats.entry(classname.to_string()).or_default();
    stats.total += 1;
    if has_model {
        stats.with_model += 1;
    }
    stats
}

struct EntityLight {
    direction: [f32; 3],
    color: [f32; 3],
    brightness: f32,
    /// `brightnessscale` (default 1, §6): multiplies into `colorLinear` alongside `brightness`.
    brightness_scale: f32,
    skycolor: [f32; 3],
    skyintensity: f32,
}

fn entity_color01(e: &Entity, key: &str) -> Option<[f32; 3]> {
    let v = e.get_vec3(key)?;
    Some([v[0] / 255.0, v[1] / 255.0, v[2] / 255.0])
}

fn place_entities(
    ctx: &mut Ctx,
    lump: &EntityLump,
    children: &mut Vec<u32>,
    sun: &mut Option<EntityLight>,
    lightmap_uv_scale: [f32; 2],
) {
    for entity in &lump.entities {
        let classname = entity.classname();
        if classname.is_empty() {
            continue;
        }
        let model = entity.get_str("model").unwrap_or("");
        let has_model = !model.is_empty();

        if classname == "light_environment" && sun.is_none() {
            let angles = entity.angles();
            // `forward = R * (1,0,0)^T`, i.e. the first *column* (`s2fmt::entities::
            // angles_to_matrix`'s own doc comment) -- verified against `REPORT.md`'s own
            // Mirage number: angles [60,318,0] -> (0.3716,-0.3346,-0.8660).
            let m = entities::angles_to_matrix(angles);
            let direction = [m[0][0], m[1][0], m[2][0]];
            *sun = Some(EntityLight {
                direction,
                color: entity_color01(entity, "color").unwrap_or([1.0, 1.0, 1.0]),
                brightness: entity
                    .get("brightness")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        s2fmt::entities::EntityValue::Int(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
                brightness_scale: entity
                    .get("brightnessscale")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        s2fmt::entities::EntityValue::Int(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
                skycolor: entity_color01(entity, "skycolor").unwrap_or([1.0, 1.0, 1.0]),
                skyintensity: entity
                    .get("skyintensity")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
            });
        }

        let stats = bump_class(&mut ctx.report, classname, has_model);
        if !has_model {
            continue;
        }
        if classname == "csgo_player_previewmodel" {
            *stats
                .reasons
                .entry("preview model".to_string())
                .or_insert(0) += 1;
            ctx.report.hidden_entities.push(json!({
                "classname": classname,
                "targetname": entity.targetname(),
                "model": model,
                "reason": "preview model",
            }));
            continue;
        }
        if !ent::should_render(entity) {
            *stats
                .reasons
                .entry("hidden (startdisabled/enabled/renderamt/rendermode)".to_string())
                .or_insert(0) += 1;
            ctx.report.hidden_entities.push(json!({
                "classname": classname,
                "targetname": entity.targetname(),
                "model": model,
                "reason": "hidden (startdisabled/enabled/renderamt/rendermode)",
            }));
            continue;
        }

        let path = compiled_path(model);
        let Some(loaded_model) = ctx.get_model(&path) else {
            let stats = ctx
                .report
                .class_stats
                .entry(classname.to_string())
                .or_default();
            *stats
                .reasons
                .entry("model not found".to_string())
                .or_insert(0) += 1;
            continue;
        };

        let transform = entity_transform(entity.origin(), entity.angles(), entity.scales());
        let tint = ent::entity_tint(entity);
        let skin = ent::skin_name(entity);
        let skin_map = skin.and_then(|s| ent::skin_remap(&loaded_model, s));
        let name = entity.targetname().or(Some(classname));

        let triangles = place_model_with_overlay(
            ctx,
            children,
            &path,
            &loaded_model,
            name,
            &transform,
            tint,
            skin_map.as_ref(),
            lightmap_uv_scale,
            false,
            None,
            0, // entities carry no precomputed handshake (Scene.cs:2150-2154); AABB-centre fallback binds them.
        );

        let stats = ctx
            .report
            .class_stats
            .entry(classname.to_string())
            .or_default();
        if triangles > 0 {
            stats.drawn += 1;
            stats.triangles += triangles;
        } else {
            *stats
                .reasons
                .entry("no render mesh (physics-only model)".to_string())
                .or_insert(0) += 1;
        }
    }
}

/// `m_entityLumps`/`m_childLumps` entries are `.vents` resource paths (`world.rs`'s
/// `world_node_path` applies the same lower-case/forward-slash transform for `.vwnod`); this is
/// that transform, `_c`-suffixed, shared by the listed lumps and the child-lump walk below.
fn entity_lump_path(name: &str) -> String {
    format!("{}_c", name.replace('\\', "/").to_ascii_lowercase())
}

/// Loads one entity lump file, `Err` describing exactly what went wrong (§2's fix: this used to
/// collapse "not found" and "found but undecodable" into a silent `None`, dropping both from
/// `dropped.missingResources`).
fn load_entity_lump(sources: &Sources, path: &str) -> Result<EntityLump, String> {
    let resource = sources
        .resource(path)
        .map_err(|_| format!("{path}: not found"))?;
    let doc = resource
        .data_kv3()
        .map_err(|e| format!("{path}: failed to decode: {e}"))?;
    entities::decode_entity_lump(&doc.root).map_err(|e| format!("{path}: failed to decode: {e}"))
}

/// Resolves `listed` (`m_entityLumps`) plus their `m_childLumps`, falling back to a directory scan
/// if the listed lumps can't all be loaded (§3's existing fallback). Every lump-file failure along
/// the way -- including the one that triggers the fallback scan -- is returned alongside the lumps
/// that did load, for the caller to fold into `dropped.missingResources` (§2).
fn resolve_entity_lumps(sources: &Sources, listed: &[String]) -> (Vec<EntityLump>, Vec<String>) {
    let mut out = Vec::new();
    let mut failures = Vec::new();
    let mut ok = !listed.is_empty();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in listed {
        let path = entity_lump_path(name);
        visited.insert(path.clone());
        match load_entity_lump(sources, &path) {
            Ok(lump) => out.push(lump),
            Err(e) => {
                failures.push(e);
                ok = false;
                break;
            }
        }
    }
    if ok && !out.is_empty() {
        // `m_childLumps` (§3 "включая дочерние"): walked by a growing index so a child lump's
        // own children are followed too (de_vertigo's own children have none, but nothing here
        // assumes that), in deterministic encounter order; `visited` (keyed the same way as the
        // path lookup) skips a name already loaded instead of looping on a cycle.
        let mut i = 0;
        while i < out.len() {
            let children = out[i].child_lumps.clone();
            for child in &children {
                let path = entity_lump_path(child);
                if !visited.insert(path.clone()) {
                    continue;
                }
                match load_entity_lump(sources, &path) {
                    Ok(lump) => out.push(lump),
                    Err(e) => failures.push(e),
                }
            }
            i += 1;
        }
        return (out, failures);
    }
    out.clear();
    for path in sources.entries_with_extension("vents_c") {
        match load_entity_lump(sources, &path) {
            Ok(lump) => out.push(lump),
            Err(e) => failures.push(e),
        }
    }
    (out, failures)
}

/// The sun's baked shadow channel (§1's fix): the first `light_environment` entity's
/// `bakedshadowindex`, falling back to `bakelightindex` on that *same* entity -- not a second
/// `light_environment` -- valid only for `0..=3` (`direct_light_shadows`/the probe dlshd atlas are
/// at most RGBA). `None` (no field, or an out-of-range value) means no baked sun shadow exists on
/// this map, so probe/lightmap sun visibility is always `1.0`, never a hard-coded channel `0`.
fn find_baked_shadow_channel(lumps: &[EntityLump]) -> Option<i64> {
    let env = lumps
        .iter()
        .flat_map(|l| l.entities.iter())
        .find(|e| e.classname().eq_ignore_ascii_case("light_environment"))?;
    let idx = ent::entity_num(env.get("bakedshadowindex"))
        .or_else(|| ent::entity_num(env.get("bakelightindex")))?;
    let ch = idx.round() as i64;
    (0..=3).contains(&ch).then_some(ch)
}

/// Exports `map` (already resolved to `sources`) to a `.glb` byte buffer and a `render.json`
/// report value.
pub fn export_map(sources: &Sources, options: &ExportOptions) -> Result<ExportResult, ExportError> {
    let (_world_path, world_resource) = sources.resource_ending_with("world.vwrld_c")?;
    let world_doc = world::decode_world(
        &world_resource
            .data_kv3()
            .map_err(crate::source::SourceError::from_resource_err)?
            .root,
    );

    // Entity lumps are loaded up front (rather than after the world-node walk, as F3a-3 had it):
    // §2's probe-volume binding needs the volumes (`env_combined_light_probe_volume` entities)
    // and their shared atlas *before* any world scene object or aggregate fragment is placed.
    let (lumps, lump_failures) = resolve_entity_lumps(sources, &world_doc.entity_lumps);
    if lumps.is_empty() {
        return Err(ExportError::NoEntityLumps);
    }
    let baked_shadow_channel = find_baked_shadow_channel(&lumps).map(|c| c as usize);
    let (probe_volumes, probe_volumes_skipped) = probes::collect_volumes(&lumps);
    let mut pre_missing: Vec<String> = lump_failures;
    let probe_atlas = match probes::atlas_texture_paths(&lumps) {
        Some((irr_path, dlshd_path)) => {
            match ProbeAtlasTextures::load(sources, &irr_path, &dlshd_path) {
                Ok(atlas) => Some(atlas),
                Err(e) => {
                    pre_missing.push(e);
                    None
                }
            }
        }
        None if !probe_volumes.is_empty() => {
            pre_missing.push(
                "probe volumes present but no lightprobetexture/lightprobetexture_dlshd found"
                    .to_string(),
            );
            None
        }
        None => None,
    };

    let mut ctx = Ctx {
        sources,
        max_texture: options.max_texture,
        builder: GltfBuilder::new(),
        models: HashMap::new(),
        raw_materials: HashMap::new(),
        resolved_materials: HashMap::new(),
        gltf_materials: HashMap::new(),
        tex_catalog: TextureCatalog::default(),
        meshes: HashMap::new(),
        probe_geometries: HashMap::new(),
        probe_volumes,
        probe_atlas,
        baked_shadow_channel,
        report: Report::default(),
    };
    ctx.report.missing_resources.extend(pre_missing);
    ctx.report.probe_volumes_skipped = probe_volumes_skipped;

    let mut world_children: Vec<u32> = Vec::new();
    for prefix in &world_doc.world_node_prefixes {
        let node_path = world::world_node_path(prefix);
        let node_resource = match sources.resource(&node_path) {
            Ok(r) => r,
            Err(_) => {
                ctx.report
                    .missing_resources
                    .push(format!("{node_path}: not found"));
                continue;
            }
        };
        let root = match node_resource.data_kv3() {
            Ok(d) => d.root,
            Err(e) => {
                ctx.report
                    .missing_resources
                    .push(format!("{node_path}: {e}"));
                continue;
            }
        };
        let node: WorldNodeRaw = match world::decode_world_node(&root) {
            Ok(n) => n,
            Err(_) => continue,
        };
        for so in &node.scene_objects {
            ctx.report.scene_objects_total += 1;
            let before = ctx.report.triangles;
            place_scene_object(
                &mut ctx,
                &mut world_children,
                so,
                world_doc.lightmap_uv_scale,
            );
            if ctx.report.triangles > before {
                ctx.report.scene_objects_placed += 1;
            }
        }
        for agg in &node.aggregates {
            ctx.report.aggregates_total += 1;
            ctx.report.fragments_total += agg.fragments.len() as u32;
            place_aggregate(
                &mut ctx,
                &mut world_children,
                agg,
                world_doc.lightmap_uv_scale,
            );
        }
    }

    let mut entity_children: Vec<u32> = Vec::new();
    let mut sun: Option<EntityLight> = None;
    for lump in &lumps {
        place_entities(
            &mut ctx,
            lump,
            &mut entity_children,
            &mut sun,
            world_doc.lightmap_uv_scale,
        );
    }

    let world_group = ctx
        .builder
        .add_node(json!({ "name": "world", "children": world_children }));
    let entities_group = ctx
        .builder
        .add_node(json!({ "name": "entities", "children": entity_children }));

    let extensions_used = if ctx.report.unlit_used {
        vec!["KHR_materials_unlit".to_string()]
    } else {
        Vec::new()
    };

    let node_count = ctx.builder.node_count();
    let mesh_count = ctx.builder.mesh_count();
    let material_count = ctx.builder.material_count();
    // §7: material textures no longer live in the glb at all (`ctx.builder`'s own image/texture
    // arrays are always empty now) -- these come from the native catalog's `render_tex/*.bin`
    // files instead.
    let texture_count = ctx.tex_catalog.count();
    let geometry_bytes = ctx.builder.geometry_bytes();
    let texture_bytes = ctx.tex_catalog.total_bytes();
    let position_float_fallback_meshes = ctx.builder.position_float_fallback_meshes();

    let sun_json = sun.as_ref().map(|s| {
        let to_sun = [-s.direction[0], -s.direction[1], -s.direction[2]];
        let linear = material::srgb_to_linear(s.color);
        let color_linear = [
            linear[0] * s.brightness * s.brightness_scale,
            linear[1] * s.brightness * s.brightness_scale,
            linear[2] * s.brightness * s.brightness_scale,
        ];
        json!({
            "direction": s.direction,
            "directionMeaning": "unit vector the sunlight travels along (from the sun toward the scene), game axes, Z up",
            "toSun": to_sun,
            "color": s.color,
            "colorSpace": "srgb",
            "colorLinear": color_linear,
            "brightness": s.brightness,
            "brightnessScale": s.brightness_scale,
            "skycolor": s.skycolor,
            "skyintensity": s.skyintensity,
            // §3: unlike the 72 local lights (light_barn/light_omni2/light_rect), which are fully
            // baked into the lightmap/probes and never evaluated at runtime (REPORT.md §1), the
            // sun's specular term is always computed at runtime (pbr.slang:159-213,238-251) --
            // constant `true`, not read from any entity field.
            "renderSpecular": true,
            "renderSpecularMeaning": "the sun always contributes a runtime specular term (pbr.slang formula in REPORT.md §1), unlike the fully-baked local lights",
            "bakedShadowChannel": ctx.baked_shadow_channel,
            "bakedShadowChannelMeaning": "index (0..3) into direct_light_shadows (and the probe dlshd atlas) that carries this sun's baked visibility, 1 - channel[bakedShadowChannel] = vis; null means this map has no baked sun shadow at all (vis = 1), from the first light_environment's bakedshadowindex (fallback bakelightindex), not m_bakedShadows[0] (that array indexes baked local lights, not the sun)",
        })
    });

    // §1: raw lightmap blocks + their always-generated fallback PNGs.
    let mut extra_files: Vec<(String, Vec<u8>)> = Vec::new();
    let (lightmap_files, lightmap_missing) = lightmaps::export_files(
        sources,
        &world_doc.light_maps,
        options.lightmap_quality_high,
    );
    let (lightmap_fallbacks, lightmap_fallback_missing) =
        lightmaps::export_fallback_files(sources, &world_doc.light_maps);
    ctx.report.missing_resources.extend(lightmap_missing);
    ctx.report
        .missing_resources
        .extend(lightmap_fallback_missing);
    let lightmaps_json: Vec<serde_json::Value> = lightmap_files
        .iter()
        .map(|f| {
            json!({
                "file": f.file_name,
                "format": f.format,
                "width": f.width,
                "height": f.height,
                "mipLevel": f.mip_level,
                "sha256": f.sha256,
                "byteLength": f.byte_length,
            })
        })
        .collect();
    let lightmap_fallbacks_json: Vec<serde_json::Value> = lightmap_fallbacks
        .iter()
        .map(|f| {
            json!({
                "file": f.file_name,
                "width": f.width,
                "height": f.height,
                "byteLength": f.byte_length,
                "encoding": f.encoding,
                "rgbmRange": f.rgbm_range,
                "decode": f.decode,
            })
        })
        .collect();
    for f in lightmap_files {
        extra_files.push((f.file_name, f.bytes));
    }
    for f in lightmap_fallbacks {
        extra_files.push((f.file_name, f.bytes));
    }

    // §4-§6: sky cube, cube fog, post-processing (tonemap/exposure/bloom/LUT).
    let env = environment::build(sources, &lumps);
    ctx.report.missing_resources.extend(env.missing);
    if let Some(bytes) = env.sky_cube_bytes {
        extra_files.push(("render_sky_cube.bin".to_string(), bytes));
    }
    extra_files.extend(env.sky_fallback_files);
    if let Some(bytes) = env.fog_cube_bytes {
        extra_files.push(("render_fog_cube.bin".to_string(), bytes));
    }
    if let Some(bytes) = env.lut_bytes {
        extra_files.push(("render_lut.bin".to_string(), bytes));
    }

    let mut per_material: Vec<(String, u64)> =
        ctx.report.per_material_triangles.into_iter().collect();
    // Triangle count descending, material path ascending as a tiebreaker: `per_material_triangles`
    // is a `HashMap`, so ties must not fall back to its (per-process-random) iteration order --
    // `export-glb`'s determinism check (§ проверки item 6) needs the same `render.json` bytes on
    // every run.
    per_material.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut tools_dropped: Vec<serde_json::Value> = ctx
        .report
        .tools_dropped
        .iter()
        .map(|(path, d)| json!({ "material": path, "drawCalls": d.draw_calls, "triangles": d.triangles }))
        .collect();
    tools_dropped.sort_by(|a, b| a["material"].as_str().cmp(&b["material"].as_str()));

    let mut entity_table: Vec<serde_json::Value> = ctx
        .report
        .class_stats
        .iter()
        .map(|(classname, s)| {
            json!({
                "classname": classname,
                "total": s.total,
                "withModel": s.with_model,
                "drawn": s.drawn,
                "triangles": s.triangles,
                "reasons": s.reasons,
            })
        })
        .collect();
    entity_table.sort_by(|a, b| a["classname"].as_str().cmp(&b["classname"].as_str()));

    let report = json!({
        "formatVersion": 3,
        "textureBudget": {
            "maxSide": options.max_texture,
            "byType": {
                "color": { "maxSide": TextureRole::Color.max_side(options.max_texture), "note": "base color, layer-2/env-layer-2 color, self-illum" },
                "normal": { "maxSide": TextureRole::Normal.max_side(options.max_texture), "note": "normal map (HemiOct RG + roughness in B), layer-2/env-layer-2 normal" },
                "mask": { "maxSide": TextureRole::Mask.max_side(options.max_texture), "note": "AO, metalness, blend modulation, tint mask, csgo_environment(_blend) g_tHeight1/2" },
            },
            "dedup": {
                "meaning": "a texture whose raw multi-level blob hashes the same as an earlier one (different vtex path, or the same texture reused in a different role at the same base mip level) is written to render_tex/ once and reused (s6f3a6_native_tex.md change item 1's 'по пути+L, затем по SHA-256 содержимого')",
                "hits": ctx.tex_catalog.dedup_hits(),
                "bytesSaved": ctx.tex_catalog.dedup_bytes_saved(),
            },
        },
        "textures": ctx.tex_catalog.textures_json(),
        "texturesMeaning": "raw, still block-compressed (or, for the one RGBA8888 texture on de_inferno, uncompressed) mip levels straight out of the game's own vtex_c, largest mip first, in render_tex/<sha12>.bin (s6f3a6_native_tex.md); format is BC7/BC1(=DXT1)/BC4(=ATI1N)/RGBA8, colorSpace is srgb/linear as decided by the shader PARAMETER this texture was read through (not by the texture itself -- the same vtex_c could in principle be srgb in one slot and linear in another, though no texture on either surveyed map actually is), codec is the post-sample decode the viewer's shader must run (hemiOct/dxt5nm/ycocg/reconstructZ/none); each level's {width,height,offset,length} indexes straight into the file's bytes. Materials reference these by index from extras (baseColorTexture, normalTexture, aoTexture, ...) rather than glTF's own textures[]/images[], which this exporter no longer populates at all (change item 7) -- a 4x4 single-mip source is instead folded into the material as a Constant (see extras' own *Constant/*ConstantCodec keys)",
        "geometryCompression": {
            "extensionsRequired": ["KHR_mesh_quantization", "KHR_meshopt_compression"],
            "position": "SHORT (unnormalized) VEC3 -- a single power-of-two step (1/16 unit) shared by every mesh, with each mesh's own offset snapped to a multiple of that step and baked into its instance node's own matrix (composed with its placement transform); a vertex shared by two meshes therefore decodes to the same world position from both. Not normalized: KHR_mesh_quantization's own implementation note prefers unnormalized SHORT for POSITION. A mesh whose coordinates would overflow the SHORT range falls back to plain FLOAT VEC3 (positionFloatFallbackMeshes below counts these)",
            "positionFloatFallbackMeshes": position_float_fallback_meshes,
            "normal": "BYTE normalized VEC3, plain per-component quantization (not octahedral)",
            "texcoord0": "UNSIGNED_SHORT normalized VEC2 when every value is within [0,1], else FLOAT (tiled UVs routinely exceed that range)",
            "meshoptVersion": 1,
            "meshoptLevel": 3,
            "note": "no uncompressed/unquantized fallback is ever written (this viewer is the only reader); it must call setMeshoptDecoder before parsing this file",
        },
        "sun": sun_json,
        "counts": {
            "nodes": node_count,
            "meshes": mesh_count,
            "triangles": ctx.report.triangles,
            "materials": material_count,
            "textures": texture_count,
            "geometryBytes": geometry_bytes,
            "textureBytes": texture_bytes,
            "sceneObjectsTotal": ctx.report.scene_objects_total,
            "sceneObjectsPlaced": ctx.report.scene_objects_placed,
            "aggregatesTotal": ctx.report.aggregates_total,
            "aggregatesFallback": ctx.report.aggregates_fallback,
            "fragmentsTotal": ctx.report.fragments_total,
            "fragmentsPlaced": ctx.report.fragments_placed,
            "lightmapDrawCalls": ctx.report.lightmap_draw_calls,
            "lightmapTriangles": ctx.report.lightmap_triangles,
            "probeDrawCalls": ctx.report.probe_draw_calls,
            "probeTriangles": ctx.report.probe_triangles,
            "unlitDrawCalls": ctx.report.unlit_draw_calls,
            "unlitTriangles": ctx.report.unlit_triangles,
            "probeSampleFailures": ctx.report.probe_sample_failures,
            "probeVolumesLoaded": ctx.probe_volumes.len(),
            "probeVolumesSkipped": ctx.report.probe_volumes_skipped,
        },
        "perMaterialTriangles": per_material.into_iter().map(|(k,v)| json!({"material": k, "triangles": v})).collect::<Vec<_>>(),
        "dropped": {
            "toolsMaterials": tools_dropped,
            "missingResources": ctx.report.missing_resources,
            "hiddenEntities": ctx.report.hidden_entities,
        },
        "entities": entity_table,
        "materialExtras": {
            "tintColorSpace": "linear (extras.tint is sRGB->linear converted, same as baseColorFactor)",
            "textureRefMeaning": "every *Texture key (baseColorTexture, normalTexture, aoTexture, metalnessTexture, layers.layer2ColorTexture, ...) is an index into this file's top-level textures[], never a glTF texture/image index (this exporter writes neither); a 4x4 single-mip source instead appears as the sibling *Constant key (raw pre-codec RGBA8 bytes) plus *ConstantCodec (and, for a color-space-sensitive slot, *ConstantColorSpace) -- see textures[]/texturesMeaning and native_texture::Loaded's doc comment",
            "envMaterialsMeaning": "csgo_environment/csgo_environment_blend materials (s6f3a7_env_materials.md): extras.env1 is layer 1's own height/roughness/colour inputs, present whenever g_tHeight1 resolves; extras.envLayer2 additionally carries layer 2's (only when csgo_environment_blend AND colour/normal/height all resolve). height{1,2}Texture/height{1,2}Constant: R = height (the blend-weight input to GetBlendWeights, csgo_environment.frag.slang:348-377), G = tintMask{1,2}'s source (remapped by tintMaskContrast{1,2}/tintMaskBrightness{1,2} into 0..1, gates how much of colorAdjust{1,2} shows through, csgo_environment.frag.slang:767,781-787), B = AO for alpha-tested materials only (not read by this viewer), A = metalness (zeroed unless metalnessEnabled{1,2}). roughnessContrast{1,2}/roughnessBrightness{1,2}: the same remap applied to the normal map's own B channel (roughness), csgo_environment.frag.slang:769. normalContrast{1,2}: normalize(mix(Up, decodedNormal, contrast)) after the HemiOct decode (csgo_environment.frag.slang:486-505 LayerNormal). aoLevels{1,2} = (x,y,z): the ambient-occlusion curve mix(x,z,pow(ao,max(y,0.001))) applied to the base-colour alpha (csgo_environment.frag.slang:1046), lerped between layers by the blend weight for envLayer2. colorAdjust{1,2}/adjust{1,2}: 16-float column-major mat4 (crates/s2render/src/color_correct.rs, RenderMaterial.cs:671-744) -- colorAdjust is g_mTextureColorAdjust{1,2} (tinted), adjust is g_mTextureAdjust{1,2} (tint forced white), mixed by tintMask{1,2} and, when colorCorrectionMode{1,2}==1, adjust replaces the raw texel as the base before that mix. envLayer2 additionally carries heightScale{1,2}/heightZeroPoint{1,2}/blendSoftness2 (GetBlendWeights' own inputs) and uvScale2/uvOffset2 (layer 2's UV transform, csgo_environment.vert.slang:175-180, applied to every layer-2 sample). envLayer2.unsupported (present only when non-empty): [{flag, why}] for a material that sets F_USE_NEW_BLENDING/F_ENABLE_LAYER_3/a biplanar g_nUVSet -- none of which this exporter/viewer implements, so weight2 (and everything mixed by it) is wrong for that material.",
            "byMaterial": ctx.report.material_extras.iter().map(|(k,v)| (k.to_string(), v.clone())).collect::<serde_json::Map<_,_>>(),
        },
        "lighting": {
            "lightmapFormatVersion": world_doc.lightmap_version,
            "uvScale": world_doc.lightmap_uv_scale,
            "uvScaleMeaning": "TEXCOORD_1 is already multiplied by this scale at export time (complex.vert.slang:347); the viewer must not multiply again",
            "lightmaps": lightmaps_json,
            "lightmapFallbacks": lightmap_fallbacks_json,
            "lightmapFallbacksMeaning": "always generated (small); use when the browser lacks EXT_texture_compression_bptc/rgtc for the raw BC6H/BC4/BC5/BC7 files above, or when a lightmap file's compressed format has no known WebGL2 mapping at all (that raw file is then skipped and reported in dropped.missingResources instead)",
            "lpvScale": LPV_SCALE,
            "lpvScaleMeaning": "_LPV is UNSIGNED_SHORT normalized (VEC4); actual value = accessor value * lpvScale for rgb (irradiance) and a (sun visibility, already 0..1 so effectively unscaled since lpvScale>=1)",
            "extrasLightingMeaning": "primitive extras.lighting is one of \"lightmap\" (reads TEXCOORD_1 against the lightmap files above), \"unlit\" (no indirect term), \"probe\" (reads the primitive's own _LPV accessor)",
            "overlayOrderMeaning": "node extras.overlayOrder absent means 0 (no explicit paint order among overlapping overlays)",
            "lightProbeHandshakeMeaning": "node extras.lightProbeHandshake (when present) is m_nLightProbeVolumePrecomputedHandshake, the scene object/fragment's precomputed probe-volume binding",
        },
        "sky": env.sky_json,
        "fog": env.fog_json,
        "postProcessing": env.post_json,
    });

    let glb = ctx.builder.finish(
        vec![world_group, entities_group],
        extensions_used,
        json!({}),
    )?;
    // Every material texture's render_tex/<sha12>.bin (§1/§7) -- `report` above already read
    // everything it needs from `ctx.tex_catalog` by reference, so consuming it here is safe.
    extra_files.extend(ctx.tex_catalog.into_files());

    Ok(ExportResult {
        glb,
        report,
        extra_files,
    })
}

/// A cheap, coarse checkpoint [`export_and_write`] reports through its `on_stage` callback -
/// `export_map` has no finer internal phases short of a large rework of this file
/// (`s6i_render_job_areas3d.md`: "иначе — стадии и честное «идёт экспорт»"), so callers get an
/// honest two-stage split instead: building the export in memory, then writing it to disk (the one
/// part that genuinely has per-file progress).
#[derive(Debug, Clone, Copy)]
pub enum ExportStage {
    Exporting,
    Writing { done: usize, total: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum ExportWriteError {
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error("failed to write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

/// `path`'s parent directory (created first) + a temp file + rename - the same atomic-write dance
/// every other cache writer in this workspace uses (`extract::cache::save_extraction`,
/// `extract::mapdata::save_stand_spots`), factored out of `cmd_render.rs`'s own former copy.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ExportWriteError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ExportWriteError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp_path = PathBuf::from(tmp_name);
    if let Err(source) = fs::write(&tmp_path, bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ExportWriteError::Io {
            path: tmp_path,
            source,
        });
    }
    if let Err(source) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ExportWriteError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// [`export_map`] plus atomically writing every file it returns into `dir`: first
/// `result.extra_files`, then `render.glb`, and `render.json` last, since its presence is what
/// marks the export usable - the sequence `cs2mod export-glb` ran by hand before this
/// existed; factored out so the server's render job (`jobs.rs::run_render`) doesn't duplicate it
/// (`s6i_render_job_areas3d.md` change item 1: "вынеси общую функцию... в s2render"). `cancel` is
/// checked before the export starts and between each file write - `export_map` itself has no
/// cancellation hook (threading one through every nested placement/material call would be a much
/// larger change than this job warrants), so a cancel arriving mid-export still finishes that
/// (possibly multi-minute) call before taking effect, exactly like `jobs.rs::run_extract`'s own
/// coarse cancellation.
pub fn export_and_write(
    sources: &Sources,
    options: &ExportOptions,
    dir: &Path,
    cancel: &AtomicBool,
    mut on_stage: impl FnMut(ExportStage),
) -> Result<ExportResult, ExportWriteError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ExportWriteError::Cancelled);
    }
    on_stage(ExportStage::Exporting);
    let result = export_map(sources, options)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(ExportWriteError::Cancelled);
    }

    let total = 2 + result.extra_files.len();
    let mut done = 0;
    on_stage(ExportStage::Writing { done, total });
    // render.json is the "export complete" marker (registry has_usable_render, api.js
    // hasUsableRender): write it only after every file it references, so a cancel or an I/O error
    // part-way leaves an export the next run rebuilds, never a "usable" one missing its textures.
    for (name, bytes) in &result.extra_files {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportWriteError::Cancelled);
        }
        write_atomic(&dir.join(name), bytes)?;
        done += 1;
        on_stage(ExportStage::Writing { done, total });
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(ExportWriteError::Cancelled);
    }
    write_atomic(&dir.join("render.glb"), &result.glb)?;
    done += 1;
    on_stage(ExportStage::Writing { done, total });

    let json_text =
        serde_json::to_string_pretty(&result.report).map_err(|e| ExportWriteError::Io {
            path: dir.join("render.json"),
            source: std::io::Error::other(e),
        })?;
    write_atomic(&dir.join("render.json"), json_text.as_bytes())?;
    done += 1;
    on_stage(ExportStage::Writing { done, total });

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Compression, InputLayoutField};
    use crate::format::DxgiFormat;
    use crate::mesh::{DrawCallFlags, SceneObject};

    struct TempDir(std::path::PathBuf);
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

    /// A resource container with no blocks (`docs/FORMATS.md` §2.1-2.2): `Resource::parse`
    /// succeeds but `data_kv3` (and so `decode_mesh_resource`) fails -- exercises the
    /// `m_renderable` path's mesh-decode-failure reporting below (§11).
    fn empty_resource_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 16];
        b[0..4].copy_from_slice(&16u32.to_le_bytes()); // file size
        b[4..6].copy_from_slice(&12u16.to_le_bytes()); // header version
        b[8..12].copy_from_slice(&8u32.to_le_bytes()); // block_offset -> zero blocks follow
        b
    }

    /// A trivial version-1 VPK tree (`docs/FORMATS.md` §1.2) with at most one inline-stored
    /// entry -- just enough for `Sources::open`/`Sources::resource` below; no numbered archives,
    /// preload, or CRC checking needed (`Vpk::read` never verifies the CRC).
    fn write_test_vpk(path: &std::path::Path, entry: Option<(&str, &[u8])>) {
        let mut tree = Vec::new();
        if let Some((entry_path, data)) = entry {
            let (dir, file) = entry_path.rsplit_once('/').unwrap_or((" ", entry_path));
            let (name, ext) = file.rsplit_once('.').unwrap_or((file, " "));
            tree.extend_from_slice(ext.as_bytes());
            tree.push(0);
            tree.extend_from_slice(dir.as_bytes());
            tree.push(0);
            tree.extend_from_slice(name.as_bytes());
            tree.push(0);
            tree.extend_from_slice(&0u32.to_le_bytes()); // crc32 (unchecked by Vpk::read)
            tree.extend_from_slice(&0u16.to_le_bytes()); // preload_len
            tree.extend_from_slice(&0x7FFFu16.to_le_bytes()); // archive index: stored inline
            tree.extend_from_slice(&0u32.to_le_bytes()); // offset into dir data
            tree.extend_from_slice(&(data.len() as u32).to_le_bytes());
            tree.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entry terminator
            tree.push(0); // end of file-name loop
            tree.push(0); // end of dir loop
        }
        tree.push(0); // end of extension loop

        let mut out = Vec::new();
        out.extend_from_slice(&0x55AA_1234u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
        out.extend_from_slice(&tree);
        if let Some((_, data)) = entry {
            out.extend_from_slice(data);
        }
        std::fs::write(path, &out).expect("write synthetic vpk");
    }

    fn open_empty_sources(dir: &std::path::Path, entry: Option<(&str, &[u8])>) -> Sources {
        write_test_vpk(&dir.join("map.vpk"), entry);
        write_test_vpk(&dir.join("pak01_dir.vpk"), None);
        Sources::open(&dir.join("map.vpk"), dir).expect("open synthetic sources")
    }

    fn empty_ctx(sources: &Sources) -> Ctx<'_> {
        Ctx {
            sources,
            max_texture: ExportOptions::default().max_texture,
            builder: GltfBuilder::new(),
            models: HashMap::new(),
            raw_materials: HashMap::new(),
            resolved_materials: HashMap::new(),
            gltf_materials: HashMap::new(),
            tex_catalog: TextureCatalog::default(),
            meshes: HashMap::new(),
            probe_geometries: HashMap::new(),
            probe_volumes: Vec::new(),
            probe_atlas: None,
            baked_shadow_channel: None,
            report: Report::default(),
        }
    }

    fn base_material(shader: &str) -> RawMaterial {
        RawMaterial {
            shader: shader.to_string(),
            ..Default::default()
        }
    }

    /// Pre-populates both material caches so `get_raw_material`/`get_resolved_material` never
    /// touch `ctx.sources`, keeping these tests fully offline past `Sources::open` itself.
    fn cache_material(ctx: &mut Ctx, path: &str, mat: RawMaterial) {
        let resolved = material::resolve(&mat);
        ctx.raw_materials
            .insert(path.to_string(), Some(Arc::new(mat)));
        ctx.resolved_materials
            .insert(path.to_string(), Arc::new(resolved));
    }

    /// A one-triangle mesh with a single draw call referencing `material_path` -- just a
    /// `POSITION` stream, matching what `build_geometry` needs when the material carries no
    /// normal/UV/blend requirement (true of every material `base_material` builds).
    fn triangle_mesh(material_path: &str) -> Mesh {
        let positions: Vec<u8> = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .iter()
            .flat_map(|p| p.iter().flat_map(|f| f.to_le_bytes()))
            .collect();
        let vertex_buffer = Buffer {
            element_count: 3,
            element_size: 12,
            fields: vec![InputLayoutField {
                semantic_name: "POSITION".to_string(),
                semantic_index: 0,
                format: DxgiFormat::R32G32B32Float,
                offset: 0,
            }],
            data: positions,
            compression: Compression::default(),
        };
        let index_buffer = Buffer {
            element_count: 3,
            element_size: 2,
            fields: vec![],
            data: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            compression: Compression::default(),
        };
        Mesh {
            vertex_buffers: vec![vertex_buffer],
            index_buffers: vec![index_buffer],
            scene_objects: vec![SceneObject {
                draw_calls: vec![DrawCall {
                    material_path: Some(material_path.to_string()),
                    is_triangle_list: true,
                    base_vertex: 0,
                    start_index: 0,
                    index_count: 3,
                    vertex_count: 3,
                    index_buffer: 0,
                    vertex_buffers: vec![0],
                    tint_color: None,
                    alpha: None,
                    flags: DrawCallFlags::None,
                    has_baked_lighting_from_lightmap: false,
                }],
            }],
        }
    }

    fn glb_json(bytes: &[u8]) -> serde_json::Value {
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[20..20 + json_len]).unwrap()
    }

    /// §2/§11's synthetic coverage for `m_renderable`: no real map exercises this scene-object
    /// path (`world.rs`'s own doc comment on `SceneObjectRaw::renderable`), so this is the only
    /// test that a resource found but failing to decode as a mesh is reported, not swallowed.
    #[test]
    fn renderable_scene_object_reports_mesh_decode_failures() {
        let dir = temp_dir("renderable-decode-fail");
        let sources = open_empty_sources(
            &dir,
            Some(("models/broken.vmesh_c", &empty_resource_bytes())),
        );
        let mut ctx = empty_ctx(&sources);
        let mut children = Vec::new();
        let so = SceneObjectRaw {
            renderable_model: None,
            renderable: Some("models/broken.vmesh".to_string()),
            transform: world::IDENTITY_TRANSFORM,
            tint: [1.0, 1.0, 1.0, 1.0],
            is_overlay: false,
            overlay_render_order: 0,
            layer_index: None,
            light_probe_volume_handshake: 0,
        };

        place_scene_object(&mut ctx, &mut children, &so, [1.0, 1.0]);

        assert!(children.is_empty());
        assert!(
            ctx.report
                .missing_resources
                .iter()
                .any(|m| m.contains("failed to decode mesh")),
            "{:?}",
            ctx.report.missing_resources
        );
    }

    /// §1/§11: the shared per-draw-call loop resolves the geometry-offset `overlay` flag as
    /// `object_overlay || material.material_says_overlay`, but `overlay_order` only ever threads
    /// into `extras.overlayOrder` for an object-level overlay (`m_nOverlayRenderOrder` is a
    /// scene-object property, not a material one) -- on Mirage most overlay-material draws come
    /// from ordinary (non-overlay-object) placements and must not get a stray `overlayOrder`.
    #[test]
    fn place_mesh_draw_calls_overlay_order_requires_the_object_flag_not_just_the_material() {
        let dir = temp_dir("overlay-or");
        let sources = open_empty_sources(&dir, None);
        let mut ctx = empty_ctx(&sources);
        cache_material(
            &mut ctx,
            "materials/plain.vmat",
            base_material("csgo_vertexlitgeneric"),
        );
        cache_material(
            &mut ctx,
            "materials/overlay.vmat",
            base_material("csgo_static_overlay"),
        );

        let mut children = Vec::new();
        // Neither the object nor the material says overlay: no overlayOrder even though one is
        // supplied.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "a",
            &triangle_mesh("materials/plain.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            false,
            Some(7),
            0,
            None,
        );
        // Material alone says overlay (csgo_static_overlay's shader): gets the vertex offset, but
        // not `overlayOrder` -- that field belongs to an overlay *object*, not a material.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "b",
            &triangle_mesh("materials/overlay.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            false,
            Some(7),
            0,
            None,
        );
        // Object flag on: gets both the offset and overlayOrder, even on a non-overlay material.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "c",
            &triangle_mesh("materials/plain.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            true,
            Some(9),
            0,
            None,
        );

        assert_eq!(children.len(), 3);
        let glb = ctx
            .builder
            .finish(children.clone(), Vec::new(), json!({}))
            .expect("small doc");
        let doc = glb_json(&glb);
        let nodes = doc["nodes"].as_array().unwrap();
        assert!(nodes[0].get("extras").is_none(), "{:?}", nodes[0]);
        assert!(nodes[1].get("extras").is_none(), "{:?}", nodes[1]);
        assert_eq!(nodes[2]["extras"]["overlayOrder"], 9);
    }

    fn triangle_mesh_dc(mesh: &Mesh) -> &DrawCall {
        &mesh.scene_objects[0].draw_calls[0]
    }

    /// §0's `classify_lighting`: a draw call with `m_bHasBakedLightingFromLightMap` *and* an
    /// actual TEXCOORD index-3 stream reads the lightmap; an unlit material with neither has no
    /// indirect term at all; everything else falls back to probes (`Mesh.cs:202-204`,
    /// `RenderableMesh.cs:314-325`).
    #[test]
    fn classify_lighting_covers_all_three_cases() {
        let mut mesh = triangle_mesh("materials/plain.vmat");
        // Case 1: lightmap -- has_baked_lighting_from_lightmap *and* a TEXCOORD index-3 field.
        mesh.vertex_buffers[0].fields.push(InputLayoutField {
            semantic_name: "TEXCOORD".to_string(),
            semantic_index: 3,
            format: DxgiFormat::R16G16Unorm,
            offset: 0,
        });
        mesh.scene_objects[0].draw_calls[0].has_baked_lighting_from_lightmap = true;
        assert_eq!(
            classify_lighting(&mesh, triangle_mesh_dc(&mesh), false),
            LightingClass::Lightmap
        );

        // Case 2: unlit -- no TEXCOORD index-3 stream, material is unlit.
        let unlit_mesh = triangle_mesh("materials/plain.vmat");
        assert_eq!(
            classify_lighting(&unlit_mesh, triangle_mesh_dc(&unlit_mesh), true),
            LightingClass::Unlit
        );

        // Case 3: probe -- neither of the above (the fallback).
        let probe_mesh = triangle_mesh("materials/plain.vmat");
        assert_eq!(
            classify_lighting(&probe_mesh, triangle_mesh_dc(&probe_mesh), false),
            LightingClass::Probe
        );

        // Case 4: flag alone isn't enough -- has_baked_lighting_from_lightmap true, but no
        // TEXCOORD index-3 stream, falls back to probes.
        let mut flag_only_mesh = triangle_mesh("materials/plain.vmat");
        flag_only_mesh.scene_objects[0].draw_calls[0].has_baked_lighting_from_lightmap = true;
        assert_eq!(
            classify_lighting(&flag_only_mesh, triangle_mesh_dc(&flag_only_mesh), false),
            LightingClass::Probe
        );

        // Case 5: the TEXCOORD stream alone isn't enough either -- present, but
        // has_baked_lighting_from_lightmap false, also falls back to probes.
        let mut uv_only_mesh = triangle_mesh("materials/plain.vmat");
        uv_only_mesh.vertex_buffers[0]
            .fields
            .push(InputLayoutField {
                semantic_name: "TEXCOORD".to_string(),
                semantic_index: 3,
                format: DxgiFormat::R16G16Unorm,
                offset: 0,
            });
        assert_eq!(
            classify_lighting(&uv_only_mesh, triangle_mesh_dc(&uv_only_mesh), false),
            LightingClass::Probe
        );
    }

    /// §1's fix: `bakedShadowChannel` comes from the first `light_environment`'s
    /// `bakedshadowindex` (fallback `bakelightindex`), `0..=3` only -- never `m_bakedShadows[0]`
    /// (a baked *local* light's channel, not the sun's).
    #[test]
    fn baked_shadow_channel_reads_bakedshadowindex_with_bakelightindex_fallback() {
        use s2fmt::entities::EntityValue;

        let env_with_index = s2fmt::entities::Entity {
            properties: vec![
                (
                    "classname".to_string(),
                    EntityValue::String("light_environment".to_string()),
                ),
                ("bakedshadowindex".to_string(), EntityValue::Int(2)),
            ],
            connections: Vec::new(),
        };
        let lump = EntityLump {
            name: String::new(),
            child_lumps: Vec::new(),
            entities: vec![env_with_index],
        };
        assert_eq!(
            find_baked_shadow_channel(std::slice::from_ref(&lump)),
            Some(2)
        );

        let env_fallback = s2fmt::entities::Entity {
            properties: vec![
                (
                    "classname".to_string(),
                    EntityValue::String("light_environment".to_string()),
                ),
                ("bakelightindex".to_string(), EntityValue::Int(1)),
            ],
            connections: Vec::new(),
        };
        let lump = EntityLump {
            name: String::new(),
            child_lumps: Vec::new(),
            entities: vec![env_fallback],
        };
        assert_eq!(
            find_baked_shadow_channel(std::slice::from_ref(&lump)),
            Some(1)
        );

        let env_out_of_range = s2fmt::entities::Entity {
            properties: vec![
                (
                    "classname".to_string(),
                    EntityValue::String("light_environment".to_string()),
                ),
                ("bakedshadowindex".to_string(), EntityValue::Int(4)),
            ],
            connections: Vec::new(),
        };
        let lump = EntityLump {
            name: String::new(),
            child_lumps: Vec::new(),
            entities: vec![env_out_of_range],
        };
        assert_eq!(find_baked_shadow_channel(std::slice::from_ref(&lump)), None);

        let env_neither = s2fmt::entities::Entity {
            properties: vec![(
                "classname".to_string(),
                EntityValue::String("light_environment".to_string()),
            )],
            connections: Vec::new(),
        };
        let lump = EntityLump {
            name: String::new(),
            child_lumps: Vec::new(),
            entities: vec![env_neither],
        };
        assert_eq!(find_baked_shadow_channel(std::slice::from_ref(&lump)), None);
    }

    fn bin_chunk(bytes: &[u8]) -> &[u8] {
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let bin_start = 20 + json_len;
        let bin_len =
            u32::from_le_bytes(bytes[bin_start..bin_start + 4].try_into().unwrap()) as usize;
        &bytes[bin_start + 8..bin_start + 8 + bin_len]
    }

    /// §0: `TEXCOORD_1` must come from vertex field semantic `texcoord` **index 3** (not 0/1),
    /// multiplied by the world's `m_vLightmapUvScale` -- verified end to end through
    /// `build_geometry` -> `GltfBuilder::add_uv1_u16` -> the actual `.glb` bytes.
    #[test]
    fn uv1_reads_texcoord_index_3_and_applies_the_lightmap_scale() {
        let raw_u16: [u16; 2] = [32768, 16384]; // ~= (0.500008, 0.250004)
        let scale = [1.2f32, 0.8f32];

        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&0.0f32.to_le_bytes()); // POSITION.x
            data.extend_from_slice(&0.0f32.to_le_bytes()); // POSITION.y
            data.extend_from_slice(&0.0f32.to_le_bytes()); // POSITION.z
            data.extend_from_slice(&0u16.to_le_bytes()); // TEXCOORD 0, unused
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&raw_u16[0].to_le_bytes()); // TEXCOORD 3 (the lightmap UV)
            data.extend_from_slice(&raw_u16[1].to_le_bytes());
        }
        let vertex_buffer = Buffer {
            element_count: 3,
            element_size: 20,
            fields: vec![
                InputLayoutField {
                    semantic_name: "POSITION".to_string(),
                    semantic_index: 0,
                    format: DxgiFormat::R32G32B32Float,
                    offset: 0,
                },
                InputLayoutField {
                    semantic_name: "TEXCOORD".to_string(),
                    semantic_index: 0,
                    format: DxgiFormat::R16G16Unorm,
                    offset: 12,
                },
                InputLayoutField {
                    semantic_name: "TEXCOORD".to_string(),
                    semantic_index: 3,
                    format: DxgiFormat::R16G16Unorm,
                    offset: 16,
                },
            ],
            data,
            compression: Compression::default(),
        };
        let index_buffer = Buffer {
            element_count: 3,
            element_size: 2,
            fields: vec![],
            data: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            compression: Compression::default(),
        };
        let mesh = Mesh {
            vertex_buffers: vec![vertex_buffer],
            index_buffers: vec![index_buffer],
            scene_objects: vec![SceneObject {
                draw_calls: vec![DrawCall {
                    material_path: Some("materials/plain.vmat".to_string()),
                    is_triangle_list: true,
                    base_vertex: 0,
                    start_index: 0,
                    index_count: 3,
                    vertex_count: 3,
                    index_buffer: 0,
                    vertex_buffers: vec![0],
                    tint_color: None,
                    alpha: None,
                    flags: DrawCallFlags::None,
                    has_baked_lighting_from_lightmap: true,
                }],
            }],
        };
        let dc = &mesh.scene_objects[0].draw_calls[0];

        let mut builder = GltfBuilder::new();
        let built = build_geometry(&mut builder, &mesh, dc, false, true, scale, false, false)
            .expect("build_geometry");
        let uv1_idx = built.uv1.expect("uv1 accessor");

        let glb = builder
            .finish(Vec::new(), Vec::new(), json!({}))
            .expect("finish");
        let doc = glb_json(&glb);
        let accessor = &doc["accessors"][uv1_idx as usize];
        let view_idx = accessor["bufferView"].as_u64().unwrap() as usize;
        let view = &doc["bufferViews"][view_idx];
        let ext = &view["extensions"]["KHR_meshopt_compression"];
        let byte_offset = ext["byteOffset"].as_u64().unwrap() as usize;
        let byte_length = ext["byteLength"].as_u64().unwrap() as usize;
        let count = ext["count"].as_u64().unwrap() as usize;
        let bin = bin_chunk(&glb);
        let compressed = &bin[byte_offset..byte_offset + byte_length];
        let decoded: Vec<[u16; 2]> = meshopt::decode_vertex_buffer(compressed, count).unwrap();
        let (got0, got1) = (decoded[0][0], decoded[0][1]);

        let raw = [
            f32::from(raw_u16[0]) / 65535.0,
            f32::from(raw_u16[1]) / 65535.0,
        ];
        let want = [raw[0] * scale[0], raw[1] * scale[1]];
        let got = [f32::from(got0) / 65535.0, f32::from(got1) / 65535.0];
        assert!((got[0] - want[0]).abs() < 1e-3, "got {got:?} want {want:?}");
        assert!((got[1] - want[1]).abs() < 1e-3, "got {got:?} want {want:?}");
    }
}
