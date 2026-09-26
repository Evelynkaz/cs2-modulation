// The 3D scene: loads `render.glb` (with progress/cache/cancel), a free-fly and orbit camera, the
// collision mesh from `/api/mesh` (picking + the "show collisions" overlay), a selected lineup's
// trajectory/smoke/player marker, and the first-person view (`s6f3b_viewer3d.md` F3b-1b/F3b-1c).
//
// Up = +Z, game units (inches) throughout - no scaling, no axis conversion (`s6f3a3_map.md` §6),
// so a raycast hit or an `enterFirstPerson` eye position is already a `setpos`.

import * as THREE from "three";
import { GLTFLoader } from "../lib/three/examples/jsm/loaders/GLTFLoader.js";
import { OrbitControls } from "../lib/three/examples/jsm/controls/OrbitControls.js";
import { MeshoptDecoder } from "../lib/three/examples/jsm/libs/meshopt_decoder.module.js";
import { MeshBVH, acceleratedRaycast } from "three-mesh-bvh";
import { parseSm3d, flattenGroups } from "./mesh3d.js?v=1";
import { sourceBasis, VERTICAL_FOV_DEG, eyeHeight, hullHeight, PLAYER_CAPSULE_RADIUS } from "./camera.js?v=1";
import { renderGlbUrl, fetchRenderJson, meshUrl, fetchTrajectory, fetchSmoke, fetchLevels, hasUsableRender } from "./api.js?v=1";
import { strings } from "./strings.js?v=1";
import { createLightingPipeline, hasGameLightingData, FEATHER_LAYER, OVERLAY_LAYER } from "./lighting.js?v=1";
import { createHdrTarget } from "./lightingPost.js?v=1";
import { loadStoredLightingMode, storeLightingMode } from "./state.js?v=1";
import { createMaterialTextureLoader, srgbToLinear } from "./materialTextures.js?v=1";

const FLY_SPEED_DEFAULT = 400; // inches/second - about walking-to-running pace.
const FLY_SPEED_MIN = 32;
const FLY_SPEED_MAX = 8000;
const FLY_SPEED_WHEEL_FACTOR = 1.15;
const FLY_SPEED_SHIFT_MULT = 3;
const MOUSE_SENSITIVITY = 0.12; // degrees per pixel of mouse movement.
const MAX_ANISOTROPY_CAP = 8;
// `render.glb` (139 MB on Mirage) exceeds Chrome's HTTP cache per-entry cap, so it never lands in
// the browser's own disk cache - kept here instead, revalidated by ETag (`s6f3b_viewer3d.md`
// "кэш браузера по ETag").
const RENDER_GLB_CACHE_NAME = "cs2mod-render-glb";

// S6n lineup previews (`s6n_lineup_previews.md`): still-image captures for a lineup card - own
// camera, own offscreen render target, never the interactive one.
const PREVIEW_DEFAULT_WIDTH = 480;
const PREVIEW_DEFAULT_HEIGHT = 270;
const PREVIEW_INSET_FOV_DEG = 12; // horizontal == vertical here since the inset render is square.
const PREVIEW_INSET_FRACTION = 0.42; // inset side length, as a fraction of min(width, height).
const PREVIEW_INSET_MARGIN = 10;
// Review fix item 7 (readability at 480x270): closer than the spec's original "~260u/~160u" so the
// player marker isn't a speck - ~170u back/~110u up, looking at chest height instead of the capsule's
// own vertical middle.
const PREVIEW_STAND_BACK = 170;
const PREVIEW_STAND_UP = 110;
const PREVIEW_STAND_LOOK_FRACTION = 0.7; // of hull height, from the feet - "chest", not mid-capsule.
const PREVIEW_STAND_ARROW_LEN = 40;
const PREVIEW_STAND_COLOR = 0xf2a33a; // accent amber - reads as "player" against any wall texture.
const PREVIEW_LAND_BACK = 240;
const PREVIEW_LAND_UP = 220;
const PREVIEW_SMOKE_RADIUS = 144;

const COLLISION_COLORS = {
  world: 0x8fa3c8,
  phantom: 0xff5fd1, // grenadeclip/window/EntityPhysicsClip/EntityBreakable-as-blocker: bright.
  door: 0x38bdf8,
  breakable: 0xffd23f,
};

function isCrouchType(type) {
  return type === "Crouch" || type === "CrouchJumpThrow";
}

function disposeMaterial(material) {
  for (const key of ["map", "normalMap"]) {
    material[key]?.dispose?.();
  }
  material.dispose();
}

// The F3b-2 uber-shader material's textures live in `.uniforms`, not the named `.map`/`.normalMap`
// fields `disposeMaterial` knows about (`s6f3b2_lighting_shader.md` §2 - a `ShaderMaterial`, not a
// `MeshStandardMaterial`) - and a mesh with game lighting keeps BOTH its "simple" and "game"
// materials alive at once (`lighting.js`: instant toggle), while `.material` only ever points at
// whichever is currently active, so disposal must reach both explicitly, not just `obj.material`.
// Roughness has no uniform of its own since `s6f3a6_native_tex.md` (it's packed in `uNormalMap`'s
// blue channel, same texture as the normal direction) - disposing this list is a redundant safety
// net anyway (`lighting.js`'s own `texLoader.dispose()`, called from the pipeline's `dispose()`,
// is what actually owns these - see this list's own doc comment above).
const GAME_MATERIAL_TEXTURE_UNIFORMS = [
  "uAlbedoMap",
  "uNormalMap",
  "uAoMap",
  "uMetalnessMap",
  "uTintMaskMap",
  "uLayer2Map",
  "uBlendModMap",
  "uSelfIllumMap",
  "uLayer2NormalMap",
  "uEnvHeight1",
  "uEnvColor2",
  "uEnvNormal2",
  "uEnvHeight2",
];

function disposeGameMaterial(material) {
  for (const key of GAME_MATERIAL_TEXTURE_UNIFORMS) {
    material.uniforms[key]?.value?.dispose?.();
  }
  material.dispose();
}

function disposeObject3D(root) {
  root.traverse((obj) => {
    obj.geometry?.dispose?.();
    const { simpleMaterial, gameMaterial } = obj.userData ?? {};
    if (simpleMaterial || gameMaterial) {
      if (simpleMaterial) disposeMaterial(simpleMaterial);
      if (gameMaterial) disposeGameMaterial(gameMaterial);
      return;
    }
    const mat = obj.material;
    if (Array.isArray(mat)) {
      mat.forEach(disposeMaterial);
    } else if (mat) {
      disposeMaterial(mat);
    }
  });
}

// ---- S6n lineup previews: pure helpers (no scene/renderer state) - the rest of `capturePreview`
// lives inside `createSceneView`, where it can reach `renderer`/`scene`/`lightingPipeline`. -------

// Source's `AngleVectors` again, aimed at an arbitrary camera instead of the interactive
// `camera`/`yawDeg`/`pitchDeg` globals - `capturePreview`'s own cameras never touch those.
function pointCameraAt(cam, pitchDeg, yawDeg) {
  const { forward, up } = sourceBasis(pitchDeg, yawDeg);
  cam.up.set(up.x, up.y, up.z);
  cam.lookAt(cam.position.x + forward.x, cam.position.y + forward.y, cam.position.z + forward.z);
}

// Player capsule + floor ring + facing arrow - shared between the interactive selected-lineup
// visuals (`buildLineupVisuals`) and `capturePreview`'s `kind: "stand"` (spec: "same visuals as
// `stand`"). Review fix item 7: bright/solid accent-amber capsule with a darker backface "rim" shell
// (a cheap static-outline trick - no post-process edge detection needed) plus a ring on the floor,
// so the marker reads at a glance at 480x270 instead of disappearing against a wall.
function addStandMarkers(group, feet, yawDeg, crouched) {
  const h = hullHeight(crouched);
  const capsuleGeo = new THREE.CapsuleGeometry(PLAYER_CAPSULE_RADIUS, Math.max(1, h - 2 * PLAYER_CAPSULE_RADIUS), 4, 8);
  const capsule = new THREE.Mesh(capsuleGeo, new THREE.MeshLambertMaterial({ color: PREVIEW_STAND_COLOR, transparent: true, opacity: 0.9 }));
  capsule.position.set(feet.x, feet.y, feet.z + h / 2);
  capsule.rotation.x = Math.PI / 2; // CapsuleGeometry's long axis is Y; stand it up along Z.
  group.add(capsule);

  const rim = new THREE.Mesh(capsuleGeo, new THREE.MeshBasicMaterial({ color: 0x7a4a12, side: THREE.BackSide }));
  rim.position.copy(capsule.position);
  rim.rotation.x = Math.PI / 2;
  rim.scale.setScalar(1.06);
  group.add(rim);

  const ring = new THREE.Mesh(
    new THREE.RingGeometry(PLAYER_CAPSULE_RADIUS + 4, PLAYER_CAPSULE_RADIUS + 9, 24),
    new THREE.MeshBasicMaterial({ color: PREVIEW_STAND_COLOR, transparent: true, opacity: 0.85, side: THREE.DoubleSide, depthWrite: false }),
  );
  ring.position.set(feet.x, feet.y, feet.z + 1);
  group.add(ring);

  const { forward } = sourceBasis(0, yawDeg);
  group.add(buildFlatArrow(new THREE.Vector3(forward.x, forward.y, 0), new THREE.Vector3(feet.x, feet.y, feet.z + 2), PREVIEW_STAND_ARROW_LEN, 0xffffff));
}

// A solid (not `THREE.ArrowHelper`'s thin `Line`, invisible at capture resolution/distance) shaft +
// cone pointing along `dir` from `origin` - same local-+Y-to-`dir` alignment `ArrowHelper.
// setDirection` uses, just built from real triangles so it reads clearly in a still image.
function buildFlatArrow(dir, origin, length, color) {
  const headLength = length * 0.35;
  const shaftLength = Math.max(1, length - headLength);
  const material = new THREE.MeshBasicMaterial({ color });
  const shaft = new THREE.Mesh(new THREE.CylinderGeometry(2, 2, shaftLength, 8), material);
  shaft.position.y = shaftLength / 2;
  const head = new THREE.Mesh(new THREE.ConeGeometry(5, headLength, 10), material);
  head.position.y = shaftLength + headLength / 2;
  const group = new THREE.Group();
  group.add(shaft, head);
  group.position.copy(origin);
  if (dir.y > 0.99999) {
    group.quaternion.set(0, 0, 0, 1);
  } else if (dir.y < -0.99999) {
    group.quaternion.set(1, 0, 0, 0);
  } else {
    const axis = new THREE.Vector3(dir.z, 0, -dir.x).normalize();
    group.quaternion.setFromAxisAngle(axis, Math.acos(dir.y));
  }
  return group;
}

// `kind: "land"`'s own markers: a translucent smoke-radius sphere + two perpendicular equator rings
// (review fix item 7 - a flat disc projects to a near-invisible sliver from a shallow angle; a
// sphere + rings reads as an actual volume), a clear landing marker with a vertical stem through it
// (same idea as `drawTarget`'s own beacon, so it's visible even when the haze itself is faint), and
// (if given) the trajectory.
function addLandMarkers(group, rest, trajectory) {
  const sphere = new THREE.Mesh(
    new THREE.SphereGeometry(PREVIEW_SMOKE_RADIUS, 20, 14),
    new THREE.MeshBasicMaterial({ color: 0xe7ebf2, transparent: true, opacity: 0.22, depthWrite: false }),
  );
  sphere.position.copy(rest);
  group.add(sphere);

  const ringGeo = new THREE.TorusGeometry(PREVIEW_SMOKE_RADIUS, 1.5, 8, 40);
  const ringMat = new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.5 });
  const equator = new THREE.Mesh(ringGeo, ringMat);
  equator.position.copy(rest);
  group.add(equator);
  const meridian = new THREE.Mesh(ringGeo, ringMat);
  meridian.position.copy(rest);
  meridian.rotation.x = Math.PI / 2;
  group.add(meridian);

  const marker = new THREE.Mesh(new THREE.SphereGeometry(6, 12, 8), new THREE.MeshBasicMaterial({ color: 0x1a7f37 }));
  marker.position.copy(rest);
  group.add(marker);
  const stem = new THREE.Line(
    new THREE.BufferGeometry().setFromPoints([
      new THREE.Vector3(rest.x, rest.y, rest.z - 6),
      new THREE.Vector3(rest.x, rest.y, rest.z + PREVIEW_SMOKE_RADIUS + 20),
    ]),
    new THREE.LineBasicMaterial({ color: 0x1a7f37, transparent: true, opacity: 0.85, depthTest: false }),
  );
  stem.renderOrder = 999;
  group.add(stem);

  if (Array.isArray(trajectory) && trajectory.length > 1) {
    const pts = trajectory.map((p) => new THREE.Vector3(p[0], p[1], p[2]));
    group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints(pts), new THREE.LineBasicMaterial({ color: 0xffb020 })));
  }
}

// WebGL reads render-target rows bottom-to-top; `ImageData` wants top-to-bottom - flip while
// copying instead of a separate pass.
function pixelsToCanvas(buffer, w, h) {
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  const imageData = ctx.createImageData(w, h);
  const rowBytes = w * 4;
  for (let y = 0; y < h; y++) {
    const srcStart = (h - 1 - y) * rowBytes;
    imageData.data.set(buffer.subarray(srcStart, srcStart + rowBytes), y * rowBytes);
  }
  ctx.putImageData(imageData, 0, 0);
  return canvas;
}

// `image/webp` per the capturePreview contract, falling back to `image/png` if the browser's
// `toBlob` can't encode webp (calls back with `null` instead of throwing).
function canvasToBlob(canvas) {
  return new Promise((resolve) => {
    canvas.toBlob(
      (blob) => {
        if (blob) {
          resolve(blob);
          return;
        }
        canvas.toBlob(resolve, "image/png");
      },
      "image/webp",
      0.92,
    );
  });
}

