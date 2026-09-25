// The F3b-2 world "uber-shader" (`s6f3b2_lighting_shader.md` §2), updated for native BC textures
// (`s6f3a6_native_tex.md` change item 4): one `THREE.ShaderMaterial` per glTF material x
// lighting-path combination. Compiled program variants are deduped by `customProgramCacheKey`
// built from the exact define set (`s6f3b2_lighting_shader.md` §2 "кэш программ по набору define'ов")
// - two materials with the same capability recipe share one WebGLProgram even though each keeps
// its own textures/uniform values.
//
// Formulas cited below are REPORT.md §1 (lightmap/probe diffuse, GGX sun specular) and §6 (cube
// fog) from `s6f3b2_lighting_shader.md`'s survey, and the F3a-6 survey's REPORT.md (HemiOct normal
// decode, sRGB-vs-linear table, alpha meaning) - `ValveResourceFormat/Renderer/Shaders/
// {complex.frag,pbr,lighting,fog,utils}.slang`. No 1/π anywhere (REPORT.md key finding #3) -
// matches the baked lightmaps, which were baked without it.

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

// F3a-6 native textures (REPORT.md "Normals and roughness"): the normal map is the game's own
// HemiOct-encoded BC7 texture, decoded per-texel below - RG is the hemi-octahedron direction, B is
// packed isotropic roughness (moves into the decoded normal's place as "roughness", not alpha).
// A material with no normal map at all (flat geometry normal) still has a roughness value -
// uRoughnessFactor alone, either the shader's own default (1.0) or a 4x4-constant normal's baked-in
// roughness folded in at material-build time (materialTextures.js's decodeHemiOctConstant).
#ifdef HAS_NORMAL_MAP
uniform sampler2D uNormalMap;
#endif
uniform float uRoughnessFactor;

uniform float uAoFactor;
#ifdef HAS_AO
uniform sampler2D uAoMap;
#endif
uniform float uMetalnessFactor;
#ifdef HAS_METALNESS_MAP
uniform sampler2D uMetalnessMap;
#endif

#ifdef HAS_TINT
uniform vec3 uTintColor;
uniform sampler2D uTintMaskMap;
#endif
#ifdef HAS_LAYERS
#ifdef HAS_LAYER2_MAP
uniform sampler2D uLayer2Map;
#else
uniform vec3 uLayer2ConstantColor;
#endif
#ifdef HAS_BLEND_MOD_MAP
uniform sampler2D uBlendModMap;
#elif defined( HAS_BLEND_MOD_CONST )
uniform vec3 uBlendModConstant;
#endif
#endif

// review fix item 1: complex.frag.slang:191,227-231 GetStandardSelfIllumination -
// exp2(brightness)*scale*tint*mask.r*mix(1,albedo,albedoFactor); previously added the raw mask
// colour unconditionally, ignoring F_SELF_ILLUM and every one of these parameters.
#ifdef HAS_SELF_ILLUM
#ifdef HAS_SELF_ILLUM_MAP
uniform sampler2D uSelfIllumMap;
#else
uniform float uSelfIllumConstantMask;
#endif
uniform float uSelfIllumScale;
uniform float uSelfIllumBrightness;
uniform vec3 uSelfIllumTint;
uniform float uSelfIllumAlbedoFactor;
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

// sRGB (gamma) -> linear, per channel - only used when a texture's own compressed format can't
// carry an sRGB GPU-native internal format on this browser/GPU (change item 1's DXT1 fallback:
// WEBGL_compressed_texture_s3tc_srgb missing, WEBGL_compressed_texture_s3tc present).
vec3 srgbToLinear( vec3 c ) {
  return mix( c / 12.92, pow( ( c + 0.055 ) / 1.055, vec3( 2.4 ) ), step( 0.04045, c ) );
}

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
  {
    vec4 s = texture2D( uAlbedoMap, vUv );
#ifdef ALBEDO_MANUAL_SRGB
    s.rgb = srgbToLinear( s.rgb );
#endif
    albedoColor *= s;
  }
#endif

#ifdef HAS_TINT
  {
    float tintAmount = texture2D( uTintMaskMap, vUv ).r;
    albedoColor.rgb = mix( albedoColor.rgb, albedoColor.rgb * uTintColor, tintAmount );
  }
