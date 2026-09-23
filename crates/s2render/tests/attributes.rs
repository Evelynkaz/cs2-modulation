//! Packed normal formats (v1/v2) on known values, cross-checked against an independent
//! transcription of the reference formula, plus every TEXCOORD/COLOR format.

use s2render::{Buffer, DxgiFormat, InputLayoutField, MeshError};

fn field(semantic_name: &str, format: DxgiFormat, offset: u32) -> InputLayoutField {
    InputLayoutField {
        semantic_name: semantic_name.to_string(),
        semantic_index: 0,
        format,
        offset,
    }
}

fn buffer_of(
    element_count: u32,
    element_size: u32,
    f: InputLayoutField,
    data: Vec<u8>,
) -> (Buffer, InputLayoutField) {
    (
        Buffer {
            element_count,
            element_size,
            fields: vec![f.clone()],
            data,
            compression: s2render::Compression::default(),
        },
        f,
    )
}

// ---- NORMAL: an independent transcription of VBIB.cs's formulas, to cross-check the crate's
// decoder against (`VBIB.cs:892-1002`). ----

fn ref_decompress_normal_v1(x: f32, y: f32) -> [f32; 3] {
    let x = x - 128.0;
    let y = y - 128.0;
    let z_sign_bit = if x < 0.0 { 1.0 } else { 0.0 };
    let t_sign_bit = if y < 0.0 { 1.0 } else { 0.0 };
    let z_sign = -((2.0 * z_sign_bit) - 1.0);
    let t_sign = -((2.0 * t_sign_bit) - 1.0);
    let x = (x * z_sign) - z_sign_bit;
    let y = (y * t_sign) - t_sign_bit;
    let x = x - 64.0;
    let y = y - 64.0;
    let x_sign_bit = if x < 0.0 { 1.0 } else { 0.0 };
    let y_sign_bit = if y < 0.0 { 1.0 } else { 0.0 };
    let x_sign = -((2.0 * x_sign_bit) - 1.0);
    let y_sign = -((2.0 * y_sign_bit) - 1.0);
    let x = ((x * x_sign) - x_sign_bit) / 63.0;
    let y = ((y * y_sign) - y_sign_bit) / 63.0;
    let z = 1.0 - x - y;
    let oolen = 1.0 / (x * x + y * y + z * z).sqrt();
    [x * oolen * x_sign, y * oolen * y_sign, z * oolen * z_sign]
}

fn ref_decompress_tangent_v1(x: f32, y: f32) -> [f32; 4] {
    let n = ref_decompress_normal_v1(x, y);
    let t_sign = if y < 128.0 { -1.0 } else { 1.0 };
    [n[0], n[1], n[2], t_sign]
}

fn ref_decompress_normal_tangent_v2(packed: u32) -> ([f32; 3], [f32; 4]) {
    let sign_bit = packed & 1;
    let t_bits = ((packed >> 1) & 0x7FF) as f32;
    let x_bits = ((packed >> 12) & 0x3FF) as f32;
    let y_bits = ((packed >> 22) & 0x3FF) as f32;
    let px = (x_bits / 1023.0) * 2.0 - 1.0;
    let py = (y_bits / 1023.0) * 2.0 - 1.0;
    let dz = 1.0 - px.abs() - py.abs();
    let mut u = [px, py, dz];
    let neg_z = (-dz).clamp(0.0, 1.0);
    let xp = if u[0] >= 0.0 { 1.0 } else { 0.0 };
    let yp = if u[1] >= 0.0 { 1.0 } else { 0.0 };
    u[0] += neg_z * (1.0 - xp) + -neg_z * xp;
    u[1] += neg_z * (1.0 - yp) + -neg_z * yp;
    let len = (u[0] * u[0] + u[1] * u[1] + u[2] * u[2]).sqrt();
    let n = [u[0] / len, u[1] / len, u[2] / len];
    let tsign = if n[2] >= 0.0 { 1.0 } else { -1.0 };
    let rcp = 1.0 / (tsign + n[2]);
    let ut = [
        -tsign * (n[0] * n[0]) * rcp + 1.0,
        -tsign * ((n[0] * n[1]) * rcp),
        -tsign * n[0],
    ];
    let angle = t_bits / 2047.0 * std::f32::consts::TAU;
    let (s, c) = angle.sin_cos();
    let cross = [
        n[1] * ut[2] - n[2] * ut[1],
        n[2] * ut[0] - n[0] * ut[2],
        n[0] * ut[1] - n[1] * ut[0],
    ];
    let t = [
        ut[0] * c + cross[0] * s,
        ut[1] * c + cross[1] * s,
        ut[2] * c + cross[2] * s,
    ];
    (
        n,
        [t[0], t[1], t[2], if sign_bit == 0 { -1.0 } else { 1.0 }],
    )
}