// CS2-style crosshair: a black outline stroke under a colored one so it reads on any background.
function drawCrosshair(ctx, cx, cy, size, gap, thickness, color) {
  const segments = [
    [cx - size - gap, cy, cx - gap, cy],
    [cx + gap, cy, cx + size + gap, cy],
    [cx, cy - size - gap, cx, cy - gap],
    [cx, cy + gap, cx, cy + size + gap],
  ];
  const stroke = (strokeColor, width) => {
    ctx.strokeStyle = strokeColor;
    ctx.lineWidth = width;
    for (const [x1, y1, x2, y2] of segments) {
      ctx.beginPath();
      ctx.moveTo(x1, y1);
      ctx.lineTo(x2, y2);
      ctx.stroke();
    }
  };
  stroke("rgba(0,0,0,0.85)", thickness + 2);
  stroke(color, thickness);
}

// ---- material extras (`s6f3a3_map.md` §4/§6): mod2x via native blend factors, layer blending and
// the tint mask via `onBeforeCompile` patches on the standard material three.js already built from
// the glTF's own PBR fields (base color/normal/alphaMode/doubleSided/unlit all need no extra code
// here - `GLTFLoader` already handles them). ----

function applyMod2x(material) {
  // `result = srcFactor*src + dstFactor*dst` with `srcFactor = dstColor`, `dstFactor = srcColor`
  // gives `2 * src * dst` - exactly Mod2x - without any shader edits.
  material.transparent = true;
  material.blending = THREE.CustomBlending;
  material.blendEquation = THREE.AddEquation;
  material.blendSrc = THREE.DstColorFactor;
  material.blendDst = THREE.SrcColorFactor;
  material.depthWrite = false;
}

// VRF complex.frag.slang:776-777: `outputColor.rgb = mix(vec3(0.5), outputColor.rgb, outputColor.a)` -
// alpha fades to the Mod2x neutral (2*0.5*dst = dst). Applied after colorspace_fragment because
// the blend runs on the encoded canvas values.
function mod2xPatch(shader) {
  shader.fragmentShader = shader.fragmentShader.replace(
    "#include <colorspace_fragment>",
    "#include <colorspace_fragment>\n\tgl_FragColor.rgb = mix( vec3( 0.5 ), gl_FragColor.rgb, gl_FragColor.a );",
  );
}

// `extras.tint` + `extras.tintMaskTexture` (§4): the model's tint is diverted out of
// `baseColorFactor` into here so it only recolors the masked area (e.g. a car's paint, not its
// glass/tires). `tintIsSrgb` comes from render.json's `materialExtras.tintColorSpace` - the
// exporter writes `extras.tint` already linear (matching `baseColorFactor`), so only convert when
// that field says otherwise; converting unconditionally would double-darken an already-linear tint.
function makeTintPatch(tint, maskTex, tintIsSrgb) {
  const color = tintIsSrgb
    ? new THREE.Color().setRGB(tint[0], tint[1], tint[2], THREE.SRGBColorSpace)
    : new THREE.Color(tint[0], tint[1], tint[2]);
  return (shader) => {
    shader.uniforms.tintColor = { value: color };
    shader.uniforms.tintMaskMap = { value: maskTex };
    shader.fragmentShader = shader.fragmentShader
      .replace("#include <common>", "#include <common>\nuniform vec3 tintColor;\nuniform sampler2D tintMaskMap;")
      .replace(
        "#include <map_fragment>",
        "#include <map_fragment>\n#ifdef USE_MAP\n\t{\n\t\tfloat tintAmount = texture2D( tintMaskMap, vMapUv ).r;\n\t\tdiffuseColor.rgb = mix( diffuseColor.rgb, diffuseColor.rgb * tintColor, tintAmount );\n\t}\n#endif",
      );
  };
}

// review fix item 7: DXT1 (BC1) sRGB without `WEBGL_compressed_texture_s3tc_srgb` uploads linear
// (`materialTextures.js`'s `wantsManualSrgb`) and needs the shader to decode sRGB->linear itself -
// `colorspace_pars_fragment` (already part of every stock fragment shader chunk list) defines
// `sRGBTransferEOTF`, the exact conversion, so this reuses it instead of duplicating the formula.
function manualSrgbMapPatch(shader) {
  shader.fragmentShader = shader.fragmentShader.replace(
    "#include <map_fragment>",
    "#include <map_fragment>\n#ifdef USE_MAP\n\tdiffuseColor.rgb = diffuse * sRGBTransferEOTF( sampledDiffuseColor ).rgb;\n#endif",
  );
}

// Layer blending (`materialExtras.layers`, cited formula from `crates/s2render/src/material.rs`):
// `m = texture(blendModulationTexture, uv)`, `b = smoothstep(max(0,m.g-m.r), min(1,m.g+m.r), w)`,
// `color = mix(layer1, layer2, b)`, `w` = the `_BLEND` vertex attribute. Without a modulation
// texture (`F_FANCY_BLENDING` wasn't mode 1) `m` falls back to `(0,1,0)` - `smoothstep(0,1,w)`,
// i.e. `b = w` - so the weight alone still blends, just without the softened edge.
// review fix item 3 (s6f3a7_env_materials.md review): `linear` skips the blend-modulation
// formula entirely for `float b = vBlendW;` - the no-`modTex` fallback below
// (`m = vec3(0,1,0)`) makes `smoothstep(max(0,m.g-m.r), min(1,m.g+m.r), w)` ==
// `smoothstep(1, 1, w)`, a division-by-zero edge case that measured 0 everywhere rather than the
// intended `b = w`, silently no-opping `envLayer2`'s "simple" mode colour blend.
function makeLayerPatch(layer2Tex, modTex, linear) {
  const layer2ManualSrgb = layer2Tex.userData?.manualSrgb === true; // review fix item 7
  return (shader) => {
    shader.uniforms.layer2Map = { value: layer2Tex };
    shader.uniforms.blendModMap = { value: modTex ?? layer2Tex };
    shader.uniforms.hasBlendMod = { value: modTex ? 1 : 0 };
    shader.vertexShader = shader.vertexShader
      .replace("#include <common>", "#include <common>\nattribute float _blend;\nvarying float vBlendW;")
      .replace("#include <begin_vertex>", "#include <begin_vertex>\nvBlendW = _blend;");
    const layer2SrgbFix = layer2ManualSrgb ? "\n\t\tlayer2Sample = sRGBTransferEOTF( layer2Sample );" : "";
    const blendFactor = linear
      ? "float b = vBlendW;"
      : "vec3 m = hasBlendMod > 0.5 ? texture2D( blendModMap, vMapUv ).rgb : vec3( 0.0, 1.0, 0.0 );\n\t\tfloat b = smoothstep( max( 0.0, m.g - m.r ), min( 1.0, m.g + m.r ), vBlendW );";
    shader.fragmentShader = shader.fragmentShader
      .replace(
        "#include <common>",
        "#include <common>\nuniform sampler2D layer2Map;\nuniform sampler2D blendModMap;\nuniform float hasBlendMod;\nvarying float vBlendW;",
      )
      .replace(
        "#include <map_fragment>",
        `#include <map_fragment>\n#ifdef USE_MAP\n\t{\n\t\tvec4 layer2Sample = texture2D( layer2Map, vMapUv );${layer2SrgbFix}\n\t\t${blendFactor}\n\t\tdiffuseColor.rgb = mix( diffuseColor.rgb, layer2Sample.rgb, b );\n\t}\n#endif`,
      );
  };
}

// HemiOct normal map for `MeshStandardMaterial` (`s6f3a6_native_tex.md` change item 4): the
// stock `USE_NORMALMAP_TANGENTSPACE` chunk (`normal_fragment_maps`) interprets the sampled texture
// as a direct tangent-space XYZ triple, which our RG-hemi-oct + B-roughness texture is not -
// replaced wholesale so the game-lighting shader's own decode (`lightingShader.js`) and this
// "simple" path agree; `tbn`/`vNormalMapUv` still come from three's own `USE_NORMALMAP` chunks
// (its derivative-based cotangent frame applies exactly like `lightingShader.js`'s `cotangentFrame`
// when there's no vertex TANGENT attribute, which render.glb never carries).
function normalMapPatch(shader) {
  shader.fragmentShader = shader.fragmentShader.replace(
    "#include <normal_fragment_maps>",
    "#ifdef USE_NORMALMAP_TANGENTSPACE\n\t{\n\t\tvec4 t = texture2D( normalMap, vNormalMapUv );\n\t\tvec2 e = vec2( t.r + t.g - 1.003922, t.r - t.g );\n\t\tvec3 mapN = normalize( vec3( e, 1.0 - abs( e.x ) - abs( e.y ) ) );\n\t\tmapN.y = -mapN.y;\n\t\tnormal = normalize( tbn * mapN );\n\t}\n#endif",
  );
}

// review fix item 6, "simple" mode side: `extras.baseColorAlphaMeaning` ("ao"/"metalness",
// REPORT.md's channel table) - three's stock `aomap_fragment`/`metalnessmap_fragment` chunks only
// read `aoMap`/`metalnessMap` (never wired in simple mode), so the alpha-derived term is injected
// unconditionally right after each chunk instead.
function albedoAlphaAoPatch(shader) {
  shader.fragmentShader = shader.fragmentShader.replace(
    "#include <aomap_fragment>",
    "#include <aomap_fragment>\n\treflectedLight.indirectDiffuse *= pow( max( diffuseColor.a, 0.0 ), 0.5 );",
  );
}
function albedoAlphaMetalnessPatch(shader) {
  shader.fragmentShader = shader.fragmentShader.replace(
    "#include <metalnessmap_fragment>",
    "#include <metalnessmap_fragment>\n\tmetalnessFactor = diffuseColor.a;",
  );
}

// `s6f3a9_effects.md` change item 2's "simple mode: at minimum fresnel + fade" - csgo_effects's
// own masks/colorBoost/depth-feather (the game-lighting path's full formula, `lightingShader.js`)
// are not replicated here, only the two cheapest terms that keep a dust/cloud card from being a
// flat opaque panel in "simple" mode too.
//
// review fix: csgo_effects is unlit (`KHR_materials_unlit`), so GLTFLoader builds a
// `MeshBasicMaterial` here, not `MeshStandardMaterial` - its stock shader chunks have no
// `normal_fragment_begin` and no `vViewPosition`/`vNormal` at all (`meshbasic_vert`/`_frag` in
// three.module.js), so the original patch's markers/varyings never existed and this whole block
// was silently never applied (`mirage_mid_simple_after.png` still showed the flat panel). This
// version carries its own view-space position/normal varyings instead of borrowing
// `MeshStandardMaterial`'s.
function makeEffectsFresnelFadePatch(fx) {
  const flipBackface = fx.dontFlipBackfaceNormals !== true;
  return (shader) => {
    shader.uniforms.fxFresnelParams = {
      value: new THREE.Vector4(fx.fresnelExponent, fx.fresnelFalloff, fx.fresnelMin, fx.fresnelMax),
    };
    shader.uniforms.fxFadeParams = { value: new THREE.Vector4(fx.fadeDistance, fx.fadeFalloff, fx.fadeMin, fx.fadeMax) };
    shader.vertexShader = shader.vertexShader
      .replace("#include <common>", "#include <common>\nvarying vec3 vFxViewPos;\nvarying vec3 vFxViewNormal;")
      .replace(
        "#include <project_vertex>",
        "#include <project_vertex>\n\tvFxViewPos = mvPosition.xyz;\n\tvFxViewNormal = normalize( normalMatrix * normal );",
      );
    const flip = flipBackface ? "\n\t\t#ifdef DOUBLE_SIDED\n\t\tif ( !gl_FrontFacing ) fxN = -fxN;\n\t\t#endif" : "";
    shader.fragmentShader = shader.fragmentShader
      .replace(
        "#include <common>",
        "#include <common>\nvarying vec3 vFxViewPos;\nvarying vec3 vFxViewNormal;\nuniform vec4 fxFresnelParams;\nuniform vec4 fxFadeParams;",
      )
      .replace(
        "#include <alphahash_fragment>",
        `#include <alphahash_fragment>\n\t{\n\t\tvec3 fxRay = vFxViewPos;\n\t\tvec3 fxN = normalize( vFxViewNormal );${flip}\n\t\tfloat fxFresnel = clamp( dot( -normalize( fxRay ), fxN ), 0.0001, 1.0 );\n\t\tfxFresnel = pow( fxFresnel, fxFresnelParams.x ) * fxFresnelParams.y;\n\t\tfxFresnel = clamp( fxFresnel, 0.0, 1.0 );\n\t\tfxFresnel = mix( fxFresnelParams.z, fxFresnelParams.w, fxFresnel );\n\t\tfloat fxFade = clamp( length( fxRay / vec3( fxFadeParams.x ) ), 0.0, 1.0 );\n\t\tfxFade = mix( fxFadeParams.z, fxFadeParams.w, fxFade );\n\t\tfxFade = pow( max( fxFade, 0.0001 ), fxFadeParams.y );\n\t\tdiffuseColor.a *= fxFresnel * fxFade;\n\t}`,
      );
  };
}

