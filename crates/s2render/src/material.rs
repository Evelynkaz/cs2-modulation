//! `vmat_c` materials: shader name, parameters, and the rendering rules resolved from them --
//! base color/normal texture choice, alpha mode, unlit, double-sided, tint, layered blending and
//! overlay -- mirroring the CS2 renderer's own decisions, not the (buggier) glTF-exporter CLI
//! path's simplified ones (`s6f3a3_map.md` §4; cross-checked against
//! `scratch/f3_survey/REPORT.md` §5).
//!
//! Ground truth: `Renderer/Renderer/Materials/RenderMaterial.cs:310-394 LoadRenderState` (blend
//! mode derivation), `IO/Gltf/GltfModelExporter.Material.cs:54-104` (tint math) and `:552-572
//! IsMaterialOverlay`, `Renderer/Shaders/complex.frag.slang:412-455` + `common/utils.slang:
//! 189-194 ApplyBlendModulation` (layer blend formula), `Utils/ColorSpace.cs:38-56`
//! (sRGB<->linear).

use std::collections::HashMap;

use s2fmt::kv3::Value;
use s2fmt::resource::{FourCC, Resource, ResourceError};

#[derive(Debug, thiserror::Error)]
pub enum MaterialError {
    #[error("{path}: resource error: {source}")]
    Resource {
        path: String,
        #[source]
        source: Box<ResourceError>,
    },
}

/// The subset of the `RED2` block's `m_SearchableUserData` this crate reads: the legacy-schema
/// fallback for base color, unlit and translucency (`REPORT.md` §5, `materials__tools__
/// wrongway_timer.vmat_c.txt:176-191`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Red2Info {
    pub unlit: Option<bool>,
    pub translucent: Option<bool>,
    pub diffuse_albedo_texture: Option<String>,
}

/// A decoded `vmat_c`: shader name (without `.vfx`) and its raw parameter tables.
#[derive(Debug, Clone, Default)]
pub struct RawMaterial {
    pub shader: String,
    pub int_params: HashMap<String, i64>,
    pub float_params: HashMap<String, f32>,
    pub vector_params: HashMap<String, [f32; 4]>,
    pub texture_params: HashMap<String, String>,
    pub int_attributes: HashMap<String, i64>,
    pub red2: Red2Info,
}

impl RawMaterial {
    /// `m_intParams[name]`, `0` if absent (the shader's own default for a bool-like flag).
    pub fn int(&self, name: &str) -> i64 {
        self.int_params.get(name).copied().unwrap_or(0)
    }

    pub fn float(&self, name: &str) -> Option<f32> {
        self.float_params.get(name).copied()
    }

    pub fn vector(&self, name: &str) -> Option<[f32; 4]> {
        self.vector_params.get(name).copied()
    }

    pub fn texture(&self, name: &str) -> Option<&str> {
        self.texture_params.get(name).map(String::as_str)
    }

    /// `m_intAttributes["tools.toolsmaterial"] == 1` (§4: check the VALUE, not merely the key's
    /// presence).
    pub fn is_tools_material(&self) -> bool {
        self.int_attributes.get("tools.toolsmaterial").copied() == Some(1)
    }
}

fn strip_vfx(s: &str) -> String {
    s.strip_suffix(".vfx").unwrap_or(s).to_string()
}

fn require_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn parse_named_params<T>(
    arr: Option<&[Value]>,
    value_key: &str,
    get: impl Fn(&Value) -> Option<T>,
) -> HashMap<String, T> {
    let mut out = HashMap::new();
    for entry in arr.into_iter().flatten() {
        if let (Some(name), Some(value)) = (
            require_str(entry, "m_name"),
            entry.get(value_key).and_then(&get),
        ) {
            out.insert(name.to_string(), value);
        }
    }
    out
}

fn parse_vector4(v: &Value) -> Option<[f32; 4]> {
    let arr = v.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    Some([
        arr[0].as_f32().unwrap_or(0.0),
        arr[1].as_f32().unwrap_or(0.0),
        arr[2].as_f32().unwrap_or(0.0),
        arr.get(3).and_then(Value::as_f32).unwrap_or(1.0),
    ])
}

fn bool_ish(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_i64().map(|i| i != 0))
}

