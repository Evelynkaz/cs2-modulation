//! Decodes typed vertex attribute arrays (POSITION, NORMAL, TEXCOORD, COLOR) out of a
//! [`Buffer`]'s raw bytes, per `VBIB.cs:419-1090`'s `:VertexAttributeFormat` table and packed-
//! normal decoders.

use crate::buffer::{Buffer, InputLayoutField};
use crate::error::MeshError;
use crate::format::DxgiFormat;

fn vertex_slice<'a>(
    buffer: &'a Buffer,
    field: &InputLayoutField,
    vertex: usize,
    len: usize,
    path: &str,
) -> Result<&'a [u8], MeshError> {
    let stride = buffer.element_size as usize;
    let start = vertex
        .checked_mul(stride)
        .and_then(|s| s.checked_add(field.offset as usize));
    let bytes = start.and_then(|start| buffer.data.get(start..start.checked_add(len)?));
    bytes.ok_or_else(|| MeshError::AttributeRange {
        path: path.to_string(),
        vertex,
        offset: field.offset as usize,
        len,
        stride,
    })
}

/// IEEE 754 binary16 -> `f32`.
fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15);
    let exponent = u32::from((bits >> 10) & 0x1F);
    let mantissa = u32::from(bits & 0x3FF);

    let magnitude = if exponent == 0 {
        (mantissa as f32) * 2f32.powi(-24) // subnormal: mantissa / 1024 * 2^-14
    } else if exponent == 0x1F {
        if mantissa == 0 {
            f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        (1.0 + (mantissa as f32) / 1024.0) * 2f32.powi(exponent as i32 - 15)
    };

    if sign == 1 { -magnitude } else { magnitude }
}

#[derive(Debug, Clone, Copy)]
enum Component {
    F32,
    Half,
    Unorm16,
    Snorm16,
    Unorm8,
}

/// `(component kind, component count)` for the float-representable formats TEXCOORD/COLOR/
/// POSITION use (`VBIB.cs:419-458`'s table).
fn component_kind(format: DxgiFormat) -> Option<(Component, usize)> {
    match format {
        DxgiFormat::R32Float => Some((Component::F32, 1)),
        DxgiFormat::R32G32Float => Some((Component::F32, 2)),
        DxgiFormat::R32G32B32Float => Some((Component::F32, 3)),
        DxgiFormat::R32G32B32A32Float => Some((Component::F32, 4)),
        DxgiFormat::R16G16Float => Some((Component::Half, 2)),
        DxgiFormat::R16G16B16A16Float => Some((Component::Half, 4)),
        DxgiFormat::R16G16Unorm => Some((Component::Unorm16, 2)),
        DxgiFormat::R16G16Snorm => Some((Component::Snorm16, 2)),
        DxgiFormat::R8G8B8A8Unorm => Some((Component::Unorm8, 4)),
        _ => None,
    }
}

/// A decoded float attribute: `components` values per vertex (1..=4), `values.len() ==
/// element_count * components`.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatAttribute {
    pub components: usize,
    pub values: Vec<f32>,
}

impl FloatAttribute {
    /// The `components` values belonging to vertex `i`.
    pub fn get(&self, i: usize) -> &[f32] {
        &self.values[i * self.components..(i + 1) * self.components]
    }
}

fn decode_float_attribute(
    buffer: &Buffer,
    field: &InputLayoutField,
    semantic: &str,
) -> Result<FloatAttribute, MeshError> {
    // The caller already checked `field.format` is one it allows for `semantic`, so this is
    // always `Some` here (`component_kind` covers every format that check permits).
    let (kind, components) = component_kind(field.format).expect("format already validated");
    let component_size = match kind {
        Component::F32 => 4,
        Component::Half | Component::Unorm16 | Component::Snorm16 => 2,
        Component::Unorm8 => 1,
    };
    let elem_len = component_size * components;

    let count = buffer.element_count as usize;
    let mut values = Vec::with_capacity(count * components);
    for i in 0..count {
        let bytes = vertex_slice(buffer, field, i, elem_len, semantic)?;
        for c in 0..components {
            let value = match kind {
                Component::F32 => f32::from_le_bytes(bytes[c * 4..c * 4 + 4].try_into().unwrap()),
                Component::Half => half_to_f32(u16::from_le_bytes(
                    bytes[c * 2..c * 2 + 2].try_into().unwrap(),
                )),
                Component::Unorm16 => {
                    f32::from(u16::from_le_bytes(
                        bytes[c * 2..c * 2 + 2].try_into().unwrap(),
                    )) / 65535.0
                }
                Component::Snorm16 => {
                    f32::from(i16::from_le_bytes(
                        bytes[c * 2..c * 2 + 2].try_into().unwrap(),
                    )) / 32767.0
                }
                Component::Unorm8 => f32::from(bytes[c]) / 255.0,
            };
            values.push(value);
        }
    }
    Ok(FloatAttribute { components, values })
}

