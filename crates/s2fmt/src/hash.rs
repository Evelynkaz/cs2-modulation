//! MurmurHash2 string tokens as used by Source 2.
//!
//! `VRF/ThirdParty/MurmurHash2.cs` hashes a `ReadOnlySpan<char>`: for each 4-char window it
//! packs `data[i] | data[i+1]<<8 | data[i+2]<<16 | data[i+3]<<24` into a `u32`, which for ASCII
//! input (chars < 0x100) is exactly the little-endian byte packing this function does over
//! `&[u8]`, so `murmur2` matches VRF's algorithm for ASCII strings. `VRF/Utils/StringToken.cs`
//! layers ASCII-lowercasing (`StringToken.Get`) and a fixed seed (`MURMUR2SEED`) on top.

/// MurmurHash2 (`m = 0x5bd1e995`, `r = 24`), matching `MurmurHash2.HashCaseSensitive`
/// (`VRF/ThirdParty/MurmurHash2.cs:79-126`) byte-for-byte on ASCII input.
pub fn murmur2(data: &[u8], seed: u32) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;

    let len = data.len();
    if len == 0 {
        return 0;
    }

    let mut h = seed ^ (len as u32);
    let (chunks, rem) = data.as_chunks::<4>();
    for chunk in chunks {
        let mut k = u32::from_le_bytes(*chunk);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);

        h = h.wrapping_mul(M);
        h ^= k;
    }

    match rem.len() {
        3 => {
            h ^= (rem[0] as u32) | ((rem[1] as u32) << 8) | ((rem[2] as u32) << 16);
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= (rem[0] as u32) | ((rem[1] as u32) << 8);
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= rem[0] as u32;
            h = h.wrapping_mul(M);
        }
        _ => {}
    }

    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

/// `StringToken.MURMUR2SEED` (`VRF/Utils/StringToken.cs:31`): "it's pi!".
pub const STRING_TOKEN_SEED: u32 = 0x3141_5926;

/// `StringToken.Get` (`VRF/Utils/StringToken.cs:43`): ASCII-lowercases `s`, then hashes with
/// [`STRING_TOKEN_SEED`]. Non-ASCII bytes are passed through unchanged, matching
/// `MurmurHash2.Hash`'s per-char `c is >= 'A' and <= 'Z' ? (char)(c | 0x20) : c` (only touches
/// ASCII uppercase).
pub fn string_token(s: &str) -> u32 {
    murmur2(s.to_ascii_lowercase().as_bytes(), STRING_TOKEN_SEED)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Values below are from an independent reference implementation of the documented
    // algorithm (not this module), transliterated straight from
    // `VRF/ThirdParty/MurmurHash2.cs:79-126` / `VRF/Utils/StringToken.cs:31-43`.
    // "classname"/"origin"/"angles"/"targetname" are also present, lowercase, in
    // `VRF/Resource/ResourceTypes/EntityLumpKnownKeys.cs` (StringToken's known-key table),
    // confirming these are exactly the tokens VRF computes for those literal keys.

    #[test]
    fn murmur2_empty_is_zero() {
        assert_eq!(murmur2(b"", 0), 0);
        assert_eq!(murmur2(b"", 0x3141_5926), 0);
    }

    #[test]
    fn murmur2_known_vectors_seed_zero() {
        assert_eq!(murmur2(b"a", 0), 0x92685f5e);
        assert_eq!(murmur2(b"ab", 0), 0x1aa14063);
        assert_eq!(murmur2(b"abc", 0), 0x13577c9b);
        assert_eq!(murmur2(b"abcd", 0), 0x26873021);
        assert_eq!(murmur2(b"hello", 0), 0xe56129cb);
    }

    #[test]
    fn string_token_known_keys() {
        assert_eq!(string_token("classname"), 0xc61b1c62);
        assert_eq!(string_token("origin"), 0xe4200216);
        assert_eq!(string_token("angles"), 0xba98dacf);
        assert_eq!(string_token("targetname"), 0x4137af6b);
    }

    #[test]
    fn string_token_matches_real_de_mirage_collision_attribute_tags() {
        // Ground truth: de_mirage world_physics.vmdl_c PHYS m_collisionAttributes carry both
        // the tag string and its precomputed numeric hash (`docs/FORMATS.md` section 6.5); these
        // pairs were read directly from a `cs2mod res ... --kv3` dump of that file (build
        // 2000908), not derived from this implementation.
        assert_eq!(string_token("Default"), 1977497166);
        assert_eq!(string_token("ConditionallySolid"), 4258586587);
        assert_eq!(string_token("conditionallysolid"), 4258586587);
        assert_eq!(string_token("passbullets"), 3421025643);
        assert_eq!(string_token("npcclip"), 286764971);
        assert_eq!(string_token("playerclip"), 3903581251);
        assert_eq!(string_token("player"), 2233579705);
        assert_eq!(string_token("ladder"), 3802511415);
        assert_eq!(string_token("csgo_grenadeclip"), 2516355760);
    }

    #[test]
    fn string_token_is_case_insensitive() {
        assert_eq!(
            string_token("MyUppercaseKey"),
            string_token("myuppercasekey")
        );
        assert_eq!(string_token("CLASSNAME"), string_token("classname"));
    }
}
