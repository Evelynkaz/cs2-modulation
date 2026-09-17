//! `vrman_c` resource manifest layout (non-NTRO): `docs/FORMATS.md`
//! section 2.5; `VRF/Resource/ResourceTypes/ResourceManifest.cs`,
//! `Utils/StreamHelpers.cs` `ReadOffsetString`.

use crate::resource::{ResourceError, trunc};
use crate::util::Reader;

/// Parses a `vrman_c` `DATA` block: `i32 version == 8` (a leading `0, 0`
/// pair means an empty manifest), `i32 count`, then `count` entries of
/// `{i32 relOffset, i32 n}` where `relOffset` is relative to the position of
/// the `relOffset` field itself and points at `n` offset-strings — each an
/// `i32` relative offset (again relative to its own field's position) to a
/// NUL-terminated UTF-8 string.
pub fn parse_resource_manifest(data: &[u8]) -> Result<Vec<Vec<String>>, ResourceError> {
    let mut r = Reader::new(data);
    let version = r.i32().map_err(|e| trunc("version", e))?;
    if version == 0 {
        let second = r.i32().map_err(|e| trunc("empty-manifest marker", e))?;
        if second == 0 {
            return Ok(Vec::new());
        }
        return Err(ResourceError::ManifestBadVersion { version });
    }
    if version != 8 {
        return Err(ResourceError::ManifestBadVersion { version });
    }

    let count = nn(r.i32().map_err(|e| trunc("count", e))?, "manifest count")?;
    let mut lists = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let field_pos = r.pos();
        let rel_offset = r.i32().map_err(|e| trunc("entry relOffset", e))?;
        let n = nn(
            r.i32().map_err(|e| trunc("entry n", e))?,
            "manifest entry n",
        )?;
        let list_pos = offset_from(field_pos, rel_offset);

        let mut list_r = Reader::new(data);
        list_r.set_pos(list_pos);
        let mut strings = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            strings.push(read_offset_string(&mut list_r, data)?);
        }
        lists.push(strings);
    }
    Ok(lists)
}

/// Reads one `{i32 relOffset}` field at the reader's current position and
/// follows it to a NUL-terminated string, relative to the field's own
/// position (the same convention as the resource block table's
/// `RelOffset`, `docs/FORMATS.md` section 2.2).
fn read_offset_string(r: &mut Reader<'_>, data: &[u8]) -> Result<String, ResourceError> {
    let field_pos = r.pos();
    let rel_offset = r.i32().map_err(|e| trunc("offset-string relOffset", e))?;
    let str_pos = offset_from(field_pos, rel_offset);
    let mut sr = Reader::new(data);
    sr.set_pos(str_pos);
    Ok(sr
        .cstr()
        .map_err(|e| trunc("offset-string", e))?
        .to_string())
}

/// Adds a signed relative offset to a field position without panicking on
/// overflow or a negative result (an out-of-range position is caught by the
/// bounds-checked `Reader` reads that follow, not here).
fn offset_from(field_pos: usize, rel_offset: i32) -> usize {
    (field_pos as i64).wrapping_add(rel_offset as i64) as usize
}

fn nn(v: i32, context: &'static str) -> Result<usize, ResourceError> {
    usize::try_from(v).map_err(|_| ResourceError::Invalid {
        detail: format!("negative {context}: {v}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_i32(buf: &mut [u8], pos: usize, value: i32) {
        buf[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn empty_manifest() {
        let data = [0i32.to_le_bytes(), 0i32.to_le_bytes()].concat();
        assert_eq!(
            parse_resource_manifest(&data).unwrap(),
            Vec::<Vec<String>>::new()
        );
    }

    #[test]
    fn bad_version_errors() {
        let data = [7i32.to_le_bytes()].concat();
        assert!(parse_resource_manifest(&data).is_err());
    }

    #[test]
    fn single_entry_two_strings() {
        // Header: version(8), count(1)
        let mut buf = Vec::new();
        buf.extend_from_slice(&8i32.to_le_bytes());
        buf.extend_from_slice(&1i32.to_le_bytes());
        // Entry: relOffset, n=2 -- placeholder relOffset patched below.
        let entry_field_pos = buf.len();
        buf.extend_from_slice(&0i32.to_le_bytes()); // relOffset placeholder
        buf.extend_from_slice(&2i32.to_le_bytes()); // n

        let list_pos = buf.len();
        patch_i32(
            &mut buf,
            entry_field_pos,
            (list_pos - entry_field_pos) as i32,
        );

        // The list itself is 2 contiguous relOffset i32 fields; the actual
        // string bytes live after the whole list, not inline with it.
        let field1_pos = buf.len();
        buf.extend_from_slice(&0i32.to_le_bytes());
        let field2_pos = buf.len();
        buf.extend_from_slice(&0i32.to_le_bytes());

        let str1_pos = buf.len();
        buf.extend_from_slice(b"hello\0");
        let str2_pos = buf.len();
        buf.extend_from_slice(b"world\0");

        patch_i32(&mut buf, field1_pos, (str1_pos - field1_pos) as i32);
        patch_i32(&mut buf, field2_pos, (str2_pos - field2_pos) as i32);

        let lists = parse_resource_manifest(&buf).unwrap();
        assert_eq!(lists, vec![vec!["hello".to_string(), "world".to_string()]]);
    }

    #[test]
    fn truncated_does_not_panic() {
        let data = [8i32.to_le_bytes(), 1i32.to_le_bytes()].concat();
        assert!(parse_resource_manifest(&data).is_err());
    }
}
