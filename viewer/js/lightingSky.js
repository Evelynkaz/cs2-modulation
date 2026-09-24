// The 2D sky (`s6f3b2_lighting_shader.md` §4, REPORT.md §4): a full-screen pass, drawn into the HDR
// target right after it is cleared and before the world - normal depth (LEQUAL against the just-
// cleared 1.0) means every opaque world fragment drawn afterwards still overwrites it, so it only
// ever shows through where there is no geometry ("рисуется только на дальней плоскости").
//
// Sampled with a raw `samplerCube` (a `CompressedCubeTexture`/`CubeTexture` used purely as a 6-face
// data container, never `scene.background`) and this module's own direction math - not three's
// background/env-map shader chunks, which flip X for the reflection convention REPORT.md §4 warns
// about. `RawShaderMaterial` here (no per-object matrices needed, just a full-screen triangle).

import * as THREE from "three";

// `rotation[i]` is row i of a row-major 3x3, `world = rotation * local` (render.json's own
// convention, both for `sky.rotation` and `fog.skyRotation`). Sampling needs `local = R^-1 * world`;
// for an orthonormal rotation `R^-1 == R^T`, computed once here instead of inverting in the shader.
export function inverseRotation(rows) {
  return new THREE.Matrix3()
    .set(rows[0][0], rows[0][1], rows[0][2], rows[1][0], rows[1][1], rows[1][2], rows[2][0], rows[2][1], rows[2][2])
    .transpose();
}

const VERTEX_SHADER = `
attribute vec2 position;
varying vec2 vClip;
void main() {
  vClip = position;
  gl_Position = vec4( position, 1.0, 1.0 );
}
`;

const FRAGMENT_SHADER = `
precision highp float;
varying vec2 vClip;
uniform mat4 uInvProjectionMatrix;
uniform mat4 uViewMatrixInverse;
uniform samplerCube uSkyCube;
uniform mat3 uSkyRotationT;
uniform vec3 uSkyTint;
uniform float uSkyExposureBias;
#ifdef SKY_RGBM
uniform float uSkyRgbmRange;
#endif
void main() {
  vec4 viewPos = uInvProjectionMatrix * vec4( vClip, 1.0, 1.0 );
  viewPos /= viewPos.w;
  vec3 worldDir = normalize( ( uViewMatrixInverse * vec4( viewPos.xyz, 0.0 ) ).xyz );
  vec3 dir = uSkyRotationT * worldDir;
#ifdef SKY_RGBM
  vec4 s = textureCube( uSkyCube, dir );
  vec3 color = s.rgb * s.a * uSkyRgbmRange;
#else
  vec3 color = textureCube( uSkyCube, dir ).rgb;
#endif
  color = max( color * exp2( uSkyExposureBias ) * uSkyTint, 0.0 );
  gl_FragColor = vec4( color, 1.0 );
}
`;

// `skyData`: { skyCube, isRgbm, rgbmRange, tint: [r,g,b], exposureBias, rotationRows }.
export function createSkyPass(skyData) {
  const geometry = new THREE.BufferGeometry();
  // One big triangle covering the whole clip-space square - cheaper than two triangles/a quad and
  // avoids a seam along the diagonal.
  geometry.setAttribute("position", new THREE.Float32BufferAttribute([-1, -1, 3, -1, -1, 3], 2));

  const defines = {};
  if (skyData.isRgbm) defines.SKY_RGBM = "";
  const material = new THREE.RawShaderMaterial({
    vertexShader: VERTEX_SHADER,
    fragmentShader: FRAGMENT_SHADER,
    defines,
    depthTest: true,
    depthWrite: true,
    uniforms: {
      uInvProjectionMatrix: { value: new THREE.Matrix4() },
      uViewMatrixInverse: { value: new THREE.Matrix4() },
      uSkyCube: { value: skyData.skyCube },
      uSkyRotationT: { value: inverseRotation(skyData.rotationRows) },
      uSkyTint: { value: new THREE.Vector3(...skyData.tint) },
      uSkyExposureBias: { value: skyData.exposureBias ?? 0 },
      ...(skyData.isRgbm ? { uSkyRgbmRange: { value: skyData.rgbmRange } } : {}),
    },
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.frustumCulled = false;
  const scene = new THREE.Scene();
  scene.add(mesh);

  return {
    material,
    render(renderer, camera) {
      material.uniforms.uInvProjectionMatrix.value.copy(camera.projectionMatrixInverse);
      material.uniforms.uViewMatrixInverse.value.copy(camera.matrixWorld);
      renderer.render(scene, camera);
    },
    dispose() {
      geometry.dispose();
      material.dispose();
    },
  };
}
