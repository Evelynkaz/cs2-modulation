// Compressed lighting textures for F3b-2 (`s6f3b2_lighting_shader.md` §3, REPORT.md §1/§4/§8):
// raw BC blocks from `render_lm_*.bin`/`render_sky_cube.bin` go straight into a `CompressedTexture`
// (or `CompressedCubeTexture` for the sky) with no re-encoding - the browser's own GPU decodes
// BC6H/BC4/BC1 - and a small always-generated RGBM/plain PNG fallback covers browsers without the
// matching `WEBGL_compressed_texture_*`/`EXT_texture_compression_*` extension.

import * as THREE from "three";

const FORMAT_ENUM = {
  BC6H_UF16: THREE.RGB_BPTC_UNSIGNED_Format,
  BC7: THREE.RGBA_BPTC_Format,
  BC4: THREE.RED_RGTC1_Format,
  BC5: THREE.RED_GREEN_RGTC2_Format,
  BC1: THREE.RGB_S3TC_DXT1_Format,
};

// Which WebGL2 extension each raw format needs (`s6f3b2_lighting_shader.md` §3).
const FORMAT_EXTENSION = {
  BC6H_UF16: "bptc",
  BC7: "bptc",
  BC4: "rgtc",
  BC5: "rgtc",
  BC1: "s3tc",
};

export function detectCompressionSupport(renderer) {
  return {
    bptc: renderer.extensions.has("EXT_texture_compression_bptc"),
    rgtc: renderer.extensions.has("EXT_texture_compression_rgtc"),
    s3tc: renderer.extensions.has("WEBGL_compressed_texture_s3tc"),
  };
}

export function canUseRawFormat(support, formatName) {
  const need = FORMAT_EXTENSION[formatName];
  return need != null && support[need] === true;
}

