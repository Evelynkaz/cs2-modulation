//! The subset of `DXGI_FORMAT` (`Resource/Enums/DXGIFormat.cs`) that CS2 render meshes actually
//! use for POSITION/NORMAL/TEXCOORD/COLOR streams (`VBIB.cs:419-458`'s `:VertexAttributeFormat`
//! table and `:892-1090`'s packed-normal formats). Everything else decodes to [`DxgiFormat::Other`]
//! rather than erroring here -- a semantic that doesn't recognise the format for its own
//! attribute is what actually errors (`crate::error::MeshError::UnsupportedFormat`).

/// A vertex/index buffer element format, as stored in `RenderInputLayoutField_t::m_Format`
/// (the raw `DXGI_FORMAT` integer; `DXGIFormat.cs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DxgiFormat {
    R32Float,
    R32G32Float,
    R32G32B32Float,
    R32G32B32A32Float,
    R16G16Float,
    R16G16Unorm,
    R16G16Snorm,
    R16G16B16A16Float,
    R8G8B8A8Unorm,
    R8G8B8A8Uint,
    R32Uint,
    /// Any other DXGI_FORMAT value, kept for reporting (the raw code).
    Other(u32),
}

impl DxgiFormat {
    /// Decodes a raw `DXGI_FORMAT` value (`DXGIFormat.cs`'s numbering).
    pub fn from_raw(value: u32) -> DxgiFormat {
        match value {
            2 => DxgiFormat::R32G32B32A32Float,
            6 => DxgiFormat::R32G32B32Float,
            10 => DxgiFormat::R16G16B16A16Float,
            16 => DxgiFormat::R32G32Float,
            28 => DxgiFormat::R8G8B8A8Unorm,
            30 => DxgiFormat::R8G8B8A8Uint,
            34 => DxgiFormat::R16G16Float,
            35 => DxgiFormat::R16G16Unorm,
            37 => DxgiFormat::R16G16Snorm,
            41 => DxgiFormat::R32Float,
            42 => DxgiFormat::R32Uint,
            other => DxgiFormat::Other(other),
        }
    }

    /// The raw `DXGI_FORMAT` value this decodes back to.
    pub fn raw(self) -> u32 {
        match self {
            DxgiFormat::R32G32B32A32Float => 2,
            DxgiFormat::R32G32B32Float => 6,
            DxgiFormat::R16G16B16A16Float => 10,
            DxgiFormat::R32G32Float => 16,
            DxgiFormat::R8G8B8A8Unorm => 28,
            DxgiFormat::R8G8B8A8Uint => 30,
            DxgiFormat::R16G16Float => 34,
            DxgiFormat::R16G16Unorm => 35,
            DxgiFormat::R16G16Snorm => 37,
            DxgiFormat::R32Float => 41,
            DxgiFormat::R32Uint => 42,
            DxgiFormat::Other(v) => v,
        }
    }

    /// `(element size in bytes, component count)`, for the formats [`GetFormatInfo`]
    /// (`VBIB.cs:1063-1090`) documents -- `None` for anything else, including formats this
    /// crate simply doesn't decode any attribute as (e.g. `BLENDINDICES`/skinning formats,
    /// out of scope per `s6f3_common.md`).
    pub fn element_size_and_components(self) -> Option<(usize, usize)> {
        match self {
            DxgiFormat::R8G8B8A8Unorm | DxgiFormat::R8G8B8A8Uint => Some((1, 4)),
            DxgiFormat::R16G16Float | DxgiFormat::R16G16Snorm | DxgiFormat::R16G16Unorm => {
                Some((2, 2))
            }
            DxgiFormat::R16G16B16A16Float => Some((2, 4)),
            DxgiFormat::R32Float => Some((4, 1)),
            DxgiFormat::R32Uint => Some((4, 1)),
            DxgiFormat::R32G32Float => Some((4, 2)),
            DxgiFormat::R32G32B32Float => Some((4, 3)),
            DxgiFormat::R32G32B32A32Float => Some((4, 4)),
            DxgiFormat::Other(_) => None,
        }
    }
}