fn parse_red2(root: &Value) -> Red2Info {
    let sud = root.get("m_SearchableUserData");
    Red2Info {
        unlit: sud.and_then(|v| v.get("unlit")).and_then(bool_ish),
        translucent: sud.and_then(|v| v.get("translucent")).and_then(bool_ish),
        diffuse_albedo_texture: sud
            .and_then(|v| v.get("LightSim_DiffuseAlbedoTexture"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Decodes a `vmat_c` resource's `DATA` block (shader/params) and, if present, its `RED2` block
/// (`m_SearchableUserData` fallback).
pub fn decode_material(resource: &Resource, path: &str) -> Result<RawMaterial, MaterialError> {
    let doc = resource
        .data_kv3()
        .map_err(|source| MaterialError::Resource {
            path: path.to_string(),
            source: Box::new(source),
        })?;
    let root = &doc.root;

    let shader = root
        .get("m_shaderName")
        .and_then(Value::as_str)
        .map(strip_vfx)
        .unwrap_or_default();

    let int_params = parse_named_params(
        root.get("m_intParams").and_then(Value::as_array),
        "m_nValue",
        Value::as_i64,
    );
    let float_params = parse_named_params(
        root.get("m_floatParams").and_then(Value::as_array),
        "m_flValue",
        Value::as_f32,
    );
    let vector_params = parse_named_params(
        root.get("m_vectorParams").and_then(Value::as_array),
        "m_value",
        parse_vector4,
    );
    let texture_params = parse_named_params(
        root.get("m_textureParams").and_then(Value::as_array),
        "m_pValue",
        |v| Value::as_str(v).map(str::to_string),
    );
    let int_attributes = parse_named_params(
        root.get("m_intAttributes").and_then(Value::as_array),
        "m_nValue",
        Value::as_i64,
    );

    let red2 = resource
        .block(FourCC::RED2)
        .and_then(|b| resource.kv3(b).ok())
        .map(|doc| parse_red2(&doc.root))
        .unwrap_or_default();

    Ok(RawMaterial {
        shader,
        int_params,
        float_params,
        vector_params,
        texture_params,
        int_attributes,
        red2,
    })
}

/// Shift the flat glTF `alphaMode` this material renders with (`RenderMaterial.cs:342-393`'s
/// `BlendMode` enum ordering: `IsTranslucent` is `blendMode >= Translucent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    Opaque,
    Mask,
    Blend,
}

/// The second material layer of a `F_LAYERS == 1` `csgo_lightmappedgeneric` (§4; `REPORT.md` §5
/// item 5). `blend_modulation` is `None` when `F_FANCY_BLENDING` isn't the tested mode 1 (only
/// mode present on Mirage; modes 2/3 read a different channel and aren't implemented).
#[derive(Debug, Clone)]
pub struct LayerBlend {
    pub layer2_color: Option<String>,
    pub layer2_normal: Option<String>,
    pub blend_modulation: Option<String>,
}

/// The `mix(layer1, layer2, b)` formula and its inputs, as a fixed doc string emitted into
/// `render.json`'s `materialExtras` so the viewer (F3b) doesn't have to re-derive it: `b =
/// smoothstep(max(0, m.g - m.r), min(1, m.g + m.r), w)`, `m = texture(g_tBlendModulation,
/// layer2UV)`, `w` = the `_BLEND` vertex attribute (`complex.frag.slang:416-417,453`;
/// `common/utils.slang:189-194 ApplyBlendModulation`, called with `(blendFactor=w,
/// blendMask=m.g, blendSoftness=m.r)` for `F_FANCY_BLENDING == 1`).
pub const LAYER_BLEND_FORMULA: &str =
    "b = smoothstep(max(0, m.g - m.r), min(1, m.g + m.r), w); color = mix(layer1, layer2, b)";

/// A material's resolved rendering rules, independent of any per-instance tint (tint is applied
/// separately by [`base_color_factor`], since the same `vmat` tinted differently becomes a
/// distinct glTF material -- §4's "ключ материала").
#[derive(Debug, Clone)]
pub struct ResolvedMaterial {
    pub shader: String,
    pub base_color_texture: Option<String>,
    pub normal_texture: Option<String>,
    /// `csgo_black_unlit`: constant black, no base color texture at all.
    pub constant_black: bool,
    pub alpha_mode: AlphaMode,
    pub alpha_cutoff: Option<f32>,
    pub double_sided: bool,
    pub unlit: bool,
    /// From the material alone (`F_OVERLAY == 1` or shader `csgo_static_overlay`); combined by
    /// the caller with the world scene object's own `Overlay` flag (§4).
    pub material_says_overlay: bool,
    pub mod2x: bool,
    pub color_tint: [f32; 3],
    pub model_tint_amount: f32,
    pub tint_mask: bool,
    pub tint_mask_texture: Option<String>,
    pub layers: Option<LayerBlend>,
    /// `g_tAmbientOcclusion` (`g_tLayer1AmbientOcclusion` on `csgo_lightmappedgeneric`, an alias
    /// for the same slot -- `MaterialLoader.cs:57`), channel R (`s6f3a4_lighting.md` change 7).
    pub ao_texture: Option<String>,
    /// `g_tMetalness` (channel G -- `complex.frag.slang:604`) if present, else the scalar
    /// `g_flMetalness` (default `0.0`, `complex.frag.slang:239`).
    pub metalness: MetalnessSource,
    /// `F_NO_SPECULAR_AT_FULL_ROUGHNESS` (`complex.frag.slang:85,751-754`): sun specular is
    /// skipped entirely when roughness is 1.
    pub no_specular_at_full_roughness: bool,
    /// `g_bFogEnabled` (`common/fog.slang:12,95`), default true.
    pub fog_enabled: bool,
    /// `g_tSelfIllumMask` + its formula parameters, only when the shader actually evaluates
    /// self-illum at all (`complex.frag.slang:191`'s `selfillum` define; review fix item 1 --
    /// previously exported unconditionally, adding the raw mask colour to every
    /// `g_tSelfIllumMask` material regardless of `F_SELF_ILLUM`/scale/tint).
    pub self_illum: Option<SelfIllum>,
    /// `csgo_environment_blend`'s second layer (`g_tColor2`/`g_tNormal2`) -- `s6f3a6_native_tex.md`
    /// change item 5's fix: these were never resolved before, so every blend material rendered
    /// with only its first layer and no normal map.
    pub env_layer2: Option<EnvLayer2>,
    /// `csgo_environment`/`csgo_environment_blend` layer 1's own height/roughness/AO/metalness
    /// inputs (`s6f3a7_env_materials.md` change items 1/3), `None` for every other shader.
    pub env1: Option<EnvHeightParams>,
    /// What the base color texture's alpha channel means for this shader
    /// (`REPORT.md`'s "Colour alpha meaning per shader"): alpha-test/translucent always read it as
    /// opacity; otherwise `csgo_environment`/`csgo_environment_blend` read it as AO
    /// (`csgo_environment.frag.slang:41,83`); otherwise `F_METALNESS_TEXTURE` reads it as
    /// metalness (`complex.frag.slang:618`); everything else ignores it.
    pub base_color_alpha_meaning: &'static str,
}

/// `csgo_environment`/`csgo_environment_blend` layer N's height/roughness/AO/metalness/colour
/// inputs, N = "1" or "2" (`csgo_environment.frag.slang:57-73` for layer 1, `:127-143` for layer
/// 2 -- same fields, `N`-suffixed). `g_tHeight{N}`: R = height (the blend-weight input,
/// `s6f3a7_env_materials.md` change item 1), G = the **tint mask** (review fix item 1 -- this
/// doc previously called it irrelevant because it conflated it with per-instance *model* tint;
/// `tintMask{N} = remap(height{N}.g, tintMaskContrast{N}, tintMaskBrightness{N})` actually gates
/// two separate things: how much of this layer's built-in colour-correction tint
/// (`colorTint`/`colorAdjust`) shows through -- always active, this struct's own fields below --
/// and, separately, how much per-instance model tint applies when `g_bModelTint{N}` is set
/// (review fix item 5, not implemented in this pass -- see the receipt)), B = AO for alpha-tested
/// materials only (not implemented -- out of this change's scope, see the receipt), A =
/// metalness, gated by `g_bMetalness{N}` (shader default `true`).
#[derive(Debug, Clone)]
pub struct EnvHeightParams {
    pub height_texture: Option<String>,
    pub roughness_contrast: f32,
    pub roughness_brightness: f32,
    pub ao_levels: [f32; 3],
    pub metalness_enabled: bool,
    /// `g_fTextureColorContrast{N}`/`g_fTextureColorSaturation{N}`/`g_fTextureColorBrightness{N}`
    /// (review fix item 1) -- `MatrixColorCorrect2`'s `(contrast, saturation, brightness)` input.
    pub color_csb: [f32; 3],
    /// `g_vTextureColorTint{N}` (linear -- unlike `g_vColorTint`, this one carries no
    /// `SrgbRead(true)` annotation in `csgo_environment.frag.slang`).
    pub color_tint: [f32; 3],
    /// `g_nColorCorrectionMode{N}`: `1` reads the *untinted* colour-adjust matrix as the base
    /// before the tint-mask mix (`csgo_environment.frag.slang:781,804`); any other value skips
    /// straight to the raw texel there.
    pub color_correction_mode: i64,
    pub tint_mask_contrast: f32,
    pub tint_mask_brightness: f32,
    /// `g_vTexCoordScale{N}`/`g_vTexCoordOffset{N}` (review fix item 2): the vertex shader's
    /// `RotateVector2D(uv, rotation, scale, offset, center)` around a fixed `center=(0.5,0.5)`
    /// with `rotation` always `0` on every material surveyed (`csgo_environment.vert.slang:
    /// 166-171,175-180`) -- exported for both layers; only layer 2's is actually applied to
    /// sampling today (layer 1 already samples at the mesh's own UV0, which every material
    /// surveyed leaves at `scale=(1,1)`/`offset=(0,0)` anyway).
    pub uv_scale: [f32; 2],
    pub uv_offset: [f32; 2],
    /// `g_fTextureNormalContrast{N}` (review fix item 7): `normalize(mix(Up, decodedNormal,
    /// contrast))` after the HemiOct decode (`csgo_environment.frag.slang:486-505 LayerNormal`).
    pub normal_contrast: f32,
    /// `g_flTexCoordRotation{N}` in degrees (default 0). Not always 0: 90 on Ancient
    /// `hr_ancient_blend_wall_02_trims_grey-moss-wet-b` layer 2 and on both layers of Train
    /// `hrts2_blend_metalpanelling03-painted`. The viewer applies it to layer 2 only.
    pub uv_rotation: f32,
}

fn env_height_params(mat: &RawMaterial, n: &str) -> EnvHeightParams {
    EnvHeightParams {
        height_texture: mat.texture(&format!("g_tHeight{n}")).map(str::to_string),
        roughness_contrast: mat
            .float(&format!("g_fTextureRoughnessContrast{n}"))
            .unwrap_or(1.0),
        roughness_brightness: mat
            .float(&format!("g_fTextureRoughnessBrightness{n}"))
            .unwrap_or(1.0),
        ao_levels: mat
            .vector(&format!("g_vAmbientOcclusionLevels{n}"))
            .map(|v| [v[0], v[1], v[2]])
            .unwrap_or([0.0, 0.5, 1.0]),
        metalness_enabled: mat
            .int_params
            .get(&format!("g_bMetalness{n}"))
            .map(|&v| v != 0)
            .unwrap_or(true),
        color_csb: [
            mat.float(&format!("g_fTextureColorContrast{n}"))
                .unwrap_or(1.0),
            mat.float(&format!("g_fTextureColorSaturation{n}"))
                .unwrap_or(1.0),
            mat.float(&format!("g_fTextureColorBrightness{n}"))
                .unwrap_or(1.0),
        ],
        color_tint: mat
            .vector(&format!("g_vTextureColorTint{n}"))
            .map(|v| [v[0], v[1], v[2]])
            .unwrap_or([1.0, 1.0, 1.0]),
        color_correction_mode: mat
            .int_params
            .get(&format!("g_nColorCorrectionMode{n}"))
            .copied()
            .unwrap_or(0),
        tint_mask_contrast: mat.float(&format!("g_fTintMaskContrast{n}")).unwrap_or(1.0),
        tint_mask_brightness: mat
            .float(&format!("g_fTintMaskBrightness{n}"))
            .unwrap_or(1.0),
        uv_scale: mat
            .vector(&format!("g_vTexCoordScale{n}"))
            .map(|v| [v[0], v[1]])
            .unwrap_or([1.0, 1.0]),
        uv_offset: mat
            .vector(&format!("g_vTexCoordOffset{n}"))
            .map(|v| [v[0], v[1]])
            .unwrap_or([0.0, 0.0]),
        uv_rotation: mat
            .float(&format!("g_flTexCoordRotation{n}"))
            .unwrap_or(0.0),
        normal_contrast: mat
            .float(&format!("g_fTextureNormalContrast{n}"))
            .unwrap_or(1.0),
    }
}

/// `csgo_environment_blend`'s second layer: `g_tColor2` (sRGB, same "color" role as the primary
/// base color), `g_tNormal2` (linear, HemiOct, same as any other normal map), and the height-band
/// blend-weight formula's own inputs (`csgo_environment.frag.slang:348-377 GetBlendWeights` --
/// the legacy, non-`F_USE_NEW_BLENDING` path; every `csgo_environment_blend` material in the
/// `s6f3_native_tex` survey and this change's own re-check of all 69 Inferno blend materials
/// leaves `F_USE_NEW_BLENDING` unset, i.e. `0`/legacy).
#[derive(Debug, Clone)]
pub struct EnvLayer2 {
    pub color2: Option<String>,
    pub normal2: Option<String>,
    /// Layer 2's own height/roughness/AO/metalness inputs.
    pub height: EnvHeightParams,
    /// Layer 1's height-blend inputs, duplicated here rather than on `ResolvedMaterial::env1`
    /// because `GetBlendWeights` is the only place either is read.
    pub height_scale1: f32,
    pub height_zero_point1: f32,
    pub height_scale2: f32,
    pub height_zero_point2: f32,
    /// `g_flBlendSoftness2` (vertex-shader default `0.01`, `csgo_environment.vert.slang:71`) --
    /// widens the seam; the per-vertex bias `csgo_environment.vert.slang:273` adds to it
    /// (`vTEXCOORD4.w`) is not exported (`s6f3a7_env_materials.md`'s receipt explains why).
    pub blend_softness2: f32,
    /// F_BLEND_BY_FACING_DIRECTION_2: (normalised facing direction, smoothstep min/max).
    pub facing2: Option<([f32; 3], [f32; 2])>,
    /// Review fix item 8: paths the legacy `GetBlendWeights`/`mix(layer1,layer2,weight2)` formula
    /// this exporter/viewer implements cannot reproduce, each `(flag name, why)` -- checked per
    /// material rather than assumed absent, since earlier revisions of this file claimed "every
    /// material surveyed" without re-checking on every map this exporter re-exports.
    pub unsupported: Vec<(&'static str, &'static str)>,
}

/// `complex.frag.slang:227-230 GetStandardSelfIllumination`: `exp2(brightness) * scale * tint *
/// mask.r * mix(1, albedo, albedoFactor)`. `tint` is sRGB->linear converted here (like
/// `g_vColorTint`) since the game reads it `SrgbRead(true)`.
#[derive(Debug, Clone)]
pub struct SelfIllum {
    pub texture: String,
    pub scale: f32,
    pub brightness: f32,
    pub tint: [f32; 3],
    pub albedo_factor: f32,
}

/// Where a material's metalness value comes from (§7: "g_tMetalness или скаляр").
#[derive(Debug, Clone, PartialEq)]
pub enum MetalnessSource {
    /// `g_tMetalness`, channel G.
    Texture(String),
    /// `g_flMetalness`, already resolved (absent -> `0.0`).
    Scalar(f32),
}

fn pick_base_color_texture(mat: &RawMaterial) -> (Option<String>, bool) {
    if mat.shader == "csgo_black_unlit" {
        return (None, true);
    }
    let key = if matches!(
        mat.shader.as_str(),
        "csgo_environment" | "csgo_environment_blend"
    ) {
        "g_tColor1"
    } else {
        "g_tColor"
    };
    if let Some(t) = mat.texture(key) {
        return (Some(t.to_string()), false);
    }
    if let Some(t) = &mat.red2.diffuse_albedo_texture {
        return (Some(t.clone()), false);
    }
    (None, false)
}

/// A texture under `materials/default/` (the compiler's "flat normal" placeholder) is no longer
/// dropped here (`s6f3a6_native_tex.md` change item 3): it's a genuine 4x4 single-mip texture with
/// a meaningful roughness value baked into it (0.96 or 0.50, `REPORT.md`'s "Normals and roughness"),
/// which `native_texture::TextureCatalog::load`'s constant-texture path now resolves on its own --
/// the caller just gets `Loaded::Constant` back instead of a texture index, no special-casing by
/// path needed here.
fn pick_normal_texture(mat: &RawMaterial) -> Option<String> {
    let key = match mat.shader.as_str() {
        "csgo_lightmappedgeneric" => "g_tLayer1NormalRoughness",
        "csgo_environment" | "csgo_environment_blend" => "g_tNormal1",
        _ => "g_tNormal",
    };
    mat.texture(key).map(str::to_string)
}

/// `csgo_glass`/`csgo_effects` are always translucent regardless of `F_TRANSLUCENT`
/// (`RenderMaterial.cs`'s `TranslucentShaders` list, restricted to the CS2 shaders this crate
/// ever sees -- `vr_*` never appears in `materials_de_mirage.csv`).
fn shader_is_always_translucent(shader: &str) -> bool {
    matches!(shader, "csgo_glass" | "csgo_effects")
}

/// Blend mode derivation, `RenderMaterial.cs:342-393 LoadRenderState`: alpha test first, then
/// translucent (which wins over alpha test), then `F_BLEND_MODE` on `csgo_static_overlay`/
/// `csgo_unlitgeneric` overrides everything except a value of `0` (leaves the prior mode alone).
/// Ranks follow the reference's `BlendMode` enum order so `>= Translucent` means "blend".
fn compute_blend(mat: &RawMaterial) -> (AlphaMode, bool) {
    const OPAQUE: u8 = 0;
    const ALPHA_TEST: u8 = 1;
    const TRANSLUCENT: u8 = 2;
    const ADDITIVE: u8 = 3;
    const MULTIPLY: u8 = 4;
    const MOD2X: u8 = 5;
    const MOD_THEN_ADD: u8 = 6;

    let mut rank = OPAQUE;
    if mat.int("F_ALPHA_TEST") > 0 {
        rank = ALPHA_TEST;
    }
    if mat.int("F_TRANSLUCENT") == 1
        || shader_is_always_translucent(&mat.shader)
        || mat.red2.translucent == Some(true)
    {
        rank = TRANSLUCENT;
    }

    let blend_mode_param = if matches!(
        mat.shader.as_str(),
        "csgo_static_overlay" | "csgo_unlitgeneric"
    ) {
        mat.int("F_BLEND_MODE")
    } else {
        0
    };
    rank = match blend_mode_param {
        1 => TRANSLUCENT,
        2 => ALPHA_TEST,
        3 => MOD2X,
        4 => ADDITIVE,
        5 => MULTIPLY,
        6 => MOD_THEN_ADD,
        _ => rank,
    };

    let alpha_mode = if rank >= TRANSLUCENT {
        AlphaMode::Blend
    } else if rank == ALPHA_TEST {
        AlphaMode::Mask
    } else {
        AlphaMode::Opaque
    };
    (alpha_mode, rank == MOD2X)
}

fn unlit(mat: &RawMaterial) -> bool {
    matches!(
        mat.shader.as_str(),
        "csgo_unlitgeneric" | "csgo_black_unlit" | "csgo_effects"
    ) || mat.red2.unlit == Some(true)
}

fn layers(mat: &RawMaterial) -> Option<LayerBlend> {
    if mat.shader != "csgo_lightmappedgeneric" || mat.int("F_LAYERS") != 1 {
        return None;
    }
    Some(LayerBlend {
        layer2_color: mat.texture("g_tLayer2Color").map(str::to_string),
        layer2_normal: mat.texture("g_tLayer2NormalRoughness").map(str::to_string),
        blend_modulation: if mat.int("F_FANCY_BLENDING") > 0 {
            mat.texture("g_tBlendModulation").map(str::to_string)
        } else {
            None
        },
    })
}

/// `g_tAmbientOcclusion`, except `csgo_lightmappedgeneric` which names the same slot
/// `g_tLayer1AmbientOcclusion` (§7; `MaterialLoader.cs:57`'s alias table).
fn ao_texture(mat: &RawMaterial) -> Option<String> {
    let key = if mat.shader == "csgo_lightmappedgeneric" {
        "g_tLayer1AmbientOcclusion"
    } else {
        "g_tAmbientOcclusion"
    };
    mat.texture(key).map(str::to_string)
}

fn metalness(mat: &RawMaterial) -> MetalnessSource {
    match mat.texture("g_tMetalness") {
        Some(t) => MetalnessSource::Texture(t.to_string()),
        None => MetalnessSource::Scalar(mat.float("g_flMetalness").unwrap_or(0.0)),
    }
}

/// `csgo_environment_blend`'s second layer (`s6f3a6_native_tex.md` change item 5): `None` when
/// neither texture is set, so a plain (non-blend) `csgo_environment` material never gets an empty
/// `extras.envLayer2`.
fn env_layer2(mat: &RawMaterial) -> Option<EnvLayer2> {
    if mat.shader != "csgo_environment_blend" {
        return None;
    }
    let color2 = mat.texture("g_tColor2").map(str::to_string);
    let normal2 = mat.texture("g_tNormal2").map(str::to_string);
    if color2.is_none() && normal2.is_none() {
        return None;
    }
    let mut unsupported = Vec::new();
    if mat.int("F_USE_NEW_BLENDING") == 1 {
        unsupported.push((
            "newBlending",
            "F_USE_NEW_BLENDING==1: this material uses BlendLayer/BlendBandWeight \
             (csgo_environment.frag.slang:314-346), not the legacy GetBlendWeights this exporter/\
             viewer implements; the exported weight2 formula will not match the game here",
        ));
    }
    if mat.int("F_ENABLE_LAYER_3") == 1 {
        unsupported.push((
            "layer3",
            "F_ENABLE_LAYER_3==1: this material has a third layer (g_tColor3/...) this exporter \
             never reads; only layers 1/2 are mixed",
        ));
    }
    let uv_set = |key: &str| mat.int_params.get(key).copied().unwrap_or(1);
    if uv_set("g_nUVSet1") == 0 || uv_set("g_nUVSet2") == 0 {
        unsupported.push((
            "biplanar",
            "g_nUVSet{1,2}==0 (biplanar/triplanar projection, csgo_environment.frag.slang:\
             403-460): this exporter always samples at the mesh's own UV0/UV1, never the \
             world-space biplanar projection",
        ));
    }
    Some(EnvLayer2 {
        color2,
        normal2,
        height: env_height_params(mat, "2"),
        height_scale1: mat.float("g_flHeightMapScale1").unwrap_or(1.0),
        height_zero_point1: mat.float("g_flHeightMapZeroPoint1").unwrap_or(0.5),
        height_scale2: mat.float("g_flHeightMapScale2").unwrap_or(1.0),
        height_zero_point2: mat.float("g_flHeightMapZeroPoint2").unwrap_or(0.5),
        blend_softness2: mat.float("g_flBlendSoftness2").unwrap_or(0.01),
        facing2: env_facing2(mat),
        unsupported,
    })
}

/// `F_BLEND_BY_FACING_DIRECTION_2 > 0` (csgo_environment.vert.slang:73-81, 246-252): the paint
/// weight is multiplied by `smoothstep(minMax.x, minMax.y, dot(direction, N) * 0.5 + 0.5)`.
/// Returns the normalised direction (z nudged off 0 as the reference does) and that min/max.
fn env_facing2(mat: &RawMaterial) -> Option<([f32; 3], [f32; 2])> {
    if mat.int("F_BLEND_BY_FACING_DIRECTION_2") <= 0 {
        return None;
    }
    let d = mat
        .vector("g_vFacingDirection2")
        .map(|v| [v[0], v[1], v[2]])
        .unwrap_or([0.0, 0.0, 1.0]);
    let d = [d[0], d[1], if d[2] == 0.0 { 0.0001 } else { d[2] }];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let dir = [d[0] / len, d[1] / len, d[2] / len];
    let spread = mat.float("g_flFacingDirectionMaskSpread2").unwrap_or(0.5);
    let falloff = mat
        .float("g_vFacingDirectionMaskFalloff2")
        .or_else(|| mat.vector("g_vFacingDirectionMaskFalloff2").map(|v| v[0]))
        .unwrap_or(0.1);
    let min_max = [
        ((1.0 - spread) - falloff).max(0.0),
        ((1.0 - spread) + 0.001 + falloff).min(1.0),
    ];
    Some((dir, min_max))
}

/// `csgo_environment`/`csgo_environment_blend` layer 1's height-blend inputs (`s6f3a7_env_
/// materials.md` change item 1); `None` for every other shader.
fn env1(mat: &RawMaterial) -> Option<EnvHeightParams> {
    if !matches!(
        mat.shader.as_str(),
        "csgo_environment" | "csgo_environment_blend"
    ) {
        return None;
    }
    Some(env_height_params(mat, "1"))
}

/// `REPORT.md`'s "Colour alpha meaning per shader": alpha-test/translucent always win (the alpha
/// channel is opacity whenever the material actually reads it that way), otherwise
/// `csgo_environment`/`csgo_environment_blend` read it as AO (`csgo_environment.frag.slang:41,83`),
/// otherwise `F_METALNESS_TEXTURE` reads it as metalness (`complex.frag.slang:618`), otherwise it's
/// unused.
fn base_color_alpha_meaning(mat: &RawMaterial, alpha_mode: AlphaMode) -> &'static str {
    if alpha_mode != AlphaMode::Opaque {
        return "opacity";
    }
    if matches!(
        mat.shader.as_str(),
        "csgo_environment" | "csgo_environment_blend"
    ) {
        return "ao";
    }
    if mat.int("F_METALNESS_TEXTURE") == 1 {
        return "metalness";
    }
    "none"
}

/// `complex.frag.slang:191`'s `selfillum` define, restricted to the shaders this crate ever sees:
/// `F_SELF_ILLUM == 1` on the vertexlit/complex family, or unconditionally on `csgo_unlitgeneric`.
/// `None` when the shader wouldn't evaluate self-illum at all, even if `g_tSelfIllumMask` happens
/// to be set (review fix item 1 -- previously this crate added the raw mask colour to every
/// material carrying that texture, regardless of the flag).
fn self_illum(mat: &RawMaterial) -> Option<SelfIllum> {
    let applies = mat.shader == "csgo_unlitgeneric" || mat.int("F_SELF_ILLUM") == 1;
    if !applies {
        return None;
    }
    let texture = mat.texture("g_tSelfIllumMask")?.to_string();
    let tint = mat
        .vector("g_vSelfIllumTint")
        .map(|v| srgb_to_linear([v[0], v[1], v[2]]))
        .unwrap_or([1.0, 1.0, 1.0]);
    Some(SelfIllum {
        texture,
        scale: mat.float("g_flSelfIllumScale").unwrap_or(1.0),
        brightness: mat.float("g_flSelfIllumBrightness").unwrap_or(0.0),
        tint,
        albedo_factor: mat.float("g_flSelfIllumAlbedoFactor").unwrap_or(0.0),
    })
}

/// Resolves a decoded material's fixed (tint-independent) rendering rules.
pub fn resolve(mat: &RawMaterial) -> ResolvedMaterial {
    let (base_color_texture, constant_black) = pick_base_color_texture(mat);
    let normal_texture = pick_normal_texture(mat);
    let (alpha_mode, mod2x) = compute_blend(mat);
    let alpha_cutoff = if alpha_mode == AlphaMode::Mask {
        Some(mat.float("g_flAlphaTestReference").unwrap_or(0.5))
    } else {
        None
    };
    let color_tint = mat
        .vector("g_vColorTint")
        .map(|v| [v[0], v[1], v[2]])
        .unwrap_or([1.0, 1.0, 1.0]);

    ResolvedMaterial {
        shader: mat.shader.clone(),
        base_color_texture,
        normal_texture,
        constant_black,
        alpha_mode,
        alpha_cutoff,
        double_sided: mat.int("F_RENDER_BACKFACES") == 1,
        unlit: unlit(mat),
        material_says_overlay: mat.int("F_OVERLAY") == 1 || mat.shader == "csgo_static_overlay",
        mod2x,
        color_tint,
        model_tint_amount: mat.float("g_flModelTintAmount").unwrap_or(1.0),
        tint_mask: mat.int("F_TINT_MASK") == 1,
        tint_mask_texture: mat.texture("g_tTintMask").map(str::to_string),
        layers: layers(mat),
        ao_texture: ao_texture(mat),
        metalness: metalness(mat),
        no_specular_at_full_roughness: mat.int("F_NO_SPECULAR_AT_FULL_ROUGHNESS") == 1,
        fog_enabled: mat.int_params.get("g_bFogEnabled").copied().unwrap_or(1) != 0,
        self_illum: self_illum(mat),
        env_layer2: env_layer2(mat),
        env1: env1(mat),
        base_color_alpha_meaning: base_color_alpha_meaning(mat, alpha_mode),
    }
}

/// sRGB (gamma) -> linear, `Utils/ColorSpace.cs:38-56`'s formula (the `Exp`-vs-`Pow` and epsilon
/// tricks there are float-precision optimizations for SIMD; scalar `powf` gives the same result).
pub fn srgb_to_linear(c: [f32; 3]) -> [f32; 3] {
    c.map(|v| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    })
}

/// Combines a material's own tint parameters with this instance's chained tint (draw call x
/// fragment/scene-object/entity, already multiplied together by the caller) into a glTF
/// `baseColorFactor`, `GltfModelExporter.Material.cs:89-104`: `lerp(1, tint, modelTintAmount)`,
/// times `g_vColorTint`, sRGB->linear (alpha stays linear, straight from the draw call's
/// `m_flAlpha`/scene-object alpha -- §4's tint multiplies RGB, not alpha), clamped to `0..1`.
/// When `F_TINT_MASK == 1` the tint must not land in `baseColorFactor` at all (it would recolour
/// the whole model, e.g. turn a car's window glass and tires the paint colour) -- instead
/// `baseColorFactor` stays neutral white (alpha still carried through) and the tint is returned,
/// sRGB->linear like `baseColorFactor` itself, separately for `extras.tint`/`extras.tintMask`
/// (§4).
pub fn base_color_factor(
    resolved: &ResolvedMaterial,
    tint_rgba: [f32; 4],
) -> ([f32; 4], Option<[f32; 3]>) {
    let amount = resolved.model_tint_amount;
    let mut base = [
        1.0 + (tint_rgba[0] - 1.0) * amount,
        1.0 + (tint_rgba[1] - 1.0) * amount,
        1.0 + (tint_rgba[2] - 1.0) * amount,
    ];
    base[0] *= resolved.color_tint[0];
    base[1] *= resolved.color_tint[1];
    base[2] *= resolved.color_tint[2];
    let alpha = tint_rgba[3].clamp(0.0, 1.0);

    if resolved.tint_mask {
        let linear_tint = srgb_to_linear(base);
        return ([1.0, 1.0, 1.0, alpha], Some(linear_tint));
    }

    let linear = srgb_to_linear(base);
    (
        [
            linear[0].clamp(0.0, 1.0),
            linear[1].clamp(0.0, 1.0),
            linear[2].clamp(0.0, 1.0),
            alpha,
        ],
        None,
    )
}

/// Overlay vertex offset along the normal, `OverlayNormalOffsetDistance = 0.01f / 0.0254f`
/// (`GltfModelExporter.Mesh.cs:23`), in inches since geometry stays in game units (§6).
pub const OVERLAY_NORMAL_OFFSET: f32 = 0.01 / 0.0254;

#[cfg(test)]
mod tests {
    use super::*;

    fn base_material(shader: &str) -> RawMaterial {
        RawMaterial {
            shader: shader.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn opaque_by_default() {
        let mat = base_material("csgo_vertexlitgeneric");
        let r = resolve(&mat);
        assert_eq!(r.alpha_mode, AlphaMode::Opaque);
        assert!(r.alpha_cutoff.is_none());
        assert!(!r.mod2x);
    }

    #[test]
    fn alpha_test_sets_mask_with_cutoff() {
        let mut mat = base_material("csgo_vertexlitgeneric");
        mat.int_params.insert("F_ALPHA_TEST".into(), 1);
        mat.float_params
            .insert("g_flAlphaTestReference".into(), 0.7);
        let r = resolve(&mat);
        assert_eq!(r.alpha_mode, AlphaMode::Mask);
        assert_eq!(r.alpha_cutoff, Some(0.7));
    }

    #[test]
    fn translucent_flag_wins_over_alpha_test() {
        let mut mat = base_material("csgo_vertexlitgeneric");
        mat.int_params.insert("F_ALPHA_TEST".into(), 1);
        mat.int_params.insert("F_TRANSLUCENT".into(), 1);
        let r = resolve(&mat);
        assert_eq!(r.alpha_mode, AlphaMode::Blend);
    }

    #[test]
    fn glass_and_effects_shaders_are_always_translucent() {
        assert_eq!(
            resolve(&base_material("csgo_glass")).alpha_mode,
            AlphaMode::Blend
        );
        assert_eq!(
            resolve(&base_material("csgo_effects")).alpha_mode,
            AlphaMode::Blend
        );
    }

    #[test]
    fn static_overlay_blend_mode_1_is_blend_and_2_is_mask() {
        let mut blend1 = base_material("csgo_static_overlay");
        blend1.int_params.insert("F_BLEND_MODE".into(), 1);
        assert_eq!(resolve(&blend1).alpha_mode, AlphaMode::Blend);

        let mut blend2 = base_material("csgo_static_overlay");
        blend2.int_params.insert("F_BLEND_MODE".into(), 2);
        assert_eq!(resolve(&blend2).alpha_mode, AlphaMode::Mask);

        let mut blend0 = base_material("csgo_static_overlay");
        blend0.int_params.insert("F_BLEND_MODE".into(), 0);
        assert_eq!(resolve(&blend0).alpha_mode, AlphaMode::Opaque);
    }

    #[test]
    fn static_overlay_blend_mode_3_is_mod2x_and_blend() {
        let mut mat = base_material("csgo_static_overlay");
        mat.int_params.insert("F_BLEND_MODE".into(), 3);
        let r = resolve(&mat);
        assert_eq!(r.alpha_mode, AlphaMode::Blend);
        assert!(r.mod2x);
    }

    #[test]
    fn unlitgeneric_blend_mode_applies_same_as_static_overlay() {
        let mut mat = base_material("csgo_unlitgeneric");
        mat.int_params.insert("F_BLEND_MODE".into(), 1);
        assert_eq!(resolve(&mat).alpha_mode, AlphaMode::Blend);
    }

    #[test]
    fn other_shaders_ignore_f_blend_mode() {
        let mut mat = base_material("csgo_vertexlitgeneric");
        mat.int_params.insert("F_BLEND_MODE".into(), 1);
        assert_eq!(resolve(&mat).alpha_mode, AlphaMode::Opaque);
    }

    #[test]
    fn unlit_from_shader_or_red2() {
        assert!(resolve(&base_material("csgo_unlitgeneric")).unlit);
        assert!(resolve(&base_material("csgo_black_unlit")).unlit);
        assert!(resolve(&base_material("csgo_effects")).unlit);
        assert!(!resolve(&base_material("csgo_vertexlitgeneric")).unlit);

        let mut red2_unlit = base_material("csgo_vertexlitgeneric");
        red2_unlit.red2.unlit = Some(true);
        assert!(resolve(&red2_unlit).unlit);
    }

    #[test]
    fn double_sided_from_render_backfaces() {
        let mut mat = base_material("csgo_vertexlitgeneric");
        mat.int_params.insert("F_RENDER_BACKFACES".into(), 1);
        assert!(resolve(&mat).double_sided);
    }

    #[test]
    fn overlay_from_material_flag_or_shader() {
        let mut overlay_flag = base_material("csgo_lightmappedgeneric");
        overlay_flag.int_params.insert("F_OVERLAY".into(), 1);
        assert!(resolve(&overlay_flag).material_says_overlay);
        assert!(resolve(&base_material("csgo_static_overlay")).material_says_overlay);
        assert!(!resolve(&base_material("csgo_vertexlitgeneric")).material_says_overlay);
    }

    #[test]
    fn base_color_texture_by_shader() {
        let mut env = base_material("csgo_environment");
        env.texture_params
            .insert("g_tColor1".into(), "materials/sky/color1.vtex".into());
        assert_eq!(
            resolve(&env).base_color_texture.as_deref(),
            Some("materials/sky/color1.vtex")
        );

        let mut generic = base_material("csgo_vertexlitgeneric");
        generic
            .texture_params
            .insert("g_tColor".into(), "materials/wall.vtex".into());
        assert_eq!(
            resolve(&generic).base_color_texture.as_deref(),
            Some("materials/wall.vtex")
        );
    }

    #[test]
    fn base_color_falls_back_to_red2_diffuse_albedo() {
        let mut mat = base_material("csgo_lightmappedgeneric");
        mat.red2.diffuse_albedo_texture = Some("materials/legacy.vtex".to_string());
        assert_eq!(
            resolve(&mat).base_color_texture.as_deref(),
            Some("materials/legacy.vtex")
        );
    }

    #[test]
    fn black_unlit_has_no_base_color_texture() {
        let mut mat = base_material("csgo_black_unlit");
        mat.texture_params
            .insert("g_tColor".into(), "materials/never_used.vtex".into());
        let r = resolve(&mat);
        assert!(r.constant_black);
        assert!(r.base_color_texture.is_none());
    }

    #[test]
    fn normal_texture_by_shader_and_environment_blend_uses_g_t_normal1() {
        let mut lm = base_material("csgo_lightmappedgeneric");
        lm.texture_params
            .insert("g_tLayer1NormalRoughness".into(), "materials/n.vtex".into());
        assert_eq!(
            resolve(&lm).normal_texture.as_deref(),
            Some("materials/n.vtex")
        );

        let mut env = base_material("csgo_environment");
        env.texture_params
            .insert("g_tNormal1".into(), "materials/n1.vtex".into());
        assert_eq!(
            resolve(&env).normal_texture.as_deref(),
            Some("materials/n1.vtex")
        );

        let mut env_blend = base_material("csgo_environment_blend");
        env_blend
            .texture_params
            .insert("g_tNormal1".into(), "materials/n1blend.vtex".into());
        assert_eq!(
            resolve(&env_blend).normal_texture.as_deref(),
            Some("materials/n1blend.vtex")
        );

        // `s6f3a6_native_tex.md` change item 3's fix: a `materials/default/` normal is no longer
        // dropped by path -- it's a real (4x4-constant) texture with meaningful roughness baked
        // in, resolved by `native_texture::TextureCatalog::load` instead.
        let mut default_n = base_material("csgo_vertexlitgeneric");
        default_n.texture_params.insert(
            "g_tNormal".into(),
            "materials/default/default_normal.vtex".into(),
        );
        assert_eq!(
            resolve(&default_n).normal_texture.as_deref(),
            Some("materials/default/default_normal.vtex")
        );
    }

    #[test]
    fn layers_require_shader_and_exact_flag_value() {
        let mut yes = base_material("csgo_lightmappedgeneric");
        yes.int_params.insert("F_LAYERS".into(), 1);
        yes.int_params.insert("F_FANCY_BLENDING".into(), 1);
        yes.texture_params
            .insert("g_tLayer2Color".into(), "materials/l2.vtex".into());
        yes.texture_params
            .insert("g_tBlendModulation".into(), "materials/mod.vtex".into());
        let r = resolve(&yes).layers.expect("layered material");
        assert_eq!(r.layer2_color.as_deref(), Some("materials/l2.vtex"));
        assert_eq!(r.blend_modulation.as_deref(), Some("materials/mod.vtex"));

        // F_LAYERS present but 0: not layered (REPORT.md's "test the value").
        let mut zero = base_material("csgo_lightmappedgeneric");
        zero.int_params.insert("F_LAYERS".into(), 0);
        assert!(resolve(&zero).layers.is_none());

        // Wrong shader: not layered even with F_LAYERS == 1.
        let mut wrong_shader = base_material("csgo_vertexlitgeneric");
        wrong_shader.int_params.insert("F_LAYERS".into(), 1);
        assert!(resolve(&wrong_shader).layers.is_none());
    }

    /// `s6f3a6_native_tex.md` change item 5's fix: `csgo_environment_blend`'s second layer is now
    /// resolved (previously dropped entirely).
    #[test]
    fn env_layer2_requires_the_blend_shader_and_at_least_one_texture() {
        let mut blend = base_material("csgo_environment_blend");
        blend
            .texture_params
            .insert("g_tColor2".into(), "materials/c2.vtex".into());
        blend
            .texture_params
            .insert("g_tNormal2".into(), "materials/n2.vtex".into());
        let r = resolve(&blend).env_layer2.expect("env layer2");
        assert_eq!(r.color2.as_deref(), Some("materials/c2.vtex"));
        assert_eq!(r.normal2.as_deref(), Some("materials/n2.vtex"));

        // Plain (non-blend) csgo_environment: never gets env_layer2, even with the same params.
        let mut plain = base_material("csgo_environment");
        plain
            .texture_params
            .insert("g_tColor2".into(), "materials/c2.vtex".into());
        assert!(resolve(&plain).env_layer2.is_none());

        // csgo_environment_blend with neither texture set: None, not Some(empty).
        let empty = base_material("csgo_environment_blend");
        assert!(resolve(&empty).env_layer2.is_none());
    }

    /// `s6f3a7_env_materials.md` change item 1: `g_tHeight1`/`g_tHeight2` and the height-band
    /// blend/roughness-remap parameters are resolved for both layers, with the shader's own
    /// defaults where a material leaves a param unset.
    #[test]
    fn env_height_params_resolve_with_shader_defaults_and_overrides() {
        // Plain (non-blend) csgo_environment still gets `env1` -- roughness remap (item 3)
        // applies with or without a second layer.
        let mut plain = base_material("csgo_environment");
        plain
            .texture_params
            .insert("g_tHeight1".into(), "materials/h1.vtex".into());
        let env1 = resolve(&plain).env1.expect("csgo_environment has env1");
        assert_eq!(env1.height_texture.as_deref(), Some("materials/h1.vtex"));
        assert_eq!(env1.roughness_contrast, 1.0);
        assert_eq!(env1.roughness_brightness, 1.0);
        assert_eq!(env1.ao_levels, [0.0, 0.5, 1.0]);
        assert!(env1.metalness_enabled, "g_bMetalness1 defaults true");

        let mut blend = base_material("csgo_environment_blend");
        blend
            .texture_params
            .insert("g_tColor2".into(), "materials/c2.vtex".into());
        blend
            .texture_params
            .insert("g_tHeight2".into(), "materials/h2.vtex".into());
        blend
            .float_params
            .insert("g_fTextureRoughnessContrast2".into(), 2.0);
        blend
            .float_params
            .insert("g_fTextureRoughnessBrightness2".into(), 0.64);
        blend
            .float_params
            .insert("g_flHeightMapZeroPoint2".into(), 0.4);
        blend.int_params.insert("g_bMetalness2".into(), 0);
        let layer2 = resolve(&blend).env_layer2.expect("env layer2");
        assert_eq!(
            layer2.height.height_texture.as_deref(),
            Some("materials/h2.vtex")
        );
        assert_eq!(layer2.height.roughness_contrast, 2.0);
        assert_eq!(layer2.height.roughness_brightness, 0.64);
        assert!(!layer2.height.metalness_enabled);
        assert_eq!(layer2.height_zero_point1, 0.5, "layer 1 default unchanged");
        assert_eq!(layer2.height_zero_point2, 0.4);
        assert_eq!(layer2.height_scale1, 1.0);
        assert_eq!(layer2.height_scale2, 1.0);
        assert_eq!(
            layer2.blend_softness2, 0.01,
            "g_flBlendSoftness2 shader default"
        );
        assert!(
            layer2.unsupported.is_empty(),
            "no unsupported flags set on this material"
        );
    }

    /// Review fix item 8: `F_USE_NEW_BLENDING`/`F_ENABLE_LAYER_3`/a biplanar `g_nUVSet{1,2}`
    /// each add a distinct `(flag, why)` to `env_layer2`'s `unsupported` list -- none of these
    /// flags change the weight2/colour/normal formula this exporter/viewer actually implements,
    /// they only get flagged so a reader of render.json knows the result is wrong there.
    #[test]
    fn env_layer2_flags_paths_this_exporter_does_not_implement() {
        let mut mat = base_material("csgo_environment_blend");
        mat.texture_params
            .insert("g_tColor2".into(), "materials/c2.vtex".into());
        mat.int_params.insert("F_USE_NEW_BLENDING".into(), 1);
        mat.int_params.insert("F_ENABLE_LAYER_3".into(), 1);
        mat.int_params.insert("g_nUVSet2".into(), 0);
        let flags: Vec<&str> = resolve(&mat)
            .env_layer2
            .expect("env layer2")
            .unsupported
            .iter()
            .map(|(flag, _)| *flag)
            .collect();
        assert_eq!(flags, vec!["newBlending", "layer3", "biplanar"]);

        // A material that leaves every one of those flags at its default: no unsupported entries.
        let mut clean = base_material("csgo_environment_blend");
        clean
            .texture_params
            .insert("g_tColor2".into(), "materials/c2.vtex".into());
        assert!(
            resolve(&clean)
                .env_layer2
                .expect("env layer2")
                .unsupported
                .is_empty()
        );
    }

    #[test]
    fn base_color_alpha_meaning_by_shader_and_alpha_mode() {
        let mut translucent = base_material("csgo_vertexlitgeneric");
        translucent.int_params.insert("F_TRANSLUCENT".into(), 1);
        assert_eq!(resolve(&translucent).base_color_alpha_meaning, "opacity");

        let mut alpha_test = base_material("csgo_vertexlitgeneric");
        alpha_test.int_params.insert("F_ALPHA_TEST".into(), 1);
        assert_eq!(resolve(&alpha_test).base_color_alpha_meaning, "opacity");

        assert_eq!(
            resolve(&base_material("csgo_environment")).base_color_alpha_meaning,
            "ao"
        );
        assert_eq!(
            resolve(&base_material("csgo_environment_blend")).base_color_alpha_meaning,
            "ao"
        );

        let mut metalness = base_material("csgo_vertexlitgeneric");
        metalness.int_params.insert("F_METALNESS_TEXTURE".into(), 1);
        assert_eq!(resolve(&metalness).base_color_alpha_meaning, "metalness");

        assert_eq!(
            resolve(&base_material("csgo_vertexlitgeneric")).base_color_alpha_meaning,
            "none"
        );
    }

    /// Review fix item 1: `g_tSelfIllumMask` alone isn't enough -- the shader only evaluates
    /// self-illum under `complex.frag.slang:191`'s `selfillum` define (`F_SELF_ILLUM == 1`, or
    /// unconditionally on `csgo_unlitgeneric`); a material with the texture but neither condition
    /// must resolve to `None`, not add the raw mask colour on top of every lit result.
    #[test]
    fn self_illum_requires_the_flag_and_reads_its_formula_params() {
        let mut off = base_material("csgo_vertexlitgeneric");
        off.texture_params
            .insert("g_tSelfIllumMask".into(), "materials/glow.vtex".into());
        assert!(resolve(&off).self_illum.is_none(), "F_SELF_ILLUM unset");

        let mut on = base_material("csgo_vertexlitgeneric");
        on.int_params.insert("F_SELF_ILLUM".into(), 1);
        on.texture_params
            .insert("g_tSelfIllumMask".into(), "materials/glow.vtex".into());
        on.float_params.insert("g_flSelfIllumScale".into(), 2.0);
        on.float_params
            .insert("g_flSelfIllumBrightness".into(), 1.0);
        on.float_params
            .insert("g_flSelfIllumAlbedoFactor".into(), 0.5);
        on.vector_params
            .insert("g_vSelfIllumTint".into(), [0.5, 0.5, 0.5, 1.0]);
        let si = resolve(&on).self_illum.expect("F_SELF_ILLUM == 1");
        assert_eq!(si.texture, "materials/glow.vtex");
        assert_eq!(si.scale, 2.0);
        assert_eq!(si.brightness, 1.0);
        assert_eq!(si.albedo_factor, 0.5);
        let expected_tint = srgb_to_linear([0.5, 0.5, 0.5]);
        for (got, want) in si.tint.iter().zip(expected_tint) {
            assert!((got - want).abs() < 1e-6, "{:?}", si.tint);
        }

        // csgo_unlitgeneric: self-illum applies even without F_SELF_ILLUM.
        let mut unlit_glow = base_material("csgo_unlitgeneric");
        unlit_glow
            .texture_params
            .insert("g_tSelfIllumMask".into(), "materials/glow.vtex".into());
        let si2 = resolve(&unlit_glow).self_illum.expect("csgo_unlitgeneric");
        assert_eq!(si2.scale, 1.0, "default scale");
        assert_eq!(si2.brightness, 0.0, "default brightness");
        assert_eq!(si2.albedo_factor, 0.0, "default albedo factor");
        assert_eq!(si2.tint, [1.0, 1.0, 1.0], "default tint");

        // Flag set but no texture at all: still None (nothing to sample).
        let mut no_tex = base_material("csgo_vertexlitgeneric");
        no_tex.int_params.insert("F_SELF_ILLUM".into(), 1);
        assert!(resolve(&no_tex).self_illum.is_none());
    }

    #[test]
    fn fancy_blending_zero_drops_the_modulation_texture() {
        let mut mat = base_material("csgo_lightmappedgeneric");
        mat.int_params.insert("F_LAYERS".into(), 1);
        mat.texture_params
            .insert("g_tBlendModulation".into(), "materials/mod.vtex".into());
        let layers = resolve(&mat).layers.expect("layered material");
        assert!(layers.blend_modulation.is_none());
    }

    #[test]
    fn base_color_factor_applies_tint_amount_and_color_tint() {
        let mat = base_material("csgo_vertexlitgeneric");
        let mut resolved = resolve(&mat);
        resolved.color_tint = [0.5, 0.5, 0.5];
        resolved.model_tint_amount = 1.0;
        let (factor, extras_tint) = base_color_factor(&resolved, [1.0, 1.0, 1.0, 1.0]);
        assert!(extras_tint.is_none());
        // 1.0 tint * 0.5 color_tint = 0.5 srgb -> linear.
        let expected = srgb_to_linear([0.5, 0.5, 0.5]);
        for i in 0..3 {
            assert!((factor[i] - expected[i]).abs() < 1e-5, "{factor:?}");
        }
        assert_eq!(factor[3], 1.0);
    }

    #[test]
    fn base_color_factor_zero_tint_amount_ignores_instance_tint_rgb_but_keeps_alpha() {
        let mat = base_material("csgo_vertexlitgeneric");
        let mut resolved = resolve(&mat);
        resolved.model_tint_amount = 0.0;
        let (factor, _) = base_color_factor(&resolved, [0.0, 0.0, 0.0, 0.5]);
        // amount 0 => lerp(1, tint, 0) == 1 for every RGB component => white, but alpha is not
        // part of that lerp -- it always carries the chained draw-call/scene-object alpha through.
        assert_eq!(factor, [1.0, 1.0, 1.0, 0.5]);
    }

    #[test]
    fn base_color_factor_tint_mask_diverts_linear_tint_to_extras() {
        let mat = base_material("csgo_vertexlitgeneric");
        let mut resolved = resolve(&mat);
        resolved.tint_mask = true;
        let (factor, extras_tint) = base_color_factor(&resolved, [0.2, 0.4, 0.6, 0.75]);
        assert_eq!(factor, [1.0, 1.0, 1.0, 0.75]);
        let tint = extras_tint.expect("tint diverted to extras");
        let expected = srgb_to_linear([0.2, 0.4, 0.6]);
        for (got, want) in tint.iter().zip(expected) {
            assert!((got - want).abs() < 1e-5, "{tint:?}");
        }
    }

    #[test]
    fn is_tools_material_checks_value_not_presence() {
        let mut mat = base_material("generic");
        mat.int_attributes.insert("tools.toolsmaterial".into(), 1);
        assert!(mat.is_tools_material());
        mat.int_attributes.insert("tools.toolsmaterial".into(), 0);
        assert!(!mat.is_tools_material());
    }
}
