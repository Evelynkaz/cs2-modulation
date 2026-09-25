//! Native BC-texture export (`s6f3a6_native_tex.md`): each unique `(path, base mip level)`
//! texture's still-block-compressed mip chain (levels `L..=last`, `s2tex::decode_raw_mip`, `L` the
//! largest mip at or under the caller's budget, `s2tex::pick_mip_for_budget`) is written once to
//! its own `render_tex/<sha12>.bin`, described by one entry in `render.json`'s `textures[]` --
//! unlike the old F3a-3/F3a-5 path (removed), there is no CPU-side pixel decode and no JPEG/PNG
//! re-encoding, so the viewer uploads the game's own BC7/BC1/BC4/RGBA8 blocks and mips straight to
//! the GPU (change items 1-2, 7). A 4x4 single-mip source is a baked-in constant instead (change
//! item 3): decoded and codec-applied on the CPU here, returned as one RGBA8 pixel rather than a
//! file, so `material.rs`'s callers can fold it into `extras` (e.g. a flat normal's roughness).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use s2fmt::resource::{FourCC, Resource, ResourceError};

use crate::source::Sources;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSpace {
    Srgb,
    Linear,
}

impl ColorSpace {
    pub fn as_str(self) -> &'static str {
        match self {
            ColorSpace::Srgb => "srgb",
            ColorSpace::Linear => "linear",
        }
    }
}

/// Post-decode pixel transform the viewer's shader must apply after sampling, mirroring
/// `s2tex::transform::TextureCodec` (`REPORT.md`'s "Codec flags"): only `HemiOct` is ever seen on
/// either surveyed map, the rest exist purely so a texture that does use them elsewhere in the
/// game doesn't silently render wrong (change item 4: "за define'ами... но есть в игре").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    None,
    HemiOct,
    Dxt5nm,
    YCoCg,
    ReconstructZ,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::HemiOct => "hemiOct",
            Codec::Dxt5nm => "dxt5nm",
            Codec::YCoCg => "ycocg",
            Codec::ReconstructZ => "reconstructZ",
        }
    }
}

fn codec_from_flags(codec: s2tex::TextureCodec) -> Codec {
    if codec.hemi_oct_rb {
        Codec::HemiOct
    } else if codec.dxt5nm {
        Codec::Dxt5nm
    } else if codec.ycocg {
        Codec::YCoCg
    } else if codec.normalize_normals {
        Codec::ReconstructZ
    } else {
        Codec::None
    }
}

/// The four pixel formats any material texture on either surveyed map ever uses
/// (`REPORT.md`'s headline: "every material texture the exporter uses is BC7, DXT1 or BC4... except
/// one RGBA8888").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFormat {
    Bc7,
    Bc1,
    Bc4,
    Rgba8,
}

