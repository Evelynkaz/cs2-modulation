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
import { createHdrTarget, createPostPass } from "./lightingPost.js?v=1";
import { renderAssetUrl } from "./api.js?v=1";
import { decodeHemiOctConstant, srgbToLinear } from "./materialTextures.js?v=1";

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

async function buildRecipe(mesh, lightingType, renderJson, shared, texLoader) {
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
  const layer2MapPromise = getTex(layer2Color.textureIndex);
  const blendModMapPromise = getTex(blendMod.textureIndex);

  const [albedoMap, normalMap, aoMap, metalnessMap, selfIllumMap, layer2Map, blendModMap] = await Promise.all([
    albedoMapPromise,
    normalMapPromise,
    aoMapPromise,
    metalnessMapPromise,
    selfIllumMapPromise,
    layer2MapPromise,
    blendModMapPromise,
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

  // review fix item 1: `mask.r` only (`complex.frag.slang:546`'s own `.r`), sRGB-linearised the
  // same way a real texture sample would be (GPU-native decode, or the manual fallback the shader
  // runs when `selfIllumManualSrgb`).
  let selfIllumConstantMask = null;
  if (selfIllum.constant && !selfIllumMap) {
    const r = selfIllum.constant.raw[0] / 255;
    selfIllumConstantMask = selfIllum.constant.colorSpace === "srgb" ? srgbToLinear(r) : r;
  }
  const hasSelfIllum = !!selfIllumMap || selfIllumConstantMask != null;

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
    hasLayers,
    layer2Map,
    layer2ManualSrgb: layer2Map?.userData?.manualSrgb === true,
    layer2ConstantColor,
    blendModMap,
    blendModConstant,
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
async function buildGameMaterials(gltf, renderJson, shared, materialPool, texLoader) {
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
      const recipe = await buildRecipe(mesh, lightingType, renderJson, shared, texLoader);
      const built = buildWorldMaterial(recipe);
      material = built.material;
      materialPool.set(built.cacheKey, true);
      cache.set(key, material);
    }
    mesh.userData.simpleMaterial = mesh.material;
    mesh.userData.gameMaterial = material;
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

  let hdrTarget = createHdrTarget(renderer, 1, 1);
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
      return materialPool.size - before;
    },

    programCount() {
      return materialPool.size;
    },

    materialTextureBytes() {
      return texLoader?.bytesLoaded() ?? 0;
    },

    materialTextureCount() {
      return texLoader?.textureCount() ?? 0;
    },

    resize(width, height) {
      hdrTarget.setSize(Math.max(1, width), Math.max(1, height));
    },

    // Draws `scene` with `camera` into the HDR target, then the sky (cleared first, normal depth
    // test keeps it behind anything opaque already/later drawn - see `lightingSky.js`), then this
    // pipeline's own tonemap/LUT/dither pass straight to the canvas (`renderer`'s current target).
    // None of these three materials include three's own `colorspace_fragment`/`tonemapping_fragment`
    // chunks (every fragment shader here is hand-written, ending in a literal `gl_FragColor`/
    // `fragColor` assignment) - so `renderer.outputColorSpace`/`toneMapping` never touch their
    // output either way, and are left exactly as `scene3d.js` set them (`SRGBColorSpace`,
    // `NoToneMapping`) - conveniently already the right tag for the post pass's sRGB-encoded bytes.
    renderFrame(scene, camera) {
      const prevAutoClear = renderer.autoClear;
      // `WebGLBackground.render()` force-clears whenever `scene.background` is a solid `Color`
      // (three.module.js's own `forceClear = true` for that case) - ignoring `autoClear = false`
      // and wiping out the sky pass's output from the same target. F3b-1's flat clear color is
      // exactly what this pipeline's own sky pass replaces, so it's suppressed here and restored
      // for whatever renders next (the "simple" path, or a future frame after toggling back).
      const prevBackground = scene.background;
      scene.background = null;

      renderer.setRenderTarget(hdrTarget);
      renderer.autoClear = true;
      skyPass.render(renderer, camera);
      renderer.autoClear = false;
      renderer.render(scene, camera);
      scene.background = prevBackground;

      renderer.setRenderTarget(null);
      postPass.render(renderer, camera, hdrTarget);

      renderer.autoClear = prevAutoClear;
    },

    dispose() {
      res.irradianceMap.dispose();
      res.shadowMap.dispose();
      res.skyCube.dispose();
      res.fogCube?.dispose(); // only set when it's a texture distinct from `res.skyCube`.
      res.lut.dispose();
      skyPass.dispose();
      postPass.dispose();
      hdrTarget.dispose();
      for (const m of gameMaterials.values()) m.dispose();
      gameMaterials.clear();
      // `texLoader` is owned by the caller (`scene3d.js`'s `materialTexLoaderReady`, shared with
      // "simple" mode) - not disposed here.
    },
  };
}
