//! `csgo_environment` per-layer colour-correction matrices (review fix item 1 of
//! `s6f3a7_env_materials.md`): a straight port of `RenderMaterial.cs:671-744
//! EvalCsgoEnvironmentColorMatrices` and `VfxEvalFunctions.cs:23-68`
//! (`MatrixColorCorrect2`/`MatrixColorTint2`), same row-vector convention as the reference
//! (translation lives in the last row, `v' = v @ M`, `@` = standard matrix product) and the same
//! left-to-right composition order. Verified against `scratch/review_f3a7/cc_table.py`'s
//! already-checked Python port -- this module's own tests reproduce its golden output for
//! `materials/cs_italy/trim/metal_trim_weathered_rust_blend_01.vmat` (both layers) to float32
//! precision.

/// Row-major (`m[row][col]`), matching the Python port's numpy arrays.
pub type Mat4 = [[f32; 4]; 4];

fn identity() -> Mat4 {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    m
}

/// `T(v)`: translation in the last row (row-vector convention).
fn translation(v: [f32; 3]) -> Mat4 {
    let mut m = identity();
    m[3][0] = v[0];
    m[3][1] = v[1];
    m[3][2] = v[2];
    m
}

/// `S(v)`.
fn scale3(v: [f32; 3]) -> Mat4 {
    let mut m = identity();
    m[0][0] = v[0];
    m[1][1] = v[1];
    m[2][2] = v[2];
    m
}

fn scale_uniform(s: f32) -> Mat4 {
    scale3([s, s, s])
}

/// Standard matrix product, `(a @ b)[i][j] = sum_k a[i][k] * b[k][j]`.
fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

fn mul3(a: &Mat4, b: &Mat4, c: &Mat4) -> Mat4 {
    mul(&mul(a, b), c)
}

fn transpose(m: &Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for (i, row) in m.iter().enumerate() {
        for (j, &v) in row.iter().enumerate() {
            out[j][i] = v;
        }
    }
    out
}

/// Row-vector times matrix, `(v @ m)[j] = sum_i v[i] * m[i][j]`.
fn vec_mul(v: [f32; 4], m: &Mat4) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    for (j, cell) in out.iter_mut().enumerate() {
        *cell = (0..4).map(|i| v[i] * m[i][j]).sum();
    }
    out
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn len3(a: [f32; 3]) -> f32 {
    dot3(a, a).sqrt()
}

fn norm3(a: [f32; 3]) -> [f32; 3] {
    let l = len3(a);
    if l > 0.0 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        a
    }
}

fn axis_angle(axis: [f32; 3], angle: f32) -> Mat4 {
    let (x, y, z) = (axis[0], axis[1], axis[2]);
    let (sa, ca) = (angle.sin(), angle.cos());
    let (xx, yy, zz, xy, xz, yz) = (x * x, y * y, z * z, x * y, x * z, y * z);
    let mut m = identity();
    m[0] = [
        xx + ca * (1.0 - xx),
        xy - ca * xy + sa * z,
        xz - ca * xz - sa * y,
        0.0,
    ];
    m[1] = [
        xy - ca * xy - sa * z,
        yy + ca * (1.0 - yy),
        yz - ca * yz + sa * x,
        0.0,
    ];
    m[2] = [
        xz - ca * xz + sa * y,
        yz - ca * yz - sa * x,
        zz + ca * (1.0 - zz),
        0.0,
    ];
    m
}

/// Rec. 709 luminance coefficients, normalized to unit length (`VfxEvalFunctions.cs:9`).
fn luma() -> [f32; 3] {
    norm3([0.2126, 0.7152, 0.0722])
}

/// The fixed rotation aligning the luminance axis with `+Z`, shared by both matrix builders.
fn luma_rotation(l: [f32; 3]) -> Mat4 {
    let c = cross3(l, [0.0, 0.0, 1.0]);
    let angle = len3(c).atan2(dot3(l, [0.0, 0.0, 1.0]));
    axis_angle(norm3(c), angle)
}

fn rgb_saturation(rgb: [f32; 3]) -> f32 {
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let mn = rgb[0].min(rgb[1]).min(rgb[2]);
    if mx == 0.0 { 0.0 } else { (mx - mn) / mx }
}

