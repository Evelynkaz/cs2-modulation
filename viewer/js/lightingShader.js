// The F3b-2 world "uber-shader" (`s6f3b2_lighting_shader.md` §2): one `THREE.ShaderMaterial` per
// glTF material x lighting-path combination, replacing the `MeshStandardMaterial` + `onBeforeCompile`
// patches F3b-1 used. Compiled program variants are deduped by `customProgramCacheKey` built from
// the exact define set (`s6f3b2_lighting_shader.md` §2 "кэш программ по набору define'ов") - two
// materials with the same capability recipe (e.g. two lightmapped, AO-only, no-metal-map materials)
// share one WebGLProgram even though each keeps its own textures/uniform values.
//
// Formulas cited below are REPORT.md §1 (lightmap/probe diffuse, GGX sun specular) and §6 (cube
// fog), themselves citing `ValveResourceFormat/Renderer/Shaders/{complex.frag,pbr,lighting,fog}.slang`.
// No 1/π anywhere (REPORT.md key finding #3) - matches the baked lightmaps, which were baked without
// it.

import * as THREE from "three";

export function channelIndex(letter) {
  return { r: 0, g: 1, b: 2, a: 3 }[letter] ?? 0;
}

// ---- vertex shader ------------------------------------------------------------------------------
// `position`/`normal`/`uv` are declared by three's own `ShaderMaterial` prefix unconditionally;
// `uv1` (TEXCOORD_1) and `_lpv`/`_blend` are not (three only auto-declares `uv1` when one of its
// *own* standard material texture slots reads channel 1, which none of ours do), so they are
// declared here, gated on the defines that mean the geometry actually carries them.
function vertexShader() {
  return `
#ifdef LIGHTING_LIGHTMAP
attribute vec2 uv1;
#endif
#ifdef LIGHTING_PROBE
attribute vec4 _lpv;
#endif
#ifdef HAS_LAYERS
attribute float _blend;
#endif

varying vec3 vWorldPos;
varying vec3 vNormal;
varying vec2 vUv;
#ifdef LIGHTING_LIGHTMAP
varying vec2 vUv1;
#endif
#ifdef LIGHTING_PROBE
varying vec4 vLpv;
#endif
#ifdef HAS_LAYERS
varying float vBlendW;
#endif

void main() {
  vec4 worldPos4 = modelMatrix * vec4( position, 1.0 );
  vWorldPos = worldPos4.xyz;
  // World-space normal, not three's view-space normalMatrix * normal (used below against
  // world-space uSunToSun, the cotangent frame from vWorldPos, and cameraPosition - a
  // view-space normal made lighting drift as the camera turned). vec4(...,0.0) * viewMatrix
  // is transpose(viewMatrix) * vec3 i.e. inverse(viewMatrix) * vec3 (viewMatrix is
  // orthonormal), which undoes exactly the view transform normalMatrix (the inverse-transpose
  // of the upper 3x3 modelView matrix) carries.
  vNormal = normalize( ( vec4( normalMatrix * normal, 0.0 ) * viewMatrix ).xyz );
  vUv = uv;
#ifdef LIGHTING_LIGHTMAP
  vUv1 = uv1;
#endif
#ifdef LIGHTING_PROBE
  vLpv = _lpv;
#endif
#ifdef HAS_LAYERS
  vBlendW = _blend;
#endif
  gl_Position = projectionMatrix * modelViewMatrix * vec4( position, 1.0 );
}
`;
}

// ---- fragment shader ----------------------------------------------------------------------------

