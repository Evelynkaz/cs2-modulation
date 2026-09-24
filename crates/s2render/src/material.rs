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
}

fn pick_base_color_texture(mat: &RawMaterial) -> (Option<String>, bool) {
    if mat.shader == "csgo_black_unlit" {
        return (None, true);
    }
    let key = if mat.shader == "csgo_environment" {
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

/// Textures under `materials/default/` are the compiler's "flat normal" placeholder -- dropping
/// them is visually identical and avoids exporting hundreds of copies of the same image (§4).
fn is_default_normal(path: &str) -> bool {
    path.to_ascii_lowercase().starts_with("materials/default/")
}

fn pick_normal_texture(mat: &RawMaterial) -> Option<String> {
    let key = match mat.shader.as_str() {
        "csgo_lightmappedgeneric" => "g_tLayer1NormalRoughness",
        "csgo_environment" => "g_tNormal1",
        _ => "g_tNormal",
    };
    let t = mat.texture(key)?;
    if is_default_normal(t) {
        return None;
    }
    Some(t.to_string())
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
    fn normal_texture_by_shader_and_default_is_skipped() {
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

        let mut default_n = base_material("csgo_vertexlitgeneric");
        default_n.texture_params.insert(
            "g_tNormal".into(),
            "materials/default/default_normal.vtex".into(),
        );
        assert!(resolve(&default_n).normal_texture.is_none());
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