/// `VfxEvalFunctions.cs:23-38 MatrixColorCorrect2`. `csb` = (contrast, saturation, brightness);
/// `color_offset` = the contrast pivot (the layer's own colour texture `Reflectivity`, `(1,1,1)`
/// when the texture is missing). Returns the once-transposed matrix (`cc_table.py`'s own `cc2`
/// return value, `r.T`) -- callers compose it with [`matrix_color_tint2`] and transpose once more
/// (via [`to_shader_mat4`]) before shipping it to the shader.
fn matrix_color_correct2(csb: [f32; 3], color_offset: [f32; 3]) -> Mat4 {
    let l = luma();
    let rot = luma_rotation(l);
    let rot_t = transpose(&rot);
    let neg_off = [-color_offset[0], -color_offset[1], -color_offset[2]];
    let mut r = mul3(
        &translation(neg_off),
        &scale_uniform(csb[0]),
        &translation(color_offset),
    );
    r = mul(&r, &scale_uniform(csb[2]));
    r = mul(&r, &scale3(l));
    r = mul(&r, &rot);
    r = mul(&r, &scale3([csb[1], csb[1], 1.0]));
    r = mul(&r, &rot_t);
    r = mul(&r, &scale3([1.0 / l[0], 1.0 / l[1], 1.0 / l[2]]));
    transpose(&r)
}

/// `VfxEvalFunctions.cs:41-68 MatrixColorTint2`. `rgb` = tint colour (linear); `strength` is
/// always `1.0` at this shader's own call site (`RenderMaterial.cs:739`). Returns the
/// once-transposed matrix, same convention as [`matrix_color_correct2`].
fn matrix_color_tint2(rgb: [f32; 3], strength: f32) -> Mat4 {
    let l = luma();
    let rot = luma_rotation(l);
    let rot_t = transpose(&rot);
    let sat = rgb_saturation(rgb);
    let lum_rot = mul(&scale3(l), &rot);
    let gray = vec_mul([rgb[0], rgb[1], rgb[2], 1.0], &lum_rot);
    let desat = 1.0 - sat;
    let satf = 1.0 - sat * sat * strength;
    let mut r = mul(&lum_rot, &translation([-gray[0], -gray[1], 0.0]));
    r = mul(&r, &scale3([desat, desat, satf]));
    r = mul(&r, &translation([gray[0], gray[1], (1.0 - satf) * gray[2]]));
    r = mul(&r, &rot_t);
    r = mul(&r, &scale3([1.0 / l[0], 1.0 / l[1], 1.0 / l[2]]));
    transpose(&r)
}

/// The two shader-ready `mat4`s for one `csgo_environment` layer:
/// `color_adjust` = `g_mTextureColorAdjust{N}` (tinted, `RenderMaterial.cs:734-742`),
/// `adjust` = `g_mTextureAdjust{N}` (tint forced to `(1,1,1)`, `:734-737`). Each is flattened
/// column-major (`to_shader_mat4`) for direct use as `THREE.Matrix4.fromArray`/a GLSL `mat4`
/// uniform with `mat4 * vec4` (column-vector) application, matching
/// `scratch/review_f3a7/cc_table.py`'s own `F @ v` usage.
pub struct LayerColorMatrices {
    pub color_adjust: [f32; 16],
    pub adjust: [f32; 16],
}

/// Flattens column-major (`out[col*4+row] = m[row][col]`) -- equivalent to
/// `cc_table.py`'s `F.T.flatten()` (row-major flatten of the transpose) without an explicit
/// second transpose.
fn to_shader_mat4(m: &Mat4) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for (r, row) in m.iter().enumerate() {
        for (c, &v) in row.iter().enumerate() {
            out[c * 4 + r] = v;
        }
    }
    out
}

/// Builds both matrices for one layer (`RenderMaterial.cs:697-743`'s per-layer loop body).
pub fn layer_color_matrices(
    csb: [f32; 3],
    tint: [f32; 3],
    color_offset: [f32; 3],
) -> LayerColorMatrices {
    let cc = matrix_color_correct2(csb, color_offset);
    let tint_matrix = matrix_color_tint2(tint, 1.0);
    let identity_tint = matrix_color_tint2([1.0, 1.0, 1.0], 1.0);
    LayerColorMatrices {
        color_adjust: to_shader_mat4(&mul(&tint_matrix, &cc)),
        adjust: to_shader_mat4(&mul(&identity_tint, &cc)),
    }
}

#[cfg(test)]
// The golden literals below are copied verbatim from `cc_table.json`'s f64 JSON output (full
// double precision, for exact traceability back to that file) even though they're bound to `f32`
// -- clippy's excessive_precision lint would otherwise ask for them to be hand-truncated.
#[allow(clippy::excessive_precision)]
mod tests {
    use super::*;