function fragmentShader() {
  return `
precision highp float;

varying vec3 vWorldPos;
varying vec3 vNormal;
varying vec2 vUv;
#ifdef LIGHTING_LIGHTMAP
varying vec2 vUv1;
#endif
#ifdef LIGHTING_PROBE
varying vec4 vLpv;
#endif
#ifdef HAS_LAYERS
varying float vBlendW;
#endif

#ifdef HAS_ALBEDO_MAP
uniform sampler2D uAlbedoMap;
#endif
uniform vec4 uBaseColorFactor;
#ifdef ALPHA_MASK
uniform float uAlphaCutoff;
#endif

#ifdef HAS_NORMAL_MAP
uniform sampler2D uNormalMap;
uniform vec2 uNormalScale;
#endif

#ifdef HAS_AO
uniform sampler2D uAoMap;
#endif
#ifdef HAS_ROUGHNESS_MAP
uniform sampler2D uRoughnessMap;
#endif
uniform float uRoughnessFactor;
#ifdef HAS_METALNESS_MAP
uniform sampler2D uMetalnessMap;
#endif
uniform float uMetalnessFactor;

#ifdef HAS_TINT
uniform vec3 uTintColor;
uniform sampler2D uTintMaskMap;
#endif
#ifdef HAS_LAYERS
uniform sampler2D uLayer2Map;
#ifdef HAS_BLEND_MOD
uniform sampler2D uBlendModMap;
#endif
#endif

#ifdef LIGHTING_LIGHTMAP
uniform sampler2D uIrradianceMap;
uniform sampler2D uShadowMap;
#ifdef IRRADIANCE_RGBM
uniform float uIrradianceRgbmRange;
#endif
#endif
#ifdef LIGHTING_PROBE
uniform float uLpvScale;
#endif

uniform vec3 uSunToSun;
uniform vec3 uSunColorLinear;

#ifdef FOG_ENABLED
// The fog's own cube - render_fog_cube.bin when the map bakes a separate one, or the same
// texture as the 2D sky (with uFogLodOffset 0) when it reuses that cube (REPORT.md §6,
// lighting.js picks which at load time; never three's CubeTexture background - see
// lightingSky.js for why).
uniform samplerCube uFogCube;
uniform float uFogLodOffset; // finest mip actually present in uFogCube (fog.skyCubeBaseLevel)
uniform mat3 uFogSkyRotationT; // pre-transposed on the CPU: R^-1*v == uFogSkyRotationT * v
uniform float uFogStart;
uniform float uFogEnd;
uniform float uFogFalloffExp;
uniform float uFogHStart;
uniform float uFogHEnd;
uniform float uFogHExp;
uniform float uFogUseHeight;
uniform float uFogMaxOpacity;
uniform float uFogLodBias;
uniform float uFogSkyExposureBias;
uniform float uFogSkyMips;
#ifdef SKY_RGBM
uniform float uSkyRgbmRange;
#endif
#endif

// Tangent-space normal mapping without a precomputed TANGENT (REPORT.md: render.glb carries none -
// s6f3b2_lighting_shader.md #2 asks for a screen-space-derivative cotangent frame).
// Standard technique (Schuler): the UV/position screen-space derivatives pin down a tangent frame
// up to scale, which is all normal mapping needs. Sign was checked by comparing a bump-mapped wall
// against the flat-normal render with the same light - see the receipt for the two screenshots.
mat3 cotangentFrame( vec3 N, vec3 p, vec2 uv ) {
  vec3 dp1 = dFdx( p );
  vec3 dp2 = dFdy( p );
  vec2 duv1 = dFdx( uv );
  vec2 duv2 = dFdy( uv );
  vec3 dp2perp = cross( dp2, N );
  vec3 dp1perp = cross( N, dp1 );
  vec3 T = dp2perp * duv1.x + dp1perp * duv2.x;
  vec3 B = dp2perp * duv1.y + dp1perp * duv2.y;
  float invmax = inversesqrt( max( dot( T, T ), dot( B, B ) ) );
  return mat3( T * invmax, B * invmax, N );
}

void main() {
  // glTF: a missing baseColorTexture means the factor stands alone, as if sampling a white
  // texture (spec §3.9.3) - not (0,0,0,0), which is what a real, unbound sampler2D returns and
  // was making BLEND water materials (factor 1,1,1,1, no texture) invisible.
  vec4 albedoColor = uBaseColorFactor;
#ifdef HAS_ALBEDO_MAP
  albedoColor *= texture2D( uAlbedoMap, vUv );
#endif

#ifdef HAS_TINT
  {
    float tintAmount = texture2D( uTintMaskMap, vUv ).r;
    albedoColor.rgb = mix( albedoColor.rgb, albedoColor.rgb * uTintColor, tintAmount );
  }
#endif
#ifdef HAS_LAYERS
  {
    vec4 layer2Sample = texture2D( uLayer2Map, vUv );
#ifdef HAS_BLEND_MOD
    vec3 m = texture2D( uBlendModMap, vUv ).rgb;
#else
    vec3 m = vec3( 0.0, 1.0, 0.0 );
#endif
    float b = smoothstep( max( 0.0, m.g - m.r ), min( 1.0, m.g + m.r ), vBlendW );
    albedoColor.rgb = mix( albedoColor.rgb, layer2Sample.rgb, b );
  }
#endif

#ifdef ALPHA_MASK
  if ( albedoColor.a < uAlphaCutoff ) discard;
#endif

  vec3 albedo = albedoColor.rgb;
  vec3 finalColor;

#ifdef LIGHTING_UNLIT
  finalColor = albedo;
#else
  float ao = 1.0;
#ifdef HAS_AO
  ao = texture2D( uAoMap, vUv )[ AO_CHANNEL ];
#endif
  float roughness = uRoughnessFactor;
#ifdef HAS_ROUGHNESS_MAP
  roughness = texture2D( uRoughnessMap, vUv )[ ROUGHNESS_CHANNEL ];
#endif
  float metalness = uMetalnessFactor;
#ifdef HAS_METALNESS_MAP
  metalness = texture2D( uMetalnessMap, vUv )[ METALNESS_CHANNEL ];
#endif

  vec3 Ngeom = normalize( vNormal );
#ifdef DOUBLE_SIDED
  if ( !gl_FrontFacing ) Ngeom = -Ngeom;
#endif

  vec3 N = Ngeom;
#ifdef HAS_NORMAL_MAP
  {
    vec3 nSample = texture2D( uNormalMap, vUv ).xyz * 2.0 - 1.0;
    nSample.xy *= uNormalScale;
    mat3 TBN = cotangentFrame( Ngeom, vWorldPos, vUv );
    N = normalize( TBN * normalize( nSample ) );
  }
#endif

  vec3 E = vec3( 0.0 );
  float vis = 1.0;
#ifdef LIGHTING_LIGHTMAP
#ifdef IRRADIANCE_RGBM
  {
    vec4 irrSample = texture2D( uIrradianceMap, vUv1 );
    E = irrSample.rgb * irrSample.a * uIrradianceRgbmRange;
  }
#else
  E = texture2D( uIrradianceMap, vUv1 ).rgb;
#endif
#ifdef NO_BAKED_SUN_SHADOW
  vis = 1.0;
#else
  // sun.bakedShadowChannel picks the live channel - direct_light_shadows is BC4 (R only),
  // BC5 (RG) or BC7 (RGBA) depending on the map, and a map with no baked sun shadow at all
  // (bakedShadowChannel null, NO_BAKED_SUN_SHADOW above) may still hold unrelated data in .r.
  vis = 1.0 - texture2D( uShadowMap, vUv1 )[ SUN_SHADOW_CHANNEL ];
#endif
#endif
#ifdef LIGHTING_PROBE
  {
    vec4 lpv = vLpv * uLpvScale;
    E = lpv.rgb;
    vis = lpv.a;
  }
#endif

  float NoL = max( dot( N, uSunToSun ), 0.0 );
  vec3 Dsun = NoL * uSunColorLinear * vis;
  vec3 diffuse = albedo * ( 1.0 - metalness ) * ao * ( Dsun + E );

  vec3 specular = vec3( 0.0 );
#ifdef RENDER_SPECULAR
  {
    bool skipSpecular = false;
#ifdef NO_SPECULAR_AT_FULL_ROUGHNESS
    skipSpecular = roughness >= 0.999;
#endif
    if ( !skipSpecular ) {
      vec3 V = normalize( cameraPosition - vWorldPos );
      vec3 L = uSunToSun;
      vec3 H = normalize( V + L );
      float NoV = max( dot( N, V ), 1e-4 );
      float NoH = max( dot( N, H ), 0.0 );
      float VoH = max( dot( V, H ), 0.0 );
      // REPORT.md §1: r = max(rx,ry) - no anisotropy data here, so rx = ry = roughness.
      float r = clamp( roughness, 0.0, 1.0 );
      float alpha = max( r * r, 1e-4 );
      float NoH2 = NoH * NoH;
      // REPORT.md §1: k <= 453.5, no 1/pi - no 0.045 roughness floor (that hid r=0 behind a
      // clamp instead of the reference's own cap on the intermediate term).
      float Dt = min( alpha / ( ( 1.0 - NoH2 ) + NoH2 * alpha * alpha ), 453.5 );
      float D = Dt * Dt;
      float k = ( r + 1.0 ) * ( r + 1.0 ) / 8.0;
      float Vspec = 1.0 / ( 4.0 * max( ( NoL * ( 1.0 - k ) + k ) * ( NoV * ( 1.0 - k ) + k ), 1e-4 ) );
      vec3 F0 = mix( vec3( 0.04 ), albedo, metalness );
      vec3 F = mix( F0, vec3( 1.0 ), pow( clamp( 1.0 - VoH, 0.0, 1.0 ), 5.0 ) );
      // REPORT.md §1: 1 + F0*0.125*(rx+ry)^4*saturate(N.V) - a vec3 term (F0 is vec3), not scalar.
      vec3 energyComp = vec3( 1.0 ) + F0 * 0.125 * pow( 2.0 * r, 4.0 ) * clamp( dot( N, V ), 0.0, 1.0 );
      specular = D * Vspec * NoL * F * uSunColorLinear * vis * energyComp;
    }
  }
#endif

  finalColor = diffuse + specular;
#endif

#ifdef FOG_ENABLED
  {
    vec3 toFrag = vWorldPos - cameraPosition;
    float d = length( toFrag.xy );
    float invRange = 1.0 / ( uFogEnd - uFogStart );
    float a = pow( max( d * invRange - uFogStart * invRange, 1e-4 ), uFogFalloffExp );
    float h = 1.0;
    if ( uFogUseHeight > 0.5 && uFogHEnd > uFogHStart ) {
      float hs = 1.0 / ( uFogHStart - uFogHEnd );
      h = pow( max( vWorldPos.z * hs + 1.0 - uFogHStart * hs, 1e-4 ), uFogHExp );
    }
    float blend = clamp( a, 0.0, 1.0 ) * clamp( h, 0.0, 1.0 );
    float lod = max( clamp( 1.0 - blend * uFogLodBias, 0.0, 1.0 ) * min( 7.0, uFogSkyMips ) - uFogLodOffset, 0.0 );
    vec3 dir = uFogSkyRotationT * normalize( toFrag );
#ifdef SKY_RGBM
    vec4 fogSample = textureCubeLodEXT( uFogCube, dir, lod );
    vec3 fogColor = fogSample.rgb * fogSample.a * uSkyRgbmRange;
#else
    vec3 fogColor = textureCubeLodEXT( uFogCube, dir, lod ).rgb;
#endif
    fogColor *= exp2( uFogSkyExposureBias );
    float distSq = dot( toFrag, toFrag );
    if ( distSq > uFogStart * uFogStart || vWorldPos.z > uFogHStart ) {
      finalColor = mix( finalColor, fogColor, clamp( blend, 0.0, 1.0 ) * uFogMaxOpacity );
    }
  }
#endif

#ifdef MOD2X
  // VRF complex.frag.slang:776-777 - fades the Mod2x neutral (0.5, so GL's 2*src*dst blend
  // leaves dst unchanged) toward the shaded color by alpha, then the blend runs on this HDR
  // linear value (s6f3b2_lighting_shader.md #2 - Source blends translucents in HDR, pre-tonemap).
  finalColor = mix( vec3( 0.5 ), finalColor, albedoColor.a );
#endif

  gl_FragColor = vec4( finalColor, albedoColor.a );
}
`;
}

