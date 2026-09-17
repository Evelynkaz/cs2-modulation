//! KV3 format/encoding GUIDs (FORMATS.md 3.2): raw 16 bytes on disk, compared byte-by-byte;
//! the text form uses the mixed-endian rule shared with .NET's `Guid.ToString()`.

use std::fmt;

/// A raw 16-byte KV3 format or encoding identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// Parses the standard mixed-endian text form, with or without surrounding `{}`.
    /// Expects lowercase or uppercase hex, e.g. `e21c7f3c-8a33-41c5-9977-a76d3a32aa0d`.
    pub fn from_text(s: &str) -> Option<Guid> {
        let s = s.strip_prefix('{').unwrap_or(s);
        let s = s.strip_suffix('}').unwrap_or(s);
        // Reject any non-ASCII byte up front: byte-indexed slicing below would otherwise be able
        // to land inside a multi-byte UTF-8 character (e.g. a hex group padded with 'é' can have
        // the right byte length without being 8/4/4/4/12 *characters*) and panic.
        if !s.is_ascii() {
            return None;
        }
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 5 {
            return None;
        }
        if [8, 4, 4, 4, 12] != parts.iter().map(|p| p.len()).collect::<Vec<_>>().as_slice() {
            return None;
        }

        let group1 = hex_bytes(parts[0])?;
        let group2 = hex_bytes(parts[1])?;
        let group3 = hex_bytes(parts[2])?;
        let group4 = hex_bytes(parts[3])?;
        let group5 = hex_bytes(parts[4])?;

        let mut raw = [0u8; 16];
        raw[0..4].copy_from_slice(&group1);
        raw[0..4].reverse();
        raw[4..6].copy_from_slice(&group2);
        raw[4..6].reverse();
        raw[6..8].copy_from_slice(&group3);
        raw[6..8].reverse();
        raw[8..10].copy_from_slice(&group4);
        raw[10..16].copy_from_slice(&group5);
        Some(Guid(raw))
    }
}

/// Decodes `s` (already verified ASCII-only by the caller) as pairs of hex digits. Rejects any
/// non-hexdigit byte explicitly, since `u8::from_str_radix` would otherwise also accept a
/// leading sign (e.g. `"+a"` parses as `10` under `from_str_radix(_, 16)`).
fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[3],
            b[2],
            b[1],
            b[0],
            b[5],
            b[4],
            b[7],
            b[6],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

/// KV3 v0 `binary_bc` encoding GUID.
pub const ENCODING_BINARY_BC: Guid = Guid([
    0x46, 0x1A, 0x79, 0x95, 0xBC, 0x95, 0x6C, 0x4F, 0xA7, 0x0B, 0x05, 0xBC, 0xA1, 0xB7, 0xDF, 0xD2,
]);

/// KV3 v0 `binary_lz4` encoding GUID.
pub const ENCODING_BINARY_LZ4: Guid = Guid([
    0x8A, 0x34, 0x47, 0x68, 0xA1, 0x63, 0x5C, 0x4F, 0xA1, 0x97, 0x53, 0x80, 0x6F, 0xD9, 0xB1, 0x19,
]);

/// KV3 v0 `binary` (uncompressed) encoding GUID.
pub const ENCODING_BINARY: Guid = Guid([
    0x00, 0x05, 0x86, 0x1B, 0xD8, 0xF7, 0xC1, 0x40, 0xAD, 0x82, 0x75, 0xA4, 0x82, 0x67, 0xE7, 0x14,
]);

/// KV3 `text` encoding GUID, used in the text header.
pub const ENCODING_TEXT: Guid = Guid([
    0x3C, 0x7F, 0x1C, 0xE2, 0x33, 0x8A, 0xC5, 0x41, 0x99, 0x77, 0xA7, 0x6D, 0x3A, 0x32, 0xAA, 0x0D,
]);

/// KV3 `generic` format GUID.
pub const FORMAT_GENERIC: Guid = Guid([
    0x7C, 0x16, 0x12, 0x74, 0xE9, 0x06, 0x98, 0x46, 0xAF, 0xF2, 0xE6, 0x3E, 0xB5, 0x90, 0x37, 0xE7,
]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_mixed_endian() {
        assert_eq!(
            ENCODING_TEXT.to_string(),
            "e21c7f3c-8a33-41c5-9977-a76d3a32aa0d"
        );
        assert_eq!(
            FORMAT_GENERIC.to_string(),
            "7412167c-06e9-4698-aff2-e63eb59037e7"
        );
    }

    #[test]
    fn roundtrips_through_text() {
        let text = ENCODING_TEXT.to_string();
        assert_eq!(Guid::from_text(&text), Some(ENCODING_TEXT));
        let braced = format!("{{{text}}}");
        assert_eq!(Guid::from_text(&braced), Some(ENCODING_TEXT));
    }

    #[test]
    fn from_text_rejects_garbage() {
        assert_eq!(Guid::from_text("not-a-guid"), None);
        assert_eq!(Guid::from_text(""), None);
    }

    #[test]
    fn from_text_multibyte_char_does_not_panic() {
        // A multi-byte UTF-8 character keeps the group's *byte* length at 8 without it being 8
        // ASCII hex digits; must return None, not panic slicing mid-character.
        assert_eq!(Guid::from_text("aéaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"), None);
    }

    #[test]
    fn from_text_rejects_signed_hex_groups() {
        // `u8::from_str_radix` accepts a leading '+'/'-' sign; `from_text` must not.
        assert_eq!(
            Guid::from_text("+a+a+a+a-+a+a-+a+a-+a+a-+a+a+a+a+a+a"),
            None
        );
    }
}