    // Golden values from `scratch/review_f3a7/exp/viewer/cc_table.json`
    // ("materials/cs_italy/trim/metal_trim_weathered_rust_blend_01.vmat"), produced by
    // `scratch/review_f3a7/cc_table.py` (the reviewer's checked Python port). `csb`/`tint`/`refl`
    // are the material's own params; `colorAdjust`/`adjust` are this module's target output.
    fn assert_close(got: &[f32; 16], want: &[f32; 16], tol: f32) {
        for (i, (&g, &w)) in got.iter().zip(want.iter()).enumerate() {
            assert!(
                (g - w).abs() <= tol,
                "index {i}: got {g} want {w} (diff {})",
                (g - w).abs()
            );
        }
    }

    #[test]
    fn layer1_matches_the_golden_python_port() {
        let csb = [0.5, 1.0, 1.100000023841858];
        let tint = [1.0, 1.0, 1.0];
        let refl = [0.4649902880191803, 0.4539320468902588, 0.4410944879055023];
        let m = layer_color_matrices(csb, tint, refl);
        let want = [
            0.5500000119209292,
            -2.336267312605123e-17,
            -1.0346524817893202e-16,
            0.0,
            -1.578599449844597e-16,
            0.5500000119209291,
            -4.3962879263364845e-17,
            0.0,
            9.139399903525831e-19,
            -1.140726238742094e-18,
            0.5500000119209291,
            0.0,
            0.2557446639536654,
            0.249662631200934,
            0.24260197360628225,
            1.0,
        ]
        .map(|v| v as f32);
        assert_close(&m.color_adjust, &want, 1e-4);
        assert_close(&m.adjust, &want, 1e-4); // tint == (1,1,1) => colorAdjust == adjust.
    }

    #[test]
    fn layer2_matches_the_golden_python_port() {
        let csb = [0.75, 0.5, 1.25];
        let tint = [1.0, 1.0, 1.0];
        let refl = [
            0.17491620779037476,
            0.09960713982582092,
            0.07308617979288101,
        ];
        let m = layer_color_matrices(csb, tint, refl);
        let want = [
            0.5064543337317753,
            0.03770433373177489,
            0.03770433373177489,
            0.0,
            0.4266971695605643,
            0.8954471695605644,
            0.42669716956056447,
            0.0,
            0.0043484967076606795,
            0.0043484967076606856,
            0.47309849670766074,
            0.0,
            0.043802323706422994,
            0.03203528183696144,
            0.02789138183181458,
            1.0,
        ]
        .map(|v| v as f32);
        assert_close(&m.color_adjust, &want, 1e-4);
        assert_close(&m.adjust, &want, 1e-4);
    }

    /// Same golden source, `materials/de_inferno/brick/inferno_brick_03_paint_blend.vmat` layer
    /// 1: a non-identity tint, exercising `colorAdjust != adjust` (mode==1, real tint vs. tint
    /// forced to white).
    #[test]
    fn non_identity_tint_matches_the_golden_python_port() {
        let csb = [1.0, 1.2610000371932983, 1.0];
        let tint = [0.9803919792175293, 0.8901960253715515, 0.7294120192527771];
        let refl = [0.42652446031570435, 0.41747212409973145, 0.405342161655426];
        let m = layer_color_matrices(csb, tint, refl);
        let want_color_adjust = [
            0.9378852774898971,
            -0.00029924758250600506,
            -0.00029924758250619865,
            0.0,
            -0.003386562864669909,
            0.9347979622077331,
            -0.0033865628646698263,
            0.0,
            -0.00003451266733844648,
            -0.00003451266733845095,
            0.9381500124050642,
            0.0,
            0.08033210998539232,
            0.057241981378553325,
            0.01608133923328652,
            1.0,
        ]
        .map(|v| v as f32);
        let want_adjust = [
            1.24000626117977,
            -0.020993776013529204,
            -0.020993776013529523,
            0.0,
            -0.23758501786790984,
            1.023415019325389,
            -0.23758501786790953,
            0.0,
            -0.0024212433118599827,
            -0.0024212433118600027,
            1.2585787938814383,
            0.0,
            0.0,
            0.0,
            0.0,
            1.0,
        ]
        .map(|v| v as f32);
        assert_close(&m.color_adjust, &want_color_adjust, 2e-4);
        assert_close(&m.adjust, &want_adjust, 2e-4);
    }

    /// Default params (contrast=saturation=brightness=1, tint=white) must be the identity --
    /// a material that never sets any of these float/vector params gets no colour shift.
    #[test]
    fn defaults_are_identity() {
        let m = layer_color_matrices([1.0, 1.0, 1.0], [1.0, 1.0, 1.0], [0.3, 0.5, 0.7]);
        let mut want = [0.0f32; 16];
        want[0] = 1.0;
        want[5] = 1.0;
        want[10] = 1.0;
        want[15] = 1.0;
        assert_close(&m.color_adjust, &want, 1e-4);
        assert_close(&m.adjust, &want, 1e-4);
    }
}
