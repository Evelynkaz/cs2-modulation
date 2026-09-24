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

async function buildRecipe(gltf, mesh, lightingType, renderJson, shared) {
  const material = mesh.material;
  const extras = material.userData ?? {};
  const getTex = (idx) => (idx != null ? gltf.parser.getDependency("texture", idx) : null);

  const tintIsSrgb = /^srgb/i.test(renderJson?.materialExtras?.tintColorSpace ?? "");
  const [aoMap, roughnessMap, metalnessMap, tintMaskMap, layer2Map, blendModMap] = await Promise.all([
    getTex(extras.aoTexture),
    getTex(extras.roughnessTexture),
    getTex(extras.metalnessTexture),
    getTex(extras.tintMask),
    getTex(extras.layers?.layer2ColorTexture),
    getTex(extras.layers?.blendModulationTexture),
  ]);
  if (aoMap) aoMap.colorSpace = THREE.NoColorSpace;
  if (roughnessMap) roughnessMap.colorSpace = THREE.NoColorSpace;
  if (metalnessMap) metalnessMap.colorSpace = THREE.NoColorSpace;
  if (tintMaskMap) tintMaskMap.colorSpace = THREE.NoColorSpace;
  if (layer2Map) layer2Map.colorSpace = THREE.SRGBColorSpace;
  if (blendModMap) blendModMap.colorSpace = THREE.NoColorSpace;

  return {
    lightingType,
    alphaMode: alphaModeOf(material),
    alphaCutoff: material.alphaTest || 0.5,
    doubleSided: material.side === THREE.DoubleSide,
    mod2x: extras.blendMode === "mod2x",
    map: material.map,
    baseColorFactor: [material.color.r, material.color.g, material.color.b, material.opacity],
    normalMap: material.normalMap ?? null,
    normalScale: material.normalScale ?? null,
    aoMap,
    aoChannel: extras.aoChannel ?? "r",
    roughnessMap,
    roughnessChannel: extras.roughnessChannel ?? "r",
    roughnessFactor: extras.roughnessTexture != null ? 1 : (material.roughness ?? 1),
    metalnessMap,
    metalnessChannel: extras.metalnessChannel ?? "g",
    metalnessFactor: extras.metalnessValue ?? material.metalness ?? 0,
    tintColor: tintMaskMap
      ? tintIsSrgb
        ? new THREE.Color().setRGB(extras.tint[0], extras.tint[1], extras.tint[2], THREE.SRGBColorSpace)
        : new THREE.Color(extras.tint[0], extras.tint[1], extras.tint[2])
      : null,
    tintMaskMap,
    layer2Map,
    blendModMap,
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
async function buildGameMaterials(gltf, renderJson, shared, materialPool) {
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
      const recipe = await buildRecipe(gltf, mesh, lightingType, renderJson, shared);
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

export async function createLightingPipeline(renderer, map, renderJson) {
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

  return {
    support: res.support,
    gpuBytes: res.gpuBytes,

    async applyToGltf(gltf) {
      const before = materialPool.size;
      await buildGameMaterials(gltf, renderJson, shared, materialPool);
      gltf.scene.traverse((o) => {
        if (o.isMesh && o.userData.gameMaterial) gameMaterials.set(o.userData.gameMaterial.uuid, o.userData.gameMaterial);
      });
      return materialPool.size - before;
    },

    programCount() {
      return materialPool.size;
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
    },
  };
}