// Builds the `defines` object (and a matching cache-key string) for one material/primitive recipe.
function buildDefines(recipe) {
  const d = {};
  if (recipe.lightingType === "lightmap") d.LIGHTING_LIGHTMAP = "";
  else if (recipe.lightingType === "probe") d.LIGHTING_PROBE = "";
  else d.LIGHTING_UNLIT = "";
  if (recipe.alphaMode === "MASK") d.ALPHA_MASK = "";
  if (recipe.doubleSided) d.DOUBLE_SIDED = "";
  if (recipe.mod2x) d.MOD2X = "";
  if (recipe.normalMap) d.HAS_NORMAL_MAP = "";
  if (recipe.aoMap) {
    d.HAS_AO = "";
    d.AO_CHANNEL = String(channelIndex(recipe.aoChannel ?? "r"));
  }
  if (recipe.roughnessMap) {
    d.HAS_ROUGHNESS_MAP = "";
    d.ROUGHNESS_CHANNEL = String(channelIndex(recipe.roughnessChannel ?? "r"));
  }
  if (recipe.metalnessMap) {
    d.HAS_METALNESS_MAP = "";
    d.METALNESS_CHANNEL = String(channelIndex(recipe.metalnessChannel ?? "g"));
  }
  if (recipe.map) d.HAS_ALBEDO_MAP = "";
  if (recipe.tintMaskMap) d.HAS_TINT = "";
  if (recipe.layer2Map) {
    d.HAS_LAYERS = "";
    if (recipe.blendModMap) d.HAS_BLEND_MOD = "";
  }
  if (recipe.lightingType === "lightmap") {
    if (recipe.irradianceIsRgbm) d.IRRADIANCE_RGBM = "";
    if (recipe.bakedShadowChannel == null) d.NO_BAKED_SUN_SHADOW = "";
    else d.SUN_SHADOW_CHANNEL = String(recipe.bakedShadowChannel);
  }
  if (recipe.fogEnabled) {
    d.FOG_ENABLED = "";
    if (recipe.skyIsRgbm) d.SKY_RGBM = "";
  }
  if (recipe.lightingType !== "unlit" && recipe.renderSpecular !== false) d.RENDER_SPECULAR = "";
  if (recipe.noSpecularAtFullRoughness) d.NO_SPECULAR_AT_FULL_ROUGHNESS = "";
  return d;
}