impl NativeFormat {
    fn from_vtex(format: s2tex::VTexFormat) -> Option<NativeFormat> {
        match format {
            s2tex::VTexFormat::Bc7 => Some(NativeFormat::Bc7),
            s2tex::VTexFormat::Dxt1 => Some(NativeFormat::Bc1),
            s2tex::VTexFormat::Ati1N => Some(NativeFormat::Bc4),
            s2tex::VTexFormat::Rgba8888 => Some(NativeFormat::Rgba8),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            NativeFormat::Bc7 => "BC7",
            NativeFormat::Bc1 => "BC1",
            NativeFormat::Bc4 => "BC4",
            NativeFormat::Rgba8 => "RGBA8",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeTextureError {
    #[error("{path}: not found in any source VPK")]
    Missing { path: String },
    #[error("{path}: failed to parse as a resource: {source}")]
    Resource {
        path: String,
        #[source]
        source: ResourceError,
    },
    #[error("{path}: {source}")]
    Tex {
        path: String,
        #[source]
        source: s2tex::TexError,
    },
    #[error(
        "{path}: material texture format {format:?} has no native WebGL mapping (only BC7/DXT1/ATI1N/RGBA8888 are used by any material texture on the surveyed maps -- REPORT.md's headline)"
    )]
    UnsupportedFormat {
        path: String,
        format: s2tex::VTexFormat,
    },
}

/// One mip level's placement inside its texture's `render_tex/<sha12>.bin` file.
#[derive(Debug, Clone, Copy)]
struct LevelInfo {
    width: u32,
    height: u32,
    offset: u64,
    length: u64,
}

/// A registered, deduplicated native texture (one entry in `render.json`'s `textures[]`).
struct TextureEntry {
    path: String,
    format: NativeFormat,
    color_space: ColorSpace,
    codec: Codec,
    file_name: String,
    sha12: String,
    levels: Vec<LevelInfo>,
    byte_length: u64,
}

/// What [`TextureCatalog::load`] hands back: either a registered texture (an index into
/// `textures[]`) or, for a 4x4 single-mip source, its constant value (change item 3) -- `raw` is
/// the block's decoded RGBA8 pixel *before* any codec transform, deliberately the same
/// pre-transform representation a real texture sample would be in, tagged with the same `codec` a
/// real texture's `textures[]` entry would carry. This lets a caller (and the viewer's shader) run
/// the exact same per-texel decode (e.g. HemiOct) on a constant as it would on a sampled texel,
/// rather than duplicating that decode in Rust just for the 4x4 case -- e.g. a `materials/default/`
/// flat normal's `raw` is still HemiOct-encoded in R/G with roughness packed in B, same as any
/// other normal map's texel, decoded to XYZ + roughness the same way on either path.
#[derive(Debug, Clone, Copy)]
pub enum Loaded {
    Texture(u32),
    Constant { raw: [u8; 4], codec: Codec },
}

/// Accumulates every unique native texture a map export touches, across every material.
#[derive(Default)]
pub struct TextureCatalog {
    /// `(compiled path, base level L, colour space)` -> already-resolved result (change item 1's
    /// "дедупликация по пути+L"): a texture reused by a second material at the same effective
    /// budget *and* colour space is decoded and written only once. Colour space is part of the
    /// key (review fix item 3) because it's decided by the shader parameter, not the texture
    /// itself -- the same vtex_c can legitimately be sRGB in one material slot (e.g. a mod2x
    /// overlay's `g_tColor`) and linear in another, and caching only by (path, level) silently
    /// handed the wrong entry's `colorSpace` to whichever slot asked second.
    by_path_level: HashMap<(String, u32, ColorSpace), Loaded>,
    /// A 4x4-constant's cache is colour-space-independent (the `Loaded::Constant` value is always
    /// pre-codec raw bytes; the caller/shader decides how to interpret them), so it's kept
    /// separate from `by_path_level` rather than needing a dummy colour space in that key.
    by_constant: HashMap<String, Loaded>,
    /// `(content hash, colour space)` -> already-registered texture index (change item 1's "затем
    /// по SHA-256 содержимого"): catches the same bytes reached via a different vtex path, or the
    /// same texture used in two different roles that happen to share a budget and colour space.
    by_content_hash: HashMap<([u8; 32], ColorSpace), u32>,
    /// Content hash alone -> the first entry index registered for it, any colour space -- used
    /// only to reuse an already-written `render_tex/<sha12>.bin`'s file/levels when the *same*
    /// bytes are needed again under a *different* colour space (review fix item 3: no second
    /// write of the blob, just a new metadata-only `TextureEntry`).
    by_hash_any_color_space: HashMap<[u8; 32], u32>,
    entries: Vec<TextureEntry>,
    files: Vec<(String, Vec<u8>)>,
    dedup_hits: u32,
    dedup_bytes_saved: u64,
}

/// First 6 bytes of a SHA-256 digest, as 12 lowercase hex characters -- matches the server's
/// `render_tex/<sha12>.bin` whitelist pattern (`^[0-9a-f]{12}\.bin$`).
fn sha12(hash: &[u8; 32]) -> String {
    hash[..6].iter().map(|b| format!("{b:02x}")).collect()
}

impl TextureCatalog {
    /// Loads `compiled_path` (already `_c`-suffixed) at `max_side`'s budget, `color_space` as
    /// decided by the *shader parameter* the caller is resolving (`REPORT.md`: "решает параметр
    /// шейдера, не текстура"), registering a new `render_tex/<sha12>.bin` entry only if this
    /// exact (path, level) and content hasn't been seen yet.
    pub fn load(
        &mut self,
        sources: &Sources,
        compiled_path: &str,
        max_side: u32,
        color_space: ColorSpace,
    ) -> Result<Loaded, NativeTextureError> {
        let bytes = sources
            .read(compiled_path)
            .ok_or_else(|| NativeTextureError::Missing {
                path: compiled_path.to_string(),
            })?;
        let resource =
            Resource::parse(bytes.clone()).map_err(|source| NativeTextureError::Resource {
                path: compiled_path.to_string(),
                source,
            })?;
        let data_block = resource
            .block(FourCC::DATA)
            .ok_or_else(|| NativeTextureError::Tex {
                path: compiled_path.to_string(),
                source: s2tex::TexError::MissingDataBlock,
            })?;
        let header = s2tex::header::parse(resource.block_bytes(data_block)).map_err(|source| {
            NativeTextureError::Tex {
                path: compiled_path.to_string(),
                source,
            }
        })?;
        let codec_flags = s2tex::resolve_codec(&resource, header.format, header.flags.is_cube());
        let codec = codec_from_flags(codec_flags);

        let is_constant = header.width == 4
            && header.height == 4
            && header.num_mip_levels == 1
            && header.depth <= 1
            && !header.flags.is_cube()
            && !header.flags.is_volume();

        if is_constant {
            let key = compiled_path.to_string();
            if let Some(&cached) = self.by_constant.get(&key) {
                return Ok(cached);
            }
            let raw = s2tex::decode_raw_mip_bytes(&bytes, 0).map_err(|source| {
                NativeTextureError::Tex {
                    path: compiled_path.to_string(),
                    source,
                }
            })?;
            let decoded = s2tex::decode_raw_mip_slice_ldr(&raw, 0).map_err(|source| {
                NativeTextureError::Tex {
                    path: compiled_path.to_string(),
                    source,
                }
            })?;
            // No codec transform here: `raw` stays in the same pre-decode representation a real
            // texture sample would be in (every texel of a genuinely constant, single solid-colour
            // block decodes the same, so this one representative pixel speaks for the whole 4x4
            // block) -- the caller/shader applies `codec` itself, the same way it would for a real
            // texture (see this enum variant's own doc comment).
            let px = [
                decoded.rgba[0],
                decoded.rgba[1],
                decoded.rgba[2],
                decoded.rgba[3],
            ];
            let result = Loaded::Constant { raw: px, codec };
            self.by_constant.insert(key, result);
            return Ok(result);
        }

        let format = NativeFormat::from_vtex(header.format).ok_or_else(|| {
            NativeTextureError::UnsupportedFormat {
                path: compiled_path.to_string(),
                format: header.format,
            }
        })?;

        let base_level = s2tex::pick_mip_for_budget(&header, max_side);
        let key = (compiled_path.to_string(), base_level, color_space);
        if let Some(&cached) = self.by_path_level.get(&key) {
            return Ok(cached);
        }

        let last_level = u32::from(header.num_mip_levels) - 1;
        let mut blob = Vec::new();
        let mut levels = Vec::new();
        // Levels increase from `base_level` (the largest mip within budget) to `last_level` (the
        // smallest, always 4x4 or less) -- on-disk mip order is smallest-first, but writing this
        // file in increasing level order lays it out largest-to-smallest (change item 1: "по
        // возрастанию номера уровня = от большого к малому").
        for level in base_level..=last_level {
            let raw = s2tex::decode_raw_mip_bytes(&bytes, level).map_err(|source| {
                NativeTextureError::Tex {
                    path: compiled_path.to_string(),
                    source,
                }
            })?;
            let offset = blob.len() as u64;
            let length = raw.bytes.len() as u64;
            blob.extend_from_slice(&raw.bytes);
            levels.push(LevelInfo {
                width: raw.width,
                height: raw.height,
                offset,
                length,
            });
        }

        let hash: [u8; 32] = Sha256::digest(&blob).into();
        let content_key = (hash, color_space);
        let result = if let Some(&idx) = self.by_content_hash.get(&content_key) {
            // Exact same bytes, already registered under this same colour space too.
            self.dedup_hits += 1;
            self.dedup_bytes_saved += blob.len() as u64;
            Loaded::Texture(idx)
        } else if let Some(&existing_idx) = self.by_hash_any_color_space.get(&hash) {
            // Same bytes, but only seen under a *different* colour space so far (review fix item
            // 3: e.g. de_train's mod2x `g_tColor` and some other material's linear read of the
            // exact same vtex_c) -- a new metadata-only entry, reusing the already-written file
            // (`file_name`/`sha12`/`levels`) instead of writing the identical blob a second time.
            let existing = &self.entries[existing_idx as usize];
            let idx = self.entries.len() as u32;
            self.entries.push(TextureEntry {
                path: compiled_path.to_string(),
                format,
                color_space,
                codec,
                file_name: existing.file_name.clone(),
                sha12: existing.sha12.clone(),
                levels: existing.levels.clone(),
                byte_length: existing.byte_length,
            });
            self.dedup_hits += 1;
            self.dedup_bytes_saved += blob.len() as u64;
            self.by_content_hash.insert(content_key, idx);
            Loaded::Texture(idx)
        } else {
            let digest_hex = sha12(&hash);
            let file_name = format!("render_tex/{digest_hex}.bin");
            let byte_length = blob.len() as u64;
            let idx = self.entries.len() as u32;
            self.entries.push(TextureEntry {
                path: compiled_path.to_string(),
                format,
                color_space,
                codec,
                file_name: file_name.clone(),
                sha12: digest_hex,
                levels,
                byte_length,
            });
            self.files.push((file_name, blob));
            self.by_content_hash.insert(content_key, idx);
            self.by_hash_any_color_space.insert(hash, idx);
            Loaded::Texture(idx)
        };
        self.by_path_level.insert(key, result);
        Ok(result)
    }

