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
#if defined( HAS_LAYERS ) || defined( HAS_ENV_LAYER2 )
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
#if defined( HAS_LAYERS ) || defined( HAS_ENV_LAYER2 )
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
#if defined( HAS_LAYERS ) || defined( HAS_ENV_LAYER2 )
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
#if defined( HAS_LAYERS ) || defined( HAS_ENV_LAYER2 )
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
#ifdef HAS_LAYER2_NORMAL_MAP
uniform sampler2D uLayer2NormalMap;
#endif
#endif

// csgo_environment(_blend) height-band blend + roughness/AO-levels/metalness remap
// (s6f3a7_env_materials.md change items 1/3, csgo_environment.frag.slang:57-73,348-377,
// 769,795-897,1046). HAS_ENV_HEIGHT1 alone: plain (non-blend) csgo_environment, roughness
// remap + AO levels + metalness only. HAS_ENV_LAYER2 (always paired with HAS_ENV_HEIGHT1):
// csgo_environment_blend, mixes layer 2 in by the height-band weight below.
#ifdef HAS_ENV_HEIGHT1
uniform sampler2D uEnvHeight1;
uniform float uEnvRoughnessContrast1;
uniform float uEnvRoughnessBrightness1;
uniform float uEnvNormalContrast1;
uniform vec3 uEnvAoLevels1;
uniform float uEnvMetalness1Enabled;
// verify_c7df fix item 1: layer 1's own UV transform (csgo_environment.vert.slang:165-171), the
// same RotateVector2D formula layer 2 already gets below -- (scale.x, scale.y, offset.x, offset.y)
// plus a rotation in degrees (e.g. Train's hrts2_blend_metalpanelling03-painted sets 90 on BOTH
// layers, which used to rotate layer 2 only and leave the two 90 degrees apart).
uniform vec4 uEnvUv1;
uniform float uEnvUvRot1;
#endif
// review fix item 1 (s6f3a7_env_materials.md review): per-layer colour correction --
// csgo_environment.frag.slang:781-787,804-810. uCCnColorAdjust/uCCnAdjust are
// g_mTextureColorAdjust{n}/g_mTextureAdjust{n} (RenderMaterial.cs:671-744,
// crates/s2render/src/color_correct.rs); uCCnMode is g_nColorCorrectionMode{n}; uCCnTm is
// (tintMaskContrast{n}, tintMaskBrightness{n}), remapping height{n}.g into the tintMask{n} this
// mixes colorAdjust in by.
#ifdef HAS_CC1
uniform mat4 uCC1ColorAdjust;
uniform mat4 uCC1Adjust;
uniform float uCC1Mode;
uniform vec2 uCC1Tm;
#endif
#ifdef HAS_CC2
uniform mat4 uCC2ColorAdjust;
uniform mat4 uCC2Adjust;
uniform float uCC2Mode;
uniform vec2 uCC2Tm;
#endif
#ifdef HAS_ENV_LAYER2
// review fix item 2: layer 2's own UV transform (csgo_environment.vert.slang:175-180,
// utils.slang:135-138 RotateVector2D) -- (scale.x, scale.y, offset.x, offset.y) plus a rotation
// in degrees (90 on a couple of Ancient/Train materials); the centre is always the default 0.5
// on every material surveyed, so it is folded into the formula below.
uniform vec4 uEnvUv2;
uniform float uEnvUvRot2;
#ifdef ENV_FACING2
// F_BLEND_BY_FACING_DIRECTION_2 (csgo_environment.vert.slang:246-252): the paint weight is
// scaled by how much the surface faces g_vFacingDirection2 -- e.g. moss only on upward faces.
uniform vec3 uEnvFacingDir2;
uniform vec2 uEnvFacingMinMax2;
#endif
uniform sampler2D uEnvColor2;
uniform sampler2D uEnvNormal2;
uniform sampler2D uEnvHeight2;
uniform float uEnvRoughnessContrast2;
uniform float uEnvRoughnessBrightness2;
uniform float uEnvNormalContrast2;
uniform vec3 uEnvAoLevels2;
uniform float uEnvMetalness2Enabled;
uniform float uEnvHeightScale1;
uniform float uEnvHeightZeroPoint1;
uniform float uEnvHeightScale2;
uniform float uEnvHeightZeroPoint2;
uniform float uEnvBlendSoftness2;
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
// review fix item 2: layer 2's own UV (csgo_environment.vert.slang:175-180's RotateVector2D
// around a fixed center=(0.5,0.5), rotation=0) -- every layer-2 sample (height2/color2/normal2/
// metalness2) reads through this, not the mesh's raw vUv.
// review fix item 10 (load time): uEnvHeight1/2 sampled exactly once each here and reused below
// (tint-mask lookup, blend weight, metalness) -- previously sampled up to 3x per fragment.
// verify_c7df fix item 1: layer 1's own UV (identity when this isn't csgo_environment(_blend) at
// all) -- every layer-1 sample (height1/albedo/normal) reads through uvL1, not the mesh's raw vUv.
  vec2 uvL1 = vUv;
