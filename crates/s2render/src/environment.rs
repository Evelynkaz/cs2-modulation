//! Sky, fog and post-processing data for the viewer (`s6f3a4_lighting.md` change items 4-6):
//! the 2D sky cube's raw BC blocks, cube fog parameters, and tonemap/exposure/bloom/LUT from the
//! master `post_processing_volume`'s `.vpost`. All three read from entities already loaded for
//! `place_entities` (`export.rs`), plus their referenced resources.
//!
//! Ground truth: `WorldLoader.cs:659-691` (sky tint/rotation), `sky.frag.slang:40-61` (sky
//! shading formula), `SceneCubemapFog.cs:46-88`/`fog.slang:34-50,93-127` (cube fog),
//! `PostProcess/PostProcessRenderer.cs` + `post_processing.frag.slang:87-88` (post); facts
//! (field names, values) cross-checked against `REPORT.md` §4/§6 on real Mirage data.

use s2fmt::entities::{Entity, EntityLump, angles_to_matrix};
use s2fmt::resource::FourCC;
use serde_json::{Value, json};

use crate::entity::entity_num;
use crate::material::{self, RawMaterial};
use crate::source::{Sources, compiled_path};

fn find_entity<'a>(lumps: &'a [EntityLump], classname: &str) -> Option<&'a Entity> {
    lumps
        .iter()
        .flat_map(|l| l.entities.iter())
        .find(|e| e.classname().eq_ignore_ascii_case(classname))
}

fn find_entity_by_targetname<'a>(lumps: &'a [EntityLump], name: &str) -> Option<&'a Entity> {
    lumps
        .iter()
        .flat_map(|l| l.entities.iter())
        .find(|e| e.targetname() == Some(name))
}

/// The main `env_sky` (§5's fix): the *last* one in lump order that isn't disabled
/// (`startdisabled`, or `enabled` explicitly false), or the very first `env_sky` at all if every
/// one of them is disabled (`WorldLoader.cs:611-667`'s own `disabled = disabled &&
/// Skybox2D != null` -- a later disabled entity never overwrites an already-chosen one, but the
/// first entity is taken regardless of its own disabled state if nothing has been chosen yet).
fn pick_env_sky(lumps: &[EntityLump]) -> Option<&Entity> {
    let mut chosen: Option<&Entity> = None;
    for e in lumps.iter().flat_map(|l| l.entities.iter()) {
        if !e.classname().eq_ignore_ascii_case("env_sky") {
            continue;
        }
        let mut disabled = e.get_bool("startdisabled").unwrap_or(false);
        if !disabled {
            disabled = !e.get_bool("enabled").unwrap_or(true);
        }
        if disabled && chosen.is_some() {
            continue;
        }
        chosen = Some(e);
    }
    chosen
}

fn num(e: &Entity, key: &str, default: f32) -> f32 {
    entity_num(e.get(key)).map(|v| v as f32).unwrap_or(default)
}

fn boolish(e: &Entity, key: &str, default: bool) -> bool {
    e.get_bool(key).unwrap_or(default)
}

/// Reads a `vmat_c`'s shader/params generically via [`material::decode_material`] -- the sky
/// material (`sky.vfx`) only needs `m_floatParams`/`m_textureParams`, already covered by that
/// generic parser (no shading rules to resolve, unlike a surface material).
fn load_raw_material(sources: &Sources, vmat_path: &str) -> Result<RawMaterial, String> {
    let compiled = compiled_path(vmat_path);
    let resource = sources
        .resource(&compiled)
        .map_err(|e| format!("{compiled}: {e}"))?;
    material::decode_material(&resource, &compiled).map_err(|e| format!("{compiled}: {e}"))
}

/// Sky cube (§4): every mip's raw BC blocks, smallest mip first, 6 faces contiguous per mip
/// (`Texture.cs:22-35`'s cube face order: +X,-X,+Y,-Y,+Z,-Z; `mip.rs`'s own on-disk order).
struct SkyCube {
    bytes: Vec<u8>,
    mips_json: Vec<Value>,
    format: &'static str,
    width: u32,
    height: u32,
    /// Total mip levels the source texture has, even when `lod_bias` truncated `mips_json`/
    /// `bytes` to a coarse tail of them (§3).
    total_mips: u32,
    /// The finest (largest) level actually present in `mips_json`/`bytes` -- `0` unless
    /// `lod_bias` was `Some` and truncated the chain.
    base_level: u32,
}