#endif
#ifdef HAS_LAYERS
  {
#ifdef HAS_LAYER2_MAP
    vec3 layer2Color = texture2D( uLayer2Map, vUv ).rgb;
#ifdef LAYER2_MANUAL_SRGB
    layer2Color = srgbToLinear( layer2Color );
#endif
#else
    vec3 layer2Color = uLayer2ConstantColor;
#endif
#ifdef HAS_BLEND_MOD_MAP
    vec3 m = texture2D( uBlendModMap, vUv ).rgb;
#elif defined( HAS_BLEND_MOD_CONST )
    vec3 m = uBlendModConstant;
#else
    vec3 m = vec3( 0.0, 1.0, 0.0 );
#endif
    float b = smoothstep( max( 0.0, m.g - m.r ), min( 1.0, m.g + m.r ), vBlendW );
    albedoColor.rgb = mix( albedoColor.rgb, layer2Color, b );
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
  float ao = uAoFactor;
#ifdef HAS_AO
  ao *= texture2D( uAoMap, vUv )[ AO_CHANNEL ];
#endif
#ifdef ALBEDO_ALPHA_AO
  // review fix item 6: extras.baseColorAlphaMeaning == "ao" (csgo_environment/_blend, opaque) -
  // REPORT.md's channel table; the sqrt-ish remap matches the reference's own AO curve.
  ao *= pow( max( albedoColor.a, 0.0 ), 0.5 );
#endif
  float metalness = uMetalnessFactor;
#ifdef HAS_METALNESS_MAP
  metalness = texture2D( uMetalnessMap, vUv )[ METALNESS_CHANNEL ];
#elif defined( ALBEDO_ALPHA_METALNESS )
  // review fix item 6: extras.baseColorAlphaMeaning == "metalness" (F_METALNESS_TEXTURE, opaque,
  // no separate g_tMetalness texture -- complex.frag.slang:618).
  metalness = albedoColor.a;
#endif
  float roughness = uRoughnessFactor;

  vec3 Ngeom = normalize( vNormal );
#ifdef DOUBLE_SIDED
  if ( !gl_FrontFacing ) Ngeom = -Ngeom;
#endif

  vec3 N = Ngeom;
#ifdef HAS_NORMAL_MAP
  {
    vec4 t = texture2D( uNormalMap, vUv );
    vec3 nSample;
#if defined( NORMAL_CODEC_DXT5NM )
    // review fix item 8: s2tex::transform's dxt5nm codec swaps R<->A before reconstructing Z
    // (Texture.cs:1333-1335's DXT5 normal-map convention -- X lives in alpha, Y in green); no
    // packed roughness channel, so roughness is left at its uRoughnessFactor default.
    {
      vec2 xy = vec2( t.a, t.g ) * 2.0 - 1.0;
      float z = sqrt( max( 0.0, 1.0 - dot( xy, xy ) ) );
      nSample = vec3( xy, z );
    }
#elif defined( NORMAL_CODEC_RECONSTRUCTZ )
    // review fix item 8: plain Z-reconstruction (s2tex::transform::reconstruct_normal_z) -- X/Y
    // straight from R/G, no swap, no packed roughness.
    {
      vec2 xy = vec2( t.r, t.g ) * 2.0 - 1.0;
      float z = sqrt( max( 0.0, 1.0 - dot( xy, xy ) ) );
      nSample = vec3( xy, z );
    }
#else
    {
      vec2 e = vec2( t.r + t.g - 1.003922, t.r - t.g );
      nSample = normalize( vec3( e, 1.0 - abs( e.x ) - abs( e.y ) ) );
      roughness = t.b; // packed roughness -- HemiOct only.
    }
#endif
    nSample.y = -nSample.y; // VRF utils.slang:261 - GLTFLoader's normalScale.y=-1 used to do this; done explicitly now.
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

  // Self-illumination (review fix item 1, complex.frag.slang:191,227-231
  // GetStandardSelfIllumination): only ever compiled in when the exporter found F_SELF_ILLUM==1
  // (or csgo_unlitgeneric) - see material.rs's self_illum.
#ifdef HAS_SELF_ILLUM
  {
#ifdef HAS_SELF_ILLUM_MAP
    float mask = texture2D( uSelfIllumMap, vUv ).r;
#ifdef SELFILLUM_MANUAL_SRGB
    mask = srgbToLinear( vec3( mask ) ).r;
#endif
#else
    float mask = uSelfIllumConstantMask;
#endif
    vec3 selfIllumScale = exp2( uSelfIllumBrightness ) * uSelfIllumScale * uSelfIllumTint;
    finalColor += selfIllumScale * mask * mix( vec3( 1.0 ), albedo, uSelfIllumAlbedoFactor );
  }
#endif
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
  if (recipe.normalMap) {
    d.HAS_NORMAL_MAP = "";
    // review fix item 8: dispatch the per-texel decode by the texture's own codec (real data has
    // hemiOct/dxt5nm/reconstructZ; ycocg never appears on a normal map - REPORT.md's survey - so
    // it has no shader branch here, only the constant-folding path below defends against it).
    if (recipe.normalCodec === "dxt5nm") d.NORMAL_CODEC_DXT5NM = "";
    else if (recipe.normalCodec === "reconstructZ") d.NORMAL_CODEC_RECONSTRUCTZ = "";
  }
  if (recipe.aoMap) {
    d.HAS_AO = "";
    d.AO_CHANNEL = String(channelIndex(recipe.aoChannel ?? "r"));
  }
  if (recipe.metalnessMap) {
    d.HAS_METALNESS_MAP = "";
    d.METALNESS_CHANNEL = String(channelIndex(recipe.metalnessChannel ?? "g"));
  }
  if (recipe.albedoMap) {
    d.HAS_ALBEDO_MAP = "";
    if (recipe.albedoManualSrgb) d.ALBEDO_MANUAL_SRGB = "";
  }
  // review fix item 6: extras.baseColorAlphaMeaning, exported but previously unused.
  if (recipe.baseColorAlphaMeaning === "ao") d.ALBEDO_ALPHA_AO = "";
  else if (recipe.baseColorAlphaMeaning === "metalness") d.ALBEDO_ALPHA_METALNESS = "";
  if (recipe.tintMaskMap) d.HAS_TINT = "";
  if (recipe.hasLayers) {
    d.HAS_LAYERS = "";
    if (recipe.layer2Map) {
      d.HAS_LAYER2_MAP = "";
      if (recipe.layer2ManualSrgb) d.LAYER2_MANUAL_SRGB = ""; // review fix item 7
    }
    if (recipe.blendModMap) d.HAS_BLEND_MOD_MAP = "";
    else if (recipe.blendModConstant) d.HAS_BLEND_MOD_CONST = "";
  }
  if (recipe.hasSelfIllum) {
    d.HAS_SELF_ILLUM = "";
    if (recipe.selfIllumMap) {
      d.HAS_SELF_ILLUM_MAP = "";
      if (recipe.selfIllumManualSrgb) d.SELFILLUM_MANUAL_SRGB = "";
    }
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
    uAoFactor: { value: recipe.aoFactor ?? 1 },
    uMetalnessFactor: { value: recipe.metalnessFactor ?? 0 },
    uSunToSun: shared.uSunToSun,
    uSunColorLinear: shared.uSunColorLinear,
  };
  if (recipe.albedoMap) uniforms.uAlbedoMap = { value: recipe.albedoMap };
  if (recipe.alphaMode === "MASK") {
    uniforms.uAlphaCutoff = { value: recipe.alphaCutoff ?? 0.5 };
  }
  if (recipe.normalMap) uniforms.uNormalMap = { value: recipe.normalMap };
  if (recipe.aoMap) uniforms.uAoMap = { value: recipe.aoMap };
  if (recipe.metalnessMap) uniforms.uMetalnessMap = { value: recipe.metalnessMap };
  if (recipe.tintMaskMap) {
    uniforms.uTintMaskMap = { value: recipe.tintMaskMap };
    uniforms.uTintColor = { value: recipe.tintColor };
  }
  if (recipe.hasLayers) {
    if (recipe.layer2Map) uniforms.uLayer2Map = { value: recipe.layer2Map };
    else uniforms.uLayer2ConstantColor = { value: recipe.layer2ConstantColor ?? new THREE.Vector3(1, 1, 1) };
    if (recipe.blendModMap) uniforms.uBlendModMap = { value: recipe.blendModMap };
    else if (recipe.blendModConstant) uniforms.uBlendModConstant = { value: recipe.blendModConstant };
  }
  if (recipe.hasSelfIllum) {
    if (recipe.selfIllumMap) uniforms.uSelfIllumMap = { value: recipe.selfIllumMap };
    else uniforms.uSelfIllumConstantMask = { value: recipe.selfIllumConstantMask ?? 0 };
    uniforms.uSelfIllumScale = { value: recipe.selfIllumScale ?? 1 };
    uniforms.uSelfIllumBrightness = { value: recipe.selfIllumBrightness ?? 0 };
    uniforms.uSelfIllumTint = { value: recipe.selfIllumTint ?? new THREE.Vector3(1, 1, 1) };
    uniforms.uSelfIllumAlbedoFactor = { value: recipe.selfIllumAlbedoFactor ?? 0 };
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