fn assert_close(a: f32, b: f32, eps: f32) {
    assert!((a - b).abs() < eps, "{a} != {b} (within {eps})");
}

#[test]
fn packed_normal_v1_matches_known_value_and_reference_formula() {
    // Hand-derived: (128, 128) -> x=y=0 pre-offset, which works out to -1/sqrt(3) on every axis.
    let expected = -(1.0f32 / 3.0f32.sqrt());
    let n = ref_decompress_normal_v1(128.0, 128.0);
    assert_close(n[0], expected, 1e-5);
    assert_close(n[1], expected, 1e-5);
    assert_close(n[2], expected, 1e-5);

    for &(x, y, tx, ty) in &[
        (128.0, 128.0, 128.0, 200.0),
        (0.0, 0.0, 255.0, 0.0),
        (255.0, 255.0, 0.0, 255.0),
        (10.0, 240.0, 200.0, 10.0),
    ] {
        let data = vec![x as u8, y as u8, tx as u8, ty as u8];
        let (buffer, f) = buffer_of(1, 4, field("NORMAL", DxgiFormat::R8G8B8A8Unorm, 0), data);
        let decoded = s2render::attributes::decode_normals(&buffer, &f).unwrap();
        let expected_normal = ref_decompress_normal_v1(x, y);
        let expected_tangent = ref_decompress_tangent_v1(tx, ty);
        assert_close(decoded[0].normal[0], expected_normal[0], 1e-5);
        assert_close(decoded[0].normal[1], expected_normal[1], 1e-5);
        assert_close(decoded[0].normal[2], expected_normal[2], 1e-5);
        let tangent = decoded[0].tangent.expect("v1 always carries a tangent");
        assert_close(tangent[0], expected_tangent[0], 1e-5);
        assert_close(tangent[1], expected_tangent[1], 1e-5);
        assert_close(tangent[2], expected_tangent[2], 1e-5);
        assert_eq!(tangent[3], expected_tangent[3]);

        // Unit length, both independently and from the crate's own decode.
        let len = (decoded[0].normal[0].powi(2)
            + decoded[0].normal[1].powi(2)
            + decoded[0].normal[2].powi(2))
        .sqrt();
        assert_close(len, 1.0, 1e-4);
    }
}

#[test]
fn packed_normal_v2_matches_known_value_and_reference_formula() {
    // Hand-derived: packed_frame 0 -> normal straight down -Z, tangent +X, bitangent sign -1.
    let data = 0u32.to_le_bytes().to_vec();
    let (buffer, f) = buffer_of(1, 4, field("NORMAL", DxgiFormat::R32Uint, 0), data);
    let decoded = s2render::attributes::decode_normals(&buffer, &f).unwrap();
    assert_eq!(decoded[0].normal, [0.0, 0.0, -1.0]);
    assert_eq!(decoded[0].tangent, Some([1.0, 0.0, 0.0, -1.0]));

    for &packed in &[0u32, 1, 0x8000_0000, 0x1234_5678, 0xFFFF_FFFF, 0x0055_AA33] {
        let data = packed.to_le_bytes().to_vec();
        let (buffer, f) = buffer_of(1, 4, field("NORMAL", DxgiFormat::R32Uint, 0), data);
        let decoded = s2render::attributes::decode_normals(&buffer, &f).unwrap();
        let (expected_normal, expected_tangent) = ref_decompress_normal_tangent_v2(packed);
        for (got, want) in decoded[0].normal.iter().zip(expected_normal.iter()) {
            assert_close(*got, *want, 1e-4);
        }
        let tangent = decoded[0].tangent.expect("v2 always carries a tangent");
        for (got, want) in tangent.iter().zip(expected_tangent.iter()) {
            assert_close(*got, *want, 1e-4);
        }
        let len = (decoded[0].normal[0].powi(2)
            + decoded[0].normal[1].powi(2)
            + decoded[0].normal[2].powi(2))
        .sqrt();
        assert_close(len, 1.0, 1e-3);
    }
}

