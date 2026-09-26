// F3b-2 orchestration (`s6f3b2_lighting_shader.md`): loads the lighting textures, builds one
// game-lighting `ShaderMaterial` per (glTF material, lighting path) found in `render.glb`, and runs
// the HDR render -> 2D sky -> tonemap/LUT pipeline every frame. `scene3d.js` owns the scene graph,
// camera and everything else - this module only replaces "how a frame gets drawn" when the user
// picks "game" lighting over F3b-1's plain `MeshStandardMaterial` ("simple").
//
// Both material sets are built once, up front, and kept alive side by side on each mesh
// (`mesh.userData.simpleMaterial`/`gameMaterial`) - switching the toggle just swaps `mesh.material`
// and is instant, at the cost of the lighting textures always being resident once `render.glb` has
// loaded (a deliberate simplification: `s6f3b2_lighting_shader.md` §7 only asks that the choice be
// remembered and cheap to flip, not that unused-mode resources be freed).

import * as THREE from "three";
import {
  detectCompressionSupport,
  canUseRawFormat,
  loadCompressedLightmap,
  loadCompressedSkyCube,
  loadFallbackTexture,
  loadFallbackSkyCube,
  loadLut,
} from "./lightingTextures.js?v=1";
import { buildWorldMaterial } from "./lightingShader.js?v=1";
import { createSkyPass, inverseRotation } from "./lightingSky.js?v=1";
import { createSkyboxPass } from "./lightingSkybox.js?v=1";
import { createHdrTarget, createPostPass } from "./lightingPost.js?v=1";
import { renderAssetUrl } from "./api.js?v=1";
import { decodeHemiOctConstant, srgbToLinear, buildConstantTexture } from "./materialTextures.js?v=1";

// `s6f3a9_effects.md` review fix item 3: the `THREE.Object3D.layers` bit a mesh whose material has
// `F_DEPTH_FEATHER==1` is placed on, via `.layers.set` (not `.enable` - it moves OFF layer 0
// entirely, so `renderFrame`'s own pass 1 below can exclude it by disabling just this layer on the
// camera). `renderFrame` draws the main scene up to three times when any such mesh is present: pass
// 1 with this layer (and `OVERLAY_LAYER`) disabled (opaque + every ordinary translucent, whose
// resulting depth gets copied out for feathering), pass 2 with *only* this layer enabled (just the
// feather-flagged effects meshes, `autoClear: false`), pass 3 with only `OVERLAY_LAYER` (review fix
// item 2 - see its own doc comment). `scene3d.js` enables both layers on the camera by default (in
// addition to its own layer 0) so a mesh set to either one stays visible whenever that camera
// renders in one single pass instead (a map with no feather/overlay split at all, "simple" mode,
// raycasting, ...).
export const FEATHER_LAYER = 1;
// review fix item 2: "always on top" scene helpers (target beacon, area-draft vertex dots/rubber-
// band line, hover ring - scene3d.js's own `depthTest:false` objects) - the ONLY things in
// scene3d.js that use `depthTest:false` at all (checked by grep, `s6f3a9_effects.md` review round
// 3's receipt lists each one and why every other S6k helper - area prisms, origin ring, the
// selected lineup's capsule/aim line/trajectory/bounce marks, the collision overlay - stays off
// this layer instead: they're all normally depth-tested, meant to be occluded by real geometry,
// not "always visible"). Before the opaque/feather two-pass split (review fix item 3) these worked
// correctly in a single `renderer.render` call purely from `depthTest:false` + a high `renderOrder`
// (998/999) sorting them last; splitting the feather-flagged effects meshes into their own later
// pass draws over them again since they're already-composited pixels by then, with no `renderOrder`
// left to save them - moving them to their own layer and giving them pass 3 (after feather, still
// `autoClear: false`) restores "always on top" regardless of how many passes the scene needs.
export const OVERLAY_LAYER = 2;

function findLightmap(lighting, needle) {
  return lighting.lightmaps.find((f) => f.file.includes(needle));
}

function findFallback(lighting, needle) {
  return lighting.lightmapFallbacks.find((f) => f.file.includes(needle));
}

// ---- resource loading (textures + the shared per-map uniform values) --------------------------