async function fetchArrayBuffer(url) {
  const res = await fetch(url, { cache: "default" });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} for ${url}`);
  }
  return res.arrayBuffer();
}

// A single raw block file is a single mip level (F3a-4 only ever exports one), so filtering is
// bilinear at best - not the in-game trilinear, which samples across a mip chain this file doesn't
// carry (`s6f3b2_lighting_shader.md` §3 "трилинейная фильтрация" - noted as a simplification).
export async function loadCompressedLightmap(url, entry) {
  const enumFormat = FORMAT_ENUM[entry.format];
  if (!enumFormat) {
    throw new Error(`unknown lightmap format ${entry.format}`);
  }
  const buffer = await fetchArrayBuffer(url);
  const tex = new THREE.CompressedTexture(
    [{ data: new Uint8Array(buffer), width: entry.width, height: entry.height }],
    entry.width,
    entry.height,
    enumFormat,
    THREE.UnsignedByteType,
  );
  tex.wrapS = THREE.ClampToEdgeWrapping;
  tex.wrapT = THREE.ClampToEdgeWrapping;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.colorSpace = THREE.NoColorSpace; // HDR/scalar data, never sRGB (REPORT.md §1).
  tex.needsUpdate = true;
  return tex;
}

// `sky.mips` covers the whole cube file: each mip's byte range holds all 6 faces back to back, in
// `sky.faceOrder` order (REPORT.md §4 "contiguous per mip, mips smallest first") - so a face's own
// slice is `byteLength/6` wide, at `byteOffset + face*(byteLength/6)`. Uploaded as a plain
// `CompressedCubeTexture` (a raw 6-image container, not `scene.background`) so nothing here runs
// through three's background/env-map sampling shaders, which flip X for the reflection convention -
// `lightingSky.js` samples this cube with its own GLSL using the exact `R^-1 * v` direction from
// REPORT.md §4/§6, so no flip anywhere applies.
export async function loadCompressedSkyCube(url, skyJson) {
  const enumFormat = FORMAT_ENUM[skyJson.format];
  if (!enumFormat) {
    throw new Error(`unknown sky format ${skyJson.format}`);
  }
  const buffer = await fetchArrayBuffer(url);
  const bytes = new Uint8Array(buffer);
  const mipsByLevel = [...skyJson.mips].sort((a, b) => a.level - b.level); // level 0 = largest, first.
  const perFace = mipsByLevel.map((m) => ({
    level: m.level,
    width: m.faceWidth,
    height: m.faceHeight,
    faceBytes: m.byteLength / 6,
    byteOffset: m.byteOffset,
  }));
  const images = [];
  for (let face = 0; face < 6; face++) {
    const mipmaps = perFace.map((m) => ({
      data: bytes.subarray(m.byteOffset + face * m.faceBytes, m.byteOffset + (face + 1) * m.faceBytes),
      width: m.width,
      height: m.height,
    }));
    images.push({ width: perFace[0].width, height: perFace[0].height, mipmaps });
  }
  const tex = new THREE.CompressedCubeTexture(images, enumFormat, THREE.UnsignedByteType);
  tex.wrapS = THREE.ClampToEdgeWrapping;
  tex.wrapT = THREE.ClampToEdgeWrapping;
  tex.minFilter = THREE.LinearMipmapLinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.colorSpace = THREE.NoColorSpace; // HDR cube values, sampled and tinted by hand (REPORT.md §4).
  tex.needsUpdate = true;
  return tex;
}

// `direct_light_shadows`' fallback PNG carries the same channel meaning as the raw file (no RGBM),
// so it needs no decode beyond reading `.r` - only `irradiance`'s fallback is RGBM8 (§ decode below,
// applied in the shader itself so it stays HDR instead of being clamped by an 8-bit CPU decode).
export async function loadFallbackTexture(url) {
  const tex = await new THREE.TextureLoader().loadAsync(url);
  // glTF (and this exporter's own TEXCOORD_1) has V=0 at the top, same as the compressed raw
  // blocks above and `render.glb`'s own base-color textures (loaded by `GLTFLoader` with
  // `flipY = false`) - `TextureLoader` defaults to `flipY = true`, so it must be turned off here
  // to sample against the same UVs.
  tex.flipY = false;
  tex.wrapS = THREE.ClampToEdgeWrapping;
  tex.wrapT = THREE.ClampToEdgeWrapping;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.generateMipmaps = false;
  tex.colorSpace = THREE.NoColorSpace; // "PNG is linear, not sRGB" (render.json).
  tex.needsUpdate = true;
  return tex;
}

// The 6 fallback sky faces (RGBM8 PNG, `s6f3b2_lighting_shader.md` §3/§4) as a plain `CubeTexture` -
// same reasoning as `loadCompressedSkyCube`: a data container only, never `scene.background`,
// sampled by `lightingSky.js`'s own shader. The browser generates a real mip chain for it (unlike
// the single-mip lightmaps, these are small enough, and the fog LOD formula needs one) - see
// `skyMipCount` on the returned texture, since it won't match `render.json`'s `sky.mips.length` (8,
// sized for the 512² compressed cube).
export async function loadFallbackSkyCube(urls) {
  const images = await Promise.all(
    urls.map(
      (u) =>
        new Promise((resolve, reject) => {
          const img = new Image();
          img.onload = () => resolve(img);
          img.onerror = () => reject(new Error(`failed to load ${u}`));
          img.src = u;
        }),
    ),
  );
  const tex = new THREE.CubeTexture(images);
  tex.wrapS = THREE.ClampToEdgeWrapping;
  tex.wrapT = THREE.ClampToEdgeWrapping;
  tex.minFilter = THREE.LinearMipmapLinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.generateMipmaps = true;
  tex.colorSpace = THREE.NoColorSpace; // RGBM, decoded in the shader.
  tex.needsUpdate = true;
  tex.skyMipCount = Math.floor(Math.log2(Math.max(images[0].width, images[0].height))) + 1;
  return tex;
}

// `render_lut.bin`: RGBA8, 32^3, x=R (`s6f3b2_lighting_shader.md` §1). No fallback - tiny, always
// the raw format, no compressed-texture extension needed.
export async function loadLut(url, dim) {
  const buffer = await fetchArrayBuffer(url);
  const tex = new THREE.Data3DTexture(new Uint8Array(buffer), dim, dim, dim);
  tex.format = THREE.RGBAFormat;
  tex.type = THREE.UnsignedByteType;
  tex.wrapS = THREE.ClampToEdgeWrapping;
  tex.wrapT = THREE.ClampToEdgeWrapping;
  tex.wrapR = THREE.ClampToEdgeWrapping;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.colorSpace = THREE.NoColorSpace;
  tex.needsUpdate = true;
  return tex;
}