// Ground truth that isn't derived from our own (or a hand-transcribed copy of VBIB.cs's) formula:
// six real packed v2 (`R32_UINT`) NORMAL values read out of de_mirage itself, paired with the
// normal ValveResourceFormat's own Source2Viewer-CLI produced for the same vertex in its
// `de_mirage.glb` export (D:\porject\modulator-work\scratch\vrf_export\de_mirage\de_mirage.glb).
// Provenance: `maps/de_mirage/worldnodes/n0_lr0_agg_merge_{bomb_site_tarp,mall_trees_branches01}
// _0.vmdl_c`'s embedded mesh, vertex indices 0-3 and 0-1 respectively (a worldnode-merged
// aggregate mesh, whose vertex positions are already baked to world space, i.e. at the identity
// transform relative to the map's own root -- matched to VRF's export by nearest position, to
// ~1e-5 in glTF metres, since neither file carries stable vertex IDs to join on directly).
// VRF's glTF axes: (x, y, z) = (src_y, src_z, src_x), no scale (`s6f3a1_mesh.md` change item 4),
// so the Source-space normal below is the inverse swizzle `(gltf_z, gltf_x, gltf_y)` of what
// `de_mirage.glb`'s NORMAL accessor stores.
#[test]
fn packed_normal_v2_matches_real_de_mirage_vrf_export() {
    let cases: &[(u32, [f32; 3])] = &[
        // n0_lr0_agg_merge_bomb_site_tarp_0.vmdl_c, vertex 0
        (0x47A4_E75E, [0.248_153_42, -0.712_848_5, 0.655_947_1]),
        // n0_lr0_agg_merge_bomb_site_tarp_0.vmdl_c, vertex 1
        (0x195E_B694, [-0.048_946_977, -0.980_132_94, 0.192_206_32]),
        // n0_lr0_agg_merge_bomb_site_tarp_0.vmdl_c, vertex 2
        (0x28E1_672E, [0.059_772_193, -0.925_805_3, 0.373_244_34]),
        // n0_lr0_agg_merge_bomb_site_tarp_0.vmdl_c, vertex 3
        (0x3367_A856, [0.360_053_1, -0.900_867_64, 0.242_484_72]),
        // n0_lr0_agg_merge_mall_trees_branches01_0.vmdl_c, vertex 0
        (0x4732_A938, [0.799_333_4, -0.599_499_94, -0.040_811_09]),
        // n0_lr0_agg_merge_mall_trees_branches01_0.vmdl_c, vertex 1
        (0x4E74_F8E8, [0.869_943_6, -0.489_169_6, -0.062_535_82]),
    ];
    for &(packed, expected) in cases {
        let data = packed.to_le_bytes().to_vec();
        let (buffer, f) = buffer_of(1, 4, field("NORMAL", DxgiFormat::R32Uint, 0), data);
        let decoded = s2render::attributes::decode_normals(&buffer, &f).unwrap();
        for (got, want) in decoded[0].normal.iter().zip(expected.iter()) {
            assert_close(*got, *want, 1e-5);
        }
    }
}

#[test]
fn normal_raw_float_passes_through() {
    let mut data = Vec::new();
    data.extend_from_slice(&1.0f32.to_le_bytes());
    data.extend_from_slice(&0.0f32.to_le_bytes());
    data.extend_from_slice(&0.0f32.to_le_bytes());
    let (buffer, f) = buffer_of(1, 12, field("NORMAL", DxgiFormat::R32G32B32Float, 0), data);
    let decoded = s2render::attributes::decode_normals(&buffer, &f).unwrap();
    assert_eq!(decoded[0].normal, [1.0, 0.0, 0.0]);
    assert_eq!(decoded[0].tangent, None);
}

#[test]
fn normal_unsupported_format_errors_with_semantic_and_format() {
    let (buffer, f) = buffer_of(1, 4, field("NORMAL", DxgiFormat::R32Float, 0), vec![0; 4]);
    let err = s2render::attributes::decode_normals(&buffer, &f).unwrap_err();
    match err {
        MeshError::UnsupportedFormat {
            semantic, format, ..
        } => {
            assert_eq!(semantic, "NORMAL");
            assert_eq!(format, DxgiFormat::R32Float);
        }
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
}

// ---- TEXCOORD: every format the table lists. ----

fn half_bits(v: f32) -> u16 {
    // Only used to build test fixtures for exact half values (1.0, 0.5, -1.0, 2.0, 0.0), so a
    // small lookup is clearer than a general float->half encoder.
    if v == 1.0 {
        0x3C00
    } else if v == 0.5 {
        0x3800
    } else if v == -1.0 {
        0xBC00
    } else if v == 2.0 {
        0x4000
    } else if v == 0.0 {
        0x0000
    } else {
        panic!("no fixture for {v}")
    }
}

#[test]
fn texcoord_r32_float_scalar() {
    let (buffer, f) = buffer_of(
        1,
        4,
        field("TEXCOORD", DxgiFormat::R32Float, 0),
        0.25f32.to_le_bytes().to_vec(),
    );
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.components, 1);
    assert_eq!(attr.get(0), &[0.25]);
}

#[test]
fn texcoord_r32g32_float() {
    let mut data = 1.5f32.to_le_bytes().to_vec();
    data.extend_from_slice(&(-2.5f32).to_le_bytes());
    let (buffer, f) = buffer_of(1, 8, field("TEXCOORD", DxgiFormat::R32G32Float, 0), data);
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[1.5, -2.5]);
}

