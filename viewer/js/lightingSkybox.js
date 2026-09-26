// The 3D skybox (`s6f3b2_lighting_shader.md` §5, REPORT.md §5): a second, independent glTF scene
// (`render_sky.glb`, its own lightmaps/probes/materials - `render.json`'s own `skybox` block,
// `s6f3a4_lighting.md` stage 2) rendered with a second camera placed at
// `skybox.origin + (mainCamera.position - skybox.reference.origin)/skybox.scale` (same orientation
// as the main camera, near/far divided by `scale` too - REPORT.md §5's own three.js note), then
// composited behind the main scene: `lighting.js`'s `renderFrame` calls this module's own
// `render()` right after the 2D sky pass and clears depth before the main scene draws (F3b-2 §5:
// "рендер скайбокса -> очистка глубины -> основная сцена"). `null` (nothing to do) when
// `renderJson.skybox` is absent - not every map has a 3D skybox, and F3a-4 stage 2 may not have run
// yet.
//
// review_f3a8 fix item 2: `skybox.reference.origin` (the main map's own `skybox_reference` entity
// origin - VRF's own `p_main = (p_sky - origin)*scale + reference.origin`, WorldLoader.cs:
// 1536-1548) is not always zero (ar_shoots, ar_baggage, de_overpass) even though its rotation/scale
// are identity on every map surveyed - composed into the camera position below, `ref` throughout.
//
// Fog on skybox surfaces reuses the OUTER map's own cube-fog texture/rotation/exposure completely
// unchanged, only rescaling the distance/height uniforms (`buildSkyboxSharedUniforms`): since
// `p_main = (p_sky - skybox.origin) * skybox.scale + ref` and the skybox camera sits at exactly
// `skybox.origin + (mainCameraPos - ref)/scale`, `p_main - mainCameraPos` always equals
// `scale * (p_sky - skyboxCameraPos)` regardless of `ref` (it cancels in the subtraction) - so
// feeding `lightingShader.js`'s *unmodified* fog GLSL the skybox's own local position/camera, but
// with `start`/`end` divided by `scale` and `hStart`/`hEnd` replaced by `(hStart - ref.z)/scale +
// skybox.origin.z` (see `buildSkyboxSharedUniforms`'s own derivation), reproduces the exact same
// blend factor the main-world formula would with zero shader changes.

import * as THREE from "three";
import { GLTFLoader } from "../lib/three/examples/jsm/loaders/GLTFLoader.js";
import { MeshoptDecoder } from "../lib/three/examples/jsm/libs/meshopt_decoder.module.js";
import { renderAssetUrl } from "./api.js?v=1";
import { canUseRawFormat, loadCompressedLightmap, loadFallbackTexture } from "./lightingTextures.js?v=1";
import { createMaterialTextureLoader } from "./materialTextures.js?v=1";
import { buildGameMaterials } from "./lighting.js?v=1";
import { VERTICAL_FOV_DEG } from "./camera.js?v=1";

function findLightmap(lighting, needle) {
  return lighting.lightmaps.find((f) => f.file.includes(needle));
}
function findFallback(lighting, needle) {
  return lighting.lightmapFallbacks.find((f) => f.file.includes(needle));
}

// Same two lightmaps `lighting.js`'s own `loadLightingResources` reads for the main map - the
// skybox's own copies, at its own `uvScale` (already baked into `render_sky.glb`'s own TEXCOORD_1),
// so this can't just reuse the outer map's already-loaded textures.
async function loadSkyboxLightmaps(map, support, lighting) {
  const irrEntry = findLightmap(lighting, "irradiance");
  let irradianceMap;
  let irradianceIsRgbm = false;
  let irradianceRgbmRange = 8;
  if (irrEntry && canUseRawFormat(support, irrEntry.format)) {
    irradianceMap = await loadCompressedLightmap(renderAssetUrl(map, irrEntry.file), irrEntry);
  } else {
    const fb = findFallback(lighting, "irradiance");
    irradianceMap = await loadFallbackTexture(renderAssetUrl(map, fb.file));
    irradianceIsRgbm = true;
    irradianceRgbmRange = fb.rgbmRange ?? 8;
  }

  const shadowEntry = findLightmap(lighting, "direct_light_shadows");
  let shadowMap;
  if (shadowEntry && canUseRawFormat(support, shadowEntry.format)) {
    shadowMap = await loadCompressedLightmap(renderAssetUrl(map, shadowEntry.file), shadowEntry);
  } else {
    const fb = findFallback(lighting, "shadow");
    shadowMap = await loadFallbackTexture(renderAssetUrl(map, fb.file));
  }

  return { irradianceMap, irradianceIsRgbm, irradianceRgbmRange, shadowMap };
}