/// `lod_bias`: `Some(cubemapfoglodbiase)` when this is the *fog*'s own separate cube -- only
/// levels from `floor(min(7,total_mips)*(1-lod_bias))` upward are ever reached by the fog lookup
/// (`fog.slang:93-127`'s `lod` ranges from that floor, at `blend=1`, up to `min(7,total_mips)`
/// itself, at `blend=0`), so finer levels are skipped entirely instead of writing every level
/// (§3's fix -- de_ancient's full chain came to 33.5 MB for a lookup that only ever reads one).
/// `None` for the 2D sky, which needs every level for direct viewing.
fn build_sky_cube(
    sources: &Sources,
    tex_path: &str,
    lod_bias: Option<f32>,
) -> Result<SkyCube, String> {
    let compiled = compiled_path(tex_path);
    let bytes = sources
        .read(&compiled)
        .ok_or_else(|| format!("{compiled}: not found"))?;
    let resource_for_header =
        s2fmt::resource::Resource::parse(bytes.clone()).map_err(|e| format!("{compiled}: {e}"))?;
    let data_block = resource_for_header
        .block(FourCC::DATA)
        .ok_or_else(|| format!("{compiled}: no DATA block"))?;
    let header = s2tex::header::parse(resource_for_header.block_bytes(data_block))
        .map_err(|e| format!("{compiled}: failed to read header: {e}"))?;
    let total_mips = u32::from(header.num_mip_levels);
    let base_level = match lod_bias {
        Some(bias) => {
            let floor = ((total_mips.min(7) as f32) * (1.0 - bias)).floor().max(0.0);
            (floor as u32).min(total_mips.saturating_sub(1))
        }
        None => 0,
    };

    let mut blob = Vec::new();
    let mut mips_json = Vec::new();
    let mut format = "";
    for level in (base_level..total_mips).rev() {
        let raw = s2tex::decode_raw_mip_bytes(&bytes, level)
            .map_err(|e| format!("{compiled}: failed to read mip {level}: {e}"))?;
        // §3's fix: an unmapped compressed format goes to `missing` instead of a raw block file
        // labelled `"unknown"`.
        format = crate::lightmaps::compressed_format_name(raw.format).ok_or_else(|| {
            format!(
                "{compiled}: format {:?} has no known WebGL2 compressed-texture mapping, not writing a raw block file",
                raw.format
            )
        })?;
        mips_json.push(json!({
            "level": raw.mip_level,
            "faceWidth": raw.width,
            "faceHeight": raw.height,
            "byteOffset": blob.len(),
            "byteLength": raw.bytes.len(),
        }));
        blob.extend_from_slice(&raw.bytes);
    }

    Ok(SkyCube {
        bytes: blob,
        mips_json,
        format,
        width: u32::from(header.width),
        height: u32::from(header.height),
        total_mips,
        base_level,
    })
}

/// RGBM8 range for the sky face PNG fallback -- Mirage's own sky texel values top out around 1.02
/// (REPORT.md §4), so `8` (the same conservative range chosen for the irradiance fallback, §1's
/// histogram) leaves ample headroom for a brighter sky on other maps (a visible sun disc, say)
/// without a measured per-map histogram to size it from.
const SKY_RGBM_RANGE: f32 = 8.0;

