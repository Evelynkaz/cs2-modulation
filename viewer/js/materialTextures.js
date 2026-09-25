// Native BC7/BC1(DXT1)/BC4(ATI1N)/RGBA8 material textures (`s6f3a6_native_tex.md`): loads
// render.json's own `textures[]` entries - raw, still block-compressed (or, for the one RGBA8888
// texture on de_inferno, uncompressed) game mip chains in `render_tex/<sha12>.bin` - as
// `THREE.CompressedTexture`/`THREE.DataTexture`, no CPU-side pixel decode, no JPEG/PNG. Distinct
// from `lightingTextures.js` (baked lighting: lightmaps/sky cube/LUT, always exactly one mip and
// never referenced from a glTF material's `extras`).
//
// Shared by both lighting paths (`lighting.js`'s game uber-shader and `scene3d.js`'s "simple"
// `MeshStandardMaterial` patches, change item 5): each material index is fetched/decoded once and
// the resulting texture reused by every consumer, same dedup story `native_texture::TextureCatalog`
// already ran once per unique `render_tex/*.bin` on the Rust side.

import * as THREE from "three";
import { renderAssetUrl } from "./api.js?v=1";

const FORMAT_ENUM = {
  BC7: THREE.RGBA_BPTC_Format,
  BC1: THREE.RGBA_S3TC_DXT1_Format, // the RGBA variant, never RGB_S3TC_DXT1_Format (REPORT.md's D3D11 note: the RGB variant gave alpha 0 for transparent-mode texels).
  BC4: THREE.RED_RGTC1_Format,
};

const CACHE_NAME = "cs2mod-render-tex";
// Bounded parallelism for `render_tex/*.bin` fetches (change item 3 "по отдельности, параллельно
// с ограничением") - high enough to saturate HTTP/1.1's typical 6-connection-per-origin cap plus
// headroom for HTTP/2 multiplexing, low enough not to fire hundreds of simultaneous requests for a
// map with 600+ unique textures.
const DEFAULT_CONCURRENCY = 8;

/**
 * Checks the four WebGL(2) extensions every native format needs (change item 1): `s3tc`/`bptc`/
 * `rgtc` for the formats themselves, `s3tcSrgb` only to let the GPU decode a DXT1 sRGB texture
 * natively - its absence doesn't block loading DXT1 at all, just forces the shader to do that
 * decode by hand (see `wantsManualSrgb`).
 */
export function detectMaterialTextureSupport(renderer) {
  return {
    bptc: renderer.extensions.has("EXT_texture_compression_bptc"),
    rgtc: renderer.extensions.has("EXT_texture_compression_rgtc"),
    s3tc: renderer.extensions.has("WEBGL_compressed_texture_s3tc"),
    s3tcSrgb: renderer.extensions.has("WEBGL_compressed_texture_s3tc_srgb"),
  };
}

/** True if `support` covers whatever extension `format` needs to upload at all (independent of
 * sRGB - RGBA8 is never compressed, so it needs nothing). */
export function canUploadFormat(support, format) {
  if (format === "BC7") return support.bptc;
  if (format === "BC1") return support.s3tc;
  if (format === "BC4") return support.rgtc;
  if (format === "RGBA8") return true;
  return false;
}

/** A human-readable sentence for whichever extension(s) are missing, for `hasGameLightingData`-style
 * callers that want to tell the user what's missing rather than silently falling back (change item
 * 1's "нет других -> понятное сообщение"). `null` when every format used by `renderJson.textures`
 * is uploadable. */
export function describeMissingExtensions(support, renderJson) {
  const used = new Set((renderJson.textures ?? []).map((t) => t.format));
  const missing = [];
  if (used.has("BC7") && !support.bptc) missing.push("EXT_texture_compression_bptc (BC7)");
  if (used.has("BC1") && !support.s3tc) missing.push("WEBGL_compressed_texture_s3tc (BC1/DXT1)");
  if (used.has("BC4") && !support.rgtc) missing.push("EXT_texture_compression_rgtc (BC4)");
  if (missing.length === 0) return null;
  return `this browser/GPU is missing: ${missing.join(", ")} - some map textures cannot be displayed`;
}

/** DXT1 (BC1) sRGB without `WEBGL_compressed_texture_s3tc_srgb`: upload as linear and let the
 * shader decode sRGB->linear itself (change item 1's fallback) - the only format/extension
 * combination that ever needs this (BC7's sRGB variant is part of the same `EXT_texture_compression_bptc`
 * extension that already gates BC7 entirely; BC4 is never sRGB). */
function wantsManualSrgb(entry, support) {
  return entry.colorSpace === "srgb" && entry.format === "BC1" && !support.s3tcSrgb;
}