// `THREE.ShaderMaterial.defines` already changes what actually gets compiled, but three's default
// program-cache key does not always look at it for a *custom* shader (`WebGLPrograms` mainly keys
// off its own standard-material parameters) - `customProgramCacheKey` makes the dedup explicit and
// lets the receipt report an exact "distinct programs" count by counting distinct keys handed out.
function cacheKey(defines) {
  return Object.keys(defines)
    .sort()
    .map((k) => `${k}=${defines[k]}`)
    .join(",");
}

/**
 * Builds one `THREE.ShaderMaterial` for a (glTF material, lighting path) pair.
 * `recipe.shared` carries the per-map lighting resources (sun, fog, sky, irradiance/shadow maps)
 * built once by `lighting.js` and reused, by reference, across every material.
 */
export function buildWorldMaterial(recipe) {
  const defines = buildDefines(recipe);
  const shared = recipe.shared;
  const uniforms = {
    uBaseColorFactor: { value: new THREE.Vector4(...recipe.baseColorFactor) },
    uRoughnessFactor: { value: recipe.roughnessFactor ?? 1 },
    uMetalnessFactor: { value: recipe.metalnessFactor ?? 0 },
    uSunToSun: shared.uSunToSun,
    uSunColorLinear: shared.uSunColorLinear,
  };
  if (recipe.map) uniforms.uAlbedoMap = { value: recipe.map };
  if (recipe.alphaMode === "MASK") {
    uniforms.uAlphaCutoff = { value: recipe.alphaCutoff ?? 0.5 };
  }
  if (recipe.normalMap) {
    uniforms.uNormalMap = { value: recipe.normalMap };
    uniforms.uNormalScale = { value: recipe.normalScale ?? new THREE.Vector2(1, 1) };
  }
  if (recipe.aoMap) uniforms.uAoMap = { value: recipe.aoMap };
  if (recipe.roughnessMap) uniforms.uRoughnessMap = { value: recipe.roughnessMap };
  if (recipe.metalnessMap) uniforms.uMetalnessMap = { value: recipe.metalnessMap };
  if (recipe.tintMaskMap) {
    uniforms.uTintMaskMap = { value: recipe.tintMaskMap };
    uniforms.uTintColor = { value: recipe.tintColor };
  }
  if (recipe.layer2Map) {
    uniforms.uLayer2Map = { value: recipe.layer2Map };
    if (recipe.blendModMap) uniforms.uBlendModMap = { value: recipe.blendModMap };
  }
  if (recipe.lightingType === "lightmap") {
    uniforms.uIrradianceMap = shared.uIrradianceMap;
    uniforms.uShadowMap = shared.uShadowMap;
    if (recipe.irradianceIsRgbm) uniforms.uIrradianceRgbmRange = shared.uIrradianceRgbmRange;
  }
  if (recipe.lightingType === "probe") {
    uniforms.uLpvScale = shared.uLpvScale;
  }
  if (recipe.fogEnabled) {
    uniforms.uFogCube = shared.uFogCube;
    uniforms.uFogLodOffset = shared.uFogLodOffset;
    uniforms.uFogSkyRotationT = shared.uFogSkyRotationT;
    uniforms.uFogStart = shared.uFogStart;
    uniforms.uFogEnd = shared.uFogEnd;
    uniforms.uFogFalloffExp = shared.uFogFalloffExp;
    uniforms.uFogHStart = shared.uFogHStart;
    uniforms.uFogHEnd = shared.uFogHEnd;
    uniforms.uFogHExp = shared.uFogHExp;
    uniforms.uFogUseHeight = shared.uFogUseHeight;
    uniforms.uFogMaxOpacity = shared.uFogMaxOpacity;
    uniforms.uFogLodBias = shared.uFogLodBias;
    uniforms.uFogSkyExposureBias = shared.uFogSkyExposureBias;
    uniforms.uFogSkyMips = shared.uFogSkyMips;
    if (recipe.skyIsRgbm) uniforms.uSkyRgbmRange = shared.uSkyRgbmRange;
  }

  const material = new THREE.ShaderMaterial({
    vertexShader: vertexShader(),
    fragmentShader: fragmentShader(),
    defines,
    uniforms,
    side: recipe.doubleSided ? THREE.DoubleSide : THREE.FrontSide,
    transparent: recipe.alphaMode === "BLEND" || recipe.mod2x === true,
    depthWrite: !(recipe.alphaMode === "BLEND" || recipe.mod2x === true),
  });
  if (recipe.mod2x) {
    material.blending = THREE.CustomBlending;
    material.blendEquation = THREE.AddEquation;
    material.blendSrc = THREE.DstColorFactor;
    material.blendDst = THREE.SrcColorFactor;
  }
  const key = "cs2mod-world:" + cacheKey(defines);
  material.customProgramCacheKey = () => key;
  return { material, cacheKey: key };
}
