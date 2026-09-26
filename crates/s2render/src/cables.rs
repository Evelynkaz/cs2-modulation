//! `path_particle_rope_clientside` entities -> straight-Bezier tube geometry (`s6f3a4_lighting.md`
//! change item 9). Straight tubes only (no Verlet sag simulation): REPORT.md §7 measured a median
//! sag of 3.1 units (0.6% of a typical cable's length) on Mirage's 84 cables, small enough to skip
//! for v1; the Bezier curve built from each pathnode's own authored tangents is still evaluated in
//! full (that is not the sag -- it is what makes an *authored* curve, if any map ever has one,
//! render correctly; on every cable surveyed the tangents are collinear with their chord, so the
//! Bezier already comes out straight).
//!
//! Ground truth: `Resource/ResourceTypes/PathParticleRope.cs:64-82` (`pathnodes` layout),
//! `Renderer/Renderer/Utils/CableMeshBuilder.cs:58-260` (Bezier segmentation, parallel-transport
//! tube, UV); field names/shapes cross-checked against a real Mirage entity dump (REPORT.md §7).

use s2fmt::entities::{Entity, entity_transform};

use crate::entity::entity_num;

/// `materials/cable/csgo_placeholder_cable.vmat` (REPORT.md §7): the one material every
/// `path_particle_rope_clientside` on every surveyed map renders with.
pub const CABLE_MATERIAL_PATH: &str = "materials/cable/csgo_placeholder_cable.vmat";

const SIDES: usize = 4;

/// One `pathnodes[]` entry: position, in-tangent, out-tangent (`PathParticleRope.cs:64-82`), all
/// entity-local.
#[derive(Debug, Clone, Copy)]
struct PathNode {
    pos: [f32; 3],
    in_tangent: [f32; 3],
    out_tangent: [f32; 3],
}

/// A parsed `path_particle_rope_clientside` entity, ready to build tube geometry from.
pub struct ParsedCable {
    nodes: Vec<PathNode>,
    /// `radius` (world units; REPORT.md §7's typical value is 0.5).
    radius: f32,
    /// `particle_spacing`: target distance between tube rings.
    spacing: f32,
    /// `pathnoderadiusscales`' first/last entries (REPORT.md §7: "5 with radius scales
    /// [1,2]/[2,1]"), `1.0`/`1.0` when the field is absent -- lerped across the whole cable by
    /// arclength parameter, not per node (every surveyed cable has exactly 2 nodes, so this is
    /// exact for all of them).
    radius_scale_start: f32,
    radius_scale_end: f32,
    /// `color_tint` (0..255) as a 0..1 RGBA multiplier, white when absent.
    pub tint: [f32; 4],
    /// `entity_transform(origin, angles, scales)`: `pathnodes` are entity-local.
    transform: [[f32; 4]; 3],
}

/// Parses a KV3 "typed string" number list like `"[ 1.0, 2.0 ]"` (REPORT.md §9's parsing trap,
/// same family as `pathnodes` below but only one bracket level deep) into floats, ignoring
/// whitespace and a trailing comma.
fn parse_flat_number_list(s: &str) -> Vec<f32> {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|tok| tok.trim().parse::<f32>().ok())
        .collect()
}

/// Parses `pathnodes`' own string encoding: a top-level `[ [9 floats], [9 floats], ... ]` array of
/// arrays, spread across multiple lines with trailing commas (the real Mirage dump in REPORT.md
/// §7). Tracks bracket depth so only the *inner* (depth-2) groups are read as number lists; a group
/// that doesn't parse to exactly 9 floats is dropped (not fatal -- the caller reports it).
fn parse_path_nodes(s: &str) -> Vec<PathNode> {
    let mut nodes = Vec::new();
    let mut depth = 0i32;
    let mut group = String::new();
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                if depth == 2 {
                    group.clear();
                }
            }
            ']' => {
                if depth == 2 {
                    let nums: Vec<f32> = group
                        .split(',')
                        .filter_map(|tok| tok.trim().parse::<f32>().ok())
                        .collect();
                    if nums.len() == 9 {
                        nodes.push(PathNode {
                            pos: [nums[0], nums[1], nums[2]],
                            in_tangent: [nums[3], nums[4], nums[5]],
                            out_tangent: [nums[6], nums[7], nums[8]],
                        });
                    }
                }
                depth -= 1;
            }
            _ if depth == 2 => group.push(c),
            _ => {}
        }
    }
    nodes
}