async function fetchArrayBufferCached(url) {
  let cache = null;
  let cached = null;
  try {
    cache = await caches.open(CACHE_NAME);
    cached = (await cache.match(url)) ?? null;
  } catch {
    cache = null; // Cache Storage unavailable (e.g. an insecure origin) - plain fetch below.
  }
  const headers = {};
  const cachedEtag = cached?.headers.get("ETag");
  if (cachedEtag) headers["If-None-Match"] = cachedEtag;
  const res = await fetch(url, { cache: "no-store", headers });
  if (res.status === 304 && cached) {
    return cached.arrayBuffer();
  }
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} for ${url}`);
  }
  const buffer = await res.arrayBuffer();
  if (cache && res.headers.get("ETag")) {
    cache.put(url, new Response(buffer.slice(0), { headers: res.headers })).catch(() => {});
  }
  return buffer;
}

// Review fix item 5: three.js calls `texture.onUpdate(texture)` immediately after the GPU upload
// (`WebGLTextures.js`'s `setTexture2D`/`setTextureCube`) - dropping every mip's `data` view (and,
// for a `DataTexture`, the base `.image.data` the constructor set) there lets the underlying
// ArrayBuffer this loader fetched be garbage-collected instead of staying resident in JS alongside
// the GPU's own copy (measured ~330 MiB retained on de_inferno before this fix). Left un-nulled:
// `t.mipmaps` itself (kept as an array of `{width,height,data:null}` placeholders - some call sites
// check `mipmaps.length`) and everything else on the texture, since a real WebGL context loss (see
// `scene3d.js`'s `webglcontextrestored` handler) recovers by reloading the page and refetching from
// Cache Storage, not by re-uploading this now-emptied JS-side copy.
function freeCpuCopyAfterUpload(tex) {
  tex.onUpdate = (t) => {
    for (const m of t.mipmaps) m.data = null;
    if (t.isDataTexture) t.image.data = null;
    t.onUpdate = null;
  };
}

function buildCompressedTexture(entry, buffer, manualSrgb, anisotropy) {
  const bytes = new Uint8Array(buffer);
  const mipmaps = entry.levels.map((l) => ({
    data: bytes.subarray(l.offset, l.offset + l.length),
    width: l.width,
    height: l.height,
  }));
  const tex = new THREE.CompressedTexture(
    mipmaps,
    entry.levels[0].width,
    entry.levels[0].height,
    FORMAT_ENUM[entry.format],
    THREE.UnsignedByteType,
  );
  // §2: repeat wrap, trilinear (mip chain is the game's own, so a real min-mip filter applies),
  // generateMipmaps stays false throughout - the chain already came from `render_tex/*.bin`.
  tex.wrapS = THREE.RepeatWrapping;
  tex.wrapT = THREE.RepeatWrapping;
  tex.minFilter = mipmaps.length > 1 ? THREE.LinearMipmapLinearFilter : THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.generateMipmaps = false;
  tex.colorSpace = manualSrgb ? THREE.NoColorSpace : entry.colorSpace === "srgb" ? THREE.SRGBColorSpace : THREE.NoColorSpace;
  // Review fix item 2: `anisotropy` must be set BEFORE the texture's first upload -
  // `WebGLTextures.js` only calls `gl.texParameterf(..., TEXTURE_MAX_ANISOTROPY, ...)` while
  // setting up a texture's sampler parameters during that same upload pass, so a value assigned
  // afterward (as `scene3d.js` used to, post `renderer.initTexture`) never reaches the GPU.
  tex.anisotropy = anisotropy;
  freeCpuCopyAfterUpload(tex);
  tex.needsUpdate = true;
  return tex;
}

function buildRgba8Texture(entry, buffer, anisotropy) {
  const bytes = new Uint8Array(buffer);
  // Review fix item 4: upload every level `render.json` lists (Inferno's textures[298] carries
  // 11), not just level 0 - `THREE.DataTexture` accepts a manual `.mipmaps` chain the same way
  // `CompressedTexture` does (`WebGLTextures.js`'s `isDataTexture` upload path checks
  // `mipmaps.length > 0` the same way), so a single level no longer meant "no other levels exist".
  const mipmaps = entry.levels.map((l) => ({
    data: bytes.subarray(l.offset, l.offset + l.length),
    width: l.width,
    height: l.height,
  }));
  const level0 = entry.levels[0];
  const tex = new THREE.DataTexture(mipmaps[0].data, level0.width, level0.height, THREE.RGBAFormat, THREE.UnsignedByteType);
  tex.mipmaps = mipmaps;
  tex.wrapS = THREE.RepeatWrapping;
  tex.wrapT = THREE.RepeatWrapping;
  tex.minFilter = mipmaps.length > 1 ? THREE.LinearMipmapLinearFilter : THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.generateMipmaps = false;
  tex.colorSpace = entry.colorSpace === "srgb" ? THREE.SRGBColorSpace : THREE.NoColorSpace;
  tex.anisotropy = anisotropy;
  freeCpuCopyAfterUpload(tex);
  tex.needsUpdate = true;
  return tex;
}

/**
 * Builds a per-map loader over `renderJson.textures[]`: `get(index)` returns a `Promise` for the
 * `THREE.Texture` at that index (shared/memoized across every caller), decoded to the same
 * `manualSrgb`/`codec` metadata the shader defines need (`tex.userData`). Bounded concurrency and
 * a combined byte-progress callback (change item 3).
 */
