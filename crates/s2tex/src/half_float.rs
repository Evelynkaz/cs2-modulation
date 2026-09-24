//! Bit-exact IEEE 754 binary16 (`half`) -> `f32` conversion, used by the raw
//! HDR pixel formats (`R16F` and friends, `s6f3a2b_hdr.md` change item 2).
//!
//! Every step below is an exact floating point operation (dividing/
//! multiplying by a power of two, or adding two values whose sum is
//! representable without rounding), so the result is bit-exact for every
//! input -- verified exhaustively against the `half` crate for all 65536
//! `u16` bit patterns in this module's test (dev-dependency only, not
//! shipped).

/// `2^exp` as an `f32`, built by writing the biased exponent directly into
/// the bit pattern (mantissa `0`, i.e. a `1.0` significand) -- exact for any
/// `exp` in `f32`'s normal range, which every case below stays within.
fn exact_pow2(exp: i32) -> f32 {
    f32::from_bits(((exp + 127) as u32) << 23)
}

/// Converts the raw bits of an IEEE 754 binary16 value to `f32`. NaN is
/// widened to a canonical `f32::NAN`, not bit-preserved payload-for-payload
/// (texture data has no use for a NaN's payload bits).
pub(crate) fn f16_to_f32(bits: u16) -> f32 {
    let sign = bits & 0x8000 != 0;
    let exp = (bits >> 10) & 0x1f;
    let mant = u32::from(bits & 0x3ff);

    let magnitude = if exp == 0 {
        // Zero or subnormal: value = mant * 2^-24 (mant in 0..1024), exact
        // since mant is a small integer and 2^-24 is an exact power of two.
        (mant as f32) * exact_pow2(-24)
    } else if exp == 0x1f {
        if mant == 0 { f32::INFINITY } else { f32::NAN }
    } else {
        // Normal: value = (1 + mant/1024) * 2^(exp-15). mant/1024 is exact
        // (power-of-two divisor), 1.0 + that exact value in [0, 1023/1024]
        // is exact (well within f32's 23-bit mantissa), and multiplying by
        // an exact power of two changes only the exponent field.
        (1.0 + (mant as f32) / 1024.0) * exact_pow2(i32::from(exp) - 15)
    };

    if sign { -magnitude } else { magnitude }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x8000), -0.0);
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xBC00), -1.0);
        assert_eq!(f16_to_f32(0x4000), 2.0);
        // exp field 13 (=> 2^-2), mantissa 0x155 (=341): (1 + 341/1024) * 0.25.
        assert_eq!(f16_to_f32(0x3555), (1.0 + 341.0 / 1024.0) * 0.25);
        assert_eq!(f16_to_f32(0x7C00), f32::INFINITY);
        assert_eq!(f16_to_f32(0xFC00), f32::NEG_INFINITY);
        assert!(f16_to_f32(0x7E00).is_nan());
        // Smallest subnormal: 2^-24.
        assert_eq!(f16_to_f32(0x0001), exact_pow2(-24));
        // Largest subnormal: 1023 * 2^-24.
        assert_eq!(f16_to_f32(0x03FF), 1023.0 * exact_pow2(-24));
    }

    #[test]
    fn matches_the_half_crate_for_every_u16_bit_pattern() {
        for bits in 0..=u16::MAX {
            let ours = f16_to_f32(bits);
            let reference = half::f16::from_bits(bits).to_f32();
            if reference.is_nan() {
                assert!(ours.is_nan(), "bits={bits:#06x}: expected NaN, got {ours}");
            } else {
                assert_eq!(
                    ours.to_bits(),
                    reference.to_bits(),
                    "bits={bits:#06x}: ours={ours} reference={reference}"
                );
            }
        }
    }
}
