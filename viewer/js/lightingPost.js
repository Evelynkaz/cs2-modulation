// HDR render target + the final full-screen pass (`s6f3b2_lighting_shader.md` §1, REPORT.md §6):
// exposure -> the map's own tonemap curve `f` -> exact sRGB encode -> 32^3 LUT -> +-1/255 dither ->
// canvas untouched. Everything upstream (`lighting.js`'s world/sky passes) writes linear HDR into
// `createHdrTarget()`'s `WebGLRenderTarget`; this is the only place that ever encodes to sRGB.
//
// Exposure is fixed at `(min+max)/2` from `postProcessing.exposure`, not metered per frame - REPORT.md
// §6 notes Mirage's own auto-exposure is already clamped to a narrow [0.8,1.0] band, and
// `s6f3b2_lighting_shader.md` §1 explicitly allows this simplification "если сложно" in place of
// downsampling the HDR buffer for average luminance every frame. `FIXED_EXPOSURE` is that flag.

import * as THREE from "three";

export const FIXED_EXPOSURE = true;

// GLSL3 (`material.glslVersion = THREE.GLSL3` below) - the LUT is a WebGL2 `sampler3D`, which
// GLSL ES 1.00 has no type for at all.
const VERTEX_SHADER = `
in vec2 position;
out vec2 vUv;
void main() {
  vUv = position * 0.5 + 0.5;
  gl_Position = vec4( position, 0.0, 1.0 );
}
`;

const FRAGMENT_SHADER = `
precision highp float;
precision highp sampler3D;
in vec2 vUv;
out vec4 fragColor;
uniform sampler2D uHdrMap;
uniform sampler3D uLut;
uniform float uExposure;
uniform float uExposureBias;
uniform float uShoulderStrength;
uniform float uLinearStrength;
uniform float uLinearAngle;
uniform float uToeStrength;
uniform float uToeNum;
uniform float uToeDenom;
uniform float uWhitePoint;
uniform float uInvFWp;
uniform float uLutDim;

float tonemapCurve( float y ) {
  float S = uShoulderStrength;
  float Ls = uLinearStrength;
  float La = uLinearAngle;
  float T = uToeStrength;
  float Tn = uToeNum;
  float Td = uToeDenom;
  return ( y * ( S * y + Ls * La ) + T * Tn ) / ( y * ( S * y + Ls ) + T * Td ) - Tn / Td;
}

vec3 tonemap( vec3 x ) {
  vec3 y = min( x * 2.8, vec3( uWhitePoint * 2.8 ) );
  return vec3( tonemapCurve( y.r ), tonemapCurve( y.g ), tonemapCurve( y.b ) ) * uInvFWp;
}

float srgbEncode( float c ) {
  return c <= 0.0031308 ? c * 12.92 : 1.055 * pow( c, 1.0 / 2.4 ) - 0.055;
}

// Cheap screen-space hash - only needs to break up 8-bit banding, not be a good RNG.
float hash( vec2 p ) {
  return fract( sin( dot( p, vec2( 12.9898, 78.233 ) ) ) * 43758.5453 );
}

void main() {
  vec3 hdr = texture( uHdrMap, vUv ).rgb;
  vec3 x = hdr * uExposure * exp2( uExposureBias );
  vec3 mapped = clamp( tonemap( x ), 0.0, 1.0 );
  vec3 srgb = vec3( srgbEncode( mapped.r ), srgbEncode( mapped.g ), srgbEncode( mapped.b ) );

  vec3 lutUv = clamp( srgb, 0.0, 1.0 ) * ( ( uLutDim - 1.0 ) / uLutDim ) + ( 0.5 / uLutDim );
  vec3 graded = texture( uLut, lutUv ).rgb;

  float dither = ( hash( gl_FragCoord.xy ) - 0.5 ) * ( 2.0 / 255.0 );
  fragColor = vec4( graded + dither, 1.0 );
}
`;

export function createHdrTarget(renderer, width, height) {
  const gl = renderer.getContext();
  let samples = 0;
  try {
    samples = Math.min(4, gl.getParameter(gl.MAX_SAMPLES) || 0);
  } catch {
    samples = 0; // SwiftShader/older drivers may not expose MAX_SAMPLES meaningfully.
  }
  return new THREE.WebGLRenderTarget(Math.max(1, width), Math.max(1, height), {
    type: THREE.HalfFloatType,
    format: THREE.RGBAFormat,
    colorSpace: THREE.NoColorSpace,
    minFilter: THREE.LinearFilter,
    magFilter: THREE.LinearFilter,
    samples,
    depthBuffer: true,
    stencilBuffer: false,
  });
}

// `toneMapParams`/`exposure`: `postProcessing.vpost.toneMapParams` and `postProcessing.exposure`
// from render.json (`s6f3b2_lighting_shader.md` §1, REPORT.md §6). `lut`: a `Data3DTexture` from
// `lightingTextures.loadLut`, `lutDim` its side (32).
export function createPostPass(toneMapParams, exposure, lut, lutDim) {
  const p = toneMapParams;
  const S = p.m_flShoulderStrength;
  const Ls = p.m_flLinearStrength;
  const La = p.m_flLinearAngle;
  const T = p.m_flToeStrength;
  const Tn = p.m_flToeNum;
  const Td = p.m_flToeDenom;
  const WP = p.m_flWhitePoint;
  const yWp = WP * 2.8;
  const fWp = (yWp * (S * yWp + Ls * La) + T * Tn) / (yWp * (S * yWp + Ls) + T * Td) - Tn / Td;

  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute("position", new THREE.Float32BufferAttribute([-1, -1, 3, -1, -1, 3], 2));

  const fixedExposure = (exposure.min + exposure.max) / 2;
  const material = new THREE.RawShaderMaterial({
    glslVersion: THREE.GLSL3,
    vertexShader: VERTEX_SHADER,
    fragmentShader: FRAGMENT_SHADER,
    depthTest: false,
    depthWrite: false,
    uniforms: {
      uHdrMap: { value: null },
      uLut: { value: lut },
      uExposure: { value: fixedExposure },
      uExposureBias: { value: p.m_flExposureBias ?? 0 },
      uShoulderStrength: { value: S },
      uLinearStrength: { value: Ls },
      uLinearAngle: { value: La },
      uToeStrength: { value: T },
      uToeNum: { value: Tn },
      uToeDenom: { value: Td },
      uWhitePoint: { value: WP },
      uInvFWp: { value: 1 / fWp },
      uLutDim: { value: lutDim },
    },
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.frustumCulled = false;
  const scene = new THREE.Scene();
  scene.add(mesh);

  return {
    material,
    // Renders `hdrTarget.texture` to whatever render target is currently bound (the canvas when
    // `null`) - `fragColor` here is already the final sRGB-encoded, LUT-graded, dithered value
    // ("канвас без изменений", `s6f3b2_lighting_shader.md` §1); this is a `RawShaderMaterial` with
    // no `colorspace_fragment` chunk, so `renderer.outputColorSpace` never touches it either way.
    render(renderer, camera, hdrTarget) {
      material.uniforms.uHdrMap.value = hdrTarget.texture;
      renderer.render(scene, camera);
    },
    dispose() {
      geometry.dispose();
      material.dispose();
    },
  };
}