async function applyMaterialExtras(gltf, renderer, texLoader, renderJson, tintIsSrgb) {
  const materialDefs = gltf.parser.json.materials || [];
  // Anisotropy is set once, at upload time, by the shared loader itself (review fix item 2:
  // `tex.anisotropy` must be assigned before `renderer.initTexture` - see
  // `materialTextures.js`'s `createMaterialTextureLoader`'s own `anisotropy` option) - not
  // reassigned here.
  // `texLoader` is shared with the game-lighting pipeline (`scene3d.js`'s own
  // `materialTexLoaderReady`, `s6f3a6_native_tex.md` change item 5's "simple mode must also
  // work") - one fetch/upload per texture regardless of how many lighting modes use it; owned and
  // disposed by the caller, not by this function or `disposeObject3D`.
  // A single texture's fetch/decode failure degrades just that slot to "missing" instead of
  // failing this whole material (mirrors `lighting.js`'s own `buildRecipe`).
  const getTex = (index) =>
    index != null
      ? texLoader.get(index).catch((e) => {
          console.error(`[cs2mod] texture load failed (render.json textures[${index}]):`, e);
          return null;
        })
      : null;
  const onMeshes = new Map(); // material instance actually rendered -> glTF material index
  gltf.scene.traverse((o) => {
    if (!o.isMesh) return;
    for (const m of Array.isArray(o.material) ? o.material : [o.material]) {
      const idx = gltf.parser.associations.get(m)?.materials;
      if (idx !== undefined) onMeshes.set(m, idx);
    }
  });
  for (const [material, i] of onMeshes) {
    const def = materialDefs[i];
    const extras = def.extras;
    if (!extras) {
      continue;
    }

    // Base color: a real texture (`extras.baseColorTexture`), or (rare) a 4x4-constant folded
    // into `material.color`/`.opacity` directly - same fold `lighting.js`'s buildRecipe does.
    if (extras.baseColorTexture != null) {
      material.map = await getTex(extras.baseColorTexture);
    } else if (extras.baseColorConstant) {
      const [r, g, b, a] = extras.baseColorConstant;
      const isSrgb = extras.baseColorConstantColorSpace === "srgb";
      const lin = isSrgb ? [srgbToLinear(r / 255), srgbToLinear(g / 255), srgbToLinear(b / 255)] : [r / 255, g / 255, b / 255];
      material.color.multiply(new THREE.Color(lin[0], lin[1], lin[2]));
      material.opacity *= a / 255;
    }

    const patches = [];
    const kinds = [];

    // review fix item 6.
    if (extras.baseColorAlphaMeaning === "ao") {
      patches.push(albedoAlphaAoPatch);
      kinds.push("albedoAlphaAo");
    } else if (extras.baseColorAlphaMeaning === "metalness") {
      patches.push(albedoAlphaMetalnessPatch);
      kinds.push("albedoAlphaMetalness");
    }

    // Normal: a real HemiOct texture needs `normalMapPatch`'s decode; a 4x4-constant is a flat
    // normal (only its roughness matters, and `MeshStandardMaterial.roughness` is already the
    // exporter's fixed 1.0 placeholder either way - no richer "simple" mode roughness existed
    // before this change either, see this function's own history).
    if (extras.normalTexture != null) {
      material.normalMap = await getTex(extras.normalTexture);
      if (material.normalMap) {
        patches.push(normalMapPatch);
        kinds.push("normal");
      }
    }

    if (extras.blendMode === "mod2x") {
      applyMod2x(material);
      patches.push(mod2xPatch);
      kinds.push("mod2x");
    }
    if (extras.tintMaskTexture != null && extras.tint && material.map) {
      const maskTex = await getTex(extras.tintMaskTexture);
      if (maskTex) {
        patches.push(makeTintPatch(extras.tint, maskTex, tintIsSrgb));
        kinds.push("tint");
      }
    } else if (extras.tintMaskConstant && extras.tint) {
      // Constant mask amount: mix(albedo, albedo*tint, k) == albedo * mix(1, tint, k), foldable
      // straight into material.color without any shader patch (same fold `lighting.js` uses).
      const k = extras.tintMaskConstant[0] / 255;
      const tint = extras.tint;
      material.color.multiply(new THREE.Color(1 + k * (tint[0] - 1), 1 + k * (tint[1] - 1), 1 + k * (tint[2] - 1)));
    }
    if (extras.layers?.layer2ColorTexture != null && material.map) {
      const layer2Tex = await getTex(extras.layers.layer2ColorTexture);
      const modTex = extras.layers.blendModulationTexture != null ? await getTex(extras.layers.blendModulationTexture) : null;
      if (layer2Tex) {
        patches.push(makeLayerPatch(layer2Tex, modTex));
        kinds.push("layers");
      }
    }
    // `s6f3a7_env_materials.md` change item 1, "simple" mode ("as far as practical"): colour-only,
    // vertex-paint-weight blend, no height bands (that formula needs `uEnvHeight1/2` sampling and
    // several more uniforms this onBeforeCompile patch system isn't set up to carry -- see the
    // receipt). Review fix item 3: `linear=true` so `makeLayerPatch` uses `b = vBlendW` directly
    // instead of its modulation-texture formula, which silently evaluates to 0 with no `modTex`
    // (`smoothstep(1,1,w)`, not `smoothstep(0,1,w)` as this comment previously claimed).
    if (extras.envLayer2?.color2Texture != null && material.map) {
      const color2Tex = await getTex(extras.envLayer2.color2Texture);
      if (color2Tex) {
        patches.push(makeLayerPatch(color2Tex, null, true));
        kinds.push("envLayer2");
      }
    }
    if (extras.effects) {
      patches.push(makeEffectsFresnelFadePatch(extras.effects));
      // review fix: the injected fragment text itself differs by this flag (the backface-flip
      // block is only present when it's needed), so it has to be part of the cache key too - two
      // materials sharing "effectsFresnelFade" alone would wrongly share one compiled program.
      kinds.push(extras.effects.dontFlipBackfaceNormals !== true ? "effectsFresnelFade:flip" : "effectsFresnelFade:noflip");
      // review fix item 2: F_ADDITIVE_BLEND materials (sun_glow_001/sun_disc_glow_001 on Mirage's
      // 3D skybox, steam_001 on Inferno) - RenderMaterial.cs:354-357,892 blends (SrcAlpha, One),
      // not the translucent (SrcAlpha, InvSrcAlpha) GLTFLoader already set from alphaMode BLEND.
      // Not the `THREE.AdditiveBlending` preset (same reasoning as `lightingShader.js`'s own fix):
      // it's (One, One) for a non-premultiplied-alpha material (this one is, the default), which
      // would ignore this material's own alpha (fresnel x fade) and render at full brightness
      // everywhere instead of fading at the glow's edges.
      if (extras.effects.blendMode === "additive") {
        material.blending = THREE.CustomBlending;
        material.blendEquation = THREE.AddEquation;
        material.blendSrc = THREE.SrcAlphaFactor;
        material.blendDst = THREE.OneFactor;
      }
    }
    // review fix item 7: applied last so its patch text lands directly after `#include <map_fragment>`,
    // ahead of tint/layers above, which pushed earlier and therefore ended up further down.
    if (material.map?.userData?.manualSrgb) {
      patches.push(manualSrgbMapPatch);
      kinds.push("manualSrgbMap");
    }
    if (patches.length > 0) {
      material.onBeforeCompile = (shader) => {
        for (const patch of patches) {
          patch(shader);
        }
      };
      const key = "cs2mod-extras:" + kinds.join("+");
      material.customProgramCacheKey = () => key;
    }
    material.needsUpdate = true;
  }
}

// ---- collision mesh: raycast proxy (never rendered) + toggleable colored overlay ----------------

