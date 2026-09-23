//! Extracts the texture compiler's special dependencies -- the strings that
//! drive `transform::resolve` -- from a resource's `RED2` (KV3, all CS2
//! content) or legacy `REDI` (binary) edit-info block
//! (Texture.cs:1295-1349, `ResourceEditInfo.cs`, `ResourceEditInfo2.cs`,
//! `ResourceEditInfoStructs/SpecialDependency.cs`).

use s2fmt::resource::{FourCC, Resource};

use crate::format::VTexFormat;
use crate::reader::Cursor;
use crate::transform::{self, TextureCodec};

const COMPILER_IDENTIFIER: &str = "CompileTexture";
const DEP_YCOCG: &str = "Texture Compiler Version Image YCoCg Conversion";
const DEP_NORMALIZE_NORMALS: &str = "Texture Compiler Version Image NormalizeNormals";
const DEP_HEMI_OCT_ISO: &str = "Texture Compiler Version Mip HemiOctIsoRoughness_RG_B";
const DEP_HEMI_OCT_ANISO: &str = "Texture Compiler Version Mip HemiOctAnisoRoughness";

/// Bounded so a hostile/corrupt legacy REDI block can't force an unbounded
/// loop; real files have at most a handful of special dependencies.
const MAX_SPECIAL_DEPENDENCIES: u32 = 4096;

/// Resolves the codec `format`/`is_cube` need from `resource`'s edit info.
/// Mirrors `Texture.cs`'s own leniency: no edit-info block (or one that
/// fails to parse) just means no special processing, the same as VRF's
/// `if (Resource.EditInfo == null) return codec;` -- this is advisory
/// metadata, not something worth failing the whole texture over.
pub fn resolve_codec(resource: &Resource, format: VTexFormat, is_cube: bool) -> TextureCodec {
    let deps = if let Some(block) = resource.block(FourCC::RED2) {
        special_dependencies_red2(resource, block).unwrap_or_default()
    } else if let Some(block) = resource.block(FourCC::REDI) {
        special_dependencies_redi(resource.block_bytes(block)).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut ycocg = false;
    let mut normalize_normals = false;
    let mut hemi_oct_rb = false;
    for (compiler, string) in &deps {
        if compiler != COMPILER_IDENTIFIER {
            continue;
        }
        match string.as_str() {
            DEP_YCOCG => ycocg = true,
            DEP_NORMALIZE_NORMALS => normalize_normals = true,
            DEP_HEMI_OCT_ISO | DEP_HEMI_OCT_ANISO => hemi_oct_rb = true,
            _ => {}
        }
    }

    transform::resolve(format, is_cube, ycocg, normalize_normals, hemi_oct_rb)
}

/// `(m_CompilerIdentifier, m_String)` for every entry in `m_SpecialDependencies`
/// (`ResourceEditInfo2.cs:83`, `SpecialDependency.cs:47-53`).
fn special_dependencies_red2(
    resource: &Resource,
    block: &s2fmt::resource::Block,
) -> Option<Vec<(String, String)>> {
    let doc = resource.kv3(block).ok()?;
    let deps = doc.root.get("m_SpecialDependencies")?.as_array()?;
    Some(
        deps.iter()
            .filter_map(|item| {
                let compiler = item.get("m_CompilerIdentifier")?.as_str()?.to_string();
                let string = item.get("m_String")?.as_str()?.to_string();
                Some((compiler, string))
            })
            .collect(),
    )
}

/// `(CompilerIdentifier, String)` for every entry in the legacy binary
/// REDI's `SpecialDependencies` sub-block (`ResourceEditInfo.cs:60-113`):
/// the fourth of a run of `(offset, count)` directory pairs (after
/// `InputDependencies`, `AdditionalInputDependencies`, `ArgumentDependencies`),
/// each 8 bytes and itself relative to its own position, followed by
/// `count` 16-byte `(String, CompilerIdentifier, Fingerprint, UserData)`
/// entries whose two strings use the offset-string convention
/// (`SpecialDependency.cs:36-42`).
fn special_dependencies_redi(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    const SPECIAL_DEPENDENCIES_SUBBLOCK: usize = 3;

    let dir_pos = SPECIAL_DEPENDENCIES_SUBBLOCK * 8;
    let mut dir = Cursor::new(bytes);
    dir.set_pos(dir_pos);
    let rel_offset = dir.u32().ok()?;
    let count = dir.u32().ok()?.min(MAX_SPECIAL_DEPENDENCIES);
    let mut item_pos = dir_pos.checked_add(rel_offset as usize)?;

    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut item = Cursor::new(bytes);
        item.set_pos(item_pos);
        let string = item.offset_string().ok()?;
        let compiler = item.offset_string().ok()?;
        // Fingerprint (u32) + UserData (u32) follow but aren't needed here.
        out.push((compiler, string));
        item_pos = item_pos.checked_add(16)?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal legacy REDI block with one `SpecialDependency`
    /// entry, laid out like `ResourceEditInfo.cs` expects.
    #[test]
    fn parses_one_redi_special_dependency() {
        let dir_pos = 3 * 8;
        let item_pos = dir_pos + 8; // right after this one (offset,count) pair
        let mut b = vec![0u8; dir_pos];
        b.extend_from_slice(&8u32.to_le_bytes()); // offset, relative to this field
        b.extend_from_slice(&1u32.to_le_bytes()); // count
        assert_eq!(b.len(), item_pos);

        // Two offset-string fields (String, CompilerIdentifier) + 8 bytes of
        // fingerprint/userdata, then the string bytes themselves.
        let string_field_pos = item_pos;
        let compiler_field_pos = item_pos + 4;
        let strings_at = item_pos + 16;
        b.extend_from_slice(&((strings_at as i64 - string_field_pos as i64) as i32).to_le_bytes());
        let compiler_str_at =
            strings_at + "Texture Compiler Version Image NormalizeNormals\0".len();
        b.extend_from_slice(
            &((compiler_str_at as i64 - compiler_field_pos as i64) as i32).to_le_bytes(),
        );
        b.extend_from_slice(&0u32.to_le_bytes()); // fingerprint
        b.extend_from_slice(&0u32.to_le_bytes()); // userdata
        b.extend_from_slice(b"Texture Compiler Version Image NormalizeNormals\0");
        b.extend_from_slice(b"CompileTexture\0");

        let deps = special_dependencies_redi(&b).unwrap();
        assert_eq!(
            deps,
            vec![(
                "CompileTexture".to_string(),
                "Texture Compiler Version Image NormalizeNormals".to_string()
            )]
        );
    }

    #[test]
    fn truncated_redi_does_not_panic() {
        for len in 0..40 {
            let bytes = vec![0u8; len];
            let _ = special_dependencies_redi(&bytes);
        }
    }
}