/// §9: the 2D sky's PNG fallback -- RGBM8-encodes each of the 6 cube faces at the mip whose face
/// is `<= 256` (falling back to the smallest available, defensively box-downsampled further if the
/// mip chain is coarser than that -- same approach as the irradiance fallback, §7), Source axis
/// order `+X,-X,+Y,-Y,+Z,-Z`.
fn build_sky_fallback_faces(
    sources: &Sources,
    tex_path: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    const FACE_NAMES: [&str; 6] = ["px", "nx", "py", "ny", "pz", "nz"];

    let compiled = compiled_path(tex_path);
    let bytes = sources
        .read(&compiled)
        .ok_or_else(|| format!("{compiled}: not found"))?;
    let hdr = s2tex::decode_hdr_bytes(&bytes, 256)
        .map_err(|e| format!("{compiled}: failed to decode sky fallback: {e}"))?;
    if hdr.layers < 6 {
        return Err(format!(
            "{compiled}: sky fallback expected 6 cube faces, got {}",
            hdr.layers
        ));
    }

    let per_layer = (hdr.width as usize) * (hdr.height as usize) * 4;
    let mut layers: Vec<Vec<f32>> = (0..6)
        .map(|i| hdr.rgba[i * per_layer..(i + 1) * per_layer].to_vec())
        .collect();
    let mut w = hdr.width;
    let mut h = hdr.height;
    while w > 256 || h > 256 {
        let mut next = Vec::with_capacity(layers.len());
        let (mut nw, mut nh) = (w, h);
        for layer in &layers {
            let (down, dw, dh) = crate::lightmaps::box_downsample_2x2(layer, w, h);
            nw = dw;
            nh = dh;
            next.push(down);
        }
        layers = next;
        w = nw;
        h = nh;
    }

    let mut out = Vec::with_capacity(6);
    for (layer, name) in layers.iter().zip(FACE_NAMES) {
        let rgba8 = crate::lightmaps::encode_rgbm8(layer, SKY_RGBM_RANGE);
        let encoded = s2tex::encode_image(&rgba8, w, h, 90, true)
            .map_err(|e| format!("{compiled}: failed to encode sky face {name}: {e}"))?;
        out.push((format!("render_sky_face_{name}.png"), encoded.bytes));
    }
    Ok(out)
}

/// Everything this module produces: JSON fragments for `render.json` plus the raw byte buffers to
/// write alongside `render.glb` (`export.rs` handles atomically writing them).
#[derive(Default)]
pub struct EnvironmentResult {
    pub sky_json: Option<Value>,
    pub sky_cube_bytes: Option<Vec<u8>>,
    /// `render_sky_face_{px,nx,py,ny,pz,nz}.png` (§9).
    pub sky_fallback_files: Vec<(String, Vec<u8>)>,
    pub fog_json: Option<Value>,
    /// `render_fog_cube.bin` (§6): only set when the fog's own cubemap differs from the 2D sky's
    /// (`fog_json`'s `skyCubeFile` names whichever file the fog actually uses).
    pub fog_cube_bytes: Option<Vec<u8>>,
    pub post_json: Option<Value>,
    pub lut_bytes: Option<Vec<u8>>,
    pub missing: Vec<String>,
}

/// A `m_floatParams`/`m_toneMapParams`-shaped KV3 object's numeric fields, read generically since
/// they're a large flat struct of named floats -- reading each by name into a JSON object mirrors
/// what the KV3 dump already looks like, so `render.json` needs no separate translation table.
fn kv3_floats(v: &s2fmt::kv3::Value, keys: &[&str]) -> Value {
    let mut m = serde_json::Map::new();
    for &k in keys {
        if let Some(f) = v.get(k).and_then(s2fmt::kv3::Value::as_f32) {
            m.insert(k.to_string(), json!(f));
        }
    }
    Value::Object(m)
}

const TONEMAP_FIELDS: &[&str] = &[
    "m_flExposureBias",
    "m_flShoulderStrength",
    "m_flLinearStrength",
    "m_flLinearAngle",
    "m_flToeStrength",
    "m_flToeNum",
    "m_flToeDenom",
    "m_flWhitePoint",
    "m_flLuminanceSource",
    "m_flExposureBiasShadows",
    "m_flExposureBiasHighlights",
    "m_flMinShadowLum",
    "m_flMaxShadowLum",
    "m_flMinHighlightLum",
    "m_flMaxHighlightLum",
];

const BLOOM_FIELDS: &[&str] = &[
    "m_flBloomStrength",
    "m_flScreenBloomStrength",
    "m_flBlurBloomStrength",
    "m_flBloomThreshold",
    "m_flBloomThresholdWidth",
    "m_flSkyboxBloomStrength",
    "m_flBloomStartValue",
];