async function loadLightingResources(renderer, map, renderJson) {
  const support = detectCompressionSupport(renderer);
  const lighting = renderJson.lighting;
  const gpuBytes = { irradiance: 0, shadows: 0, sky: 0, fog: 0, lut: 0 };

  const irrEntry = findLightmap(lighting, "irradiance");
  let irradianceMap;
  let irradianceIsRgbm = false;
  let irradianceRgbmRange = 8;
  if (irrEntry && canUseRawFormat(support, irrEntry.format)) {
    irradianceMap = await loadCompressedLightmap(renderAssetUrl(map, irrEntry.file), irrEntry);
    gpuBytes.irradiance = irrEntry.byteLength;
  } else {
    const fb = findFallback(lighting, "irradiance");
    irradianceMap = await loadFallbackTexture(renderAssetUrl(map, fb.file));
    irradianceIsRgbm = true;
    irradianceRgbmRange = fb.rgbmRange ?? 8;
    gpuBytes.irradiance = fb.width * fb.height * 4;
  }

  const shadowEntry = findLightmap(lighting, "direct_light_shadows");
  let shadowMap;
  if (shadowEntry && canUseRawFormat(support, shadowEntry.format)) {
    shadowMap = await loadCompressedLightmap(renderAssetUrl(map, shadowEntry.file), shadowEntry);
    gpuBytes.shadows = shadowEntry.byteLength;
  } else {
    const fb = findFallback(lighting, "shadow");
    shadowMap = await loadFallbackTexture(renderAssetUrl(map, fb.file));
    gpuBytes.shadows = fb.width * fb.height * 4;
  }

  const sky = renderJson.sky;
  let skyCube;
  let skyIsRgbm = false;
  let skyRgbmRange = 8;
  if (sky.file && canUseRawFormat(support, sky.format)) {
    skyCube = await loadCompressedSkyCube(renderAssetUrl(map, sky.file), sky);
    gpuBytes.sky = sky.mips.reduce((a, m) => a + m.byteLength, 0);
  } else {
    const urls = sky.fallback.files.map((f) => renderAssetUrl(map, f));
    skyCube = await loadFallbackSkyCube(urls);
    skyIsRgbm = true;
    skyRgbmRange = sky.fallback.rgbmRange ?? 8;
    // The fallback faces' pixel size isn't in render.json (only the compressed PNG file exists on
    // disk) - read it back off the texture `loadFallbackSkyCube` just decoded.
    gpuBytes.sky = 6 * skyCube.image[0].width * skyCube.image[0].height * 4;
  }

  // Fog cube (`s6f3b2_lighting_shader.md` §4, REPORT.md §6): `render_fog_cube.bin` when the map
  // baked a separate cube for the fog, or the string "sky.mips" (not an array) in
  // `fog.skyCubeMipLayout` when `fog.skyCubeFile` is `render_sky_cube.bin` - the fog then samples
  // the exact same bytes as the 2D sky, already loaded above as `skyCube`. `fogCube`/`fogLodOffset`
  // stay `null`/`0` (fall back to reusing `skyCube`) whenever there is no fog, the fog reuses the
  // sky's own cube, or its own format isn't uploadable (no PNG fallback exists for this file).
  const fog = renderJson.fog;
  let fogCube = null;
  let fogLodOffset = 0;
  if (fog && fog.skyCubeFile === "render_fog_cube.bin" && canUseRawFormat(support, fog.skyCubeFormat)) {
    fogCube = await loadCompressedSkyCube(renderAssetUrl(map, fog.skyCubeFile), {
      format: fog.skyCubeFormat,
      mips: fog.skyCubeMipLayout,
    });
    fogLodOffset = fog.skyCubeBaseLevel ?? 0;
    gpuBytes.fog = fog.skyCubeMipLayout.reduce((a, m) => a + m.byteLength, 0);
  }

  const lutInfo = renderJson.postProcessing.vpost;
  const lut = await loadLut(renderAssetUrl(map, lutInfo.colorCorrectionVolumeFile), lutInfo.colorCorrectionVolumeDim);
  gpuBytes.lut = lutInfo.colorCorrectionVolumeDim ** 3 * 4;

  return {
    support,
    irradianceMap,
    irradianceIsRgbm,
    irradianceRgbmRange,
    shadowMap,
    skyCube,
    skyIsRgbm,
    skyRgbmRange,
    fogCube,
    fogLodOffset,
    lut,
    lutDim: lutInfo.colorCorrectionVolumeDim,
    gpuBytes,
  };
}

function buildSharedUniforms(renderJson, res) {
  const sun = renderJson.sun;
  const fog = renderJson.fog; // may be null - not every map bakes cube fog (e.g. de_poseidon).
  const sky = renderJson.sky;
  return {
    uSunToSun: { value: new THREE.Vector3(...sun.toSun) },
    uSunColorLinear: { value: new THREE.Vector3(...sun.colorLinear) },
    uLpvScale: { value: renderJson.lighting.lpvScale },
    uIrradianceMap: { value: res.irradianceMap },
    uShadowMap: { value: res.shadowMap },
    uIrradianceRgbmRange: { value: res.irradianceRgbmRange },
    uSkyRgbmRange: { value: res.skyRgbmRange },
    uFogCube: { value: res.fogCube ?? res.skyCube },
    uFogLodOffset: { value: res.fogCube ? res.fogLodOffset : 0 },
    // Neutral values below when `fog` is null - never read in a shader (`buildRecipe` only sets
    // `fogEnabled` when `renderJson.fog != null`), but built unconditionally here regardless.
    uFogSkyRotationT: { value: fog ? inverseRotation(fog.skyRotation) : new THREE.Matrix3() },
    uFogStart: { value: fog?.start ?? 0 },
    uFogEnd: { value: fog?.end ?? 1 },
    uFogFalloffExp: { value: fog?.falloffExp ?? 1 },
    uFogHStart: { value: fog?.hStart ?? 0 },
    uFogHEnd: { value: fog?.hEnd ?? 0 },
    uFogHExp: { value: fog?.hExp ?? 1 },
    uFogUseHeight: { value: fog?.useHeight ? 1 : 0 },
    uFogMaxOpacity: { value: fog?.maxOpacity ?? 0 },
    uFogLodBias: { value: fog?.lodBias ?? 0 },
    uFogSkyExposureBias: { value: fog?.skyExposureBias ?? 0 },
    uFogSkyMips: { value: fog?.skyMips ?? 0 },
    // `s6f3a9_effects.md`: csgo_effects's live g_flTime-equivalent (mask panning), shared unchanged
    // by every effects material (like `uSunToSun`) and updated once per frame in `renderFrame`.
    uTime: { value: 0 },
    // review fix item 3: F_DEPTH_FEATHER's own inputs - `uEffectsSceneDepth` is filled in once
    // `featherDepthTarget` is built (lazily, `applyToGltf`); the other three are refreshed every
    // frame in `renderFrame`, right before the feather-layer pass reads them.
    uEffectsSceneDepth: { value: null },
    uEffectsProjInverse: { value: new THREE.Matrix4() },
    uEffectsViewInverse: { value: new THREE.Matrix4() },
    uEffectsResolution: { value: new THREE.Vector2(1, 1) },
    _bakedShadowChannel: sun.bakedShadowChannel,
    _skyTint: sky.tint,
    _skyExposureBias: sky.exposureBias,
    _skyRotationRows: sky.rotation,
  };
}

// `s6f3b2_lighting_shader.md` §2 "перенести из текущих патчей F3b-1 onBeforeCompile" - blend-mode
// inference below reads back what `GLTFLoader`'s own standard-material builder already decided
// (`transparent`/`alphaTest`) instead of re-parsing the glTF JSON a second time.
function alphaModeOf(material) {
  if (material.transparent) return "BLEND";
  if (material.alphaTest > 0) return "MASK";
  return "OPAQUE";
}