// See this file's own top comment for the fog rescale derivation.
function buildSkyboxSharedUniforms(outerRenderJson, outerShared, skyboxReport, res, skybox) {
  const fog = outerRenderJson.fog;
  const scale = skybox.scale;
  const originZ = skybox.origin[2];
  // review_f3a8 fix item 2: reference.origin.z (not always 0 - ar_shoots, ar_baggage, de_overpass).
  const refZ = skybox.reference?.origin?.[2] ?? 0;
  return {
    uSunToSun: outerShared.uSunToSun,
    uSunColorLinear: outerShared.uSunColorLinear,
    uLpvScale: outerShared.uLpvScale,
    // `s6f3a9_effects.md`: shared, unmodified, with the main pipeline's own live clock (cloud/dust
    // card mask panning). F_DEPTH_FEATHER stays feather=1 here specifically (the main scene now
    // implements it for real, review fix item 3) - this skybox scene has no opaque depth of its
    // own to feather against, see `createSkyboxPass`'s own `buildGameMaterials` call.
    uTime: outerShared.uTime,
    uIrradianceMap: { value: res.irradianceMap },
    uShadowMap: { value: res.shadowMap },
    uIrradianceRgbmRange: { value: res.irradianceRgbmRange },
    uSkyRgbmRange: outerShared.uSkyRgbmRange,
    uFogCube: outerShared.uFogCube,
    uFogLodOffset: outerShared.uFogLodOffset,
    uFogSkyRotationT: outerShared.uFogSkyRotationT,
    // The only 4 fog uniforms this module computes itself - fresh objects, never the outer
    // pipeline's own (which stay at the un-rescaled main-world values other materials still need).
    uFogStart: { value: fog ? fog.start / scale : 0 },
    uFogEnd: { value: fog ? fog.end / scale : 1 },
    uFogHStart: { value: fog ? (fog.hStart - refZ) / scale + originZ : 0 },
    uFogHEnd: { value: fog ? (fog.hEnd - refZ) / scale + originZ : 0 },
    uFogFalloffExp: outerShared.uFogFalloffExp,
    uFogHExp: outerShared.uFogHExp,
    uFogUseHeight: outerShared.uFogUseHeight,
    uFogMaxOpacity: outerShared.uFogMaxOpacity,
    uFogLodBias: outerShared.uFogLodBias,
    uFogSkyExposureBias: outerShared.uFogSkyExposureBias,
    uFogSkyMips: outerShared.uFogSkyMips,
    _bakedShadowChannel: skyboxReport.sun?.bakedShadowChannel ?? null,
    _irradianceIsRgbm: res.irradianceIsRgbm,
    _skyIsRgbm: outerShared._skyIsRgbm,
  };
}

async function fetchGlb(url) {
  const res = await fetch(url, { cache: "default" });
  if (!res.ok) throw new Error(`HTTP ${res.status} for ${url}`);
  return res.arrayBuffer();
}

/**
 * Loads and prepares the 3D skybox named by `outerRenderJson.skybox` (`null` if this map has none -
 * F3b-2 §5's "нет данных -- пропустить без ошибок"). `outerShared` is `lighting.js`'s own per-map
 * uniform bag (already built for the main scene) - reused here for the sun and the 2D-sky-reused
 * fog cube; only the skybox's own lightmaps and the 4 rescaled fog distance/height uniforms are
 * built fresh.
 */