#ifdef HAS_ENV_HEIGHT1
  {
    vec2 p = uEnvUv1.xy * ( vUv - 0.5 );
    float r = radians( uEnvUvRot1 );
    uvL1 = vec2( cos( r ) * p.x - sin( r ) * p.y, sin( r ) * p.x + cos( r ) * p.y ) + 0.5 + uEnvUv1.zw;
  }
  vec4 envHeight1Sample = texture2D( uEnvHeight1, uvL1 );
#endif
#ifdef HAS_ENV_LAYER2
  vec2 vUv2;
  {
    vec2 p = uEnvUv2.xy * ( vUv - 0.5 );
    float r = radians( uEnvUvRot2 );
    vUv2 = vec2( cos( r ) * p.x - sin( r ) * p.y, sin( r ) * p.x + cos( r ) * p.y ) + 0.5 + uEnvUv2.zw;
  }
  vec4 envHeight2Sample = texture2D( uEnvHeight2, vUv2 );
#endif

#ifdef HAS_ALBEDO_MAP
  {
    vec4 s = texture2D( uAlbedoMap, uvL1 );
#ifdef ALBEDO_MANUAL_SRGB
    s.rgb = srgbToLinear( s.rgb );
#endif
    // review fix item 1: csgo_environment.frag.slang:781-787 -- tintMask1 = remap(height1.g),
    // mixes the untinted-or-raw colour with the tinted colour-adjust matrix's own output.
#ifdef HAS_CC1
    {
      float tm = clamp( ( ( envHeight1Sample.g - 0.5 ) * uCC1Tm.x + 0.5 ) * uCC1Tm.y, 0.0, 1.0 );
      vec3 base = uCC1Mode > 0.5 ? ( uCC1Adjust * s ).rgb : s.rgb;
      s.rgb = mix( base, ( uCC1ColorAdjust * s ).rgb, tm );
    }
#endif
    albedoColor *= s;
  }
#endif

// Height-band blend weight (csgo_environment.frag.slang:348-377 GetBlendWeights, legacy
// F_USE_NEW_BLENDING==0 path only -- always run here regardless of what the material actually
// sets; render.json's own extras.envLayer2.unsupported (crates/s2render/src/material.rs review
// fix item 8) flags a material that needs BlendLayer/BlendBandWeight instead, which this shader
// does not implement). Computed once, up front, and reused below for colour/normal/roughness/AO/
// metalness -- every one of those is mix(layer1, layer2, envWeight2), i.e. the reference's own
// CombineColor/CombineNormal/CombineRoughness/CombineOcclusion at their default
// overlay=0/replace=1/combine=0 (:250-260,238-241,243-247,233-236) -- this shader does not read
// a material's own values for those params if it overrides any of them.
// The per-vertex softness bias (vColorBlendValues.w) is not carried by _blend (that varying
// only ever held the paint weight, vColorBlendValues.x) -- only the material's own
// g_flBlendSoftness2 constant widens the seam here, so a level-painted softness override
// would render a slightly harder-edged seam than the game's.
#ifdef HAS_ENV_LAYER2
  float envWeight2;
  {
    float baseHeight1 = envHeight1Sample.r - uEnvHeightZeroPoint1;
    float baseHeight2 = envHeight2Sample.r - uEnvHeightZeroPoint2;
    float softness = clamp( uEnvBlendSoftness2, 0.001, 1.0 );
    float hh1 = uEnvHeightScale1 + softness;
    float height1w = baseHeight1 * hh1;
    float hh2 = uEnvHeightScale2 + softness;
    float h22 = baseHeight2 * ( uEnvHeightScale2 - softness );
    float b1 = ( -uEnvHeightZeroPoint1 * hh1 - ( 1.0 - uEnvHeightZeroPoint2 ) * hh2 ) - softness;
    float b2 = ( 1.0 - uEnvHeightZeroPoint1 ) * hh1 - ( -uEnvHeightZeroPoint2 * hh2 );
    float envPaint2 = vBlendW;
#ifdef ENV_FACING2
    envPaint2 *= smoothstep( uEnvFacingMinMax2.x, uEnvFacingMinMax2.y, dot( uEnvFacingDir2, normalize( vNormal ) ) * 0.5 + 0.5 );
#endif
    float h2x = h22 + mix( b1, b2, envPaint2 );
    float mx = max( height1w, h2x );
    float w1 = max( height1w - mx + softness, 0.0 ) + 0.001;
    float w2 = max( h2x - mx + softness, 0.0 );
    envWeight2 = w2 / ( w1 + w2 );
  }
  {
    vec4 s2 = texture2D( uEnvColor2, vUv2 );
#ifdef ENV_COLOR2_MANUAL_SRGB
    s2.rgb = srgbToLinear( s2.rgb );
#endif
#ifdef HAS_CC2
    {
      float tm = clamp( ( ( envHeight2Sample.g - 0.5 ) * uCC2Tm.x + 0.5 ) * uCC2Tm.y, 0.0, 1.0 );
      vec3 base = uCC2Mode > 0.5 ? ( uCC2Adjust * s2 ).rgb : s2.rgb;
      s2.rgb = mix( base, ( uCC2ColorAdjust * s2 ).rgb, tm );
    }
#endif
    albedoColor.rgb = mix( albedoColor.rgb, s2.rgb, envWeight2 );
    albedoColor.a = mix( albedoColor.a, s2.a, envWeight2 );
  }