/// Decodes a `.vpost` resource's `DATA` block into the JSON `render.json` carries under
/// `postProcessing.vpost` (§6). `m_colorCorrectionVolumeData`'s blob is returned separately (the
/// caller writes it to `render_lut.bin` verbatim -- already RGBA8, x=R, per §6).
fn build_vpost(sources: &Sources, vpost_path: &str) -> Result<(Value, Option<Vec<u8>>), String> {
    let compiled = compiled_path(vpost_path);
    let resource = sources
        .resource(&compiled)
        .map_err(|e| format!("{compiled}: {e}"))?;
    let doc = resource
        .data_kv3()
        .map_err(|e| format!("{compiled}: {e}"))?;
    let root = &doc.root;

    let has_tonemap = root
        .get("m_bHasTonemapParams")
        .and_then(s2fmt::kv3::Value::as_bool)
        .unwrap_or(false);
    let tonemap = root
        .get("m_toneMapParams")
        .map(|v| kv3_floats(v, TONEMAP_FIELDS));

    let has_bloom = root
        .get("m_bHasBloomParams")
        .and_then(s2fmt::kv3::Value::as_bool)
        .unwrap_or(false);
    let bloom = root
        .get("m_bloomParams")
        .map(|v| kv3_floats(v, BLOOM_FIELDS));

    let has_color_correction = root
        .get("m_bHasColorCorrection")
        .and_then(s2fmt::kv3::Value::as_bool)
        .unwrap_or(false);
    let lut_dim = root
        .get("m_nColorCorrectionVolumeDim")
        .and_then(s2fmt::kv3::Value::as_i64)
        .unwrap_or(0);
    let lut_bytes = root
        .get("m_colorCorrectionVolumeData")
        .and_then(s2fmt::kv3::Value::as_blob)
        .map(<[u8]>::to_vec);

    const TONE_MAP_FORMULA: &str = "y=min(x*2.8, WP*2.8); f(y)=(y*(S*y+Ls*La)+T*Tn)/(y*(S*y+Ls)+T*Td) - Tn/Td; out = f(y)/f(WP*2.8) -> sRGB encode -> LUT(saturate(out)*(1-1/32)+0.5/32, bilinear, clamp) -> dither +-1/255 (post_processing.frag.slang:87-88; x = hdr * exposure * 2^m_flExposureBias, S=m_flShoulderStrength, Ls=m_flLinearStrength, La=m_flLinearAngle, T=m_flToeStrength, Tn=m_flToeNum, Td=m_flToeDenom, WP=m_flWhitePoint)";

    let json = json!({
        "source": vpost_path,
        "hasTonemapParams": has_tonemap,
        "toneMapParams": tonemap,
        "toneMapFormula": TONE_MAP_FORMULA,
        "hasBloomParams": has_bloom,
        "bloomParams": bloom,
        "bloomFormula": "weight = saturate((exposure*luma(0.3,0.59,0.11) - m_flBloomThreshold)/m_flBloomThresholdWidth)*m_flScreenBloomStrength; blur; s = saturate(b*exp/(b*exp+0.187)*1.035); color = color + s - color*s",
        "hasColorCorrection": has_color_correction,
        "colorCorrectionVolumeDim": lut_dim,
        "colorCorrectionVolumeFile": lut_bytes.as_ref().map(|_| "render_lut.bin"),
        "colorCorrectionVolumeFormat": "RGBA8, x=R, y=G, z=B; bilinear, clamp; indexed by sRGB-encoded colour",
    });
    Ok((json, lut_bytes))
}