// One material texture slot's resolved source (`s6f3a6_native_tex.md` change item 7's schema):
// either `extras.<key>Texture` (an index into render.json's own `textures[]`, loaded through
// `texLoader`) or `extras.<key>Constant` (a 4x4 source folded to one RGBA8 value at export time,
// `native_texture::Loaded::Constant`) plus its codec/colorSpace siblings. `neither` when the
// material has no data for this slot at all.
function resolveSlot(extras, key) {
  const textureIndex = extras[`${key}Texture`];
  if (textureIndex != null) return { textureIndex, constant: null };
  const raw = extras[`${key}Constant`];
  if (raw) {
    return {
      textureIndex: null,
      constant: { raw, codec: extras[`${key}ConstantCodec`], colorSpace: extras[`${key}ConstantColorSpace`] },
    };
  }
  return { textureIndex: null, constant: null };
}

async function buildRecipe(mesh, lightingType, renderJson, shared, texLoader, opts) {
  const material = mesh.material;
  const extras = material.userData ?? {};
  // A single texture's fetch/decode failure (bad network, unsupported format) degrades just that
  // slot to "missing" instead of failing the whole material - and everything downstream of it in
  // the same map - the way an unhandled rejection through `buildGameMaterials`'s `await` would.
  const getTex = (index) =>
    index != null && texLoader
      ? texLoader.get(index).catch((e) => {
          console.error(`[cs2mod] texture load failed (render.json textures[${index}]):`, e);
          return null;
        })
      : null;

  const tintIsSrgb = /^srgb/i.test(renderJson?.materialExtras?.tintColorSpace ?? "");

  // Base color: a real texture, or (rare) a 4x4-constant folded straight into the factor - the
  // constant's raw bytes are in the same pre-decode representation a real sample would be
  // (`native_texture::Loaded`'s own doc comment), so an sRGB-role constant needs the same
  // srgb->linear step the GPU would otherwise do for a real sRGB texture.
  let baseColorFactor = [material.color.r, material.color.g, material.color.b, material.opacity];
  const baseColor = resolveSlot(extras, "baseColor");
  if (baseColor.constant) {
    const [r, g, b, a] = baseColor.constant.raw;
    const isSrgb = baseColor.constant.colorSpace === "srgb";
    const lin = isSrgb
      ? [srgbToLinear(r / 255), srgbToLinear(g / 255), srgbToLinear(b / 255)]
      : [r / 255, g / 255, b / 255];
    baseColorFactor = [baseColorFactor[0] * lin[0], baseColorFactor[1] * lin[1], baseColorFactor[2] * lin[2], baseColorFactor[3] * (a / 255)];
  }

  // Tint mask: a real texture keeps its own per-pixel HAS_TINT path; a constant mask amount is
  // instead folded analytically into baseColorFactor (mix(albedo, albedo*tint, k) == albedo *
  // mix(1, tint, k), and k doesn't vary per-fragment when it's a constant) - no shader work needed.
  const tintMask = resolveSlot(extras, "tintMask");
  let tintMaskMap = null;
  if (tintMask.textureIndex != null && extras.tint) {
    tintMaskMap = await getTex(tintMask.textureIndex);
  } else if (tintMask.constant && extras.tint) {
    const k = tintMask.constant.raw[0] / 255;
    const tint = extras.tint; // already linear (materialExtras.tintColorSpace / material.rs).
    baseColorFactor[0] *= 1 + k * (tint[0] - 1);
    baseColorFactor[1] *= 1 + k * (tint[1] - 1);
    baseColorFactor[2] *= 1 + k * (tint[2] - 1);
  }

  const albedoMapPromise = getTex(baseColor.textureIndex);
  const normal = resolveSlot(extras, "normal");
  const normalMapPromise = getTex(normal.textureIndex);
  const ao = resolveSlot(extras, "ao");
  const aoMapPromise = getTex(ao.textureIndex);
  const metalness = resolveSlot(extras, "metalness");
  const metalnessMapPromise = getTex(metalness.textureIndex);
  const selfIllum = resolveSlot(extras, "selfIllum");
  const selfIllumMapPromise = getTex(selfIllum.textureIndex);

  const layers = extras.layers ?? null;
  const layer2Color = layers ? resolveSlot(layers, "layer2Color") : { textureIndex: null, constant: null };
  const blendMod = layers ? resolveSlot(layers, "blendModulation") : { textureIndex: null, constant: null };
  // `s6f3a7_env_materials.md` change item 5: layer 2's raw normal texel (mixed with layer 1's
  // before decode, same as colour) -- exported since F3a-6 but unused until now.
  const layer2Normal = layers ? resolveSlot(layers, "layer2Normal") : { textureIndex: null, constant: null };
  const layer2MapPromise = getTex(layer2Color.textureIndex);
  const blendModMapPromise = getTex(blendMod.textureIndex);
  const layer2NormalMapPromise = getTex(layer2Normal.textureIndex);

  // `s6f3a7_env_materials.md` change item 1: csgo_environment(_blend)'s own height/roughness-
  // remap/AO-levels/metalness inputs (`extras.env1`/`extras.envLayer2`).
  const env1Extras = extras.env1 ?? null;
  const env1Height = env1Extras ? resolveSlot(env1Extras, "height1") : { textureIndex: null, constant: null };
  // Review fix item 4: a 4x4-constant height source is a real texture, not "missing" - build a
  // 1x1 `DataTexture` from it (`buildConstantTexture`) instead of letting `env1`/`envLayer2` be
  // silently dropped whenever `g_tHeight{1,2}` folded to a constant.
  const env1HeightMapPromise = env1Height.textureIndex != null ? getTex(env1Height.textureIndex) : env1Height.constant ? buildConstantTexture(env1Height.constant.raw) : null;

  const envLayer2Extras = extras.envLayer2 ?? null;
  const envColor2 = envLayer2Extras ? resolveSlot(envLayer2Extras, "color2") : { textureIndex: null, constant: null };
  const envNormal2 = envLayer2Extras ? resolveSlot(envLayer2Extras, "normal2") : { textureIndex: null, constant: null };
  const envHeight2 = envLayer2Extras ? resolveSlot(envLayer2Extras, "height2") : { textureIndex: null, constant: null };
  const envColor2MapPromise = getTex(envColor2.textureIndex);
  const envNormal2MapPromise = getTex(envNormal2.textureIndex);
  const envHeight2MapPromise = envHeight2.textureIndex != null ? getTex(envHeight2.textureIndex) : envHeight2.constant ? buildConstantTexture(envHeight2.constant.raw) : null;

  const [
    albedoMap,
    normalMap,
    aoMap,
    metalnessMap,
    selfIllumMap,
    layer2Map,
    blendModMap,
    layer2NormalMap,
    env1HeightMap,
    envColor2Map,
    envNormal2Map,
    envHeight2Map,
  ] = await Promise.all([
    albedoMapPromise,
    normalMapPromise,
    aoMapPromise,
    metalnessMapPromise,
    selfIllumMapPromise,
    layer2MapPromise,
    blendModMapPromise,
    layer2NormalMapPromise,
    env1HeightMapPromise,
    envColor2MapPromise,
    envNormal2MapPromise,
    envHeight2MapPromise,
  ]);

  // A flat (materials/default/) normal's constant: only its baked-in roughness matters (change
  // item 3) - no perturbation, so no texture/uniform for the direction at all. review fix item 8:
  // the packed-roughness-in-blue trick is a HemiOct-only convention (`s2tex::transform::
  // decode_hemi_oct`) - a dxt5nm/reconstructZ constant (dev/reflectivity_* on Train/Nuke/Ancient,
  // effects/black on Anubis) has no such channel, so it must fall back to the default 1.0 instead
  // of running the HemiOct math on bytes it doesn't apply to (previously landed near roughness 0,
  // i.e. a mirror).
  let roughnessFactor = 1;
  if (normalMap) {
    // real per-texel decode happens in the shader; the factor is only the fallback below.
  } else if (normal.constant && normal.constant.codec === "hemiOct") {
    roughnessFactor = decodeHemiOctConstant(normal.constant.raw).roughness;
  }

  let aoFactor = 1;
  if (!aoMap && ao.constant) aoFactor = ao.constant.raw[0] / 255;

  let metalnessFactor = extras.metalnessValue ?? 0;
  if (!metalnessMap && metalness.constant) metalnessFactor = metalness.constant.raw[1] / 255;

  let layer2ConstantColor = null;
  if (layers && !layer2Map && layer2Color.constant) {
    const [r, g, b] = layer2Color.constant.raw;
    const isSrgb = layer2Color.constant.colorSpace === "srgb";
    layer2ConstantColor = new THREE.Vector3(
      ...(isSrgb ? [srgbToLinear(r / 255), srgbToLinear(g / 255), srgbToLinear(b / 255)] : [r / 255, g / 255, b / 255]),
    );
  }
  let blendModConstant = null;
  if (layers && !blendModMap && blendMod.constant) {
    const [r, g, b] = blendMod.constant.raw;
    blendModConstant = new THREE.Vector3(r / 255, g / 255, b / 255);
  }
  const hasLayers = !!layers && (!!layer2Map || !!layer2ConstantColor);

  // `s6f3a7_env_materials.md` change items 1/3: `env1` is `null` unless the height texture
  // actually loaded (matches `export.rs`'s own "only emit extras.env1 when it loads" rule, review
  // fix item 4's constant-texture fallback included); `envLayer2` additionally requires
  // colour/normal/height all present (`EnvLayer2`'s own doc comment -- every csgo_environment_
  // blend material surveyed has all three, in texture or constant form).
  let env1 = null;
  if (env1HeightMap) {
    env1 = {
      heightMap: env1HeightMap,
      roughnessContrast: env1Extras.roughnessContrast1 ?? 1,
      roughnessBrightness: env1Extras.roughnessBrightness1 ?? 1,
      normalContrast: env1Extras.normalContrast1 ?? 1, // review fix item 7
      aoLevels: new THREE.Vector3(...(env1Extras.aoLevels1 ?? [0, 0.5, 1])),
      metalnessEnabled: env1Extras.metalnessEnabled1 !== false,
      // review fix item 1: per-layer colour-correction matrices (`RenderMaterial.cs:671-744`,
      // `crates/s2render/src/color_correct.rs`), gated by tintMask1 = remap(height1.g).
      colorAdjust: env1Extras.colorAdjust1 ? new THREE.Matrix4().fromArray(env1Extras.colorAdjust1) : null,
      adjust: env1Extras.adjust1 ? new THREE.Matrix4().fromArray(env1Extras.adjust1) : null,
      colorCorrectionMode: env1Extras.colorCorrectionMode1 ?? 0,
      tintMaskContrast: env1Extras.tintMaskContrast1 ?? 1,
      tintMaskBrightness: env1Extras.tintMaskBrightness1 ?? 1,
      // verify_c7df fix item 1: layer 1's own UV transform (`csgo_environment.vert.slang:165-171`),
      // applied to every layer-1 sample the same way layer 2's already is.
      uvScale: env1Extras.uvScale1 ?? [1, 1],
      uvOffset: env1Extras.uvOffset1 ?? [0, 0],
      uvRotation: env1Extras.uvRotation1 ?? 0,
    };
  }
  let envLayer2 = null;
  if (env1 && envColor2Map && envNormal2Map && envHeight2Map) {
    envLayer2 = {
      colorMap: envColor2Map,
      color2ManualSrgb: envColor2Map?.userData?.manualSrgb === true,
      normalMap: envNormal2Map,
      heightMap: envHeight2Map,
      roughnessContrast: envLayer2Extras.roughnessContrast2 ?? 1,
      roughnessBrightness: envLayer2Extras.roughnessBrightness2 ?? 1,
      normalContrast: envLayer2Extras.normalContrast2 ?? 1, // review fix item 7
      aoLevels: new THREE.Vector3(...(envLayer2Extras.aoLevels2 ?? [0, 0.5, 1])),
      metalnessEnabled: envLayer2Extras.metalnessEnabled2 !== false,
      heightScale1: envLayer2Extras.heightScale1 ?? 1,
      heightZeroPoint1: envLayer2Extras.heightZeroPoint1 ?? 0.5,
      heightScale2: envLayer2Extras.heightScale2 ?? 1,
      heightZeroPoint2: envLayer2Extras.heightZeroPoint2 ?? 0.5,
      blendSoftness2: envLayer2Extras.blendSoftness2 ?? 0.01,
      // review fix item 2: layer 2's own UV transform (`csgo_environment.vert.slang:175-180`),
      // applied to every layer-2 sample (height2/color2/normal2/metalness2).
      uvScale: envLayer2Extras.uvScale2 ?? [1, 1],
      uvOffset: envLayer2Extras.uvOffset2 ?? [0, 0],
      uvRotation: envLayer2Extras.uvRotation2 ?? 0,
      // F_BLEND_BY_FACING_DIRECTION_2: present only on materials that set it.
      facingDir: envLayer2Extras.facingDirection2 ?? null,
      facingMinMax: envLayer2Extras.facingMinMax2 ?? null,
      // review fix item 1.
      colorAdjust: envLayer2Extras.colorAdjust2 ? new THREE.Matrix4().fromArray(envLayer2Extras.colorAdjust2) : null,
      adjust: envLayer2Extras.adjust2 ? new THREE.Matrix4().fromArray(envLayer2Extras.adjust2) : null,
      colorCorrectionMode: envLayer2Extras.colorCorrectionMode2 ?? 0,
      tintMaskContrast: envLayer2Extras.tintMaskContrast2 ?? 1,
      tintMaskBrightness: envLayer2Extras.tintMaskBrightness2 ?? 1,
    };
  }

  // review fix item 1: `mask.r` only (`complex.frag.slang:546`'s own `.r`), sRGB-linearised the
  // same way a real texture sample would be (GPU-native decode, or the manual fallback the shader
  // runs when `selfIllumManualSrgb`).
  let selfIllumConstantMask = null;
  if (selfIllum.constant && !selfIllumMap) {
    const r = selfIllum.constant.raw[0] / 255;
    selfIllumConstantMask = selfIllum.constant.colorSpace === "srgb" ? srgbToLinear(r) : r;
  }
  const hasSelfIllum = !!selfIllumMap || selfIllumConstantMask != null;

  // `s6f3a9_effects.md`: csgo_effects's own masks/fresnel/feather/fade opacity formula.
  const fxExtras = extras.effects ?? null;
  let effects = null;
  if (fxExtras) {
    const m1 = resolveSlot(fxExtras, "mask1");
    const m2 = resolveSlot(fxExtras, "mask2");
    const m3 = resolveSlot(fxExtras, "mask3");
    const [mask1Map, mask2Map, mask3Map] = await Promise.all([getTex(m1.textureIndex), getTex(m2.textureIndex), getTex(m3.textureIndex)]);
    // A mask that folded to a 4x4 constant is uniform everywhere it's sampled, so scale/pan-speed
    // (which only ever change WHERE it's sampled) can't change its value - fold it into
    // opacityScale up front instead of building a texture/sampler for it.
    let constantOpacity = 1;
    if (!mask1Map && m1.constant) constantOpacity *= m1.constant.raw[0] / 255;
    if (!mask2Map && m2.constant) constantOpacity *= m2.constant.raw[0] / 255;
    if (!mask3Map && m3.constant) constantOpacity *= m3.constant.raw[0] / 255;
    effects = {
      mask1Map,
      mask1Scale: fxExtras.mask1Scale ?? [1, 1],
      mask1PanSpeed: fxExtras.mask1PanSpeed ?? [0, 0],
      mask2Map,
      mask2Scale: fxExtras.mask2Scale ?? [1, 1],
      mask2PanSpeed: fxExtras.mask2PanSpeed ?? [0, 0],
      mask3Map,
      mask3Scale: fxExtras.mask3Scale ?? [1, 1],
      mask3PanSpeed: fxExtras.mask3PanSpeed ?? [0, 0],
      opacityScale: (fxExtras.opacityScale ?? 1) * constantOpacity,
      colorBoost: fxExtras.colorBoost ?? 1,
      fadeDistance: fxExtras.fadeDistance ?? 1,
      fadeFalloff: fxExtras.fadeFalloff ?? 1,
      fadeMin: fxExtras.fadeMin ?? 0,
      fadeMax: fxExtras.fadeMax ?? 1,
      fresnelExponent: fxExtras.fresnelExponent ?? 0.001,
      fresnelFalloff: fxExtras.fresnelFalloff ?? 1,
      fresnelMin: fxExtras.fresnelMin ?? 0,
      fresnelMax: fxExtras.fresnelMax ?? 1,
      // review fix item 3: real F_DEPTH_FEATHER in the main scene (the two-pass/layer scheme in
      // `createLightingPipeline`'s `renderFrame`, `FEATHER_LAYER`'s own doc comment) - the 3D
      // skybox still can't (`opts.disableEffectsDepthFeather`, `lightingSkybox.js`'s own call):
      // it draws in one single pass with its own camera/scene, before the main scene's opaque
      // depth even exists yet, and clears depth right after - there is nothing to feather against.
      depthFeather: fxExtras.depthFeather === true && opts?.disableEffectsDepthFeather !== true,
      featherDistance: fxExtras.featherDistance ?? 1,
      featherFalloff: fxExtras.featherFalloff ?? 1,
      flipBackface: material.side === THREE.DoubleSide && fxExtras.dontFlipBackfaceNormals !== true,
      // review fix item 2: F_ADDITIVE_BLEND (RenderMaterial.cs:354-357,892) - sun_glow_001/
      // sun_disc_glow_001 (Mirage's 3D skybox), steam_001 (Inferno) - blends (SrcAlpha, One), not
      // the (SrcAlpha, InvSrcAlpha) every other csgo_effects material uses.
      additive: fxExtras.blendMode === "additive",
    };
    if (effects.depthFeather) shared._depthFeatherNeeded = true;
  }

  return {
    lightingType,
    alphaMode: alphaModeOf(material),
    alphaCutoff: material.alphaTest || 0.5,
    doubleSided: material.side === THREE.DoubleSide,
    mod2x: extras.blendMode === "mod2x",
    albedoMap,
    albedoManualSrgb: albedoMap?.userData?.manualSrgb === true,
    baseColorFactor,
    normalMap,
    // review fix item 8: which per-texel decode the shader must run on a *real* normal map -
    // HemiOct is the common case, but a handful of dev/reflectivity_*/effects textures use
    // dxt5nm or a plain Z-reconstruction instead (`native_texture.rs`'s `codec`, read back off
    // the loaded texture's own `userData`, set by `materialTextures.js`).
    normalCodec: normalMap?.userData?.codec ?? "hemiOct",
    aoMap,
    aoChannel: extras.aoChannel ?? "r",
    aoFactor,
    roughnessFactor,
    metalnessMap,
    metalnessChannel: extras.metalnessChannel ?? "g",
    metalnessFactor,
    tintColor: tintMaskMap
      ? tintIsSrgb
        ? new THREE.Color().setRGB(extras.tint[0], extras.tint[1], extras.tint[2], THREE.SRGBColorSpace)
        : new THREE.Color(extras.tint[0], extras.tint[1], extras.tint[2])
      : null,
    baseColorAlphaMeaning: extras.baseColorAlphaMeaning,
    tintMaskMap,
    effects,
    hasLayers,
    layer2Map,
    layer2ManualSrgb: layer2Map?.userData?.manualSrgb === true,
    layer2ConstantColor,
    blendModMap,
    blendModConstant,
    layer2NormalMap,
    env1,
    envLayer2,
    hasSelfIllum,
    selfIllumMap,
    selfIllumManualSrgb: selfIllumMap?.userData?.manualSrgb === true,
    selfIllumConstantMask,
    selfIllumScale: extras.selfIllumScale ?? 1,
    selfIllumBrightness: extras.selfIllumBrightness ?? 0,
    selfIllumTint: extras.selfIllumTint ? new THREE.Vector3(...extras.selfIllumTint) : new THREE.Vector3(1, 1, 1),
    selfIllumAlbedoFactor: extras.selfIllumAlbedoFactor ?? 0,
    fogEnabled: extras.fogEnabled === true && renderJson.fog != null,
    skyIsRgbm: shared._skyIsRgbm,
    irradianceIsRgbm: shared._irradianceIsRgbm,
    bakedShadowChannel: shared._bakedShadowChannel,
    noSpecularAtFullRoughness: extras.noSpecularAtFullRoughness === true,
    renderSpecular: renderJson.sun.renderSpecular !== false,
    shared,
  };
}