#endif

#ifdef HAS_TINT
  {
    float tintAmount = texture2D( uTintMaskMap, vUv ).r;
    albedoColor.rgb = mix( albedoColor.rgb, albedoColor.rgb * uTintColor, tintAmount );
  }
#endif
#ifdef HAS_LAYERS
  float layerBlendB;
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
    layerBlendB = smoothstep( max( 0.0, m.g - m.r ), min( 1.0, m.g + m.r ), vBlendW );
    albedoColor.rgb = mix( albedoColor.rgb, layer2Color, layerBlendB );
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

// s6f3a7_env_materials.md change item 1: layer 1/2's own g_vAmbientOcclusionLevels{1,2} and
// g_tHeight{1,2}.a metalness (gated by g_bMetalness{1,2}), mixed by envWeight2 the same way
// as colour above (csgo_environment.frag.slang:1046,873-898).
#ifdef HAS_ENV_HEIGHT1
  vec3 envAoLevels = uEnvAoLevels1;
  float envMetalness = envHeight1Sample.a * uEnvMetalness1Enabled;
#ifdef HAS_ENV_LAYER2
  envMetalness = mix( envMetalness, envHeight2Sample.a * uEnvMetalness2Enabled, envWeight2 );
  envAoLevels = mix( uEnvAoLevels1, uEnvAoLevels2, envWeight2 );
#endif
#endif
#ifdef ALBEDO_ALPHA_AO
  // review fix item 6: extras.baseColorAlphaMeaning == "ao" (csgo_environment/_blend, opaque) -
  // REPORT.md's channel table.
#ifdef HAS_ENV_HEIGHT1
  // s6f3a7_env_materials.md change item 1: the reference's own AO curve
  // (csgo_environment.frag.slang:1046) -- mix(x,z,pow(ao,y)); at the shader's default levels
  // (0,0.5,1) this is exactly the sqrt approximation replaced below.
  ao *= mix( envAoLevels.x, envAoLevels.z, pow( max( albedoColor.a, 0.0 ), max( envAoLevels.y, 0.001 ) ) );
#else
  ao *= pow( max( albedoColor.a, 0.0 ), 0.5 );
#endif
#endif
  float metalness = uMetalnessFactor;
#ifdef HAS_METALNESS_MAP
  metalness = texture2D( uMetalnessMap, vUv )[ METALNESS_CHANNEL ];
#elif defined( ALBEDO_ALPHA_METALNESS )
  // review fix item 6: extras.baseColorAlphaMeaning == "metalness" (F_METALNESS_TEXTURE, opaque,
  // no separate g_tMetalness texture -- complex.frag.slang:618).
  metalness = albedoColor.a;
#elif defined( HAS_ENV_HEIGHT1 )
  metalness = envMetalness;
#endif
  float roughness = uRoughnessFactor;

  vec3 Ngeom = normalize( vNormal );