    /// `render.json`'s `textures[]` array (change item 2).
    pub fn textures_json(&self) -> Vec<serde_json::Value> {
        self.entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "path": e.path,
                    "format": e.format.as_str(),
                    "colorSpace": e.color_space.as_str(),
                    "codec": e.codec.as_str(),
                    "file": e.file_name,
                    "sha12": e.sha12,
                    "byteLength": e.byte_length,
                    "levels": e.levels.iter().map(|l| serde_json::json!({
                        "width": l.width,
                        "height": l.height,
                        "offset": l.offset,
                        "length": l.length,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    /// Every `render_tex/<sha12>.bin` file this export needs to write, consuming the catalog.
    pub fn into_files(self) -> Vec<(String, Vec<u8>)> {
        self.files
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.byte_length).sum()
    }

    pub fn dedup_hits(&self) -> u32 {
        self.dedup_hits
    }

    pub fn dedup_bytes_saved(&self) -> u64 {
        self.dedup_bytes_saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "s2render-native-texture-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// A trivial version-1 VPK tree with one inline-stored entry (`s2render::export`'s own test
    /// helper of the same name/shape, duplicated locally rather than shared across modules'
    /// `#[cfg(test)]`).
    fn write_test_vpk(path: &std::path::Path, entry: Option<(&str, &[u8])>) {
        let mut tree = Vec::new();
        if let Some((entry_path, data)) = entry {
            let (dir, file) = entry_path.rsplit_once('/').unwrap_or((" ", entry_path));
            let (name, ext) = file.rsplit_once('.').unwrap_or((file, " "));
            tree.extend_from_slice(ext.as_bytes());
            tree.push(0);
            tree.extend_from_slice(dir.as_bytes());
            tree.push(0);
            tree.extend_from_slice(name.as_bytes());
            tree.push(0);
            tree.extend_from_slice(&0u32.to_le_bytes()); // crc32 (unchecked by Vpk::read)
            tree.extend_from_slice(&0u16.to_le_bytes()); // preload_len
            tree.extend_from_slice(&0x7FFFu16.to_le_bytes()); // archive index: stored inline
            tree.extend_from_slice(&0u32.to_le_bytes()); // offset into dir data
            tree.extend_from_slice(&(data.len() as u32).to_le_bytes());
            tree.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entry terminator
            tree.push(0); // end of file-name loop
            tree.push(0); // end of dir loop
        }
        tree.push(0); // end of extension loop

        let mut out = Vec::new();
        out.extend_from_slice(&0x55AA_1234u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
        out.extend_from_slice(&tree);
        if let Some((_, data)) = entry {
            out.extend_from_slice(data);
        }
        std::fs::write(path, &out).expect("write synthetic vpk");
    }

    fn open_one_texture_sources(dir: &std::path::Path, path: &str, vtex_bytes: &[u8]) -> Sources {
        write_test_vpk(&dir.join("map.vpk"), Some((path, vtex_bytes)));
        write_test_vpk(&dir.join("pak01_dir.vpk"), None);
        Sources::open(&dir.join("map.vpk"), dir).expect("open synthetic sources")
    }

    /// A minimal single-mip-level DXT1 `vtex_c`: 8x8 (2 blocks x 2 blocks = 4 blocks, 8 bytes
    /// each = 32 bytes), no extra data, `bytes` becomes every block's content (repeated to fill).
    fn minimal_dxt1_vtex(fill: u8) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&1u16.to_le_bytes()); // version
        header.extend_from_slice(&0u16.to_le_bytes()); // flags
        header.extend_from_slice(&[0u8; 16]); // reflectivity
        header.extend_from_slice(&8u16.to_le_bytes()); // width
        header.extend_from_slice(&8u16.to_le_bytes()); // height
        header.extend_from_slice(&1u16.to_le_bytes()); // depth
        header.push(1); // format = Dxt1
        header.push(1); // num_mip_levels
        header.extend_from_slice(&0u32.to_le_bytes()); // picmip0res
        header.extend_from_slice(&0u32.to_le_bytes()); // extraDataOffset
        header.extend_from_slice(&0u32.to_le_bytes()); // extraDataCount

        // Resource container: file_size, header_version=12, (skip resource version u16), block
        // table at rel_offset 8 from its own field, one DATA block.
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes()); // file_size, patched below
        out.extend_from_slice(&12u16.to_le_bytes()); // header_version
        out.extend_from_slice(&0u16.to_le_bytes()); // resource version
        out.extend_from_slice(&8u32.to_le_bytes()); // block_offset
        out.extend_from_slice(&1u32.to_le_bytes()); // block_count
        out.extend_from_slice(b"DATA");
        out.extend_from_slice(&8u32.to_le_bytes()); // rel_offset
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&[fill; 32]); // mip payload, right after the DATA block's own span.
        let file_size = out.len() as u32;
        out[0..4].copy_from_slice(&file_size.to_le_bytes());
        out
    }

    /// review fix item 3: the same `vtex_c` read through two different colour-space roles (e.g.
    /// de_train's mod2x `g_tColor` vs. a linear read of the identical file) must produce two
    /// distinct `textures[]` entries, each with its own (correct) `colorSpace`, sharing the same
    /// `render_tex/<sha12>.bin` file/levels rather than one entry silently carrying the wrong
    /// colour space for whichever slot asked second.
    #[test]
    fn same_texture_two_color_spaces_dedups_the_file_not_the_color_space() {
        let dir = temp_dir("colorspace-dedup");
        let vtex = minimal_dxt1_vtex(0x42);
        let sources = open_one_texture_sources(&dir, "materials/dev/reflectivity.vtex_c", &vtex);
        let mut catalog = TextureCatalog::default();

        let srgb = catalog
            .load(
                &sources,
                "materials/dev/reflectivity.vtex_c",
                1024,
                ColorSpace::Srgb,
            )
            .expect("srgb load");
        let linear = catalog
            .load(
                &sources,
                "materials/dev/reflectivity.vtex_c",
                1024,
                ColorSpace::Linear,
            )
            .expect("linear load");

        let Loaded::Texture(srgb_idx) = srgb else {
            panic!("expected a real texture");
        };
        let Loaded::Texture(linear_idx) = linear else {
            panic!("expected a real texture");
        };
        assert_ne!(
            srgb_idx, linear_idx,
            "must be two distinct textures[] entries"
        );

        let textures = catalog.textures_json();
        assert_eq!(textures[srgb_idx as usize]["colorSpace"], "srgb");
        assert_eq!(textures[linear_idx as usize]["colorSpace"], "linear");
        // Same bytes -> same file, no second write of the blob.
        assert_eq!(
            textures[srgb_idx as usize]["file"],
            textures[linear_idx as usize]["file"]
        );
        assert_eq!(
            catalog.into_files().len(),
            1,
            "the blob is written only once"
        );
    }

    /// Loading the exact same (path, level, colour space) twice must return the same index, not
    /// register a duplicate entry -- the ordinary dedup path item 3 must not have broken.
    #[test]
    fn same_texture_same_color_space_is_not_duplicated() {
        let dir = temp_dir("colorspace-nodup");
        let vtex = minimal_dxt1_vtex(0x7f);
        let sources = open_one_texture_sources(&dir, "materials/dev/reflectivity.vtex_c", &vtex);
        let mut catalog = TextureCatalog::default();

        let a = catalog
            .load(
                &sources,
                "materials/dev/reflectivity.vtex_c",
                1024,
                ColorSpace::Linear,
            )
            .expect("first load");
        let b = catalog
            .load(
                &sources,
                "materials/dev/reflectivity.vtex_c",
                1024,
                ColorSpace::Linear,
            )
            .expect("second load");
        let (Loaded::Texture(a_idx), Loaded::Texture(b_idx)) = (a, b) else {
            panic!("expected real textures");
        };
        assert_eq!(a_idx, b_idx);
        assert_eq!(catalog.count(), 1);
    }

    #[test]
    fn sha12_is_twelve_lowercase_hex_chars() {
        let hash = [
            0xABu8, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let s = sha12(&hash);
        assert_eq!(s.len(), 12);
        assert_eq!(s, "abcdef012345");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn native_format_maps_the_four_used_formats_and_rejects_others() {
        assert_eq!(
            NativeFormat::from_vtex(s2tex::VTexFormat::Bc7),
            Some(NativeFormat::Bc7)
        );
        assert_eq!(
            NativeFormat::from_vtex(s2tex::VTexFormat::Dxt1),
            Some(NativeFormat::Bc1)
        );
        assert_eq!(
            NativeFormat::from_vtex(s2tex::VTexFormat::Ati1N),
            Some(NativeFormat::Bc4)
        );
        assert_eq!(
            NativeFormat::from_vtex(s2tex::VTexFormat::Rgba8888),
            Some(NativeFormat::Rgba8)
        );
        assert_eq!(NativeFormat::from_vtex(s2tex::VTexFormat::Bc6H), None);
        assert_eq!(NativeFormat::from_vtex(s2tex::VTexFormat::Dxt5), None);
    }
}