export async function createSkyboxPass(renderer, map, outerRenderJson, outerShared, support) {
  const skybox = outerRenderJson.skybox;
  if (!skybox?.file) return null;
  const skyboxReport = skybox.report;

  const [res, buffer] = await Promise.all([
    loadSkyboxLightmaps(map, support, skyboxReport.lighting),
    fetchGlb(renderAssetUrl(map, skybox.file)),
  ]);
  const texLoader = createMaterialTextureLoader(map, skyboxReport, renderer);
  const shared = buildSkyboxSharedUniforms(outerRenderJson, outerShared, skyboxReport, res, skybox);
  // `buildRecipe`'s own `renderJson.fog != null` gate (`lighting.js`) must see the OUTER map's fog
  // presence (this skybox's own nested report has `fog: null` - F3a-4 stage 2 never writes its own
  // 2D sky/fog/post-processing files, see render.json's own `skyFilesNote`); every fog uniform a
  // material actually reads comes from `shared` above, not from this object's own `.fog` contents.
  const patchedReport = { ...skyboxReport, fog: outerRenderJson.fog };

  const gltfLoader = new GLTFLoader();
  gltfLoader.setMeshoptDecoder(MeshoptDecoder);
  const gltf = await new Promise((resolve, reject) => {
    gltfLoader.parse(buffer, "", resolve, reject);
  });

  const materialPool = new Map();
  // review fix item 3 (s6f3a9_effects.md): the 3D skybox draws in one single pass, before the main
  // scene's own opaque depth exists (and clears depth right after itself) - there is no opaque
  // depth here to feather a skybox effects material against (e.g. dust_002_skybox.vmat), so
  // F_DEPTH_FEATHER stays forced off for every material built through this call.
  await buildGameMaterials(gltf, patchedReport, shared, materialPool, texLoader, { disableEffectsDepthFeather: true });
  const gameMaterials = new Map();
  gltf.scene.traverse((o) => {
    if (o.isMesh && o.userData.gameMaterial) {
      o.material = o.userData.gameMaterial;
      gameMaterials.set(o.material.uuid, o.material);
    }
  });

  // review_f3a8 fix item 1: a fixed `mainCamera.far/scale` (e.g. 40000/16 = 2500 sky units) clips
  // real background scenery on maps whose skybox geometry reaches further out than that (Inferno's
  // own treeline/mountain, Dust2's background) - the far plane must also cover this scene's own
  // bounds from wherever the camera ends up, not just the naively rescaled main-camera distance.
  gltf.scene.updateMatrixWorld(true);
  const skySphere = new THREE.Box3().setFromObject(gltf.scene).getBoundingSphere(new THREE.Sphere());

  const camera = new THREE.PerspectiveCamera(VERTICAL_FOV_DEG, 1, 1, 1000);
  camera.up.set(0, 0, 1);
  // review_f3a8 fix item 2: skybox_reference's own origin (not always zero - ar_shoots, ar_baggage,
  // de_overpass), composed the way VRF does (this file's own top comment).
  const ref = skybox.reference?.origin ?? [0, 0, 0];

  return {
    programCount() {
      return materialPool.size;
    },
    materialTextureBytes() {
      return texLoader.bytesLoaded();
    },
    materialTextureCount() {
      return texLoader.textureCount();
    },
    // Places `camera` at `skybox.origin + (mainCamera.position - ref)/scale` with the main
    // camera's own orientation/FOV, then draws `gltf.scene` with it. Caller (`lighting.js`'s
    // `renderFrame`) is responsible for the surrounding `autoClear`/depth-clear sequencing (right
    // after the 2D sky pass, depth cleared before the main scene).
    render(renderer, mainCamera) {
      camera.position.set(
        skybox.origin[0] + (mainCamera.position.x - ref[0]) / skybox.scale,
        skybox.origin[1] + (mainCamera.position.y - ref[1]) / skybox.scale,
        skybox.origin[2] + (mainCamera.position.z - ref[2]) / skybox.scale,
      );
      camera.quaternion.copy(mainCamera.quaternion);
      camera.fov = mainCamera.fov;
      camera.aspect = mainCamera.aspect;
      camera.near = Math.max(mainCamera.near / skybox.scale, 1e-3);
      // review_f3a8 fix item 1: never clip the skybox's own geometry -- its bounding sphere from
      // wherever `camera` just ended up, not only the naively rescaled main-camera distance.
      camera.far = Math.max(
        mainCamera.far / skybox.scale,
        camera.position.distanceTo(skySphere.center) + skySphere.radius,
      );
      camera.updateProjectionMatrix();
      camera.updateMatrixWorld(true);
      renderer.render(gltf.scene, camera);
    },
    dispose() {
      res.irradianceMap.dispose();
      res.shadowMap.dispose();
      texLoader.dispose();
      for (const m of gameMaterials.values()) m.dispose();
      gameMaterials.clear();
      gltf.scene.traverse((o) => {
        if (o.isMesh) o.geometry.dispose();
      });
    },
  };
}