/// Builds sky/fog/post-processing data from the already-loaded entity lumps (§4-§6). Every piece
/// is independent and best-effort: a missing/undecodable resource is noted in `missing` rather
/// than aborting the whole export.
pub fn build(sources: &Sources, lumps: &[EntityLump]) -> EnvironmentResult {
    let mut result = EnvironmentResult::default();

    // §4: 2D sky, from the main lump's `env_sky` (`WorldLoader.cs:659-691`, §5's fix: the last
    // enabled one, else the first).
    let mut sky_mips = 0usize;
    let mut sky_texture_path: Option<String> = None;
    let mut sky_cube_format: Option<&'static str> = None;
    let (mut sky_cube_width, mut sky_cube_height) = (0u32, 0u32);
    if let Some(env_sky) = pick_env_sky(lumps) {
        let skyname = env_sky.get_str("skyname").unwrap_or("");
        let tint_color = env_sky
            .get_vec3("tint_color")
            .unwrap_or([255.0, 255.0, 255.0]);
        let brightnessscale = num(env_sky, "brightnessscale", 1.0);
        let rotation = angles_to_matrix(env_sky.angles());
        // §4: "tint = tint_color/255 x brightnessscale, not linearized".
        let tint = [
            tint_color[0] / 255.0 * brightnessscale,
            tint_color[1] / 255.0 * brightnessscale,
            tint_color[2] / 255.0 * brightnessscale,
        ];

        if skyname.is_empty() {
            result.missing.push("env_sky: no skyname".to_string());
        } else {
            match load_raw_material(sources, skyname) {
                Ok(mat) => {
                    let sky_exposure_bias = mat.float("g_flBrightnessExposureBias").unwrap_or(0.0)
                        + mat.float("g_flRenderOnlyExposureBias").unwrap_or(0.0);
                    match mat.texture("g_tSkyTexture") {
                        Some(tex_path) => {
                            sky_texture_path = Some(tex_path.to_string());
                            match build_sky_cube(sources, tex_path, None) {
                                Ok(cube) => {
                                    sky_mips = cube.mips_json.len();
                                    sky_cube_format = Some(cube.format);
                                    sky_cube_width = cube.width;
                                    sky_cube_height = cube.height;
                                    let fallback = build_sky_fallback_faces(sources, tex_path);
                                    let fallback_json = match &fallback {
                                        Ok(faces) => Some(json!({
                                            "files": faces.iter().map(|f| f.0.clone()).collect::<Vec<_>>(),
                                            "faceOrder": ["+X","-X","+Y","-Y","+Z","-Z"],
                                            "encoding": "RGBM8",
                                            "rgbmRange": SKY_RGBM_RANGE,
                                            "decode": "linear = rgb*a*8; PNG is linear, not sRGB",
                                        })),
                                        Err(e) => {
                                            result.missing.push(e.clone());
                                            None
                                        }
                                    };
                                    result.sky_json = Some(json!({
                                        "file": "render_sky_cube.bin",
                                        "format": cube.format,
                                        "width": cube.width,
                                        "height": cube.height,
                                        "faceOrder": ["+X","-X","+Y","-Y","+Z","-Z"],
                                        "mips": cube.mips_json,
                                        "mipsOrder": "smallest first, matching the file's own on-disk mip order",
                                        "rotation": rotation,
                                        "rotationMeaning": "row-major 3x3 rotation from env_sky's angles: rotation[i] is row i, world = rotation*local; sample direction is rotation^-1 * viewDir (sky.frag.slang:40-61)",
                                        "tint": tint,
                                        "tintSpace": "not linearized -- multiply directly into the sampled cube colour",
                                        "exposureBias": sky_exposure_bias,
                                        "exposureFormula": "colour = textureCube(dir) * 2^exposureBias * tint, max(0)",
                                        "fallback": fallback_json,
                                    }));
                                    result.sky_cube_bytes = Some(cube.bytes);
                                    if let Ok(faces) = fallback {
                                        for (name, bytes) in faces {
                                            result.sky_fallback_files.push((name, bytes));
                                        }
                                    }
                                }
                                Err(e) => result.missing.push(e),
                            }
                        }
                        None => result.missing.push(format!("{skyname}: no g_tSkyTexture")),
                    }
                }
                Err(e) => result.missing.push(e),
            }
        }
    }

    // §5: cube fog, from `env_cubemap_fog` (`SceneCubemapFog.cs:46-88`, `fog.slang:34-50,93-127`,
    // `WorldLoader.cs:781-911`). Review fixes 6/10: `cubemapfogsource` picks where the cubemap
    // comes from -- 1 = the env_sky named by `cubemapfogskyentity` (its own skyname, angles,
    // brightnessscale); 2 = `cubemapfogskymaterial` with *this* entity's own angles and brightness
    // 1; 0 = `cubemapfogtexture` directly (no material, so no exposure bias).
    if let Some(fog) = find_entity(lumps, "env_cubemap_fog") {
        let fog_source = num(fog, "cubemapfogsource", 0.0).round() as i64;
        let fog_own_rotation = angles_to_matrix(fog.angles());

        struct FogSource {
            rotation: [[f32; 3]; 3],
            brightness_scale: f32,
            material: Option<String>,
            direct_texture: Option<String>,
        }

        let resolved: Result<FogSource, String> = match fog_source {
            1 => {
                let sky_ent_name = fog.get_str("cubemapfogskyentity").unwrap_or("");
                match find_entity_by_targetname(lumps, sky_ent_name) {
                    Some(sky_ent) => {
                        let scale = num(sky_ent, "brightnessscale", 1.0);
                        Ok(FogSource {
                            rotation: angles_to_matrix(sky_ent.angles()),
                            brightness_scale: if scale > 0.0 { scale } else { 1.0 },
                            material: sky_ent.get_str("skyname").map(str::to_string),
                            direct_texture: None,
                        })
                    }
                    None => Err(format!(
                        "env_cubemap_fog: cubemapfogskyentity '{sky_ent_name}' not found, fog disabled"
                    )),
                }
            }
            2 => Ok(FogSource {
                rotation: fog_own_rotation,
                brightness_scale: 1.0,
                material: fog.get_str("cubemapfogskymaterial").map(str::to_string),
                direct_texture: None,
            }),
            _ => Ok(FogSource {
                rotation: fog_own_rotation,
                brightness_scale: 1.0,
                material: None,
                direct_texture: fog.get_str("cubemapfogtexture").map(str::to_string),
            }),
        };

        match resolved {
            Err(e) => result.missing.push(e),
            Ok(src) => {
                let tex_and_bias: Result<(String, f32), String> =
                    if let Some(mat_path) = &src.material {
                        match load_raw_material(sources, mat_path) {
                            Ok(mat) => {
                                let bias = mat.float("g_flBrightnessExposureBias").unwrap_or(0.0)
                                    + mat.float("g_flRenderOnlyExposureBias").unwrap_or(0.0)
                                    + src.brightness_scale.log2();
                                match mat.texture("g_tSkyTexture") {
                                    Some(t) => Ok((t.to_string(), bias)),
                                    None => Err(format!("{mat_path}: no g_tSkyTexture")),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    } else if let Some(t) = &src.direct_texture {
                        Ok((t.clone(), 0.0))
                    } else {
                        Err("env_cubemap_fog: no cubemap texture source resolved".to_string())
                    };

                match tex_and_bias {
                    Err(e) => result.missing.push(e),
                    Ok((tex_path, exposure_bias)) => {
                        let lod_bias = num(fog, "cubemapfoglodbiase", 0.0);
                        // §6: reuse the 2D sky's own file when the fog samples the same texture
                        // (no duplicate bytes); otherwise write a second cube just for the fog.
                        // §2/§3's fix: either way, carry enough to decode it standalone -- format,
                        // face size, and its own per-mip layout (the reuse case just points back
                        // at `sky.mips`, identical bytes) -- and, for the fog's own separate cube,
                        // only the coarse tail of mips the fog lookup can ever reach.
                        struct FogCubeInfo {
                            file: &'static str,
                            mips: usize,
                            base_level: u32,
                            format: &'static str,
                            width: u32,
                            height: u32,
                            mip_layout: Value,
                            mips_order: &'static str,
                        }
                        let same_as_sky = sky_texture_path.as_deref() == Some(tex_path.as_str());
                        let cube = if same_as_sky {
                            Some(FogCubeInfo {
                                file: "render_sky_cube.bin",
                                mips: sky_mips,
                                base_level: 0,
                                format: sky_cube_format.unwrap_or(""),
                                width: sky_cube_width,
                                height: sky_cube_height,
                                mip_layout: json!("sky.mips"),
                                mips_order: "same as sky.mipsOrder (this file's mips are identical to the 2D sky cube's)",
                            })
                        } else {
                            match build_sky_cube(sources, &tex_path, Some(lod_bias)) {
                                Ok(cube) => {
                                    let info = FogCubeInfo {
                                        file: "render_fog_cube.bin",
                                        mips: cube.total_mips as usize,
                                        base_level: cube.base_level,
                                        format: cube.format,
                                        width: cube.width,
                                        height: cube.height,
                                        mip_layout: json!(cube.mips_json),
                                        mips_order: "smallest first, matching the file's own on-disk mip order; only levels >= skyCubeBaseLevel are present, since the fog lookup never reaches a finer one (§3)",
                                    };
                                    result.fog_cube_bytes = Some(cube.bytes);
                                    Some(info)
                                }
                                Err(e) => {
                                    result.missing.push(e);
                                    None
                                }
                            }
                        };

                        if let Some(cube) = cube {
                            result.fog_json = Some(json!({
                                "start": num(fog, "cubemapfogstartdistance", 0.0),
                                "end": num(fog, "cubemapfogenddistance", 0.0),
                                "falloffExp": num(fog, "cubemapfogfalloffexponent", 1.0),
                                "hStart": num(fog, "cubemapfogheightstart", 0.0),
                                "hEnd": num(fog, "cubemapfogheightend", 0.0),
                                "hExp": num(fog, "cubemapfogheightexponent", 1.0),
                                "maxOpacity": num(fog, "cubemapfogmaxopacity", 1.0),
                                "lodBias": lod_bias,
                                "useHeight": boolish(fog, "cubemapheightfog", false),
                                "skyCubeFile": cube.file,
                                "skyCubeFileMeaning": "either render_sky_cube.bin (this fog samples the same cubemap as the 2D sky) or render_fog_cube.bin (a different cubemap written just for the fog)",
                                "skyCubeFormat": cube.format,
                                "skyCubeWidth": cube.width,
                                "skyCubeHeight": cube.height,
                                "skyCubeMipLayout": cube.mip_layout,
                                "skyCubeMipLayoutMeaning": "same shape as sky.mips (level/faceWidth/faceHeight/byteOffset/byteLength) when skyCubeFile is render_fog_cube.bin; the string \"sky.mips\" when it's render_sky_cube.bin, since the layout is that array",
                                "skyCubeMipsOrder": cube.mips_order,
                                "skyCubeBaseLevel": cube.base_level,
                                "skyCubeBaseLevelMeaning": "index of the finest mip actually present in skyCubeFile -- gl.texParameteri(TEXTURE_CUBE_MAP, TEXTURE_BASE_LEVEL, skyCubeBaseLevel) before sampling, since levels below it were never written (render_fog_cube.bin only; always 0 for render_sky_cube.bin)",
                                "skyRotation": src.rotation,
                                "skyRotationMeaning": "row-major 3x3: rotation[i] is row i, world = rotation*local (same convention as sky.rotation)",
                                "skyExposureBias": exposure_bias,
                                "skyBrightnessScale": src.brightness_scale,
                                "skyMips": cube.mips,
                                "formula": "d=length((P-Cam).xy); a=pow(max(d/(end-start)-start/(end-start),1e-4),falloffExp); h=(useHeight && hEnd>hStart) ? pow(max(P.z/(hStart-hEnd)+1-hStart/(hStart-hEnd),1e-4),hExp) : 1; blend=saturate(a)*saturate(h); lod=saturate(1-blend*lodBias)*min(7,skyMips); fogColor=textureLod(skyCube, rotation^-1 * normalize(P-Cam), lod) * 2^skyExposureBias; color=mix(color,fogColor,saturate(blend)*maxOpacity) -- applied only if |P-Cam|^2 > start^2 or P.z > hStart (SceneCubemapFog.cs:63-88, WorldLoader.cs:800-817, fog.slang:93-127); skyExposureBias already includes log2(skyBrightnessScale) (WorldLoader.cs:886), so it is not added again here",
                            }));
                        }
                    }
                }
            }
        }
    }

    // §6: post-processing, from the `master` `post_processing_volume`'s `.vpost`.
    let volume = lumps
        .iter()
        .flat_map(|l| l.entities.iter())
        .filter(|e| e.classname().eq_ignore_ascii_case("post_processing_volume"))
        .find(|e| e.get_bool("master") == Some(true))
        .or_else(|| find_entity(lumps, "post_processing_volume"));
    if let Some(volume) = volume {
        let exposure = json!({
            "enabled": boolish(volume, "enableexposure", true),
            "min": num(volume, "minexposure", 0.8),
            "max": num(volume, "maxexposure", 1.0),
            "speedUp": num(volume, "exposurespeedup", 1.0),
            "speedDown": num(volume, "exposurespeeddown", 1.0),
            "compensation": "m_flExposureBias in vpost.toneMapParams (no separate entity-level compensation key found)",
            "formula": "exposure = clamp(0.2691/avgLuma, min, max), smoothed at speedUp/speedDown (PostProcessRenderer.cs:339-460); x = hdr * exposure * 2^m_flExposureBias",
        });
        match volume.get_str("postprocessing") {
            Some(vpost_path) if !vpost_path.is_empty() => match build_vpost(sources, vpost_path) {
                Ok((vpost_json, lut_bytes)) => {
                    result.post_json = Some(json!({ "exposure": exposure, "vpost": vpost_json }));
                    result.lut_bytes = lut_bytes;
                }
                Err(e) => result.missing.push(e),
            },
            _ => result.missing.push(format!(
                "{}: post_processing_volume has no postprocessing path",
                volume.targetname().unwrap_or("<unnamed>")
            )),
        }
    }

    result
}