/// POSITION: `R32G32B32_FLOAT` only (`VBIB.cs:605-615 GetVector3AttributeArray`).
pub fn decode_positions(
    buffer: &Buffer,
    field: &InputLayoutField,
) -> Result<Vec<[f32; 3]>, MeshError> {
    if field.format != DxgiFormat::R32G32B32Float {
        return Err(MeshError::UnsupportedFormat {
            path: "POSITION".to_string(),
            semantic: "POSITION".to_string(),
            format: field.format,
        });
    }
    let attr = decode_float_attribute(buffer, field, "POSITION")?;
    Ok(attr
        .values
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| [c[0], c[1], c[2]])
        .collect())
}

/// TEXCOORD: float32/half/unorm16/snorm16 for two components, plus the other component counts
/// the format table lists (`VBIB.cs:419-458`).
pub fn decode_texcoord(
    buffer: &Buffer,
    field: &InputLayoutField,
) -> Result<FloatAttribute, MeshError> {
    match field.format {
        DxgiFormat::R32Float
        | DxgiFormat::R32G32Float
        | DxgiFormat::R32G32B32Float
        | DxgiFormat::R32G32B32A32Float
        | DxgiFormat::R16G16Float
        | DxgiFormat::R16G16Unorm
        | DxgiFormat::R16G16Snorm
        | DxgiFormat::R8G8B8A8Unorm
        | DxgiFormat::R16G16B16A16Float => decode_float_attribute(buffer, field, "TEXCOORD"),
        other => Err(MeshError::UnsupportedFormat {
            path: "TEXCOORD".to_string(),
            semantic: "TEXCOORD".to_string(),
            format: other,
        }),
    }
}

/// COLOR: `R8G8B8A8_UNORM` and `R32G32B32A32_FLOAT` (`VBIB.cs:439-441`).
pub fn decode_color(
    buffer: &Buffer,
    field: &InputLayoutField,
) -> Result<FloatAttribute, MeshError> {
    match field.format {
        DxgiFormat::R8G8B8A8Unorm | DxgiFormat::R32G32B32A32Float => {
            decode_float_attribute(buffer, field, "COLOR")
        }
        other => Err(MeshError::UnsupportedFormat {
            path: "COLOR".to_string(),
            semantic: "COLOR".to_string(),
            format: other,
        }),
    }
}

/// A decoded normal, with the packed formats' bitangent-sign tangent when present.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Normal {
    pub normal: [f32; 3],
    pub tangent: Option<[f32; 4]>,
}

/// Version 1 packed normal (`R8G8B8A8_UNORM`; `VBIB.cs:892-929 DecompressNormal`). `x`/`y` are
/// the raw byte values (0..255) as floats.
fn decompress_normal_v1(x: f32, y: f32) -> [f32; 3] {
    let x = x - 128.0;
    let y = y - 128.0;

    let z_sign_bit = if x < 0.0 { 1.0 } else { 0.0 };
    let t_sign_bit = if y < 0.0 { 1.0 } else { 0.0 };
    let z_sign = -((2.0 * z_sign_bit) - 1.0);
    let t_sign = -((2.0 * t_sign_bit) - 1.0);

    let x = (x * z_sign) - z_sign_bit; // 0..127
    let y = (y * t_sign) - t_sign_bit;
    let x = x - 64.0; // -64..63
    let y = y - 64.0;

    let x_sign_bit = if x < 0.0 { 1.0 } else { 0.0 };
    let y_sign_bit = if y < 0.0 { 1.0 } else { 0.0 };
    let x_sign = -((2.0 * x_sign_bit) - 1.0);
    let y_sign = -((2.0 * y_sign_bit) - 1.0);

    let x = ((x * x_sign) - x_sign_bit) / 63.0; // 0..1
    let y = ((y * y_sign) - y_sign_bit) / 63.0;
    let z = 1.0 - x - y;

    let oolen = 1.0 / (x * x + y * y + z * z).sqrt();
    [x * oolen * x_sign, y * oolen * y_sign, z * oolen * z_sign]
}

/// Version 1 packed tangent (`VBIB.cs:931-937 DecompressTangent`): the same formula as the
/// normal, plus a bitangent sign derived from the raw (pre-offset) `y` byte.
fn decompress_tangent_v1(x: f32, y: f32) -> [f32; 4] {
    let normal = decompress_normal_v1(x, y);
    let t_sign = if y < 128.0 { -1.0 } else { 1.0 };
    [normal[0], normal[1], normal[2], t_sign]
}