async function loadCollisionMesh(map) {
  const res = await fetch(meshUrl(map), { cache: "default" });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}`);
  }
  const buffer = await res.arrayBuffer();
  const { positions, groups } = parseSm3d(buffer);
  const { indices, ranges } = flattenGroups(groups);

  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
  geometry.setIndex(new THREE.BufferAttribute(indices, 1));
  for (const r of ranges) {
    geometry.addGroup(r.start, r.count, r.materialIndex);
  }
  geometry.computeVertexNormals();
  geometry.computeBoundingBox();
  geometry.computeBoundingSphere();

  // Review fix item 5: a brute-force `Raycaster.intersectObject` walks every triangle in the
  // collision mesh - measured ~5ms/cast on Mirage (~125k tris) but up to ~80ms on Inferno
  // (~1.85M), stalling the once-per-frame hover raycast to ~10fps. `MeshBVH` builds a bounding
  // volume hierarchy once at load time; `acceleratedRaycast`, assigned on `pickProxy` alone below
  // (not patched onto `THREE.Mesh.prototype` - render.glb's own meshes are untouched), walks that
  // instead of every triangle.
  const bvhBuildStart = performance.now();
  geometry.boundsTree = new MeshBVH(geometry);
  console.debug(`[cs2mod] ${map}: MeshBVH build ${(performance.now() - bvhBuildStart).toFixed(1)}ms (${(indices.length / 3) | 0} tris)`);

  // Group order matches `mesh_payload.rs`'s SM3D layout: 0 world, 1 phantom, 2 door, 3 breakable.
  const overlayMaterials = [
    new THREE.MeshLambertMaterial({ color: COLLISION_COLORS.world, transparent: true, opacity: 0.35, side: THREE.DoubleSide, depthWrite: false }),
    new THREE.MeshBasicMaterial({ color: COLLISION_COLORS.phantom, transparent: true, opacity: 0.85, side: THREE.DoubleSide, depthWrite: false }),
    new THREE.MeshLambertMaterial({ color: COLLISION_COLORS.door, transparent: true, opacity: 0.6, side: THREE.DoubleSide, depthWrite: false }),
    new THREE.MeshLambertMaterial({ color: COLLISION_COLORS.breakable, transparent: true, opacity: 0.6, side: THREE.DoubleSide, depthWrite: false }),
  ];
  const overlay = new THREE.Mesh(geometry, overlayMaterials);
  overlay.visible = false;
  overlay.renderOrder = 10;

  // The pick proxy shares the same geometry but is never added to the scene graph - raycasting
  // against it directly (`raycaster.intersectObject(pickProxy, false)`) skips the "skip invisible
  // objects" check `Raycaster` applies during scene traversal, so it stays pickable regardless of
  // whether the overlay is shown.
  const pickProxy = new THREE.Mesh(geometry, overlayMaterials[0]);
  pickProxy.updateMatrixWorld(true);
  pickProxy.raycast = acceleratedRaycast;

  return { geometry, overlay, pickProxy };
}

// ---- the scene view --------------------------------------------------------------------------

export function createSceneView(container, map, mapSummary, initialTheme) {
  const renderer = new THREE.WebGLRenderer({ antialias: true });
  renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.toneMapping = THREE.NoToneMapping; // temporary lighting (F3b-2 replaces this) - keep it flat and predictable.
  container.append(renderer.domElement);
  // three.js keeps a module-global DFG LUT texture (three.module.js:16074) whose dispose listeners retain
  // every renderer ever created - so nothing reachable from renderer.domElement may keep this view alive.
  const domListeners = new AbortController();
  const canvasOn = (type, fn, opts = {}) => renderer.domElement.addEventListener(type, fn, { ...opts, signal: domListeners.signal });
  // Texture CPU copies are freed after upload (`materialTextures.js`), so a restored context can't
  // re-upload them - reload the page to rebuild the view.
  canvasOn("webglcontextrestored", () => {
    if (!destroyed) location.reload();
  });
  renderer.domElement.tabIndex = 0;
  renderer.domElement.style.outline = "none";
  renderer.domElement.setAttribute("aria-label", `3D-сцена карты ${map}`);

  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x0b1220);

  const camera = new THREE.PerspectiveCamera(VERTICAL_FOV_DEG, 1, 4, 40000);
  camera.up.set(0, 0, 1);
  camera.position.set(0, 0, 2000);
  // `s6f3a9_effects.md` review fixes: `lighting.js`'s own `FEATHER_LAYER`/`OVERLAY_LAYER` - a
  // depth-feather effects mesh, or an "always on top" scene helper (target beacon, hover ring,
  // area-draft tool), lives ONLY on one of these layers (not layer 0), so game mode's opaque/
  // feather/overlay three-pass split (`lighting.js`'s `renderFrame`) can select each independently.
  // Enabling both here too (in addition to the default layer 0) keeps everything visible whenever
  // this camera renders in a single pass instead - "simple" mode (which never calls `renderFrame`
  // at all), raycasting, a returning user whose stored preference is "simple", a map with no
  // feather material to split passes for in the first place.
  camera.layers.enable(FEATHER_LAYER);
  camera.layers.enable(OVERLAY_LAYER);

  // `s6f3b_viewer3d.md`: "свет — временный: солнце из render.json + полусферический".
  const hemi = new THREE.HemisphereLight(0x9fc3ff, 0x1a1f2b, 0.9);
  scene.add(hemi);
  const sun = new THREE.DirectionalLight(0xffffff, 3);
  sun.position.set(-2000, 2000, 3000);
  scene.add(sun);
  scene.add(sun.target);

  let destroyed = false;
  let renderGltf = null;
  let collision = null; // { geometry, overlay, pickProxy }
  let mapCenter = new THREE.Vector3(0, 0, 0);

  // ---- F3b-2 game lighting (`s6f3b2_lighting_shader.md` §7) - "game"/"simple" toggle, remembered
  // via localStorage; `lightingSupported` is only known once render.json arrives, so until then
  // `effectiveLightingMode()` stays "simple" regardless of the stored preference.
  let lightingMode = loadStoredLightingMode() === "simple" ? "simple" : "game";
  let lightingSupported = false;
  let lightingPipeline = null;
  function effectiveLightingMode() {
    return lightingSupported ? lightingMode : "simple";
  }
  function applyLightingMode() {
    if (!renderGltf) {
      return;
    }
    const useGame = lightingPipeline && effectiveLightingMode() === "game";
    renderGltf.scene.traverse((o) => {
      if (!o.isMesh || !o.userData.gameMaterial) {
        return;
      }
      o.material = useGame ? o.userData.gameMaterial : o.userData.simpleMaterial;
    });
  }

  // ---- resize -----------------------------------------------------------------------------------
  // `updateStyle: true` - the canvas's CSS box always gets an explicit size that matches its
  // drawing buffer, rather than relying on its `width`/`height` content attributes (which is all
  // an `updateStyle: false` call sets) to also happen to equal its CSS layout size.
  let lastWidth = 1;
  let lastHeight = 1;
  function resize() {
    const rect = container.getBoundingClientRect();
    const w = Math.max(1, rect.width);
    const h = Math.max(1, rect.height);
    lastWidth = w;
    lastHeight = h;
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h, true);
    // The canvas's drawing buffer is CSS size x pixel ratio (what `setSize(..., true)` just set) -
    // sizing the HDR target in CSS pixels alone rendered game lighting at half resolution whenever
    // `devicePixelRatio` was 2.
    const pr = renderer.getPixelRatio();
    lightingPipeline?.resize(w * pr, h * pr);
  }
  const resizeObserver = new ResizeObserver(resize);
  resizeObserver.observe(container);
  resize();

  // ---- camera modes: free flight (WASD + right-mouse-drag look) and orbit-around-a-point --------
  let mode = "fly"; // "fly" | "orbit" | "fpv"
  let yawDeg = 45;
  let pitchDeg = 45;
  let flySpeed = FLY_SPEED_DEFAULT;
  let cameraTouched = false; // user input or FPV has placed the camera - the late overview placement must not override it
  const keys = new Set();
  let pointerOver = false;
  let looking = false;
  let orbitDragging = false;
  // S6k hover preview: `lastMouse` is just the latest pointer position (cheap, updated on every
  // `pointermove`); the actual raycast against the collision mesh only runs once per animation
  // frame, and only if `lastMouse` moved since the last one (`updateHover`, below the area tool).
  let lastMouse = null;
  let lastRaycastMouse = null;

  function applyLookAngles() {
    const { forward, up } = sourceBasis(pitchDeg, yawDeg);
    camera.up.set(up.x, up.y, up.z);
    const target = new THREE.Vector3(camera.position.x + forward.x, camera.position.y + forward.y, camera.position.z + forward.z);
    camera.lookAt(target);
  }

  canvasOn("contextmenu", (e) => e.preventDefault());
  canvasOn("mouseenter", () => {
    pointerOver = true;
    // Take focus from whatever toolbar button was clicked last - otherwise Space ("fly up")
    // re-activates that focused <button> (toggles collisions / camera mode) instead of flying.
    renderer.domElement.focus({ preventScroll: true });
  });
  canvasOn("mouseleave", () => {
    pointerOver = false;
    hideHover();
  });
  canvasOn("pointermove", (e) => {
    lastMouse = { x: e.clientX, y: e.clientY };
  });
  canvasOn("mousedown", (e) => {
    if (mode !== "fly" || e.button !== 2) {
      return;
    }
    renderer.domElement.requestPointerLock?.();
  });
  // Pointer lock and keyboard state are necessarily document/window-level listeners (the Pointer
  // Lock API fires on `document`, and a key can be released after the pointer left the canvas) -
  // named so `destroy()` can remove them instead of leaking one set per 3D view ever opened.
  const onPointerLockChange = () => {
    looking = document.pointerLockElement === renderer.domElement;
  };
  const onDocMouseMove = (e) => {
    if (!looking || mode !== "fly") {
      return;
    }
    cameraTouched = true;
    yawDeg -= e.movementX * MOUSE_SENSITIVITY;
    pitchDeg = Math.max(-89, Math.min(89, pitchDeg + e.movementY * MOUSE_SENSITIVITY));
    applyLookAngles();
  };
  const onKeyDown = (e) => {
    if (!pointerOver && !looking) {
      return;
    }
    // S6k: Enter/Backspace/Esc drive the armed area tool instead of flying while one is drafting -
    // none of the three are otherwise used below (WASD/Space/Ctrl/Shift), so flying is unaffected.
    if (activeDraftKey && mode !== "fpv" && handleDraftKey(e)) {
      e.preventDefault();
      return;
    }
    if (e.code === "Space") {
      e.preventDefault(); // no page scroll, no button activation while flying
    }
    keys.add(e.code);
  };
  const onKeyUp = (e) => keys.delete(e.code);
  document.addEventListener("pointerlockchange", onPointerLockChange);
  document.addEventListener("mousemove", onDocMouseMove);
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("keyup", onKeyUp);

  canvasOn(
    "wheel",
    (e) => {
      if (!pointerOver) {
        return;
      }
      e.preventDefault();
      flySpeed = Math.max(FLY_SPEED_MIN, Math.min(FLY_SPEED_MAX, flySpeed * (e.deltaY < 0 ? FLY_SPEED_WHEEL_FACTOR : 1 / FLY_SPEED_WHEEL_FACTOR)));
    },
    { passive: false },
  );

  let onClickHandler = null;
  let onRightClickHandler = null;
  const raycaster = new THREE.Raycaster();
  // Review fix item 5: this raycaster only ever powers picks (target/origin/area-vertex clicks,
  // the hover preview) - all of them want the single nearest hit, never the full sorted list, so
  // `MeshBVH`'s accelerated raycast can stop at the first hit instead of walking every candidate
  // leaf. The origin marker's own downward ray (`updateOrigin`) is a separate `THREE.Raycaster`
  // instance and deliberately leaves this unset - it needs every hit along the ray, not just the
  // nearest, to pick the floor matching the Shift-click's own z out of a stack.
  raycaster.firstHitOnly = true;
  const ndc = new THREE.Vector2();
  let downAt = null;

  // Full hit info (point + world-space normal) against the collision `pickProxy` - shared by the
  // target/origin pick below, the area draft tool, and the hover preview (`raycastAt` is the
  // point-only shorthand the plain pick/area-vertex code only ever needed before hover existed).
  function raycastFullAt(clientX, clientY) {
    if (!collision) {
      return null;
    }
    const rect = renderer.domElement.getBoundingClientRect();
    ndc.x = ((clientX - rect.left) / rect.width) * 2 - 1;
    ndc.y = -((clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(ndc, camera);
    const hits = raycaster.intersectObject(collision.pickProxy, false);
    if (hits.length === 0) {
      return null;
    }
    const hit = hits[0];
    const normal = hit.face ? hit.face.normal.clone().transformDirection(hit.object.matrixWorld) : new THREE.Vector3(0, 0, 1);
    return { point: hit.point, normal };
  }
  function raycastAt(clientX, clientY) {
    return raycastFullAt(clientX, clientY)?.point ?? null;
  }

  canvasOn("pointerdown", (e) => {
    if (e.button === 0) {
      downAt = { x: e.clientX, y: e.clientY };
    }
  });
  canvasOn("pointerup", (e) => {
    if (e.button !== 0 || !downAt || mode === "fpv") {
      downAt = null;
      return;
    }
    const moved = Math.abs(e.clientX - downAt.x) + Math.abs(e.clientY - downAt.y);
    const shiftKey = e.shiftKey;
    const clientX = e.clientX;
    const clientY = e.clientY;
    downAt = null;
    if (moved > 6 || !collision) {
      return;
    }
    // S6k: a click while an area tool is armed only ever places/closes a vertex - it must not also
    // set the target (mirrors the 2D tool's own behaviour).
    if (activeDraftKey) {
      handleDraftClick(activeDraftKey, clientX, clientY);
      return;
    }
    const hit = raycastFullAt(clientX, clientY);
    if (!hit) {
      return;
    }
    const { point, normal } = hit;
    if (shiftKey) {
      onRightClickHandler?.(point.x, point.y, point.z);
      return;
    }
    // Review fix (`s6r_sightline_target.md`): the world-space hit normal's Z, so a sightline click
    // on a wall can tell itself apart from one on a floor/roof - every other consumer of `onClick`
    // (point target, area vertex) simply ignores this extra argument.
    onClickHandler?.(point.x, point.y, point.z, normal.z);
  });
  // S6k: closes the armed area tool's polygon, same as clicking its first vertex - the two clicks
  // making up this dblclick already each ran `handleDraftClick` via `pointerup` above (mirrors
  // map2d.js's own dblclick handling, including dropping the resulting near-duplicate vertex).
  canvasOn("dblclick", () => {
    if (!activeDraftKey || mode === "fpv" || !lastMouse) {
      return;
    }
    const a = draftAreas[activeDraftKey];
    if (a.closed || a.points.length < 2) {
      return;
    }
    const last1 = worldToScreenPx(a.points[a.points.length - 1]);
    const last2 = worldToScreenPx(a.points[a.points.length - 2]);
    if (Math.hypot(last1.x - last2.x, last1.y - last2.y) < DRAFT_CLOSE_PX) {
      a.points.pop();
    }
    if (a.points.length < 3) {
      return;
    }
    closeDraft(activeDraftKey);
  });

  // Orbit mode (`THREE.OrbitControls` handles arbitrary `camera.up` via its own basis realignment,
  // so it orbits correctly around our Z-up world).
  const orbit = new OrbitControls(camera, renderer.domElement);
  orbit.enabled = false;
  orbit.enablePan = true;
  orbit.maxDistance = 20000;
  orbit.addEventListener("start", () => {
    cameraTouched = true;
    orbitDragging = true;
  });
  orbit.addEventListener("end", () => {
    orbitDragging = false;
  });

  function setMode(next) {
    if (mode === next) {
      return;
    }
    cameraTouched = true;
    mode = next;
    orbit.enabled = mode === "orbit";
    if (mode === "orbit") {
      camera.up.set(0, 0, 1); // applyLookAngles left the tilted Source up here; OrbitControls.update() lookAt()s with it
      orbit.target.copy(currentTarget ? new THREE.Vector3(currentTarget.x, currentTarget.y, currentTarget.z) : mapCenter);
      orbit.update();
    } else if (mode === "fly") {
      // Resume flight from wherever the orbit camera ended up, looking at its current target.
      const dir = new THREE.Vector3();
      camera.getWorldDirection(dir);
      pitchDeg = (Math.asin(Math.max(-1, Math.min(1, -dir.z))) * 180) / Math.PI;
      yawDeg = (Math.atan2(dir.y, dir.x) * 180) / Math.PI;
      applyLookAngles();
    }
  }

  const clock = new THREE.Clock();
  function tickFly(dt) {
    if (mode !== "fly" || dt <= 0) {
      return;
    }
    const { forward, right } = sourceBasis(pitchDeg, yawDeg);
    const speed = flySpeed * (keys.has("ShiftLeft") || keys.has("ShiftRight") ? FLY_SPEED_SHIFT_MULT : 1) * dt;
    const move = new THREE.Vector3();
    if (keys.has("KeyW")) {
      move.x += forward.x;
      move.y += forward.y;
      move.z += forward.z;
    }
    if (keys.has("KeyS")) {
      move.x -= forward.x;
      move.y -= forward.y;
      move.z -= forward.z;
    }
    if (keys.has("KeyD")) {
      move.x += right.x;
      move.y += right.y;
    }
    if (keys.has("KeyA")) {
      move.x -= right.x;
      move.y -= right.y;
    }
    if (keys.has("Space")) {
      move.z += 1;
    }
    if (keys.has("ControlLeft") || keys.has("ControlRight")) {
      move.z -= 1;
    }
    if (move.lengthSq() > 0) {
      cameraTouched = true;
      move.normalize().multiplyScalar(speed);
      camera.position.add(move);
    }
  }

  let animHandle = null;
  function animate() {
    if (destroyed) {
      return;
    }
    animHandle = requestAnimationFrame(animate);
    const dt = clock.getDelta();
    tickFly(dt);
    if (mode === "orbit") {
      orbit.update();
    }
    updateHover();
    if (lightingPipeline && effectiveLightingMode() === "game") {
      // `s6f3a9_effects.md`: csgo_effects mask panning's live g_flTime-equivalent.
      lightingPipeline.renderFrame(scene, camera, { timeSeconds: clock.elapsedTime });
    } else {
      renderer.render(scene, camera);
    }
  }
  animHandle = requestAnimationFrame(animate);

  // ---- collision mesh load (raycasting + overlay) ----------------------------------------------
  const collisionReady = loadCollisionMesh(map)
    .then((c) => {
      if (destroyed) {
        disposeObject3D(c.overlay);
        return null;
      }
      collision = c;
      scene.add(c.overlay);
      const box = c.geometry.boundingBox;
      if (box) {
        box.getCenter(mapCenter);
        const size = new THREE.Vector3();
        box.getSize(size);
        const dist = Math.max(size.x, size.y, 400) * 0.65;
        const overview = new THREE.Vector3(mapCenter.x - dist * 0.5, mapCenter.y - dist * 0.5, box.max.z + dist * 0.8);
        if (mode === "fpv") {
          // FPV entered before /api/mesh arrived: keep the eye; make Esc return to the overview instead of (0,0,2000).
          if (fpvBefore && !fpvBefore.touched) {
            fpvBefore.pos.copy(overview);
          }
        } else if (!cameraTouched) {
          camera.position.copy(overview);
          applyLookAngles();
        }
      }
      return c;
    })
    .catch((e) => {
      console.error("collision mesh load failed", e);
      return null;
    });

  // render.json, fetched once and shared by the sun update below and `tintColorSpace` (§4) - only
  // when render.json is at least `MIN_RENDER_FORMAT_VERSION` (review fix item 11: an older format
  // version referenced textures through glTF's own `baseColorTexture`/`normalTexture`, which this
  // exporter no longer populates - loading one here would render every material untextured instead
  // of `main.js`'s "3D data must be rebuilt" message; `main.js`'s own `hasUsableRender` gate
  // already keeps `createSceneView` from being called at all for such a map, this is this module's
  // own defence in depth for any other caller).
  const usableRender = hasUsableRender(mapSummary);
  const renderJsonReady = usableRender ? fetchRenderJson(map) : Promise.resolve({ data: null });

  let loadProgressHandler = null;
  let loadDoneHandler = null;
  let loadErrorHandler = null;
  let lightingReadyHandler = null;

  // Review fix item 9: one combined progress report (`loaded`/`total` in bytes) across
  // render.glb's own fetch AND every `render_tex/*.bin` the native texture loader pulls in -
  // `total` grows from 0 as each side's own total becomes known (render.glb's `Content-Length`
  // header; the texture loader's total is the sum of render.json's own `textures[].byteLength`,
  // known as soon as render.json itself is parsed), `loaded` is the running sum of both.
  const progress = { glbLoaded: 0, glbTotal: 0, texLoaded: 0, texTotal: 0 };
  function reportProgress() {
    loadProgressHandler?.(progress.glbLoaded + progress.texLoaded, progress.glbTotal + progress.texTotal);
  }

  // One native-texture loader (`s6f3a6_native_tex.md`) per view, shared by BOTH the "simple"
  // material patches below (`applyMaterialExtras`) and the game-lighting pipeline
  // (`createLightingPipeline`, in `lightingReady`) - each map texture is otherwise fetched AND
  // uploaded to the GPU twice, once per lighting mode, doubling material-texture GPU memory and
  // network traffic for no reason (both modes read the identical render.json `textures[]`).
  // Review fix item 2: anisotropy must reach the GPU sampler at upload time (three.js's
  // `WebGLTextures.js` only sets `TEXTURE_MAX_ANISOTROPY` while first uploading a texture), so it's
  // passed into the loader itself rather than assigned on the resulting `THREE.Texture` afterward.
  const materialAnisotropy = Math.min(MAX_ANISOTROPY_CAP, renderer.capabilities.getMaxAnisotropy());
  const materialTexLoaderReady = renderJsonReady.then(({ data }) => {
    if (!data?.textures) {
      return null;
    }
    progress.texTotal = data.textures.reduce((a, t) => a + (t.byteLength || 0), 0);
    return createMaterialTextureLoader(map, data, renderer, {
      anisotropy: materialAnisotropy,
      onProgress: (bytesLoaded) => {
        progress.texLoaded = bytesLoaded;
        reportProgress();
      },
    });
  });
  // Review fix item 10 (s6f3a7_env_materials.md review, load time): `applyMaterialExtras`/
  // `buildGameMaterials` walk meshes one at a time and `await` each material's own textures
  // before moving to the next, so with ~930 materials on Inferno (191 of them newly needing
  // env1/envLayer2's up to 5 extra textures each) the *first* material's fetch doesn't even start
  // until render.glb has finished parsing, and every later material waits its turn behind
  // whichever came before it - `texLoader.get(index)` is memoized and its own concurrency queue
  // starts pumping the moment it's called, not the moment its caller awaits it, so firing every
  // texture index referenced anywhere in `materialExtras.byMaterial` here - as soon as the loader
  // exists, in parallel with render.glb's own fetch - lets the concurrent queue work through them
  // while the mesh loop is still building, instead of gating each one behind a whole material's
  // sequential turn.
  Promise.all([renderJsonReady, materialTexLoaderReady]).then(([{ data }, texLoader]) => {
    if (destroyed || !texLoader || !data?.materialExtras?.byMaterial) {
      return;
    }
    const indices = new Set();
    const collect = (obj) => {
      if (!obj || typeof obj !== "object") return;
      for (const [k, v] of Object.entries(obj)) {
        if (k.endsWith("Texture") && typeof v === "number") indices.add(v);
        else if (v && typeof v === "object") collect(v);
      }
    };
    for (const extras of Object.values(data.materialExtras.byMaterial)) collect(extras);
    for (const idx of indices) {
      texLoader.get(idx)?.catch(() => {}); // real consumers await the same promise and report errors themselves
    }
  });

  // ---- render.glb load (progress + Cache Storage by ETag + cancel via AbortController) -----------
  const glbController = new AbortController();

  // `s6f3b_viewer3d.md` F3b-1a "кэш браузера по ETag": `render.glb` is too large for Chrome's own
  // HTTP cache entry cap, so the revalidation is done by hand against Cache Storage instead of
  // relying on `cache: "default"` - `cache: "no-store"` below skips the (useless, for this file)
  // HTTP cache entirely and always round-trips our own `If-None-Match`.
  async function fetchArrayBufferWithProgress(url, signal, bypassCache = false) {
    let cache = null;
    let cached = null;
    try {
      cache = await caches.open(RENDER_GLB_CACHE_NAME);
      cached = bypassCache ? null : ((await cache.match(url)) ?? null);
    } catch {
      cache = null; // Cache Storage unavailable (e.g. an insecure origin) - fall back to a plain fetch.
    }
    const headers = {};
    const cachedEtag = cached?.headers.get("ETag");
    if (cachedEtag) {
      headers["If-None-Match"] = cachedEtag;
    }
    const res = await fetch(url, { signal, cache: "no-store", headers });
    if (res.status === 304 && cached) {
      try {
        const buf = await cached.arrayBuffer();
        progress.glbLoaded = buf.byteLength;
        progress.glbTotal = buf.byteLength;
        reportProgress();
        return buf;
      } catch {
        // The cached entry matched by ETag but can't actually be read back (a corrupt Cache
        // Storage backing store) - drop it and redownload rather than failing the whole load;
        // `bypassCache` on the retry stops this from looping if it somehow 304s again.
        cache.delete(url).catch(() => {});
        return fetchArrayBufferWithProgress(url, signal, true);
      }
    }
    if (!res.ok) {
      throw new Error(`HTTP ${res.status}`);
    }
    const total = Number(res.headers.get("Content-Length")) || 0;
    progress.glbTotal = total;
    let buffer;
    if (!res.body) {
      buffer = await res.arrayBuffer();
    } else {
      const reader = res.body.getReader();
      const chunks = [];
      let loaded = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        chunks.push(value);
        loaded += value.byteLength;
        progress.glbLoaded = loaded;
        reportProgress();
      }
      const buf = new Uint8Array(loaded);
      let at = 0;
      for (const c of chunks) {
        buf.set(c, at);
        at += c.byteLength;
      }
      buffer = buf.buffer;
    }
    if (cache && res.headers.get("ETag")) {
      cache.put(url, new Response(buffer, { headers: res.headers })).catch(() => {});
    }
    return buffer;
  }

  const gltfLoader = new GLTFLoader();
  // Harmless today (`render.glb` doesn't use EXT_meshopt_compression/KHR_meshopt_compression yet)
  // but `GLTFLoader` throws on a meshopt-compressed file with no decoder registered, and
  // crates/s2render is gaining that geometry compression concurrently with this file.
  gltfLoader.setMeshoptDecoder(MeshoptDecoder);
  const glbReady = (async () => {
    // Yields once before any handler-invoking branch runs (including the very next line) so a
    // caller that registers `onLoadError`/`onLoadProgress`/etc. right after `createSceneView`
    // returns (as `main.js` does) never misses a callback this function fires with zero `await`s
    // of its own before it - the stale-`formatVersion` bail-out right below used to call
    // `loadErrorHandler` fully synchronously, before `createSceneView` had even returned to its
    // caller, so nothing was listening yet.
    await Promise.resolve();
    if (!usableRender) {
      loadErrorHandler?.(strings.view3d.renderOutdated);
      return null;
    }
    let buffer;
    try {
      buffer = await fetchArrayBufferWithProgress(renderGlbUrl(map), glbController.signal);
    } catch (e) {
      if (e.name !== "AbortError" && !destroyed) {
        loadErrorHandler?.(e.message ?? String(e));
      }
      return null;
    }
    if (destroyed) {
      return null;
    }
    const gltf = await new Promise((resolve, reject) => {
      gltfLoader.parse(buffer, "", resolve, reject);
    }).catch((e) => {
      loadErrorHandler?.(e.message ?? String(e));
      return null;
    });
    if (!gltf || destroyed) {
      if (gltf) {
        disposeObject3D(gltf.scene);
      }
      return null;
    }
    const { data: renderJson } = await renderJsonReady;
    const tintIsSrgb = /^srgb/i.test(renderJson?.materialExtras?.tintColorSpace ?? "");
    const texLoader = await materialTexLoaderReady;
    if (texLoader) {
      await applyMaterialExtras(gltf, renderer, texLoader, renderJson, tintIsSrgb);
    }
    if (destroyed) {
      disposeObject3D(gltf.scene);
      return null;
    }
    renderGltf = gltf;
    scene.add(gltf.scene);
    loadDoneHandler?.();
    return gltf;
  })();

  // Sun direction/color from `render.json`, once it's ready - upgrades the placeholder directional
  // light without blocking `render.glb`'s own load.
  renderJsonReady.then(({ data }) => {
    if (destroyed || !data?.sun) {
      return;
    }
    const d = data.sun.direction;
    const c = data.sun.color ?? [1, 1, 1];
    sun.color.setRGB(c[0], c[1], c[2]);
    sun.intensity = Math.max(0.2, data.sun.brightness ?? 3);
    const dist = 4000;
    sun.position.set(mapCenter.x - d[0] * dist, mapCenter.y - d[1] * dist, mapCenter.z - d[2] * dist);
    sun.target.position.copy(mapCenter);
    sun.target.updateMatrixWorld();
  });

  // ---- F3b-2 game lighting: build once render.json + render.glb are both in, in parallel with
  // everything else - `lightingSupported` (and therefore the "game"/"simple" toggle) only becomes
  // true if this map's render.json actually carries F3a-4's lighting/sky/post-processing data.
  const lightingReady = renderJsonReady.then(async ({ data }) => {
    if (destroyed || !hasGameLightingData(data)) {
      lightingReadyHandler?.(false);
      return null;
    }
    lightingSupported = true;
    lightingReadyHandler?.(true);
    let pipeline;
    try {
      const texLoader = await materialTexLoaderReady;
      pipeline = await createLightingPipeline(renderer, map, data, texLoader);
    } catch (e) {
      console.error("lighting pipeline load failed", e);
      lightingSupported = false;
      lightingReadyHandler?.(false);
      return null;
    }
    if (destroyed) {
      pipeline.dispose();
      return null;
    }
    pipeline.resize(lastWidth * renderer.getPixelRatio(), lastHeight * renderer.getPixelRatio());
    const gltf = await glbReady;
    if (!gltf || destroyed) {
      pipeline.dispose();
      return null;
    }
    await pipeline.applyToGltf(gltf);
    if (destroyed) {
      pipeline.dispose();
      return null;
    }
    lightingPipeline = pipeline;
    applyLightingMode();
    return pipeline;
  });

  // ---- target / lineup visuals -------------------------------------------------------------------
  let currentTarget = null; // {x,y,z,label}
  const targetGroup = new THREE.Group();
  scene.add(targetGroup);
  function drawTarget() {
    targetGroup.clear();
    if (!currentTarget) {
      return;
    }
    const marker = new THREE.Mesh(
      new THREE.SphereGeometry(6, 12, 8),
      new THREE.MeshBasicMaterial({ color: 0xb3261e }),
    );
    marker.position.set(currentTarget.x, currentTarget.y, currentTarget.z);
    targetGroup.add(marker);
    const stem = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints([
        new THREE.Vector3(currentTarget.x, currentTarget.y, currentTarget.z - 40),
        new THREE.Vector3(currentTarget.x, currentTarget.y, currentTarget.z + 40),
      ]),
      new THREE.LineBasicMaterial({ color: 0xb3261e }),
    );
    targetGroup.add(stem);
    // A target set through a real surface (e.g. a window opening) can land on a floor entirely
    // hidden behind geometry from the current camera - a thin beacon drawn on top of everything,
    // reaching well above the marker, still shows where it is.
    const beacon = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints([
        new THREE.Vector3(currentTarget.x, currentTarget.y, currentTarget.z),
        new THREE.Vector3(currentTarget.x, currentTarget.y, currentTarget.z + 200),
      ]),
      new THREE.LineBasicMaterial({ color: 0xb3261e, transparent: true, opacity: 0.6, depthTest: false }),
    );
    beacon.renderOrder = 999;
    beacon.layers.set(OVERLAY_LAYER); // review fix item 2: `OVERLAY_LAYER`'s own doc comment.
    targetGroup.add(beacon);
  }

  // `s6r_sightline_target.md`: the sightline being drawn/solved for - a marker at each eye point
  // plus, once both are placed, a thick translucent lane between them at eye height. On
  // `OVERLAY_LAYER` (like the target beacon above) so it reads through whatever geometry the
  // straight line between the eyes would otherwise be hidden behind - the whole point of drawing
  // it is to show the lane a smoke has to seal.
  const SIGHTLINE_COLOR = 0x0e7490;
  const SIGHTLINE_LANE_HALF_WIDTH = 24;
  let currentSightline = null; // { from: {x,y,z}, to: {x,y,z}|null }
  const sightlineGroup = new THREE.Group();
  scene.add(sightlineGroup);
  function eyeMarker(p) {
    const marker = new THREE.Mesh(
      new THREE.SphereGeometry(6, 12, 8),
      new THREE.MeshBasicMaterial({ color: SIGHTLINE_COLOR, depthTest: false }),
    );
    marker.position.set(p.x, p.y, p.z);
    marker.renderOrder = 999;
    marker.layers.set(OVERLAY_LAYER);
    return marker;
  }
  function drawSightline() {
    disposeObject3D(sightlineGroup);
    sightlineGroup.clear();
    if (!currentSightline) {
      return;
    }
    const { from, to } = currentSightline;
    sightlineGroup.add(eyeMarker(from));
    if (!to) {
      return;
    }
    sightlineGroup.add(eyeMarker(to));
    const a = new THREE.Vector3(from.x, from.y, from.z);
    const b = new THREE.Vector3(to.x, to.y, to.z);
    const dir = new THREE.Vector3().subVectors(b, a);
    if (dir.lengthSq() < 1e-6) {
      return;
    }
    dir.normalize();
    const side = new THREE.Vector3(0, 0, 1).cross(dir).normalize().multiplyScalar(SIGHTLINE_LANE_HALF_WIDTH);
    const positions = [
      a.x - side.x, a.y - side.y, a.z - side.z, a.x + side.x, a.y + side.y, a.z + side.z,
      b.x + side.x, b.y + side.y, b.z + side.z, a.x - side.x, a.y - side.y, a.z - side.z,
      b.x + side.x, b.y + side.y, b.z + side.z, b.x - side.x, b.y - side.y, b.z - side.z,
    ];
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
    const lane = new THREE.Mesh(
      geometry,
      new THREE.MeshBasicMaterial({
        color: SIGHTLINE_COLOR, transparent: true, opacity: 0.35, side: THREE.DoubleSide,
        depthWrite: false, depthTest: false,
      }),
    );
    lane.renderOrder = 999;
    lane.layers.set(OVERLAY_LAYER);
    sightlineGroup.add(lane);
  }

  // ---- origin/target areas (`s6i_render_job_areas3d.md`): translucent prism "walls" around each
  // drawn area's polygon - cold for the throw area, warm for the landing area, the radar's own
  // areaOrigin/areaTarget hex - from a floor level up to a capped ceiling. Editing stays in 2D;
  // `setArea`/`clearArea` below just redraw from whatever `main.js` currently has. Basic/line
  // materials, like the target/lineup markers above, so they stay visible regardless of which
  // lighting pipeline is active (`applyLightingMode` never touches this group).
  const AREA_COLORS = { origin: 0x7c3aed, target: 0xc2410c };
  const AREA_DEFAULT_HEIGHT = 96; // `s6i_render_job_areas3d.md`: "или 96 единиц" when no zMax is given.
  const areaGroups = { origin: new THREE.Group(), target: new THREE.Group() };
  scene.add(areaGroups.origin, areaGroups.target);
  // Bumped on every `setArea`/`clearArea` call for that key - guards a slower `/api/levels` floor
  // lookup from overwriting a newer call's result once it finally resolves.
  const areaGen = { origin: 0, target: 0 };

  function clearAreaGroup(key) {
    disposeObject3D(areaGroups[key]);
    areaGroups[key].clear();
  }

  function buildAreaPrism(key, polygon, zMin, zMax) {
    clearAreaGroup(key);
    const color = AREA_COLORS[key];
    const positions = [];
    for (let i = 0; i < polygon.length; i++) {
      const [x0, y0] = polygon[i];
      const [x1, y1] = polygon[(i + 1) % polygon.length];
      positions.push(
        x0, y0, zMin, x1, y1, zMin, x1, y1, zMax,
        x0, y0, zMin, x1, y1, zMax, x0, y0, zMax,
      );
    }
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
    const walls = new THREE.Mesh(
      geometry,
      new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.25, side: THREE.DoubleSide, depthWrite: false }),
    );
    areaGroups[key].add(walls);

    const outline = (z) => {
      const pts = polygon.map(([x, y]) => new THREE.Vector3(x, y, z));
      pts.push(pts[0].clone());
      areaGroups[key].add(new THREE.Line(new THREE.BufferGeometry().setFromPoints(pts), new THREE.LineBasicMaterial({ color })));
    };
    outline(zMin);
    outline(zMax);
  }

  // The floor under `polygon`'s centroid, from `/api/levels` - the fallback source `setArea` uses
  // when it isn't given a `zMin` (`s6i_render_job_areas3d.md`: "иначе из /api/levels"); the
  // topmost level there, matching `main.js`'s own top-first default for a spot with several floors
  // stacked under it.
  async function resolveAreaFloor(polygon) {
    const cx = polygon.reduce((sum, p) => sum + p[0], 0) / polygon.length;
    const cy = polygon.reduce((sum, p) => sum + p[1], 0) / polygon.length;
    const { data } = await fetchLevels(map, cx, cy);
    const levels = data?.levels ?? [];
    return levels.length > 0 ? Math.max(...levels.map((l) => l.z)) : null;
  }

  function updateArea(key, polygon, zMin, zMax) {
    const gen = ++areaGen[key];
    if (!polygon || polygon.length < 3) {
      clearAreaGroup(key);
      return;
    }
    if (zMin != null) {
      buildAreaPrism(key, polygon, zMin, zMax != null ? zMax : zMin + AREA_DEFAULT_HEIGHT);
      return;
    }
    clearAreaGroup(key);
    resolveAreaFloor(polygon).then((floor) => {
      if (destroyed || gen !== areaGen[key] || floor == null) {
        return;
      }
      buildAreaPrism(key, polygon, floor, zMax != null ? zMax : floor + AREA_DEFAULT_HEIGHT);
    });
  }

  // ---- origin marker (`s6k_draw_in_3d.md` item 4): Shift+LMB in 3D calls `handleOriginClick`, same
  // as the 2D right-click - the marker needs the origin's own floor, found via a downward raycast
  // against the collision mesh from high above. `o.z`, when the click that set `o` just happened in
  // 3D, is the exact surface that raycast should reproduce (there can be another floor stacked
  // above it, e.g. a roof, that a plain "topmost hit" would pick instead).
  const ORIGIN_COLOR = 0x2563eb;
  const originGroup = new THREE.Group();
  scene.add(originGroup);
  let originGen = 0;

  function clearOriginGroup() {
    disposeObject3D(originGroup);
    originGroup.clear();
  }

  function buildOriginMarker(o, floorZ) {
    clearOriginGroup();
    const marker = new THREE.Mesh(new THREE.SphereGeometry(6, 12, 8), new THREE.MeshBasicMaterial({ color: ORIGIN_COLOR }));
    marker.position.set(o.x, o.y, floorZ + 2);
    originGroup.add(marker);
    if (o.reach > 0) {
      const segments = 48;
      const pts = [];
      for (let i = 0; i <= segments; i++) {
        const a = (i / segments) * Math.PI * 2;
        pts.push(new THREE.Vector3(o.x + Math.cos(a) * o.reach, o.y + Math.sin(a) * o.reach, floorZ + 2));
      }
      originGroup.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints(pts), new THREE.LineBasicMaterial({ color: ORIGIN_COLOR })));
    }
  }

  function updateOrigin(o) {
    const gen = ++originGen;
    if (!o) {
      clearOriginGroup();
      return;
    }
    collisionReady.then((c) => {
      if (destroyed || gen !== originGen || !c) {
        return;
      }
      const top = (c.geometry.boundingBox?.max.z ?? 20000) + 1000;
      const downRay = new THREE.Raycaster(new THREE.Vector3(o.x, o.y, top), new THREE.Vector3(0, 0, -1));
      const hits = downRay.intersectObject(c.pickProxy, false);
      let floorZ;
      if (o.z != null) {
        floorZ = hits.find((h) => h.point.z <= o.z + 1)?.point.z ?? hits[0]?.point.z ?? o.z;
      } else {
        floorZ = hits[0]?.point.z ?? o.z ?? 0;
      }
      buildOriginMarker(o, floorZ);
    });
  }

  // ---- area drawing tool (`s6k_draw_in_3d.md` item 1): the same `setAreaMode`/`onAreaChange`
  // interface map2d.js exposes, drafting a polygon vertex-by-vertex directly in 3D instead of only
  // on the 2D radar. `draftAreas[key].points` keeps the hit z per vertex (`{x,y,z}`) - `main.js`
  // needs those heights (item 2) to set the z range from where the polygon was actually clicked.
  const draftGroups = { origin: new THREE.Group(), target: new THREE.Group() };
  scene.add(draftGroups.origin, draftGroups.target);
  const draftAreas = {
    origin: { points: [], closed: false, onChange: null },
    target: { points: [], closed: false, onChange: null },
  };
  let activeDraftKey = null;
  const DRAFT_CLOSE_PX = 12;

  function clearDraftGroup(key) {
    disposeObject3D(draftGroups[key]);
    draftGroups[key].clear();
  }

  function emitDraftChange(key) {
    const a = draftAreas[key];
    a.onChange?.({
      points: a.points.map((p) => ({ x: p.x, y: p.y })),
      closed: a.closed,
      zs: a.points.map((p) => p.z),
    });
  }

  function worldToScreenPx(p) {
    const v = new THREE.Vector3(p.x, p.y, p.z).project(camera);
    const rect = renderer.domElement.getBoundingClientRect();
    return { x: rect.left + (v.x * 0.5 + 0.5) * rect.width, y: rect.top + (-v.y * 0.5 + 0.5) * rect.height };
  }

  // Vertex markers + polyline + (while still drafting) a rubber-band segment out to `cursorPoint` -
  // all drawn on top of the rest of the scene (`depthTest:false`, a high `renderOrder`), in the same
  // colour the committed prism for this key already uses (`AREA_COLORS`).
  function renderDraftGroup(key, cursorPoint) {
    clearDraftGroup(key);
    const a = draftAreas[key];
    if (a.points.length === 0) {
      return;
    }
    const color = AREA_COLORS[key];
    const group = draftGroups[key];
    const dotMaterial = new THREE.MeshBasicMaterial({ color, depthTest: false, depthWrite: false });
    for (const p of a.points) {
      const dot = new THREE.Mesh(new THREE.SphereGeometry(5, 10, 8), dotMaterial);
      dot.position.set(p.x, p.y, p.z);
      dot.renderOrder = 999;
      dot.layers.set(OVERLAY_LAYER); // review fix item 2: `OVERLAY_LAYER`'s own doc comment.
      group.add(dot);
    }
    const linePts = a.points.map((p) => new THREE.Vector3(p.x, p.y, p.z));
    if (cursorPoint && !a.closed) {
      linePts.push(new THREE.Vector3(cursorPoint.x, cursorPoint.y, cursorPoint.z));
    } else if (a.closed && linePts.length >= 3) {
      linePts.push(linePts[0].clone());
    }
    if (linePts.length >= 2) {
      const line = new THREE.Line(
        new THREE.BufferGeometry().setFromPoints(linePts),
        new THREE.LineBasicMaterial({ color, depthTest: false }),
      );
      line.renderOrder = 999;
      line.layers.set(OVERLAY_LAYER); // review fix item 2: `OVERLAY_LAYER`'s own doc comment.
      group.add(line);
    }
  }

  function nearestFirstVertexHit(key, clientX, clientY) {
    const a = draftAreas[key];
    if (a.points.length === 0) {
      return false;
    }
    const s = worldToScreenPx(a.points[0]);
    return Math.hypot(clientX - s.x, clientY - s.y) <= DRAFT_CLOSE_PX;
  }

  function closeDraft(key) {
    draftAreas[key].closed = true;
    renderDraftGroup(key, null); // drop the rubber-band segment now that there is no "next vertex"
    emitDraftChange(key);
  }

  function handleDraftClick(key, clientX, clientY) {
    const a = draftAreas[key];
    if (a.closed) {
      return;
    }
    if (a.points.length >= 3 && nearestFirstVertexHit(key, clientX, clientY)) {
      closeDraft(key);
      return;
    }
    const point = raycastAt(clientX, clientY);
    if (!point) {
      return;
    }
    a.points.push({ x: point.x, y: point.y, z: point.z });
    emitDraftChange(key);
    renderDraftGroup(key, point);
  }

  // Enter/Backspace/Esc, same keys and meanings as the 2D tool's own hint text
  // (`strings.solveParams.areaHint`) - returns whether it consumed the key, so `onKeyDown` above
  // knows not to also feed it into the fly-movement key set.
  function handleDraftKey(e) {
    const key = activeDraftKey;
    const a = draftAreas[key];
    if (a.closed) {
      return false;
    }
    if (e.key === "Enter" && a.points.length >= 3) {
      closeDraft(key);
      return true;
    }
    if (e.key === "Backspace" && a.points.length > 0) {
      a.points.pop();
      emitDraftChange(key);
      renderDraftGroup(key, null);
      return true;
    }
    if (e.key === "Escape" && a.points.length > 0) {
      a.points = [];
      emitDraftChange(key);
      clearDraftGroup(key);
      return true;
    }
    return false;
  }

  // ---- hover preview (coordinator follow-up to `s6k_draw_in_3d.md`): a ring at the surface the
  // cursor is over, plus a small floating "z <height>[, place]" label, so a pick landing behind
  // geometry (or on an unexpected floor) is obvious before the user commits to it. Also doubles as
  // the area tool's own "next vertex" preview, reusing the same once-per-frame raycast instead of a
  // second one (`updateHover`, called from `animate()`).
  const hoverRing = new THREE.Mesh(
    new THREE.RingGeometry(10, 13, 24),
    new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.85, side: THREE.DoubleSide, depthTest: false }),
  );
  hoverRing.renderOrder = 998;
  hoverRing.layers.set(OVERLAY_LAYER); // review fix item 2: `OVERLAY_LAYER`'s own doc comment.
  hoverRing.visible = false;
  scene.add(hoverRing);
  const hoverLabel = document.createElement("div");
  hoverLabel.style.cssText =
    "position:fixed; pointer-events:none; display:none; font: 11px sans-serif; padding:2px 6px; " +
    "border-radius:3px; background:rgba(0,0,0,0.65); color:#fff; white-space:nowrap; z-index:5; transform:translateY(-50%);";
  document.body.append(hoverLabel);

  const HOVER_PLACE_REUSE_DIST = 16;
  const HOVER_PLACE_THROTTLE_MS = 250;
  let hoverPoint = null; // the current hover hit, kept fresh so an async repaint never labels a stale spot
  let hoverPlaceCache = null; // { x, y, levels } - last `/api/levels` answer, reused only within `HOVER_PLACE_REUSE_DIST`
  let hoverPlaceFetchedAt = 0;
  let hoverPlaceFetching = false;
  let hoverRelabelTimer = null; // review fix item 6's one trailing retry - see `updateHoverLabel`

  function placeNameFor(levels, z) {
    if (!levels || levels.length === 0) {
      return null;
    }
    let best = levels[0];
    let bestDist = Math.abs(best.z - z);
    for (const lvl of levels) {
      const d = Math.abs(lvl.z - z);
      if (d < bestDist) {
        bestDist = d;
        best = lvl;
      }
    }
    return bestDist <= 64 ? (best.name ?? null) : null;
  }

  // Review fix item 6: a stale place name (or a fresh one shown for the wrong spot) could appear
  // two ways - `hoverPlaceCache` used for display regardless of how far `point` had since moved
  // from it, and an async repaint (the fetch's own `.then`) relabelling the *point that started the
  // fetch* instead of wherever the cursor actually is by the time it resolves. Fixed by only ever
  // trusting the cache within `HOVER_PLACE_REUSE_DIST`, and always repainting from `hoverPoint` (the
  // latest one) rather than a captured argument.
  function updateHoverLabel(point) {
    hoverPoint = point;
    const zText = `z ${Math.round(point.z)}`;
    const withinReuse = hoverPlaceCache && Math.hypot(point.x - hoverPlaceCache.x, point.y - hoverPlaceCache.y) <= HOVER_PLACE_REUSE_DIST;
    const place = withinReuse ? placeNameFor(hoverPlaceCache.levels, point.z) : null;
    hoverLabel.textContent = place ? `${place}, ${zText}` : zText;
    hoverLabel.style.display = "block";
    hoverLabel.style.left = `${lastMouse.x + 12}px`;
    hoverLabel.style.top = `${lastMouse.y}px`;

    if (withinReuse || hoverPlaceFetching) {
      return; // already have, or are already fetching, an answer close enough to here
    }
    const elapsed = Date.now() - hoverPlaceFetchedAt;
    if (elapsed < HOVER_PLACE_THROTTLE_MS) {
      // Still inside the throttle window - one trailing retry once it opens, so a cursor that stops
      // moving right on the boundary still ends up with a fresh label instead of none at all (the
      // per-frame `updateHover` gate above only re-runs this on the *next* mouse movement).
      if (!hoverRelabelTimer) {
        hoverRelabelTimer = setTimeout(() => {
          hoverRelabelTimer = null;
          if (hoverRing.visible && hoverPoint) {
            updateHoverLabel(hoverPoint);
          }
        }, HOVER_PLACE_THROTTLE_MS - elapsed);
      }
      return;
    }
    hoverPlaceFetching = true;
    hoverPlaceFetchedAt = Date.now();
    fetchLevels(map, point.x, point.y)
      .then(({ data }) => {
        hoverPlaceFetching = false;
        hoverPlaceCache = { x: point.x, y: point.y, levels: data?.levels ?? [] };
        if (hoverRing.visible && hoverPoint) {
          updateHoverLabel(hoverPoint); // repaint from wherever the cursor is now, not this fetch's own point
        }
      })
      .catch(() => {
        hoverPlaceFetching = false;
      });
  }

  function hideHover() {
    hoverRing.visible = false;
    hoverLabel.style.display = "none";
    lastRaycastMouse = null;
    hoverPoint = null;
    if (hoverRelabelTimer) {
      clearTimeout(hoverRelabelTimer);
      hoverRelabelTimer = null;
    }
  }

  function updateHover() {
    if (!pointerOver || mode === "fpv" || looking || orbitDragging || !lastMouse) {
      hideHover();
      return;
    }
    if (lastRaycastMouse && lastRaycastMouse.x === lastMouse.x && lastRaycastMouse.y === lastMouse.y) {
      return; // pointer hasn't moved since the last raycast - nothing to redo
    }
    lastRaycastMouse = lastMouse;
    const hit = raycastFullAt(lastMouse.x, lastMouse.y);
    if (!hit) {
      hideHover();
      return;
    }
    hoverRing.visible = true;
    hoverRing.position.copy(hit.point).addScaledVector(hit.normal, 0.5);
    hoverRing.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), hit.normal);
    updateHoverLabel(hit.point);
    if (activeDraftKey && !draftAreas[activeDraftKey].closed) {
      renderDraftGroup(activeDraftKey, hit.point);
    }
  }

  let lineups = [];
  let selectedId = null;
  const lineupGroup = new THREE.Group();
  scene.add(lineupGroup);

  let visualsGen = 0; // bumped on every selection change - a slower, older fetch must not draw into the new one
  function clearLineupVisuals() {
    visualsGen++;
    disposeObject3D(lineupGroup);
    lineupGroup.clear();
  }

  async function buildLineupVisuals(l) {
    clearLineupVisuals();
    const gen = visualsGen;
    const crouched = isCrouchType(l.type);
    const feet = new THREE.Vector3(l.feet[0], l.feet[1], l.feet[2]);
    const eyeZ = feet.z + eyeHeight(crouched);

    // Player marker + facing arrow (S6n: "same visuals as `stand`" - shared with `capturePreview`).
    addStandMarkers(lineupGroup, feet, l.yaw, crouched);

    // Aim line: eye -> forward direction, out to a fixed reach (illustrative, not the throw arc).
    const { forward } = sourceBasis(l.pitch, l.yaw);
    const aimEnd = new THREE.Vector3(feet.x + forward.x * 512, feet.y + forward.y * 512, eyeZ + forward.z * 512);
    lineupGroup.add(
      new THREE.Line(
        new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(feet.x, feet.y, eyeZ), aimEnd]),
        new THREE.LineDashedMaterial({ color: 0x9aa1ad, dashSize: 8, gapSize: 6 }),
      ).computeLineDistances(),
    );

    // Rest point.
    const rest = new THREE.Mesh(new THREE.SphereGeometry(5, 10, 8), new THREE.MeshBasicMaterial({ color: 0x1a7f37 }));
    rest.position.set(l.rest[0], l.rest[1], l.rest[2]);
    lineupGroup.add(rest);

    // Trajectory arc + bounce marks - `contacts` is the sim's own recorded `BounceRecord.contact`
    // list (`crates/server/src/physics.rs`), not a Z-local-minimum guess over the tick points.
    const { data: traj } = await fetchTrajectory(map, {
      x: l.feet[0],
      y: l.feet[1],
      z: l.feet[2],
      type: l.type,
      pitch: l.pitch,
      yaw: l.yaw,
      strength: l.strength,
      runDeg: l.runDeg,
      broken: l.broken,
    });
    if (gen !== visualsGen || destroyed) {
      return;
    }
    if (traj?.points?.length > 1) {
      const pts = traj.points.map((p) => new THREE.Vector3(p[0], p[1], p[2]));
      lineupGroup.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints(pts), new THREE.LineBasicMaterial({ color: 0xffb020 })));
    }
    for (const b of traj?.contacts ?? []) {
      const mark = new THREE.Mesh(new THREE.SphereGeometry(3, 8, 6), new THREE.MeshBasicMaterial({ color: 0xffb020 }));
      mark.position.set(b[0], b[1], b[2]);
      lineupGroup.add(mark);
    }

    // Smoke volume at the rest point (`s6f3b_viewer3d.md`: "ячейки - полупрозрачные инстансы").
    const { data: smoke } = await fetchSmoke(map, l.rest[0], l.rest[1], l.rest[2]);
    if (gen !== visualsGen || destroyed) {
      return;
    }
    if (smoke?.cells?.length > 0) {
      const count = smoke.cells.length / 3;
      const cellSize = smoke.voxel || 16;
      const inst = new THREE.InstancedMesh(
        new THREE.BoxGeometry(cellSize * 0.95, cellSize * 0.95, cellSize * 0.95),
        new THREE.MeshBasicMaterial({ color: 0xd7dae0, transparent: true, opacity: 0.16, depthWrite: false }),
        count,
      );
      const m = new THREE.Matrix4();
      for (let i = 0; i < count; i++) {
        m.setPosition(smoke.cells[i * 3], smoke.cells[i * 3 + 1], smoke.cells[i * 3 + 2]);
        inst.setMatrixAt(i, m);
      }
      inst.instanceMatrix.needsUpdate = true;
      lineupGroup.add(inst);
    }
  }

  // ---- first-person view (`s6f3b_viewer3d.md` F3b-1c) --------------------------------------------
  let fpvExit = null;
  let fpvBefore = null;

  function enterFirstPerson(l, onExit) {
    // The free-fly camera's position/angles before entering FPV - `exitFirstPerson` must put the
    // camera back here, not leave it stranded at the throw position (likely inside/against
    // geometry, since that's ground level, not wherever the user had been flying).
    // Switching lineups while already in FPV keeps the ORIGINAL pre-FPV pose (not the previous lineup's eye/"fpv").
    const before = mode === "fpv" && fpvBefore ? fpvBefore : { pos: camera.position.clone(), yaw: yawDeg, pitch: pitchDeg, mode, touched: cameraTouched };
    fpvBefore = before;
    cameraTouched = true;
    mode = "fpv";
    orbit.enabled = false;
    const crouched = isCrouchType(l.type);
    const feet = new THREE.Vector3(l.feet[0], l.feet[1], l.feet[2]);
    const eyeZ = feet.z + eyeHeight(crouched);
    camera.position.set(feet.x, feet.y, eyeZ);
    yawDeg = l.yaw;
    pitchDeg = l.pitch;
    applyLookAngles();

    fpvExit = () => {
      mode = before.mode;
      orbit.enabled = mode === "orbit";
      camera.position.copy(before.pos);
      yawDeg = before.yaw;
      pitchDeg = before.pitch;
      applyLookAngles();
      if (mode === "orbit") {
        camera.up.set(0, 0, 1);
        orbit.update();
      }
      onExit?.();
    };
    return { console: l.console, type: l.type, click: l.click, how: l.how };
  }

  function exitFirstPerson() {
    fpvExit?.();
    fpvExit = null;
  }

  // ---- S6n lineup previews (`s6n_lineup_previews.md`): still images for a lineup card - own
  // camera, own offscreen render target, never touches `camera`/`orbit`/`mode` or the canvas, so it
  // works mid-flight/orbit and never flashes the interactive view with the preview camera.

  // render.glb loads itself as soon as the view is created (`glbReady`, above) - this just lets a
  // caller await that instead of polling `capturePreview`'s own null-if-not-loaded return. Review
  // fix item 5: also waits for game lighting and the collision mesh, so the very first capture a
  // caller makes right after this resolves isn't silently stuck in "simple" mode (and then cached
  // forever by the UI) just because `lightingReady`/`collisionReady` hadn't settled yet.
  async function ensureLoaded() {
    const [gltf] = await Promise.all([glbReady, lightingReady.catch(() => null), collisionReady]);
    return gltf !== null;
  }

  // Review fix item 7: scores each of 8 candidate directions around `lookAt` (at `dist`/`upOffset`
  // from it) instead of just taking the first with a clear single ray - `sampleTargets` (e.g. a
  // capsule's top/mid/base) are each raycast from the candidate position against the collision
  // `pickProxy` (`MeshBVH`'s accelerated raycast, same as every other pick in this file); a
  // direction is scored by how many of them are visible, then by how much open space is behind the
  // candidate camera position itself (`openness`, a ray cast from `lookAt` outward through the
  // camera spot) - a camera jammed into the very wall/corner the player is pressed against scores
  // low even when its single center ray to `lookAt` happens to clear that wall's edge, so this
  // prefers the side that actually shows that wall behind the player instead of hiding behind it.
  // Falls back to the first candidate if the collision mesh hasn't loaded yet, or every direction is
  // fully blocked.
  function pickBestCameraPos(lookAt, dist, upOffset, preferredAngle, sampleTargets = [lookAt]) {
    let best = null;
    let bestScore = -Infinity;
    for (let i = 0; i < 8; i++) {
      const angle = preferredAngle + (i / 8) * Math.PI * 2;
      const pos = new THREE.Vector3(lookAt.x + Math.cos(angle) * dist, lookAt.y + Math.sin(angle) * dist, lookAt.z + upOffset);
      if (!collision) {
        return pos;
      }
      if (!best) {
        best = pos; // always have a fallback, even if every direction below scores -1 (fully blocked)
      }
      let visible = 0;
      for (const target of sampleTargets) {
        const toTarget = target.clone().sub(pos);
        const rayLen = toTarget.length();
        const ray = new THREE.Raycaster(pos, toTarget.normalize(), 0, Math.max(0, rayLen - 4));
        ray.firstHitOnly = true;
        if (ray.intersectObject(collision.pickProxy, false).length === 0) {
          visible++;
        }
      }
      if (visible === 0) {
        continue;
      }
      const outward = pos.clone().sub(lookAt).normalize();
      const openRay = new THREE.Raycaster(lookAt, outward, 0, dist);
      openRay.firstHitOnly = true;
      const openHits = openRay.intersectObject(collision.pickProxy, false);
      const openness = openHits.length > 0 ? openHits[0].distance : dist;
      const score = visible * 10000 + openness;
      if (score > bestScore) {
        bestScore = score;
        best = pos;
      }
    }
    return best;
  }

  // Renders `scene` from `previewCamera` into a fresh offscreen `WebGLRenderTarget` at `w`x`h`,
  // through whichever lighting pipeline the interactive view currently uses - "game" mode gets its
  // own scratch HDR buffer sized for this capture (the shared `hdrTarget` is sized for the
  // interactive canvas, not this resolution). Caller disposes the returned target. Synchronous and
  // self-contained (review fix items 2/3): saves/restores the renderer's own current target and
  // `autoClear` around its own render, so two `capturePreview` calls racing via `Promise.all` (or a
  // second call starting before an earlier one's final `await canvasToBlob` resolves) never see or
  // clobber each other's target - `renderOffscreen`/`capturePreview`'s synchronous section never
  // yields, so no other code can run between "bind a target" and "restore the previous one".
  function renderOffscreen(previewCamera, w, h) {
    // Review fix item 4: `lookAt`/`.quaternion` changes don't touch `matrixWorld` - it's normally
    // refreshed lazily by `WebGLRenderer.render()` itself, but `lightingSky.js`'s sky pass reads
    // `camera.matrixWorld` directly and runs BEFORE `renderer.render(scene, camera)` in
    // `renderFrame` below, so it would otherwise sample the sky with a stale (pre-`lookAt`)
    // rotation on every capture.
    previewCamera.updateMatrixWorld();
    // Rebase-onto-main fix: feather-flagged effects meshes (Mirage's mid dust haze,
    // `FEATHER_LAYER`) and "always on top" helpers (`OVERLAY_LAYER`) live OFF layer 0 entirely -
    // copying the interactive `camera`'s own mask (layer 0 + both) is what keeps them visible.
    // `lighting.js`'s `renderFrame` overrides the mask itself per-pass for the opaque/feather/
    // overlay three-pass split below, but "simple" mode renders in a single plain `renderer.render`
    // call with no such override, so a preview camera stuck on the `THREE.Camera` default (layer 0
    // only) would silently drop feather-flagged dust cards from simple-mode previews.
    previewCamera.layers.mask = camera.layers.mask;
    // Review fix item 1: `lightingPost.js`'s post pass already writes out fragment-shader-encoded
    // sRGB bytes (a hand-written `RawShaderMaterial`, no `colorspace_fragment` chunk) - tagging its
    // output target `SRGBColorSpace` made the GPU encode those already-encoded bytes a second time
    // (washed out). "Simple" mode's stock `MeshStandardMaterial`/`MeshLambertMaterial` shaders DO
    // rely on the target's own `SRGBColorSpace` to do that encode, so only "game" mode's target
    // switches to `NoColorSpace`.
    const game = !!(lightingPipeline && effectiveLightingMode() === "game");
    const target = new THREE.WebGLRenderTarget(w, h, {
      type: THREE.UnsignedByteType,
      format: THREE.RGBAFormat,
      colorSpace: game ? THREE.NoColorSpace : THREE.SRGBColorSpace,
      minFilter: THREE.LinearFilter,
      magFilter: THREE.LinearFilter,
      depthBuffer: true,
      stencilBuffer: false,
    });
    const prevTarget = renderer.getRenderTarget();
    const prevAutoClear = renderer.autoClear;
    if (game) {
      const hdr = createHdrTarget(renderer, w, h);
      // No `timeSeconds`: omitting it leaves `uTime` at whatever the live interactive loop last set
      // it to (`s6n_lineup_previews.md`'s own "cache nothing, disturb nothing" - a capture doesn't
      // need to advance csgo_effects's mask panning animation).
      lightingPipeline.renderFrame(scene, previewCamera, { outputTarget: target, overrideHdrTarget: hdr });
      hdr.dispose();
    } else {
      renderer.setRenderTarget(target);
      renderer.autoClear = true;
      renderer.render(scene, previewCamera);
    }
    renderer.setRenderTarget(prevTarget);
    renderer.autoClear = prevAutoClear;
    return target;
  }

  function readPixels(target, w, h) {
    const buffer = new Uint8Array(w * h * 4);
    renderer.readRenderTargetPixels(target, 0, 0, w, h, buffer);
    return buffer;
  }

  async function capturePreview({ kind, feet: feetArr, pitch, yaw, type, rest: restArr, trajectory, width, height, inset }) {
    if (!renderGltf) {
      return null;
    }
    if (kind !== "aim" && kind !== "stand" && kind !== "land") {
      return null;
    }
    const w = width || PREVIEW_DEFAULT_WIDTH;
    const h = height || PREVIEW_DEFAULT_HEIGHT;
    const t0 = performance.now();
    const crouched = isCrouchType(type);
    const feet = feetArr ? new THREE.Vector3(feetArr[0], feetArr[1], feetArr[2]) : null;
    const rest = restArr ? new THREE.Vector3(restArr[0], restArr[1], restArr[2]) : null;
    if ((kind === "aim" || kind === "stand") && !feet) {
      return null;
    }
    if (kind === "land" && !rest) {
      return null;
    }

    const previewCamera = new THREE.PerspectiveCamera(VERTICAL_FOV_DEG, w / h, camera.near, camera.far);
    const tempGroup = new THREE.Group();
    scene.add(tempGroup);
    // Review fix item 6: never let the interactive view's own overlays (the selected lineup, the
    // collision overlay when shown, the hover ring, area prisms/drafts, the target/origin markers)
    // leak into a capture - hidden for the synchronous render section below, restored exactly as
    // found in `finally`.
    const hidden = [lineupGroup, collision?.overlay, hoverRing, areaGroups.origin, areaGroups.target, originGroup, draftGroups.origin, draftGroups.target, targetGroup].filter(
      Boolean,
    );
    const hiddenWas = hidden.map((o) => o.visible);
    let mainTarget = null;
    let insetTarget = null;
    let canvas = null;
    // Review fix items 2/3: everything renderer/scene-touching below is synchronous (no `await`) -
    // cleanup runs in `finally`, still synchronously, before the one and only `await` in this whole
    // function (the final `canvasToBlob`, after `finally`) - so a second `capturePreview` call
    // racing via `Promise.all` always finds a clean baseline (nothing hidden, nothing added, the
    // renderer's own target/autoClear back to what they were) whenever ITS OWN synchronous section
    // runs, instead of tripping over this call's still-live temp group or bound render target.
    try {
      hidden.forEach((o) => {
        o.visible = false;
      });
      if (kind === "aim") {
        previewCamera.position.set(feet.x, feet.y, feet.z + eyeHeight(crouched));
        pointCameraAt(previewCamera, pitch, yaw);
        mainTarget = renderOffscreen(previewCamera, w, h);
        canvas = pixelsToCanvas(readPixels(mainTarget, w, h), w, h);
        const ctx = canvas.getContext("2d");
        drawCrosshair(ctx, w / 2, h / 2, 9, 3, 2, "#39ff6a");
        if (inset !== false) {
          const insetSize = Math.round(Math.min(w, h) * PREVIEW_INSET_FRACTION);
          const insetCamera = new THREE.PerspectiveCamera(PREVIEW_INSET_FOV_DEG, 1, camera.near, camera.far);
          insetCamera.position.copy(previewCamera.position);
          pointCameraAt(insetCamera, pitch, yaw);
          insetTarget = renderOffscreen(insetCamera, insetSize, insetSize);
          const insetCanvas = pixelsToCanvas(readPixels(insetTarget, insetSize, insetSize), insetSize, insetSize);
          const dx = w - insetSize - PREVIEW_INSET_MARGIN;
          const dy = PREVIEW_INSET_MARGIN;
          const r = insetSize / 2;
          ctx.save();
          ctx.beginPath();
          ctx.arc(dx + r, dy + r, r, 0, Math.PI * 2);
          ctx.closePath();
          ctx.clip();
          ctx.drawImage(insetCanvas, dx, dy);
          ctx.restore();
          ctx.lineWidth = 2;
          ctx.strokeStyle = "rgba(255,255,255,0.9)";
          ctx.beginPath();
          ctx.arc(dx + r, dy + r, r, 0, Math.PI * 2);
          ctx.stroke();
          drawCrosshair(ctx, dx + r, dy + r, 6, 2, 1, "#39ff6a");
        }
      } else if (kind === "stand") {
        addStandMarkers(tempGroup, feet, yaw, crouched);
        const h2 = hullHeight(crouched);
        const lookAt = new THREE.Vector3(feet.x, feet.y, feet.z + h2 * PREVIEW_STAND_LOOK_FRACTION);
        // Prefer directly behind the player's own facing (spec: "behind ... looking at it") -
        // `pickBestCameraPos` scores all 8 directions from there and picks the best, not just the
        // first with a clear single ray.
        const standAngle = (yaw * Math.PI) / 180 + Math.PI;
        const sampleTargets = [
          new THREE.Vector3(feet.x, feet.y, feet.z + h2 * 0.9),
          new THREE.Vector3(feet.x, feet.y, feet.z + h2 * 0.5),
          new THREE.Vector3(feet.x, feet.y, feet.z + h2 * 0.15),
        ];
        previewCamera.position.copy(pickBestCameraPos(lookAt, PREVIEW_STAND_BACK, PREVIEW_STAND_UP, standAngle, sampleTargets));
        previewCamera.up.set(0, 0, 1);
        previewCamera.lookAt(lookAt);
        mainTarget = renderOffscreen(previewCamera, w, h);
        canvas = pixelsToCanvas(readPixels(mainTarget, w, h), w, h);
      } else if (kind === "land") {
        addLandMarkers(tempGroup, rest, trajectory);
        // Default to looking back along the incoming flight path when a trajectory is given (a
        // more legible angle than an arbitrary fixed direction); `pickBestCameraPos` still scores
        // all 8 directions from there.
        let preferredAngle = -(Math.PI * 3) / 4;
        if (Array.isArray(trajectory) && trajectory.length > 1) {
          const [ax, ay] = trajectory[trajectory.length - 2];
          const [bx, by] = trajectory[trajectory.length - 1];
          const dx = bx - ax;
          const dy = by - ay;
          if (dx !== 0 || dy !== 0) {
            preferredAngle = Math.atan2(dy, dx);
          }
        }
        previewCamera.position.copy(pickBestCameraPos(rest, PREVIEW_LAND_BACK, PREVIEW_LAND_UP, preferredAngle));
        previewCamera.up.set(0, 0, 1);
        previewCamera.lookAt(rest);
        mainTarget = renderOffscreen(previewCamera, w, h);
        canvas = pixelsToCanvas(readPixels(mainTarget, w, h), w, h);
      }
    } finally {
      scene.remove(tempGroup);
      disposeObject3D(tempGroup);
      mainTarget?.dispose();
      insetTarget?.dispose();
      hidden.forEach((o, i) => {
        o.visible = hiddenWas[i];
      });
    }
    console.debug(`[cs2mod] capturePreview ${kind} ${(performance.now() - t0).toFixed(1)}ms`);
    return canvas ? await canvasToBlob(canvas) : null;
  }

  const view = {
    onClick(cb) {
      onClickHandler = cb;
    },
    // Shift+LMB, not a real right-click (`s6k_draw_in_3d.md` item 4 - right-mouse stays reserved
    // for the free-fly look) - same registration name as map2d.js's own "set the origin" gesture.
    onRightClick(cb) {
      onRightClickHandler = cb;
    },
    setTarget(t) {
      currentTarget = t;
      drawTarget();
    },
    clearTarget() {
      currentTarget = null;
      drawTarget();
    },
    // `s6r_sightline_target.md`: `sl` is `{ from: {x,y,z}, to: {x,y,z}|null }`.
    setSightline(sl) {
      currentSightline = sl;
      drawSightline();
    },
    clearSightline() {
      currentSightline = null;
      drawSightline();
    },
    setOrigin(o) {
      updateOrigin(o);
    },
    clearOrigin() {
      updateOrigin(null);
    },
    setArea(key, polygon, zMin, zMax) {
      updateArea(key, polygon, zMin, zMax);
    },
    clearArea(key) {
      areaGen[key]++;
      clearAreaGroup(key);
    },
    setAreaMode(key, active) {
      if (active) {
        if (activeDraftKey && activeDraftKey !== key) {
          const other = draftAreas[activeDraftKey];
          if (!other.closed) {
            other.points = [];
            emitDraftChange(activeDraftKey);
          }
          clearDraftGroup(activeDraftKey);
        }
        activeDraftKey = key;
        // Unlike the 2D tool, 3D has no vertex-dragging - arming an already-closed key starts a
        // fresh polygon instead of every click silently doing nothing.
        if (draftAreas[key].closed) {
          draftAreas[key] = { points: [], closed: false, onChange: draftAreas[key].onChange };
        }
      } else if (activeDraftKey === key) {
        activeDraftKey = null;
        const a = draftAreas[key];
        if (!a.closed) {
          a.points = [];
          emitDraftChange(key);
        }
        clearDraftGroup(key);
      }
    },
    onAreaChange(key, cb) {
      draftAreas[key].onChange = cb;
    },
    recolor() {},
    setLineups(list) {
      lineups = list;
    },
    setSelected(id) {
      selectedId = id;
      const l = lineups.find((x) => x.id === id);
      if (!l) {
        clearLineupVisuals();
        return;
      }
      buildLineupVisuals(l).catch((e) => console.error("lineup 3D visuals failed", e));
    },
    setHover() {},
    addCheckedPoints() {},
    addVerifiedPoints() {},
    clearPoints() {},

    resize() {
      resize();
    },
    setCameraMode(next) {
      setMode(next);
    },
    getCameraMode() {
      return mode;
    },
    setCollisionOverlay(show) {
      if (collision) {
        collision.overlay.visible = show;
      }
    },
    setLightingMode(next) {
      if (next !== "game" && next !== "simple") {
        return;
      }
      lightingMode = next;
      storeLightingMode(next);
      applyLightingMode();
    },
    getLightingMode() {
      return effectiveLightingMode();
    },
    isLightingSupported() {
      return lightingSupported;
    },
    // Debug/measurement hook (`s6f3a6_native_tex.md`'s GPU-memory receipt table): `null` before the
    // game-lighting pipeline exists (e.g. "simple" mode only, or still loading).
    getLightingStats() {
      if (!lightingPipeline) return null;
      return {
        lightingGpuBytes: lightingPipeline.gpuBytes,
        materialTextureBytes: lightingPipeline.materialTextureBytes(),
        materialTextureCount: lightingPipeline.materialTextureCount(),
        programCount: lightingPipeline.programCount(),
        missingExtensionsMessage: lightingPipeline.missingExtensionsMessage,
      };
    },
    onLoadProgress(cb) {
      loadProgressHandler = cb;
    },
    onLoadDone(cb) {
      loadDoneHandler = cb;
    },
    onLoadError(cb) {
      loadErrorHandler = cb;
    },
    onLightingReady(cb) {
      lightingReadyHandler = cb;
    },
    isCollisionReady() {
      return collision !== null;
    },
    getSelectedLineup() {
      return lineups.find((x) => x.id === selectedId) ?? null;
    },
    // S6n lineup previews (`s6n_lineup_previews.md`): resolves to a Blob (image/webp, fallback
    // image/png) or null when render.glb isn't loaded yet - call `ensureLoaded()` first to wait
    // for it instead of polling.
    capturePreview(opts) {
      return capturePreview(opts);
    },
    ensureLoaded() {
      return ensureLoaded();
    },
    enterFirstPerson(l, onExit) {
      return enterFirstPerson(l, onExit);
    },
    exitFirstPerson() {
      exitFirstPerson();
    },
    inFirstPerson() {
      return mode === "fpv";
    },

    destroy() {
      destroyed = true;
      glbController.abort();
      if (animHandle) {
        cancelAnimationFrame(animHandle);
      }
      resizeObserver.disconnect();
      orbit.dispose();
      document.removeEventListener("pointerlockchange", onPointerLockChange);
      document.removeEventListener("mousemove", onDocMouseMove);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      if (document.pointerLockElement === renderer.domElement) {
        document.exitPointerLock?.();
      }
      collisionReady.then((c) => {
        if (c) {
          disposeObject3D(c.overlay);
        }
      });
      glbReady.then((gltf) => {
        if (gltf) {
          disposeObject3D(gltf.scene);
        }
      });
      lightingReady.then((pipeline) => {
        pipeline?.dispose();
      });
      materialTexLoaderReady.then((l) => l?.dispose());
      domListeners.abort();
      areaGen.origin++;
      areaGen.target++;
      clearAreaGroup("origin");
      clearAreaGroup("target");
      originGen++;
      clearOriginGroup();
      clearDraftGroup("origin");
      clearDraftGroup("target");
      if (hoverRelabelTimer) {
        clearTimeout(hoverRelabelTimer);
        hoverRelabelTimer = null;
      }
      hoverLabel.remove();
      hoverRing.geometry.dispose();
      hoverRing.material.dispose();
      if (collision) {
        disposeObject3D(collision.overlay);
      }
      if (renderGltf) {
        disposeObject3D(renderGltf.scene);
      }
      if (lightingPipeline) {
        lightingPipeline.dispose();
      }
      scene.clear();
      renderGltf = null;
      collision = null;
      lightingPipeline = null;
      renderer.dispose();
      renderer.forceContextLoss();
      renderer.domElement.remove();
    },
  };

  // Debug/measurement hook (S6n lineup previews, `s6n_lineup_previews.md`): exposes this scene view
  // to the browser-automation harness, only when the page opted into it via `?debug=1` - never runs
  // otherwise, so it can't be reached from a normal page load.
  if (new URLSearchParams(location.search).get("debug") === "1") {
    window.__cs2modScene = view;
    // Not part of the `capturePreview` contract - just lets the harness read
    // `renderer.info.memory` for its own before/after GPU-memory check.
    window.__cs2modRenderer = renderer;
  }

  return view;
}