// Sets `.renderOrder` from `extras.overlayOrder` (`s6f3b2_lighting_shader.md` §2, render.json
// "absent means 0") on every mesh under a node that carries it - a node with more than one glTF
// primitive becomes a `Group` of meshes, none of which individually inherit the group's own
// `renderOrder` from three, so this is applied to each descendant mesh directly.
function applyOverlayOrder(root) {
  root.traverse((node) => {
    const order = node.userData?.overlayOrder;
    if (order == null) return;
    node.traverse((child) => {
      if (child.isMesh) child.renderOrder = order;
    });
  });
}

/**
 * Builds every game-lighting material `render.glb` needs and stores it at
 * `mesh.userData.gameMaterial` (`mesh.userData.simpleMaterial` is filled in by `scene3d.js` before
 * calling this, from whatever `MeshStandardMaterial` F3b-1's own pass already built).
 */
/** Exported for `lightingSkybox.js`'s own reuse (§5, `s6f3b2_lighting_shader.md`): the 3D skybox's
 * `render_sky.glb` needs the exact same per-(material,lightingType) game-material build, just fed
 * its own `render.json` (`renderJson.skybox.report`) and its own `shared` uniform bag. */
export async function buildGameMaterials(gltf, renderJson, shared, materialPool, texLoader, opts) {
  const cache = new Map(); // `${materialIndex}:${lightingType}` -> THREE.ShaderMaterial
  const meshes = [];
  gltf.scene.traverse((o) => {
    if (o.isMesh) meshes.push(o);
  });
  applyOverlayOrder(gltf.scene);
  for (const mesh of meshes) {
    const lightingType = mesh.geometry.userData?.lighting ?? "unlit";
    const materialIndex = gltf.parser.associations.get(mesh.material)?.materials;
    const key = `${materialIndex}:${lightingType}`;
    let material = cache.get(key);
    if (!material) {
      const recipe = await buildRecipe(mesh, lightingType, renderJson, shared, texLoader, opts);
      const built = buildWorldMaterial(recipe);
      material = built.material;
      // review fix item 3: stashed once per distinct material (not per mesh) so every mesh below
      // sharing this cache entry - including one built on a later loop iteration, a cache hit -
      // gets the same `FEATHER_LAYER` placement.
      material.userData.effectsDepthFeatherLayer = recipe.effects?.depthFeather === true;
      materialPool.set(built.cacheKey, true);
      cache.set(key, material);
    }
    mesh.userData.simpleMaterial = mesh.material;
    mesh.userData.gameMaterial = material;
    // `FEATHER_LAYER`'s own doc comment: `.set` (not `.enable`) - this mesh moves OFF layer 0
    // entirely, so the opaque/feather two-pass split in `renderFrame` can actually exclude it from
    // the first pass by disabling this layer on the camera. `scene3d.js` enables this same layer
    // on the camera by default (in addition to its own layer 0) so the mesh stays visible whenever
    // that camera renders in one single pass instead (simple mode, raycasting, ...).
    if (material.userData.effectsDepthFeatherLayer) mesh.layers.set(FEATHER_LAYER);
  }
}