/// Version 2 packed normal+tangent (`R32_UINT`; `VBIB.cs:939-1002 DecompressNormalTangents2`).
/// Operation order/grouping is kept exactly as the reference (its own comment at `:978` warns
/// that rearranging it changes the result under compressed-data float precision).
fn decompress_normal_tangent_v2(packed_frame: u32) -> ([f32; 3], [f32; 4]) {
    let sign_bit = packed_frame & 1;
    let t_bits = ((packed_frame >> 1) & 0x7FF) as f32;
    let x_bits = ((packed_frame >> 12) & 0x3FF) as f32;
    let y_bits = ((packed_frame >> 22) & 0x3FF) as f32;

    // Unpack from 0..1 to -1..1.
    let packed_x = (x_bits / 1023.0) * 2.0 - 1.0;
    let packed_y = (y_bits / 1023.0) * 2.0 - 1.0;

    // Z is never given a sign; negative values come from |x|+|y| exceeding 1.0.
    let derived_z = 1.0 - packed_x.abs() - packed_y.abs();
    let mut unpacked = [packed_x, packed_y, derived_z];

    let negative_z_compensation = (-derived_z).clamp(0.0, 1.0);
    let x_positive = if unpacked[0] >= 0.0 { 1.0 } else { 0.0 };
    let y_positive = if unpacked[1] >= 0.0 { 1.0 } else { 0.0 };

    unpacked[0] +=
        negative_z_compensation * (1.0 - x_positive) + -negative_z_compensation * x_positive;
    unpacked[1] +=
        negative_z_compensation * (1.0 - y_positive) + -negative_z_compensation * y_positive;

    let len =
        (unpacked[0] * unpacked[0] + unpacked[1] * unpacked[1] + unpacked[2] * unpacked[2]).sqrt();
    let normal = [unpacked[0] / len, unpacked[1] / len, unpacked[2] / len];

    let tangent_sign = if normal[2] >= 0.0 { 1.0 } else { -1.0 };
    let rcp_tangent_z = 1.0 / (tangent_sign + normal[2]);

    let unaligned_tangent = [
        -tangent_sign * (normal[0] * normal[0]) * rcp_tangent_z + 1.0,
        -tangent_sign * ((normal[0] * normal[1]) * rcp_tangent_z),
        -tangent_sign * normal[0],
    ];

    let angle = t_bits / 2047.0 * std::f32::consts::TAU;
    let (sin_a, cos_a) = angle.sin_cos();
    // cross(normal, unaligned_tangent).
    let cross = [
        normal[1] * unaligned_tangent[2] - normal[2] * unaligned_tangent[1],
        normal[2] * unaligned_tangent[0] - normal[0] * unaligned_tangent[2],
        normal[0] * unaligned_tangent[1] - normal[1] * unaligned_tangent[0],
    ];
    let tangent = [
        unaligned_tangent[0] * cos_a + cross[0] * sin_a,
        unaligned_tangent[1] * cos_a + cross[1] * sin_a,
        unaligned_tangent[2] * cos_a + cross[2] * sin_a,
    ];

    (
        normal,
        [
            tangent[0],
            tangent[1],
            tangent[2],
            if sign_bit == 0 { -1.0 } else { 1.0 },
        ],
    )
}

/// NORMAL: raw `R32G32B32_FLOAT`, packed v1 `R8G8B8A8_UNORM`, or packed v2 `R32_UINT`
/// (`VBIB.cs:679-712 GetNormalTangentArray`).
pub fn decode_normals(buffer: &Buffer, field: &InputLayoutField) -> Result<Vec<Normal>, MeshError> {
    let count = buffer.element_count as usize;
    let mut out = Vec::with_capacity(count);
    match field.format {
        DxgiFormat::R32G32B32Float => {
            for i in 0..count {
                let bytes = vertex_slice(buffer, field, i, 12, "NORMAL")?;
                let normal = [
                    f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                    f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    f32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                ];
                out.push(Normal {
                    normal,
                    tangent: None,
                });
            }
        }
        DxgiFormat::R8G8B8A8Unorm => {
            for i in 0..count {
                let bytes = vertex_slice(buffer, field, i, 4, "NORMAL")?;
                let (x, y) = (f32::from(bytes[0]), f32::from(bytes[1]));
                let (tx, ty) = (f32::from(bytes[2]), f32::from(bytes[3]));
                out.push(Normal {
                    normal: decompress_normal_v1(x, y),
                    tangent: Some(decompress_tangent_v1(tx, ty)),
                });
            }
        }
        DxgiFormat::R32Uint => {
            for i in 0..count {
                let bytes = vertex_slice(buffer, field, i, 4, "NORMAL")?;
                let packed = u32::from_le_bytes(bytes.try_into().unwrap());
                let (normal, tangent) = decompress_normal_tangent_v2(packed);
                out.push(Normal {
                    normal,
                    tangent: Some(tangent),
                });
            }
        }
        other => {
            return Err(MeshError::UnsupportedFormat {
                path: "NORMAL".to_string(),
                semantic: "NORMAL".to_string(),
                format: other,
            });
        }
    }
    Ok(out)
}