#ifdef DOUBLE_SIDED
  if ( !gl_FrontFacing ) Ngeom = -Ngeom;
#endif

  vec3 N = Ngeom;
#ifdef HAS_NORMAL_MAP
  {
    vec4 t = texture2D( uNormalMap, uvL1 );
#ifdef HAS_LAYER2_NORMAL_MAP
    // s6f3a7_env_materials.md change item 5, complex.frag.slang:455-456: "more correct to
    // blend normals after decoding, but it's not actually how S2 does it" -- the two layers' raw
    // (still HemiOct-encoded) texels are mixed here, before any decode, same layerBlendB as the
    // colour blend above; roughness (packed in .b) rides along for free.
    t = mix( t, texture2D( uLayer2NormalMap, vUv ), layerBlendB );
#endif
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
#ifdef HAS_ENV_HEIGHT1
      // s6f3a7_env_materials.md change item 3: csgo_environment.frag.slang:769's contrast/
      // brightness remap on the roughness channel -- unimplemented before this change for every
      // csgo_environment(_blend) material (the raw t.b above was used as-is).
      roughness = clamp( ( ( t.b - 0.5 ) * uEnvRoughnessContrast1 + 0.5 ) * uEnvRoughnessBrightness1, 0.0, 1.0 );
#endif
    }
#endif
    nSample.y = -nSample.y; // VRF utils.slang:261 - GLTFLoader's normalScale.y=-1 used to do this; done explicitly now.
#ifdef HAS_ENV_HEIGHT1
    // verify_c7df fix item 1: this normal was sampled at the rotated uvL1, so its own tangent-
    // space X/Y are rotated too -- counter-rotate by -uEnvUvRot1 before mixing/using it, same idea
    // as layer 2's own negated-rotation fixup below.
    {
      float r1 = radians( -uEnvUvRot1 );
      nSample.xy = vec2( cos( r1 ) * nSample.x - sin( r1 ) * nSample.y, sin( r1 ) * nSample.x + cos( r1 ) * nSample.y );
    }
    // review fix item 7: csgo_environment.frag.slang:486-505 LayerNormal's own contrast remap --
    // Up=(0,0,1) is invariant under the Y-flip above, so applying it before/after is equivalent.
    nSample = normalize( mix( vec3( 0.0, 0.0, 1.0 ), nSample, uEnvNormalContrast1 ) );
#endif
#ifdef HAS_ENV_LAYER2
    {
      // Layer 2's own HemiOct normal + remapped roughness, mixed with layer 1's in tangent space
      // before the TBN transform -- CombineNormal/CombineRoughness at their default params
      // (csgo_environment.frag.slang:238-247,892,896), same envWeight2 as colour above.
      vec4 t2 = texture2D( uEnvNormal2, vUv2 );
      vec2 e2 = vec2( t2.r + t2.g - 1.003922, t2.r - t2.g );
      vec3 nSample2 = normalize( vec3( e2, 1.0 - abs( e2.x ) - abs( e2.y ) ) );
      nSample2.y = -nSample2.y;
      // The normal turns with its UVs; negated because the decode's Y flip mirrors the angle
      // (csgo_environment.frag.slang:493-495).
      {
        float r = radians( -uEnvUvRot2 );
        nSample2.xy = vec2( cos( r ) * nSample2.x - sin( r ) * nSample2.y, sin( r ) * nSample2.x + cos( r ) * nSample2.y );
      }
      nSample2 = normalize( mix( vec3( 0.0, 0.0, 1.0 ), nSample2, uEnvNormalContrast2 ) );
      float roughness2 = clamp( ( ( t2.b - 0.5 ) * uEnvRoughnessContrast2 + 0.5 ) * uEnvRoughnessBrightness2, 0.0, 1.0 );
      nSample = mix( nSample, nSample2, envWeight2 );
      roughness = mix( roughness, roughness2, envWeight2 );
    }
