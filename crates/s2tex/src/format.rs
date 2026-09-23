//! Pixel formats a vtex_c mip can be stored in (`Resource/Enums/VTexFormat.cs`),
//! and the per-format facts the rest of the crate needs (Texture.cs:258-299,
//! 648-679).

use crate::error::TexError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)] // names mirror VTexFormat's own spelling
pub enum VTexFormat {
    Unknown,
    Dxt1,
    Dxt5,
    I8,
    Rgba8888,
    R16,
    Rg1616,
    Rgba16161616,
    R16F,
    Rg1616F,
    Rgba16161616F,
    R32F,
    Rg3232F,
    Rgb323232F,
    Rgba32323232F,
    JpegRgba8888,
    PngRgba8888,
    JpegDxt5,
    PngDxt5,
    Bc6H,
    Bc7,
    Ati2N,
    Ia88,
    Etc2,
    Etc2Eac,
    R11Eac,
    Rg11Eac,
    Ati1N,
    Bgra8888,
    WebpRgba8888,
    WebpDxt5,
}

impl VTexFormat {
    /// Parses the header's raw format byte (`VTexFormat` enum values 0-30).
    pub fn from_raw(raw: u8) -> Result<VTexFormat, TexError> {
        use VTexFormat::*;
        Ok(match raw {
            0 => Unknown,
            1 => Dxt1,
            2 => Dxt5,
            3 => I8,
            4 => Rgba8888,
            5 => R16,
            6 => Rg1616,
            7 => Rgba16161616,
            8 => R16F,
            9 => Rg1616F,
            10 => Rgba16161616F,
            11 => R32F,
            12 => Rg3232F,
            13 => Rgb323232F,
            14 => Rgba32323232F,
            15 => JpegRgba8888,
            16 => PngRgba8888,
            17 => JpegDxt5,
            18 => PngDxt5,
            19 => Bc6H,
            20 => Bc7,
            21 => Ati2N,
            22 => Ia88,
            23 => Etc2,
            24 => Etc2Eac,
            25 => R11Eac,
            26 => Rg11Eac,
            27 => Ati1N,
            28 => Bgra8888,
            29 => WebpRgba8888,
            30 => WebpDxt5,
            other => return Err(TexError::UnknownFormat { raw: other }),
        })
    }

    /// Bytes per pixel (non block-compressed formats) or per 4x4 block
    /// (block-compressed ones); `_ => 1` covers I8 and the raw-image
    /// formats, which never reach the block-size math (Texture.cs:258-284).
    pub fn block_size(self) -> u32 {
        use VTexFormat::*;
        match self {
            Dxt1 => 8,
            Dxt5 => 16,
            Rgba8888 => 4,
            R16 => 2,
            Rg1616 => 4,
            Rgba16161616 => 8,
            R16F => 2,
            Rg1616F => 4,
            Rgba16161616F => 8,
            R32F => 4,
            Rg3232F => 8,
            Rgb323232F => 12,
            Rgba32323232F => 16,
            Bc6H => 16,
            Bc7 => 16,
            Ia88 => 2,
            Etc2 => 8,
            Etc2Eac => 16,
            R11Eac => 8,
            Rg11Eac => 16,
            Bgra8888 => 4,
            Ati1N => 8,
            Ati2N => 16,
            _ => 1,
        }
    }

    /// True if this format stores 4x4 blocks rather than individual pixels
    /// (Texture.cs:289-299).
    pub fn is_block_compressed(self) -> bool {
        use VTexFormat::*;
        matches!(
            self,
            Dxt1 | Dxt5 | Bc6H | Bc7 | Etc2 | Etc2Eac | R11Eac | Rg11Eac | Ati1N | Ati2N
        )
    }

    /// True for formats whose values are outside the LDR `[0,1]` range
    /// (Texture.cs:648-659). We reject all of these at decode time (see
    /// `decode::decode_mip`); kept here only to mirror the reference switch.
    pub fn is_high_dynamic_range(self) -> bool {
        use VTexFormat::*;
        matches!(
            self,
            R16 | Rg1616
                | Rgba16161616
                | R16F
                | Rg1616F
                | Rgba16161616F
                | R32F
                | Rg3232F
                | Rgb323232F
                | Rgba32323232F
                | Bc6H
        )
    }

    /// True for formats that carry a single non-color channel of data (a
    /// mask, packed roughness/AO) rather than visually-graded color: JPEG's
    /// chroma subsampling below quality 90 lives mostly in the channels
    /// these formats decode into, so `encode::encode` treats them as
    /// lossless-only (`s6f3a2_tex.md` change item 6).
    pub fn is_mask(self) -> bool {
        matches!(self, VTexFormat::Ati1N | VTexFormat::Ati2N | VTexFormat::I8)
    }

    pub fn is_raw_jpeg(self) -> bool {
        matches!(self, VTexFormat::JpegDxt5 | VTexFormat::JpegRgba8888)
    }

    pub fn is_raw_png(self) -> bool {
        matches!(self, VTexFormat::PngDxt5 | VTexFormat::PngRgba8888)
    }

    pub fn is_raw_webp(self) -> bool {
        matches!(self, VTexFormat::WebpDxt5 | VTexFormat::WebpRgba8888)
    }

    /// True for formats whose mip data is a single embedded whole-image
    /// blob (JPEG/PNG/WebP) rather than the usual mip chain
    /// (Texture.cs:661-679).
    pub fn is_raw_image(self) -> bool {
        self.is_raw_jpeg() || self.is_raw_png() || self.is_raw_webp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_known_values() {
        assert_eq!(VTexFormat::from_raw(1).unwrap(), VTexFormat::Dxt1);
        assert_eq!(VTexFormat::from_raw(20).unwrap(), VTexFormat::Bc7);
        assert_eq!(VTexFormat::from_raw(30).unwrap(), VTexFormat::WebpDxt5);
    }

    #[test]
    fn rejects_unknown_value() {
        assert!(matches!(
            VTexFormat::from_raw(31),
            Err(TexError::UnknownFormat { raw: 31 })
        ));
    }

    #[test]
    fn mask_formats_are_ati1n_ati2n_i8() {
        for f in [VTexFormat::Ati1N, VTexFormat::Ati2N, VTexFormat::I8] {
            assert!(f.is_mask(), "{f:?} should be a mask format");
        }
        for f in [VTexFormat::Dxt1, VTexFormat::Rgba8888, VTexFormat::Bc7] {
            assert!(!f.is_mask(), "{f:?} should not be a mask format");
        }
    }

    #[test]
    fn block_compressed_set_matches_reference() {
        for f in [
            VTexFormat::Dxt1,
            VTexFormat::Dxt5,
            VTexFormat::Bc6H,
            VTexFormat::Bc7,
            VTexFormat::Etc2,
            VTexFormat::Etc2Eac,
            VTexFormat::R11Eac,
            VTexFormat::Rg11Eac,
            VTexFormat::Ati1N,
            VTexFormat::Ati2N,
        ] {
            assert!(f.is_block_compressed(), "{f:?} should be block-compressed");
        }
        for f in [
            VTexFormat::Rgba8888,
            VTexFormat::Bgra8888,
            VTexFormat::I8,
            VTexFormat::Ia88,
        ] {
            assert!(
                !f.is_block_compressed(),
                "{f:?} should not be block-compressed"
            );
        }
    }
}