/// Parses one `path_particle_rope_clientside` entity, `None` if `pathnodes` is missing/unreadable
/// or has fewer than 2 nodes (nothing to draw).
pub fn parse_cable(e: &Entity) -> Option<ParsedCable> {
    let nodes = parse_path_nodes(e.get_str("pathnodes")?);
    if nodes.len() < 2 {
        return None;
    }
    let radius = entity_num(e.get("radius")).unwrap_or(0.5) as f32;
    let spacing = entity_num(e.get("particle_spacing")).unwrap_or(60.0) as f32;
    let scales = parse_flat_number_list(e.get_str("pathnoderadiusscales").unwrap_or(""));
    let radius_scale_start = scales.first().copied().unwrap_or(1.0);
    let radius_scale_end = scales.last().copied().unwrap_or(1.0);
    let tint = e
        .get_vec3("color_tint")
        .map(|c| [c[0] / 255.0, c[1] / 255.0, c[2] / 255.0, 1.0])
        .unwrap_or([1.0, 1.0, 1.0, 1.0]);
    let transform = entity_transform(e.origin(), e.angles(), e.scales());
    Some(ParsedCable {
        nodes,
        radius,
        spacing,
        radius_scale_start,
        radius_scale_end,
        tint,
        transform,
    })
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale3(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn length3(a: [f32; 3]) -> f32 {
    dot3(a, a).sqrt()
}
fn normalize3(a: [f32; 3]) -> [f32; 3] {
    let l = length3(a);
    if l > 1e-8 {
        scale3(a, 1.0 / l)
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// `p0*(1-t)^3 + 3*p1*(1-t)^2*t + 3*p2*(1-t)*t^2 + p3*t^3`.
fn bezier_point(ctrl: [[f32; 3]; 4], t: f32) -> [f32; 3] {
    let mt = 1.0 - t;
    let mut out = [0.0f32; 3];
    let w = [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t];
    for i in 0..4 {
        out = add3(out, scale3(ctrl[i], w[i]));
    }
    out
}

/// Derivative of [`bezier_point`], for the tube's tangent frame.
fn bezier_tangent(ctrl: [[f32; 3]; 4], t: f32) -> [f32; 3] {
    let mt = 1.0 - t;
    let d0 = scale3(sub3(ctrl[1], ctrl[0]), 3.0 * mt * mt);
    let d1 = scale3(sub3(ctrl[2], ctrl[1]), 6.0 * mt * t);
    let d2 = scale3(sub3(ctrl[3], ctrl[2]), 3.0 * t * t);
    add3(add3(d0, d1), d2)
}

/// 16-chord arclength estimate (`CableMeshBuilder.cs:58-138`'s own method, REPORT.md §7).
fn estimate_arc_length(ctrl: [[f32; 3]; 4]) -> f32 {
    let mut len = 0.0f32;
    let mut prev = bezier_point(ctrl, 0.0);
    for i in 1..=16 {
        let p = bezier_point(ctrl, i as f32 / 16.0);
        len += length3(sub3(p, prev));
        prev = p;
    }
    len
}

/// Rodrigues' rotation formula: rotates `v` by `angle` radians around unit axis `k`.
fn rotate_axis_angle(v: [f32; 3], k: [f32; 3], angle: f32) -> [f32; 3] {
    let (s, c) = angle.sin_cos();
    let kxv = cross3(k, v);
    let kdv = dot3(k, v);
    add3(
        add3(scale3(v, c), scale3(kxv, s)),
        scale3(k, kdv * (1.0 - c)),
    )
}

/// Parallel-transports `old_normal` (defined at tangent `old_t`) to the frame at tangent `new_t`:
/// the minimal rotation that takes `old_t` to `new_t`, applied to `old_normal` too
/// (`CableMeshBuilder.cs:169-213`).
fn parallel_transport(old_t: [f32; 3], new_t: [f32; 3], old_normal: [f32; 3]) -> [f32; 3] {
    let axis = cross3(old_t, new_t);
    let axis_len = length3(axis);
    let cos_angle = dot3(old_t, new_t).clamp(-1.0, 1.0);
    if axis_len < 1e-6 {
        return if cos_angle < 0.0 {
            scale3(old_normal, -1.0)
        } else {
            old_normal
        };
    }
    let k = scale3(axis, 1.0 / axis_len);
    let angle = axis_len.atan2(cos_angle);
    normalize3(rotate_axis_angle(old_normal, k, angle))
}

/// One built cable: world-space tube geometry (never shared/instanced -- every cable is its own
/// primitive) plus the world position [`ParsedCable`]'s midpoint sample binds a probe volume to.
pub struct BuiltCable {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub midpoint: [f32; 3],
}

/// Builds a 4-sided tube along `cable`'s Bezier path(s) (`CableMeshBuilder.cs:169-260`,
/// REPORT.md §7): one Bezier segment per consecutive pathnode pair (`p0, p0+out0, p1+in1, p1`),
/// concatenated into one global sample list (segment joins share one ring, not duplicated) before
/// building the parallel-transport frame and emitting rings -- `U = i*(0.25/(N-1))`,
/// `V = j/sides`, quads `(a+j,a+j+1,b+j+1),(a+j,b+j+1,b+j)`. `None` only if the cable's own arc
/// length estimate rounds to a single degenerate sample (should not happen: `N` is always
/// `max(4, ...)`).
pub fn build_tube(cable: &ParsedCable) -> Option<BuiltCable> {
    let mut centers: Vec<[f32; 3]> = Vec::new();
    let mut tangents: Vec<[f32; 3]> = Vec::new();

    for seg in cable.nodes.windows(2) {
        let p0 = crate::probes::apply_affine(&cable.transform, seg[0].pos);
        let out0 = crate::probes::apply_linear(&cable.transform, seg[0].out_tangent);
        let p1 = crate::probes::apply_affine(&cable.transform, seg[1].pos);
        let in1 = crate::probes::apply_linear(&cable.transform, seg[1].in_tangent);
        let ctrl = [p0, add3(p0, out0), add3(p1, in1), p1];

        let arc_length = estimate_arc_length(ctrl);
        let n_samples = ((arc_length / cable.spacing.max(1.0)).ceil() as usize).max(4);
        let start = if centers.is_empty() { 0 } else { 1 }; // share the previous segment's last ring
        for i in start..n_samples {
            let t = i as f32 / (n_samples - 1) as f32;
            centers.push(bezier_point(ctrl, t));
            tangents.push(normalize3(bezier_tangent(ctrl, t)));
        }
    }

    let n = centers.len();
    if n < 2 {
        return None;
    }

    // Parallel-transport frame: first normal `normalize(cross(T,Z))`, or `cross(T,X)` when the
    // cable is (near-)vertical (`CableMeshBuilder.cs:169-213`).
    let z_axis = [0.0, 0.0, 1.0];
    let x_axis = [1.0, 0.0, 0.0];
    let mut first_normal = cross3(tangents[0], z_axis);
    if length3(first_normal) < 1e-4 {
        first_normal = cross3(tangents[0], x_axis);
    }
    let mut normals_frame = Vec::with_capacity(n);
    normals_frame.push(normalize3(first_normal));
    for i in 1..n {
        normals_frame.push(parallel_transport(
            tangents[i - 1],
            tangents[i],
            normals_frame[i - 1],
        ));
    }

    let ring_verts = SIDES + 1;
    let mut positions = Vec::with_capacity(n * ring_verts);
    let mut normals = Vec::with_capacity(n * ring_verts);
    let mut uvs = Vec::with_capacity(n * ring_verts);

    for i in 0..n {
        let t = i as f32 / (n - 1) as f32;
        let radius = cable.radius
            * (cable.radius_scale_start + (cable.radius_scale_end - cable.radius_scale_start) * t);
        let normal_axis = normals_frame[i];
        let binormal_axis = normalize3(cross3(tangents[i], normal_axis));
        let u = i as f32 * (0.25 / (n - 1) as f32);
        for j in 0..=SIDES {
            let angle = (j % SIDES) as f32 * std::f32::consts::TAU / SIDES as f32;
            let (s, c) = angle.sin_cos();
            let dir = add3(scale3(normal_axis, c), scale3(binormal_axis, s));
            positions.push(add3(centers[i], scale3(dir, radius)));
            normals.push(dir);
            uvs.push([u, j as f32 / SIDES as f32]);
        }
    }

    let mut indices = Vec::with_capacity((n - 1) * SIDES * 6);
    for i in 0..n - 1 {
        let a = (i * ring_verts) as u32;
        let b = ((i + 1) * ring_verts) as u32;
        for j in 0..SIDES as u32 {
            indices.extend_from_slice(&[a + j, a + j + 1, b + j + 1]);
            indices.extend_from_slice(&[a + j, b + j + 1, b + j]);
        }
    }

    let midpoint = centers[n / 2];
    Some(BuiltCable {
        positions,
        normals,
        uvs,
        indices,
        midpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2fmt::entities::EntityValue;

    fn cable_entity(pathnodes: &str) -> Entity {
        Entity {
            properties: vec![
                (
                    "classname".to_string(),
                    EntityValue::String("path_particle_rope_clientside".to_string()),
                ),
                (
                    "pathnodes".to_string(),
                    EntityValue::String(pathnodes.to_string()),
                ),
                ("radius".to_string(), EntityValue::Float(0.5)),
                ("particle_spacing".to_string(), EntityValue::Float(60.0)),
            ],
            connections: Vec::new(),
        }
    }

    /// The real Mirage `pathNodes` blob from REPORT.md §7: 2 nodes, straight (tangent = chord/3).
    const MIRAGE_WIRE19: &str = "[\n\t[\n\t\t0.0, 0.0, 0.0, 0.0,\n\t\t0.0, 0.0, -164.0, -50.66667,\n\t\t-16.0,\n\t],\n\t[\n\t\t-492.0, -152.0, -48.0, 164.0,\n\t\t50.66667, 16.0, 0.0, 0.0,\n\t\t0.0,\n\t],\n]";

    #[test]
    fn parses_the_real_mirage_pathnodes_blob() {
        let nodes = parse_path_nodes(MIRAGE_WIRE19);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].pos, [0.0, 0.0, 0.0]);
        assert_eq!(nodes[0].out_tangent, [-164.0, -50.66667, -16.0]);
        assert_eq!(nodes[1].pos, [-492.0, -152.0, -48.0]);
        assert_eq!(nodes[1].in_tangent, [164.0, 50.66667, 16.0]);
    }

    #[test]
    fn flat_number_list_parses_bracketed_csv() {
        assert_eq!(parse_flat_number_list("[ 1.0, 2.0 ]"), vec![1.0, 2.0]);
        assert_eq!(parse_flat_number_list(""), Vec::<f32>::new());
    }

    /// A straight authored cable (out-tangent/in-tangent collinear with the chord, exactly like
    /// every real Mirage cable in REPORT.md §7) must build a tube whose centerline is a straight
    /// line, not a curve -- the Bezier formula is evaluated in full, not special-cased, so this
    /// is the check that it actually degenerates correctly.
    #[test]
    fn straight_authored_tangents_produce_a_straight_centerline() {
        let cable = parse_cable(&cable_entity(MIRAGE_WIRE19)).expect("parses");
        let built = build_tube(&cable).expect("builds");
        let p0 = [0.0, 0.0, 0.0];
        let p1 = [-492.0, -152.0, -48.0];
        let dir = normalize3(sub3(p1, p0));
        // Every ring's first vertex direction from p0 must be parallel to `dir` (cross ~ 0).
        for i in 0..(built.positions.len() / (SIDES + 1)) {
            let center_approx = built.positions[i * (SIDES + 1)]; // not the true center, but on the tube
            let to_p1 = sub3(p1, p0);
            let t_est = dot3(sub3(center_approx, p0), to_p1) / dot3(to_p1, to_p1);
            let expected_center = add3(p0, scale3(to_p1, t_est.clamp(0.0, 1.0)));
            let radial = sub3(center_approx, expected_center);
            // radial offset must equal the tube radius (0.5), not grow -- i.e. the centerline
            // really is the straight chord.
            assert!(
                (length3(radial) - 0.5).abs() < 0.05,
                "ring {i}: radial len {}",
                length3(radial)
            );
            let _ = dir;
        }
    }

    #[test]
    fn fewer_than_two_nodes_is_rejected() {
        assert!(parse_cable(&cable_entity("[[0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]]")).is_none());
    }
}