#endif
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
    // `s6f3a7_env_materials.md` change item 5: layer 2's raw (pre-decode) normal texel, mixed in
    // with layer 1's before the HemiOct decode.
    if (recipe.layer2NormalMap) d.HAS_LAYER2_NORMAL_MAP = "";
  }
  // `s6f3a7_env_materials.md` change items 1/3: csgo_environment(_blend)'s own height/roughness-
  // remap/AO-levels/metalness inputs.
  if (recipe.env1) {
    d.HAS_ENV_HEIGHT1 = "";
    // review fix item 1: per-layer colour-correction (only when the material actually carries a
    // non-identity-enough matrix -- always exported when a colour texture loaded, so this is
    // really "did env1/envLayer2 load a colour matrix at all").
    if (recipe.env1.colorAdjust) d.HAS_CC1 = "";
    if (recipe.envLayer2) {
      d.HAS_ENV_LAYER2 = "";
      if (recipe.envLayer2.color2ManualSrgb) d.ENV_COLOR2_MANUAL_SRGB = "";
      if (recipe.envLayer2.colorAdjust) d.HAS_CC2 = "";
      if (recipe.envLayer2.facingDir) d.ENV_FACING2 = "";
    }
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
    if (recipe.layer2NormalMap) uniforms.uLayer2NormalMap = { value: recipe.layer2NormalMap };
  }
  if (recipe.env1) {
    uniforms.uEnvHeight1 = { value: recipe.env1.heightMap };
    uniforms.uEnvRoughnessContrast1 = { value: recipe.env1.roughnessContrast };
    uniforms.uEnvRoughnessBrightness1 = { value: recipe.env1.roughnessBrightness };
    uniforms.uEnvNormalContrast1 = { value: recipe.env1.normalContrast ?? 1 };
    uniforms.uEnvAoLevels1 = { value: recipe.env1.aoLevels };
    uniforms.uEnvMetalness1Enabled = { value: recipe.env1.metalnessEnabled ? 1 : 0 };
    // verify_c7df fix item 1.
    uniforms.uEnvUv1 = { value: new THREE.Vector4(recipe.env1.uvScale[0], recipe.env1.uvScale[1], recipe.env1.uvOffset[0], recipe.env1.uvOffset[1]) };
    uniforms.uEnvUvRot1 = { value: recipe.env1.uvRotation ?? 0 };
    if (recipe.env1.colorAdjust) {
      uniforms.uCC1ColorAdjust = { value: recipe.env1.colorAdjust };
      uniforms.uCC1Adjust = { value: recipe.env1.adjust };
      uniforms.uCC1Mode = { value: recipe.env1.colorCorrectionMode === 1 ? 1 : 0 };
      uniforms.uCC1Tm = { value: new THREE.Vector2(recipe.env1.tintMaskContrast, recipe.env1.tintMaskBrightness) };
    }
    if (recipe.envLayer2) {
      const l2 = recipe.envLayer2;
      uniforms.uEnvColor2 = { value: l2.colorMap };
      uniforms.uEnvNormal2 = { value: l2.normalMap };
      uniforms.uEnvHeight2 = { value: l2.heightMap };
      uniforms.uEnvRoughnessContrast2 = { value: l2.roughnessContrast };
      uniforms.uEnvRoughnessBrightness2 = { value: l2.roughnessBrightness };
      uniforms.uEnvNormalContrast2 = { value: l2.normalContrast ?? 1 };
      uniforms.uEnvAoLevels2 = { value: l2.aoLevels };
      uniforms.uEnvMetalness2Enabled = { value: l2.metalnessEnabled ? 1 : 0 };
      uniforms.uEnvHeightScale1 = { value: l2.heightScale1 };
      uniforms.uEnvHeightZeroPoint1 = { value: l2.heightZeroPoint1 };
      uniforms.uEnvHeightScale2 = { value: l2.heightScale2 };
      uniforms.uEnvHeightZeroPoint2 = { value: l2.heightZeroPoint2 };
      uniforms.uEnvBlendSoftness2 = { value: l2.blendSoftness2 };
      // review fix item 2.
      uniforms.uEnvUv2 = { value: new THREE.Vector4(l2.uvScale[0], l2.uvScale[1], l2.uvOffset[0], l2.uvOffset[1]) };
      uniforms.uEnvUvRot2 = { value: l2.uvRotation ?? 0 };
      if (l2.facingDir) {
        uniforms.uEnvFacingDir2 = { value: new THREE.Vector3(...l2.facingDir) };
        uniforms.uEnvFacingMinMax2 = { value: new THREE.Vector2(...l2.facingMinMax) };
      }
      if (l2.colorAdjust) {
        uniforms.uCC2ColorAdjust = { value: l2.colorAdjust };
        uniforms.uCC2Adjust = { value: l2.adjust };
        uniforms.uCC2Mode = { value: l2.colorCorrectionMode === 1 ? 1 : 0 };
        uniforms.uCC2Tm = { value: new THREE.Vector2(l2.tintMaskContrast, l2.tintMaskBrightness) };
      }
    }
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