export function createMaterialTextureLoader(
  map,
  renderJson,
  renderer,
  { concurrency = DEFAULT_CONCURRENCY, onProgress, anisotropy = 1 } = {},
) {
  const support = detectMaterialTextureSupport(renderer);
  const cache = new Map(); // textures[] index -> Promise<THREE.Texture>
  const queue = [];
  let active = 0;
  let bytesLoaded = 0;

  function pump() {
    while (active < concurrency && queue.length > 0) {
      const job = queue.shift();
      active++;
      job().finally(() => {
        active--;
        pump();
      });
    }
  }

  function schedule(fn) {
    return new Promise((resolve, reject) => {
      queue.push(() =>
        fn()
          .then(resolve)
          .catch(reject),
      );
      pump();
    });
  }

  async function loadOne(index) {
    const entry = renderJson.textures[index];
    if (!entry) {
      throw new Error(`render.json textures[${index}] is missing`);
    }
    if (!canUploadFormat(support, entry.format)) {
      throw new Error(`texture ${entry.path}: format ${entry.format} not supported by this browser/GPU`);
    }
    return schedule(async () => {
      const buffer = await fetchArrayBufferCached(renderAssetUrl(map, entry.file));
      bytesLoaded += buffer.byteLength;
      onProgress?.(bytesLoaded);
      const manualSrgb = wantsManualSrgb(entry, support);
      const tex =
        entry.format === "RGBA8"
          ? buildRgba8Texture(entry, buffer, anisotropy)
          : buildCompressedTexture(entry, buffer, manualSrgb, anisotropy);
      tex.userData.manualSrgb = manualSrgb;
      tex.userData.codec = entry.codec;
      // Uploads eagerly (rather than lazily on first render); `onUpdate` (set above, in
      // `freeCpuCopyAfterUpload`) fires during this call and drops the CPU-side copy right after
      // (review fix item 5).
      renderer.initTexture(tex);
      return tex;
    });
  }

  return {
    support,
    missingExtensionsMessage: describeMissingExtensions(support, renderJson),
    /** `null` for a missing/absent index; otherwise a `Promise<THREE.Texture>`, memoized. */
    get(index) {
      if (index == null) return null;
      if (!cache.has(index)) cache.set(index, loadOne(index));
      return cache.get(index);
    },
    bytesLoaded() {
      return bytesLoaded;
    },
    textureCount() {
      return cache.size;
    },
    dispose() {
      for (const p of cache.values()) {
        p.then((t) => t.dispose()).catch(() => {});
      }
      cache.clear();
    },
  };
}

// ---- codec decode shared by both lighting paths' CONSTANT (4x4 single-mip) handling -------------

/** `s2tex::transform::decode_hemi_oct`, ported: RG hemi-oct -> unit XYZ, B (packed roughness) ->
 * roughness `0..1`. Used only for a material's `*Constant` extras (a 4x4 source folded to one
 * value at export time, `native_texture::Loaded::Constant`) - a real (non-constant) normal map is
 * decoded per-texel in the fragment shader instead (`lightingShader.js`). */
export function decodeHemiOctConstant(raw) {
  const r = raw[0];
  const g = raw[1];
  const nx = (r + g) / 255 - 1.003922;
  const ny = (r - g) / 255;
  const nz = 1 - Math.abs(nx) - Math.abs(ny);
  const len = Math.sqrt(nx * nx + ny * ny + nz * nz) || 1;
  return { normal: [nx / len, ny / len, nz / len], roughness: raw[2] / 255 };
}

/** sRGB (gamma) -> linear, one channel, `0..1` input/output - `material.rs`'s `srgb_to_linear`,
 * used to fold a `*Constant` sRGB-role byte value into a linear uniform the same way the exporter
 * folds a scalar tint. */
export function srgbToLinear(c) {
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}