#[test]
fn texcoord_r16g16_float() {
    let mut data = half_bits(1.0).to_le_bytes().to_vec();
    data.extend_from_slice(&half_bits(-1.0).to_le_bytes());
    let (buffer, f) = buffer_of(1, 4, field("TEXCOORD", DxgiFormat::R16G16Float, 0), data);
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_close(attr.get(0)[0], 1.0, 1e-3);
    assert_close(attr.get(0)[1], -1.0, 1e-3);
}

#[test]
fn texcoord_r16g16_unorm() {
    let mut data = 0u16.to_le_bytes().to_vec();
    data.extend_from_slice(&65535u16.to_le_bytes());
    let (buffer, f) = buffer_of(1, 4, field("TEXCOORD", DxgiFormat::R16G16Unorm, 0), data);
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[0.0, 1.0]);
}

#[test]
fn texcoord_r16g16_snorm() {
    let mut data = 0i16.to_le_bytes().to_vec();
    data.extend_from_slice(&(-32767i16).to_le_bytes());
    let (buffer, f) = buffer_of(1, 4, field("TEXCOORD", DxgiFormat::R16G16Snorm, 0), data);
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[0.0, -1.0]);
}

#[test]
fn texcoord_r32g32b32_float() {
    let mut data = 1.0f32.to_le_bytes().to_vec();
    data.extend_from_slice(&2.0f32.to_le_bytes());
    data.extend_from_slice(&3.0f32.to_le_bytes());
    let (buffer, f) = buffer_of(
        1,
        12,
        field("TEXCOORD", DxgiFormat::R32G32B32Float, 0),
        data,
    );
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[1.0, 2.0, 3.0]);
}

#[test]
fn texcoord_r32g32b32a32_float() {
    let mut data = Vec::new();
    for v in [1.0f32, 2.0, 3.0, 4.0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let (buffer, f) = buffer_of(
        1,
        16,
        field("TEXCOORD", DxgiFormat::R32G32B32A32Float, 0),
        data,
    );
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn texcoord_r8g8b8a8_unorm() {
    let data = vec![0u8, 128, 255, 64];
    let (buffer, f) = buffer_of(1, 4, field("TEXCOORD", DxgiFormat::R8G8B8A8Unorm, 0), data);
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    let got = attr.get(0);
    assert_close(got[0], 0.0, 1e-6);
    assert_close(got[1], 128.0 / 255.0, 1e-6);
    assert_close(got[2], 1.0, 1e-6);
    assert_close(got[3], 64.0 / 255.0, 1e-6);
}

#[test]
fn texcoord_r16g16b16a16_float() {
    let mut data = Vec::new();
    for v in [1.0f32, 0.5, -1.0, 2.0] {
        data.extend_from_slice(&half_bits(v).to_le_bytes());
    }
    let (buffer, f) = buffer_of(
        1,
        8,
        field("TEXCOORD", DxgiFormat::R16G16B16A16Float, 0),
        data,
    );
    let attr = s2render::attributes::decode_texcoord(&buffer, &f).unwrap();
    let got = attr.get(0);
    assert_close(got[0], 1.0, 1e-3);
    assert_close(got[1], 0.5, 1e-3);
    assert_close(got[2], -1.0, 1e-3);
    assert_close(got[3], 2.0, 1e-3);
}

#[test]
fn texcoord_unsupported_format_errors() {
    let (buffer, f) = buffer_of(1, 4, field("TEXCOORD", DxgiFormat::R32Uint, 0), vec![0; 4]);
    assert!(s2render::attributes::decode_texcoord(&buffer, &f).is_err());
}

// ---- COLOR ----

#[test]
fn color_r8g8b8a8_unorm() {
    let data = vec![255u8, 0, 0, 255];
    let (buffer, f) = buffer_of(1, 4, field("COLOR", DxgiFormat::R8G8B8A8Unorm, 0), data);
    let attr = s2render::attributes::decode_color(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn color_r32g32b32a32_float() {
    let mut data = Vec::new();
    for v in [0.1f32, 0.2, 0.3, 1.0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let (buffer, f) = buffer_of(
        1,
        16,
        field("COLOR", DxgiFormat::R32G32B32A32Float, 0),
        data,
    );
    let attr = s2render::attributes::decode_color(&buffer, &f).unwrap();
    assert_eq!(attr.get(0), &[0.1, 0.2, 0.3, 1.0]);
}

#[test]
fn color_unsupported_format_errors() {
    let (buffer, f) = buffer_of(1, 4, field("COLOR", DxgiFormat::R16G16Float, 0), vec![0; 4]);
    assert!(s2render::attributes::decode_color(&buffer, &f).is_err());
}

// ---- POSITION ----

#[test]
fn position_requires_r32g32b32_float() {
    let (buffer, f) = buffer_of(1, 4, field("POSITION", DxgiFormat::R32Float, 0), vec![0; 4]);
    assert!(s2render::attributes::decode_positions(&buffer, &f).is_err());
}