/** `renderJson` must have `.lighting`/`.sky`/`.postProcessing.vpost` - callers check this first. */
export function hasGameLightingData(renderJson) {
  return !!(renderJson?.lighting?.lightmaps && renderJson?.sky?.file != null && renderJson?.postProcessing?.vpost?.hasTonemapParams);
}

export async function createLightingPipeline(renderer, map, renderJson, texLoader) {
  const res = await loadLightingResources(renderer, map, renderJson);
  const shared = buildSharedUniforms(renderJson, res);
  shared._irradianceIsRgbm = res.irradianceIsRgbm;
  shared._skyIsRgbm = res.skyIsRgbm;

  const skyPass = createSkyPass({
    skyCube: res.skyCube,
    isRgbm: res.skyIsRgbm,
    rgbmRange: res.skyRgbmRange,
    tint: shared._skyTint,
    exposureBias: shared._skyExposureBias,
    rotationRows: shared._skyRotationRows,
  });
  const postPass = createPostPass(
    renderJson.postProcessing.vpost.toneMapParams,
    renderJson.postProcessing.exposure,
    res.lut,
    res.lutDim,
  );

  // `s6f3b2_lighting_shader.md` §5: `null` when this map has no 3D skybox yet, or its own load
  // failed - never fatal to the rest of the pipeline ("нет данных -- пропустить без ошибок").
  let skyboxPass = null;
  try {
    skyboxPass = await createSkyboxPass(renderer, map, renderJson, shared, res.support);
  } catch (e) {
    console.error("[cs2mod] 3D skybox failed to load, skipping it:", e);
  }

  let hdrTarget = createHdrTarget(renderer, 1, 1);
  // review fix item 3: F_DEPTH_FEATHER's own opaque-scene depth, resolved out of `hdrTarget`'s own
  // (possibly multisampled) depth via `renderer.copyTextureToTexture` (a GL `blitFramebuffer`,
  // which needs a matching-size destination framebuffer of its own - not a bare texture) right
  // after the opaque/ordinary-translucent pass, in `renderFrame`. Built lazily, only once a map
  // actually loads an effects material with F_DEPTH_FEATHER==1 (`shared._depthFeatherNeeded`, set
  // by `buildRecipe`), so every other map pays nothing extra.
  let featherDepthTarget = null;
  const materialPool = new Map(); // distinct customProgramCacheKey values actually handed out.
  const gameMaterials = new Map(); // all THREE.ShaderMaterial instances, for dispose().

  // Native material textures (`s6f3a6_native_tex.md` change item 3): `texLoader` is shared with
  // "simple" mode's own material patches (`scene3d.js`'s `applyMaterialExtras`, built once per
  // view and passed in here) - a texture referenced by several materials, or by both lighting
  // modes, is fetched/decoded/uploaded once, not once per mode. Owned by the caller (`scene3d.js`),
  // not disposed by this module's own `dispose()` below.
  if (texLoader?.missingExtensionsMessage) {
    console.warn(`[cs2mod] ${texLoader.missingExtensionsMessage}`);
  }

  return {
    support: res.support,
    gpuBytes: res.gpuBytes,
    materialTextureSupport: texLoader?.support,
    missingExtensionsMessage: texLoader?.missingExtensionsMessage,

    async applyToGltf(gltf) {
      const before = materialPool.size;
      await buildGameMaterials(gltf, renderJson, shared, materialPool, texLoader);
      gltf.scene.traverse((o) => {
        if (o.isMesh && o.userData.gameMaterial) gameMaterials.set(o.userData.gameMaterial.uuid, o.userData.gameMaterial);
      });
      if (shared._depthFeatherNeeded && !featherDepthTarget) {
        const w = Math.max(1, hdrTarget.width);
        const h = Math.max(1, hdrTarget.height);
        featherDepthTarget = new THREE.WebGLRenderTarget(w, h, { depthBuffer: true, depthTexture: new THREE.DepthTexture(w, h) });
        shared.uEffectsSceneDepth.value = featherDepthTarget.depthTexture;
      }
      return materialPool.size - before;
    },

    programCount() {
      return materialPool.size + (skyboxPass?.programCount() ?? 0);
    },

    materialTextureBytes() {
      return (texLoader?.bytesLoaded() ?? 0) + (skyboxPass?.materialTextureBytes() ?? 0);
    },

    materialTextureCount() {
      return (texLoader?.textureCount() ?? 0) + (skyboxPass?.materialTextureCount() ?? 0);
    },

    resize(width, height) {
      const w = Math.max(1, width);
      const h = Math.max(1, height);
      hdrTarget.setSize(w, h);
      // review fix item 3: must stay pixel-identical to `hdrTarget` - depth/stencil blits
      // (`renderer.copyTextureToTexture`'s own `blitFramebuffer` path) require matching source/
      // destination dimensions.
      featherDepthTarget?.setSize(w, h);
      shared.uEffectsResolution.value.set(w, h);
    },

    // Draws `scene` with `camera` into the HDR target, then the sky (cleared first, normal depth
    // test keeps it behind anything opaque already/later drawn - see `lightingSky.js`), then this
    // pipeline's own tonemap/LUT/dither pass to `options.outputTarget` (`null`, the default, means
    // the canvas - `renderer`'s current target). None of these three materials include three's own
    // `colorspace_fragment`/`tonemapping_fragment` chunks (every fragment shader here is hand-
    // written, ending in a literal `gl_FragColor`/`fragColor` assignment) - so
    // `renderer.outputColorSpace`/`toneMapping` never touch their output either way, and are left
    // exactly as `scene3d.js` set them (`SRGBColorSpace`, `NoToneMapping`) - conveniently already
    // the right tag for the post pass's sRGB-encoded bytes.
    //
    // `options` (review fix item 3, the parallel-previews branch's own offscreen captures):
    //  - `timeSeconds`: `scene3d.js`'s own `THREE.Clock.elapsedTime` - csgo_effects's mask panning
    //    (`g_flTime` in the reference shader, `s6f3a9_effects.md`) reads this through `uTime`,
    //    shared unchanged by every effects material (like `uSunToSun`); omitted leaves it at
    //    whatever it last was (a capture doesn't need to advance the animation).
    //  - `outputTarget`: where the post pass writes - `null` (default) is the canvas, otherwise a
    //    `THREE.WebGLRenderTarget` the caller wants the graded LDR frame in instead.
    //  - `overrideHdrTarget`: the HDR target the main scene actually draws into and the post pass
    //    reads back from, in place of this pipeline's own persistent `hdrTarget` (sized for the
    //    live canvas) - a preview capture's own off-canvas-resolution target. F_DEPTH_FEATHER only
    //    runs when this target already carries its own `depthTexture` (`canFeather` below); when it
    //    does, a temporary depth-copy destination is allocated for this one call and disposed at
    //    the end, since a preview capture is occasional, unlike `hdrTarget`'s own persistent
    //    `featherDepthTarget` (built once, reused every frame of the live animation loop).
    renderFrame(scene, camera, { timeSeconds, outputTarget = null, overrideHdrTarget = null } = {}) {
      if (timeSeconds != null) shared.uTime.value = timeSeconds;
      const target = overrideHdrTarget ?? hdrTarget;
      const prevAutoClear = renderer.autoClear;
      // `WebGLBackground.render()` force-clears whenever `scene.background` is a solid `Color`
      // (three.module.js's own `forceClear = true` for that case) - ignoring `autoClear = false`
      // and wiping out the sky pass's output from the same target. F3b-1's flat clear color is
      // exactly what this pipeline's own sky pass replaces, so it's suppressed here and restored
      // for whatever renders next (the "simple" path, or a future frame after toggling back).
      const prevBackground = scene.background;
      scene.background = null;

      renderer.setRenderTarget(target);
      renderer.autoClear = true;
      skyPass.render(renderer, camera);
      renderer.autoClear = false;
      // §5: the 3D skybox draws next (its own depth, against the 2D sky's depth=1 background),
      // then depth is cleared before the main scene - so main geometry always wins the depth test
      // against the skybox regardless of the two cameras' unrelated depth ranges (F3b-2 §5's own
      // "рендер скайбокса -> очистка глубины -> основная сцена").
      if (skyboxPass) {
        skyboxPass.render(renderer, camera);
        renderer.clearDepth();
      }
      // review fix item 3: an override target the caller didn't attach a depthTexture to (it
      // doesn't care about F_DEPTH_FEATHER) skips feathering rather than failing.
      const canFeather = !!featherDepthTarget && !!target.depthTexture;
      let tempFeatherDepth = null;
      // review fix items 2/3: F_DEPTH_FEATHER's own opaque/feather/overlay three-pass split
      // (`FEATHER_LAYER`/`OVERLAY_LAYER`'s own doc comments) - pass 1 (opaque + ordinary
      // translucents, both special layers disabled) draws and its resulting depth is blitted out;
      // pass 2 (only FEATHER_LAYER, autoClear false) draws the feather-flagged effects meshes on
      // top, reading that copy; pass 3 (only OVERLAY_LAYER) draws the "always on top" scene helpers
      // (target beacon, hover ring, area-draft tool) over THAT, so pass 2 never ends up on top of
      // them. Trade-off: feather-flagged effects meshes always draw after every OTHER translucent
      // object in the scene (not correctly depth-sorted among them, since they're now a separate
      // draw call) - acceptable since they're atmospheric dust/steam cards that rarely share screen
      // space with other translucent geometry (glass, ...).
      if (canFeather) {
        const featherDest = overrideHdrTarget
          ? (tempFeatherDepth = new THREE.WebGLRenderTarget(target.width, target.height, {
              depthBuffer: true,
              depthTexture: new THREE.DepthTexture(target.width, target.height),
            }))
          : featherDepthTarget;
        shared.uEffectsSceneDepth.value = featherDest.depthTexture;
        shared.uEffectsResolution.value.set(target.width, target.height);
        const prevCameraLayers = camera.layers.mask;
        camera.layers.disable(FEATHER_LAYER);
        camera.layers.disable(OVERLAY_LAYER);
        renderer.render(scene, camera);
        // `renderer.copyTextureToTexture`'s depth path blits between the two textures' OWN
        // framebuffers (`properties.get(texture).__renderTarget`); that back-reference is only
        // set up the first time a render target is actually used ("rendered to" - `target` just
        // was, by the render() call above, but `featherDest` never is, only ever copied INTO) -
        // `initRenderTarget` is three's own documented fix for exactly this case, and is re-checked
        // every frame (a cheap no-op once already set up) since `resize()`'s own `setSize()`
        // disposes and re-allocates `featherDepthTarget`'s GPU storage, invalidating it again.
        renderer.initRenderTarget(featherDest);
        renderer.copyTextureToTexture(target.depthTexture, featherDest.depthTexture);
        // review fix item 1: `copyTextureToTexture`'s depth path unbinds both READ/DRAW
        // framebuffers once it's done (three.module.js's own blit call, ~19494-19495) without
        // telling the renderer's own `_currentRenderTarget` bookkeeping - the next `render()` call
        // trusts that bookkeeping and skips rebinding, so its draws silently land on whatever's
        // still bound at the raw GL level (the canvas) instead of `target`. `setRenderTarget`
        // forces the rebind (this was the bug behind the feather-flagged dust cards - Mirage's mid
        // haze - going invisible in game mode: the corrected pixels landed on the canvas, then the
        // post pass's own `renderer.setRenderTarget(outputTarget)` below overwrote them anyway).
        renderer.setRenderTarget(target);
        shared.uEffectsProjInverse.value.copy(camera.projectionMatrixInverse);
        shared.uEffectsViewInverse.value.copy(camera.matrixWorld);
        camera.layers.set(FEATHER_LAYER);
        renderer.render(scene, camera);
        camera.layers.set(OVERLAY_LAYER);
        renderer.render(scene, camera);
        camera.layers.mask = prevCameraLayers;
      } else {
        renderer.render(scene, camera);
      }
      scene.background = prevBackground;

      renderer.setRenderTarget(outputTarget);
      postPass.render(renderer, camera, target);

      renderer.autoClear = prevAutoClear;
      tempFeatherDepth?.dispose();
    },

    dispose() {
      res.irradianceMap.dispose();
      res.shadowMap.dispose();
      res.skyCube.dispose();
      res.fogCube?.dispose(); // only set when it's a texture distinct from `res.skyCube`.
      res.lut.dispose();
      skyPass.dispose();
      skyboxPass?.dispose();
      postPass.dispose();
      hdrTarget.dispose();
      featherDepthTarget?.dispose();
      for (const m of gameMaterials.values()) m.dispose();
      gameMaterials.clear();
      // `texLoader` is owned by the caller (`scene3d.js`'s `materialTexLoaderReady`, shared with
      // "simple" mode) - not disposed here.
    },
  };
}
