//! Triangle-level intersection primitives, ported bit-for-bit from the
//! reference C# solver (PolyForm NC, user accepted): Möller–Trumbore
//! ray/segment vs. triangle, the Akenine-Möller triangle vs. box SAT test,
//! and the swept-box-vs-triangle SAT sweep used for hull collision.

use crate::math::V3;

/// The shared Möller–Trumbore ray/segment-vs-triangle core. Returns the
/// parametric hit distance `t` along `direction`, or `None` when the ray
/// misses the triangle's plane or barycentric bounds. Callers apply their
/// own `t` acceptance window (see [`RayWindow`]) — ported from
/// `cs2-smoke-solver/src/Sim/MollerTrumbore.cs:21-46`.
pub fn moller_trumbore(origin: V3, direction: V3, a: V3, b: V3, c: V3) -> Option<f32> {
    const EPSILON: f32 = 1e-7;
    let edge1 = b - a;
    let edge2 = c - a;
    let h = direction.cross(edge2);
    let det = edge1.dot(h);
    if det.abs() < EPSILON {
        return None;
    }
    let inv_det = 1.0 / det;
    let s = origin - a;
    let u = inv_det * s.dot(h);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(edge1);
    let v = inv_det * direction.dot(q);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    Some(inv_det * edge2.dot(q))
}

/// The `t` acceptance window applied on top of [`moller_trumbore`]. The two
/// windows differ on purpose (`MollerTrumbore.cs:10-18`):
/// - `Physics`: `(1e-5, 1]` — a contact right at the segment end (`t == 1`)
///   is still a contact; only sub-`1e-5` self-grazes are rejected.
/// - `Sightline`: `(1e-4, 1 - 1e-4)` — both segment endpoints sit on
///   surfaces (eye/target probes), so both ends back off to avoid
///   self-hitting the probed surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RayWindow {
    Physics,
    Sightline,
}

impl RayWindow {
    pub fn accepts(self, t: f32) -> bool {
        match self {
            RayWindow::Physics => t > 1e-5 && t <= 1.0,
            RayWindow::Sightline => t > 1e-4 && t < 1.0 - 1e-4,
        }
    }
}

/// Triangle vs. axis-aligned box separating-axis test (Akenine-Möller),
/// ported from `cs2-smoke-solver/src/Sim/TriBoxOverlap.cs:10-91`, axis test
/// order and all, including the (correct, if unconventional-looking)
/// `AxisTestY` sign convention used there.
pub fn tri_box_overlap(center: V3, half: V3, a: V3, b: V3, c: V3) -> bool {
    let v0 = a - center;
    let v1 = b - center;
    let v2 = c - center;

    let e0 = v1 - v0;
    let e1 = v2 - v1;
    let e2 = v0 - v2;

    if !axis_test_x(e0, v0, v2, half)
        || !axis_test_y(e0, v0, v2, half)
        || !axis_test_z(e0, v1, v2, half)
    {
        return false;
    }
    if !axis_test_x(e1, v0, v2, half)
        || !axis_test_y(e1, v0, v2, half)
        || !axis_test_z(e1, v0, v1, half)
    {
        return false;
    }
    if !axis_test_x(e2, v0, v1, half)
        || !axis_test_y(e2, v0, v1, half)
        || !axis_test_z(e2, v1, v2, half)
    {
        return false;
    }

    if !overlaps_on_axis(v0.x, v1.x, v2.x, half.x)
        || !overlaps_on_axis(v0.y, v1.y, v2.y, half.y)
        || !overlaps_on_axis(v0.z, v1.z, v2.z, half.z)
    {
        return false;
    }

    let normal = e0.cross(e1);
    plane_box_overlap(normal, v0, half)
}

fn overlaps_on_axis(p0: f32, p1: f32, p2: f32, half: f32) -> bool {
    let min = p0.min(p1.min(p2));
    let max = p0.max(p1.max(p2));
    min <= half && max >= -half
}

fn plane_box_overlap(normal: V3, vert: V3, half: V3) -> bool {
    let vmin = V3::new(
        if normal.x > 0.0 {
            -half.x - vert.x
        } else {
            half.x - vert.x
        },
        if normal.y > 0.0 {
            -half.y - vert.y
        } else {
            half.y - vert.y
        },
        if normal.z > 0.0 {
            -half.z - vert.z
        } else {
            half.z - vert.z
        },
    );
    let vmax = V3::new(
        if normal.x > 0.0 {
            half.x - vert.x
        } else {
            -half.x - vert.x
        },
        if normal.y > 0.0 {
            half.y - vert.y
        } else {
            -half.y - vert.y
        },
        if normal.z > 0.0 {
            half.z - vert.z
        } else {
            -half.z - vert.z
        },
    );
    if normal.dot(vmin) > 0.0 {
        return false;
    }
    normal.dot(vmax) >= 0.0
}

fn axis_test_x(edge: V3, va: V3, vb: V3, half: V3) -> bool {
    let p0 = edge.z * va.y - edge.y * va.z;
    let p1 = edge.z * vb.y - edge.y * vb.z;
    let rad = edge.z.abs() * half.y + edge.y.abs() * half.z;
    p0.min(p1) <= rad && p0.max(p1) >= -rad
}

fn axis_test_y(edge: V3, va: V3, vb: V3, half: V3) -> bool {
    let p0 = edge.x * va.z - edge.z * va.x;
    let p1 = edge.x * vb.z - edge.z * vb.x;
    let rad = edge.z.abs() * half.x + edge.x.abs() * half.z;
    p0.min(p1) <= rad && p0.max(p1) >= -rad
}

fn axis_test_z(edge: V3, va: V3, vb: V3, half: V3) -> bool {
    let p0 = edge.y * va.x - edge.x * va.y;
    let p1 = edge.y * vb.x - edge.x * vb.y;
    let rad = edge.y.abs() * half.x + edge.x.abs() * half.y;
    p0.min(p1) <= rad && p0.max(p1) >= -rad
}

/// A swept-box-vs-triangle SAT contact: `t` is the entry time in `[0, 1]`
/// along the sweep, `normal` is oriented against the sweep direction, and
/// `face` is true iff the contact came through the triangle's face plane
/// (as opposed to an edge or box axis).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepContact {
    pub t: f32,
    pub normal: V3,
    pub face: bool,
}

/// Exact swept-AABB-vs-triangle query (separating-axis sweep: 3 box axes,
/// the triangle normal, and the 9 edge cross axes), ported from
/// `cs2-smoke-solver/src/Sim/TriangleCollider.Sat.cs:106-200`
/// (`SweptBoxTriangle`). Axis order, tie rule (entry axis replaced only when
/// `axLo > tEnter` strictly), the `tEnter <= 1e-6` start-overlap rule and its
/// `dot(normalize(dir), n0) < -0.01` incidence-angle gate, and all epsilons
/// (`1e-12` degenerate triangle, `1e-10` degenerate axis, `1e-9` static
/// axis) are reproduced exactly.
pub fn swept_box_triangle(
    origin: V3,
    direction: V3,
    half: V3,
    a: V3,
    b: V3,
    c: V3,
) -> Option<SweepContact> {
    let a = a - origin;
    let b = b - origin;
    let c = c - origin;

    let tri_normal = (b - a).cross(c - a);
    if tri_normal.length_squared() < 1e-12 {
        return None;
    }

    let mut t_enter = 0.0f32;
    let mut t_exit = 1.0f32;
    let mut enter_axis = tri_normal;

    let mut axis = |l: V3| -> bool {
        if l.length_squared() < 1e-10 {
            return true; // degenerate axis, no separation information
        }
        let s0 = a.dot(l);
        let s1 = b.dot(l);
        let s2 = c.dot(l);
        let m = s0.min(s1.min(s2));
        let mm = s0.max(s1.max(s2));
        let r = half.x * l.x.abs() + half.y * l.y.abs() + half.z * l.z.abs();
        let w = direction.dot(l);
        if w.abs() < 1e-9 {
            return m <= r && mm >= -r;
        }
        // overlap while m - t*w <= r and M - t*w >= -r
        let t_a = (m - r) / w;
        let t_b = (mm + r) / w;
        let (ax_lo, ax_hi) = if w > 0.0 { (t_a, t_b) } else { (t_b, t_a) };
        if ax_lo > t_enter {
            t_enter = ax_lo;
            enter_axis = l;
        }
        t_exit = t_exit.min(ax_hi);
        t_enter <= t_exit
    };

    if !axis(V3::new(1.0, 0.0, 0.0))
        || !axis(V3::new(0.0, 1.0, 0.0))
        || !axis(V3::new(0.0, 0.0, 1.0))
        || !axis(tri_normal)
    {
        return None;
    }
    for e in [b - a, c - b, a - c] {
        if !axis(V3::new(1.0, 0.0, 0.0).cross(e))
            || !axis(V3::new(0.0, 1.0, 0.0).cross(e))
            || !axis(V3::new(0.0, 0.0, 1.0).cross(e))
        {
            return None;
        }
    }
    if t_enter <= 1e-6 {
        // Overlapping at the start of the step (the post-bounce backoff can
        // leave the hull touching the surface it just left). Report a
        // contact only when moving deeper into the plane from the side the
        // sweep starts on; flipping the normal against the motion here
        // would ghost-collide the BACKFACE of the surface just bounced off.
        let mut n0 = tri_normal.normalize();
        // Triangle vertices are relative to the sweep start, so the start
        // center's signed distance to the plane is -dot(a, n).
        if a.dot(n0) > 0.0 {
            n0 = -n0;
        }
        // Require a real incidence angle, not a parallel graze.
        return if direction.normalize().dot(n0) < -0.01 {
            Some(SweepContact {
                t: 0.0,
                normal: n0,
                face: true,
            })
        } else {
            None
        };
    }

    // Contact normal from the axis that produced the entry time. Entering
    // through the face plane gives the face normal; entering laterally
    // gives the edge or box axis instead.
    let mut normal = enter_axis.normalize();
    if normal.dot(direction) > 0.0 {
        normal = -normal;
    }
    Some(SweepContact {
        t: t_enter,
        normal,
        face: enter_axis == tri_normal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mt_hits_center() {
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(1.0, 0.0, 0.0);
        let c = V3::new(0.0, 1.0, 0.0);
        let t = moller_trumbore(V3::new(0.25, 0.25, 1.0), V3::new(0.0, 0.0, -1.0), a, b, c);
        assert_eq!(t, Some(1.0));
    }

    #[test]
    fn mt_misses_outside_triangle() {
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(1.0, 0.0, 0.0);
        let c = V3::new(0.0, 1.0, 0.0);
        let t = moller_trumbore(V3::new(2.0, 2.0, 1.0), V3::new(0.0, 0.0, -1.0), a, b, c);
        assert_eq!(t, None);
    }

    #[test]
    fn mt_misses_parallel_ray() {
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(1.0, 0.0, 0.0);
        let c = V3::new(0.0, 1.0, 0.0);
        let t = moller_trumbore(V3::new(0.25, 0.25, 1.0), V3::new(1.0, 0.0, 0.0), a, b, c);
        assert_eq!(t, None);
    }

    #[test]
    fn mt_edge_case_grazes_shared_edge() {
        // Two triangles sharing edge (1,0,0)-(0,1,0); a ray straight down the
        // shared edge should hit exactly one of them under the u/v <= 1 rule
        // (u+v==1 on the edge is accepted, so both may report a hit at the
        // same t - that's fine, this just pins the accept/reject boundary).
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(1.0, 0.0, 0.0);
        let c = V3::new(0.0, 1.0, 0.0);
        let t = moller_trumbore(V3::new(0.5, 0.5, 1.0), V3::new(0.0, 0.0, -1.0), a, b, c);
        assert_eq!(t, Some(1.0));
    }

    #[test]
    fn tri_box_overlap_classic_hit() {
        let center = V3::ZERO;
        let half = V3::new(1.0, 1.0, 1.0);
        let a = V3::new(-0.5, -0.5, 0.0);
        let b = V3::new(0.5, -0.5, 0.0);
        let c = V3::new(0.0, 0.5, 0.0);
        assert!(tri_box_overlap(center, half, a, b, c));
    }

    #[test]
    fn tri_box_overlap_classic_miss() {
        let center = V3::ZERO;
        let half = V3::new(1.0, 1.0, 1.0);
        let a = V3::new(10.0, 10.0, 10.0);
        let b = V3::new(11.0, 10.0, 10.0);
        let c = V3::new(10.0, 11.0, 10.0);
        assert!(!tri_box_overlap(center, half, a, b, c));
    }

    #[test]
    fn tri_box_overlap_touching_flush_face() {
        // Triangle flush against the +X face of the box (x == half.x).
        let center = V3::ZERO;
        let half = V3::new(1.0, 1.0, 1.0);
        let a = V3::new(1.0, -0.5, -0.5);
        let b = V3::new(1.0, 0.5, -0.5);
        let c = V3::new(1.0, 0.0, 0.5);
        assert!(tri_box_overlap(center, half, a, b, c));
    }

    #[test]
    fn tri_box_overlap_edge_pierces_face() {
        let center = V3::ZERO;
        let half = V3::new(1.0, 1.0, 1.0);
        let a = V3::new(0.0, 0.0, 0.5);
        let b = V3::new(2.0, 0.0, 0.5);
        let c = V3::new(0.0, 2.0, 0.5);
        assert!(tri_box_overlap(center, half, a, b, c));
    }

    fn box_sweep(origin: V3, dir: V3, half: V3, a: V3, b: V3, c: V3) -> Option<SweepContact> {
        swept_box_triangle(origin, dir, half, a, b, c)
    }

    #[test]
    fn sweep_into_flat_floor_enters_via_box_z_axis() {
        // A perfectly axis-aligned floor: the box Z axis and the (unnormalized,
        // parallel) face normal axis are tested in order X, Y, Z, faceNormal
        // and compute the same geometric entry time, so per the ported tie
        // rule ("replaced only when axLo > tEnter strictly") the Z box axis -
        // tested first - keeps the tie and `face` is false here even though
        // the contact is a flat, face-on hit. This is NOT a fixed rule: it
        // depends on the triangle's (unnormalized) normal magnitude versus
        // f32 rounding, and the reference itself returns `face == true` for
        // some flat, axis-aligned triangles (e.g. one with legs (1,0,0) and
        // (0,1,0): cross of those is exactly (0,0,1), bit-identical to the
        // UnitZ box axis, so the tie is read as a face hit - see the golden
        // table in `mod golden` below, case 1). Stage-4 rest-on-ground logic
        // must not rely on `face` for flat surfaces; only the entry `normal`
        // is guaranteed. This test pins the large-triangle (magnitude-scaled
        // normal) side of that split (TriangleCollider.Sat.cs:142-149,
        // 189-199).
        let a = V3::new(-100.0, -100.0, 0.0);
        let b = V3::new(100.0, -100.0, 0.0);
        let c = V3::new(0.0, 100.0, 0.0);
        let hit = box_sweep(
            V3::new(0.0, 0.0, 5.0),
            V3::new(0.0, 0.0, -6.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        )
        .unwrap();
        // Box bottom (z - half.z) touches the plane at box center z = 1.
        assert!((hit.t - 4.0 / 6.0).abs() < 1e-5);
        assert!(!hit.face);
        assert!(hit.normal.z > 0.99);
    }

    #[test]
    fn sweep_into_tilted_floor_gives_face_normal() {
        // A floor tilted so its normal isn't parallel to any single box
        // axis: the true entry time now depends on the combined geometry,
        // so the face-normal axis (tested last) gives a strictly later
        // entry than any single box axis and wins on its own merits.
        let a = V3::new(-100.0, -100.0, -40.0);
        let b = V3::new(100.0, -100.0, 40.0);
        let c = V3::new(0.0, 100.0, 0.0);
        let hit = box_sweep(
            V3::new(0.0, 0.0, 5.0),
            V3::new(0.0, 0.0, -20.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        )
        .unwrap();
        assert!(hit.face);
        assert!(hit.normal.z > 0.0);
        assert!(hit.normal.dot(V3::new(0.0, 0.0, -20.0)) <= 0.0);
    }

    #[test]
    fn sweep_into_wall_gives_face_normal() {
        // Wall triangle tilted off the Y axis so the face normal (tested
        // last) is the axis that actually determines the entry time.
        let a = V3::new(-100.0, -20.0, -100.0);
        let b = V3::new(100.0, -20.0, -100.0);
        let c = V3::new(0.0, 20.0, 100.0);
        let hit = box_sweep(
            V3::new(0.0, -5.0, 0.0),
            V3::new(0.0, 12.0, 0.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        )
        .unwrap();
        assert!(hit.face);
        assert!(hit.normal.y < 0.0);
        assert!(hit.normal.dot(V3::new(0.0, 12.0, 0.0)) <= 0.0);
    }

    #[test]
    fn sweep_start_overlap_moving_in_reports_contact() {
        let a = V3::new(-100.0, -100.0, 0.0);
        let b = V3::new(100.0, -100.0, 0.0);
        let c = V3::new(0.0, 100.0, 0.0);
        // Box already overlapping the floor plane (center z = 0.5, half.z=1),
        // moving further down.
        let hit = box_sweep(
            V3::new(0.0, 0.0, 0.5),
            V3::new(0.0, 0.0, -1.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        );
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().t, 0.0);
    }

    #[test]
    fn sweep_start_overlap_moving_away_reports_none() {
        let a = V3::new(-100.0, -100.0, 0.0);
        let b = V3::new(100.0, -100.0, 0.0);
        let c = V3::new(0.0, 100.0, 0.0);
        let hit = box_sweep(
            V3::new(0.0, 0.0, 0.5),
            V3::new(0.0, 0.0, 1.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        );
        assert!(hit.is_none());
    }

    #[test]
    fn sweep_graze_parallel_to_wall_is_rejected_on_start_overlap() {
        let a = V3::new(-100.0, -100.0, 0.0);
        let b = V3::new(100.0, -100.0, 0.0);
        let c = V3::new(0.0, 100.0, 0.0);
        // Overlapping the plane but sliding sideways (no incidence angle).
        let hit = box_sweep(
            V3::new(0.0, 0.0, 0.5),
            V3::new(1.0, 0.0, 0.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        );
        assert!(hit.is_none());
    }

    #[test]
    fn sweep_degenerate_triangle_is_skipped() {
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(1.0, 0.0, 0.0);
        let c = V3::new(2.0, 0.0, 0.0); // colinear -> zero area
        let hit = box_sweep(
            V3::new(0.5, 5.0, 0.0),
            V3::new(0.0, -10.0, 0.0),
            V3::new(1.0, 1.0, 1.0),
            a,
            b,
            c,
        );
        assert!(hit.is_none());
    }

    #[test]
    fn sweep_corner_catch_hits() {
        // A box falling straight down onto the corner vertex c of a
        // triangle in the flat z=0 plane: the footprint clearly covers the
        // vertex, so this is a genuine corner-catch contact, not a miss.
        // The plane is still purely Z-normal, so - per the same tie rule as
        // the flat-floor case - the box Z axis wins and `face` is false.
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(10.0, 0.0, 0.0);
        let c = V3::new(10.0, 10.0, 0.0);
        let hit = box_sweep(
            V3::new(10.0, 10.0, 5.0),
            V3::new(0.0, 0.0, -10.0),
            V3::new(2.0, 2.0, 2.0),
            a,
            b,
            c,
        );
        assert!(hit.is_some());
        assert!(!hit.unwrap().face);
    }

    #[test]
    fn sweep_ray_misses_when_box_clears_triangle() {
        let a = V3::new(0.0, 0.0, 0.0);
        let b = V3::new(10.0, 0.0, 0.0);
        let c = V3::new(10.0, 10.0, 0.0);
        // Box stays well clear of the triangle's Y extent the whole sweep.
        let hit = box_sweep(
            V3::new(-5.0, -10.0, 0.0),
            V3::new(20.0, 0.0, 0.0),
            V3::new(0.5, 0.5, 0.5),
            a,
            b,
            c,
        );
        assert!(hit.is_none());
    }
}

#[cfg(test)]
mod golden {
    //! Golden bit-exact cross-check against the reference C# implementation
    //! (`cs2-smoke-solver`, .NET `net10.0`, compiled and run via `dotnet run`
    //! against the exact source files in `cs2-smoke-solver/src/Sim/`).
    //!
    //! 50 cases: 4 hand-picked to hit specific code paths (an edge-cross-axis
    //! contact found by exhaustive random search; a triangle with legs
    //! (1,0,0)/(0,1,0) whose normal cross((1,0,0),(0,1,0)) == (0,0,1) is the
    //! exact bit pattern of the UnitZ box axis, so `face` reads true even
    //! though the Z axis is tested first (see the module doc on
    //! `swept_box_triangle`); a start-overlap contact; a graze rejection) plus
    //! 46 generated by the fixed-seed `gen_prims` generator in the reviewer's
    //! `xcheck` harness (`scratchpad/xcheck/src/main.rs`).
    //!
    //! Values were produced by: `xcheck gen <dir> <mirage world.cgeo>` to write
    //! `cases.bin`, then the reference harness (`scratchpad/cs_x/Program.cs`,
    //! `dotnet run -- <dir>`) to read it and write `cs_out.bin` with the C#
    //! reference's `MollerTrumbore.Intersect`, `TriBoxOverlap.Test` and the
    //! private `TriangleCollider.SweptBoxTriangle` (invoked via reflection)
    //! results, then decoded to exact f32 bit patterns for this table.
    use super::*;

    struct GoldenCase {
        a: V3,
        b: V3,
        c: V3,
        o: V3,
        d: V3,
        h: V3,
        mt: Option<u32>,
        tb: bool,
        sweep: Option<(u32, [u32; 3], bool)>,
    }

    const CASES: &[GoldenCase] = &[
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            b: V3::new(
                f32::from_bits(0x41200000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40400000),
            ),
            c: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x41200000),
                f32::from_bits(0xc0000000),
            ),
            o: V3::new(
                f32::from_bits(0xc04c0d90),
                f32::from_bits(0xc109e838),
                f32::from_bits(0xc105e628),
            ),
            d: V3::new(
                f32::from_bits(0x41532462),
                f32::from_bits(0x4115cb58),
                f32::from_bits(0x41384e34),
            ),
            h: V3::new(
                f32::from_bits(0x3f36e3bb),
                f32::from_bits(0x3fb01694),
                f32::from_bits(0x4028207f),
            ),
            mt: None,
            tb: false,
            sweep: Some((0x3f761a54, [0x3e48d2ab, 0xbf7b0756, 0x80000000], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            b: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            c: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x3f800000),
                f32::from_bits(0x00000000),
            ),
            o: V3::new(
                f32::from_bits(0x3e800000),
                f32::from_bits(0x3e800000),
                f32::from_bits(0x40a00000),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0xc1200000),
            ),
            h: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
            ),
            mt: Some(0x3f000000),
            tb: false,
            sweep: Some((0x3ecccccd, [0x00000000, 0x00000000, 0x3f800000], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc2c80000),
                f32::from_bits(0xc2c80000),
                f32::from_bits(0x00000000),
            ),
            b: V3::new(
                f32::from_bits(0x42c80000),
                f32::from_bits(0xc2c80000),
                f32::from_bits(0x00000000),
            ),
            c: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x42c80000),
                f32::from_bits(0x00000000),
            ),
            o: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x3f000000),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0xbf800000),
            ),
            h: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
            ),
            mt: Some(0x3f000000),
            tb: true,
            sweep: Some((0x00000000, [0x00000000, 0x00000000, 0x3f800000], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc2c80000),
                f32::from_bits(0xc2c80000),
                f32::from_bits(0x00000000),
            ),
            b: V3::new(
                f32::from_bits(0x42c80000),
                f32::from_bits(0xc2c80000),
                f32::from_bits(0x00000000),
            ),
            c: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x42c80000),
                f32::from_bits(0x00000000),
            ),
            o: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x3f000000),
            ),
            d: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            h: V3::new(
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
                f32::from_bits(0x3f800000),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45007449),
                f32::from_bits(0x4305f943),
                f32::from_bits(0xc46ceb18),
            ),
            b: V3::new(
                f32::from_bits(0x450c59a3),
                f32::from_bits(0x42b460ae),
                f32::from_bits(0xc450beb7),
            ),
            c: V3::new(
                f32::from_bits(0x4505043b),
                f32::from_bits(0xc0b1ff60),
                f32::from_bits(0xc472fb6b),
            ),
            o: V3::new(
                f32::from_bits(0x450aaa47),
                f32::from_bits(0x429f698e),
                f32::from_bits(0xc4580ab2),
            ),
            d: V3::new(
                f32::from_bits(0xc028c9fb),
                f32::from_bits(0x4087d586),
                f32::from_bits(0xc0b287bf),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0xbf2c6afd),
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc51ec58f),
                f32::from_bits(0xc4a30970),
                f32::from_bits(0xc4f8b877),
            ),
            b: V3::new(
                f32::from_bits(0xc51ec58f),
                f32::from_bits(0xc4a30970),
                f32::from_bits(0xc4f112c4),
            ),
            c: V3::new(
                f32::from_bits(0xc522bae8),
                f32::from_bits(0xc4ae561a),
                f32::from_bits(0xc4f8b877),
            ),
            o: V3::new(
                f32::from_bits(0xc5206328),
                f32::from_bits(0xc4a8a8e3),
                f32::from_bits(0xc4f725b7),
            ),
            d: V3::new(
                f32::from_bits(0x400fa122),
                f32::from_bits(0x3cd02a00),
                f32::from_bits(0xbfe98bbd),
            ),
            h: V3::new(
                f32::from_bits(0x4041f6e4),
                f32::from_bits(0x41726030),
                f32::from_bits(0x4171ad1d),
            ),
            mt: Some(0xc022e3eb),
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x44d83e44),
                f32::from_bits(0xc4508347),
                f32::from_bits(0xc3eca158),
            ),
            b: V3::new(
                f32::from_bits(0x44ce675f),
                f32::from_bits(0xc44ccf99),
                f32::from_bits(0xc3eca158),
            ),
            c: V3::new(
                f32::from_bits(0x44cf8989),
                f32::from_bits(0xc4451054),
                f32::from_bits(0xc3eca158),
            ),
            o: V3::new(
                f32::from_bits(0x44d30fca),
                f32::from_bits(0xc44a23bd),
                f32::from_bits(0xc3eae5f2),
            ),
            d: V3::new(
                f32::from_bits(0x3f93fd41),
                f32::from_bits(0x4180dfb6),
                f32::from_bits(0xc0dc5789),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc406ce4c),
                f32::from_bits(0xc4f9e09f),
                f32::from_bits(0x44dc05c7),
            ),
            b: V3::new(
                f32::from_bits(0xc406ce4c),
                f32::from_bits(0xc4fdeba6),
                f32::from_bits(0x44df9c58),
            ),
            c: V3::new(
                f32::from_bits(0xc406ce4c),
                f32::from_bits(0xc4f92d58),
                f32::from_bits(0x44dd32e3),
            ),
            o: V3::new(
                f32::from_bits(0xc40971dc),
                f32::from_bits(0xc4fb1073),
                f32::from_bits(0x44dd5efa),
            ),
            d: V3::new(
                f32::from_bits(0xc0b486cd),
                f32::from_bits(0xbfe62f40),
                f32::from_bits(0xbf5c4b94),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0xbfef7fd7),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4da1d4a),
                f32::from_bits(0xc50e6bbf),
                f32::from_bits(0xc4ac8c57),
            ),
            b: V3::new(
                f32::from_bits(0xc4ee2a4e),
                f32::from_bits(0xc50d9113),
                f32::from_bits(0xc4ac8c57),
            ),
            c: V3::new(
                f32::from_bits(0xc4d21ee9),
                f32::from_bits(0xc5140d46),
                f32::from_bits(0xc4ac8c57),
            ),
            o: V3::new(
                f32::from_bits(0xc4dd2e53),
                f32::from_bits(0xc50f4959),
                f32::from_bits(0xc4ac2329),
            ),
            d: V3::new(
                f32::from_bits(0x41923359),
                f32::from_bits(0xc06fee21),
                f32::from_bits(0xc10fdc56),
            ),
            h: V3::new(
                f32::from_bits(0x3f6f2c8c),
                f32::from_bits(0x412f5764),
                f32::from_bits(0x3f5ef03f),
            ),
            mt: Some(0x3ebb2ace),
            tb: false,
            sweep: Some((0x3e8993cd, [0x00000000, 0x80000000, 0x3f800000], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc489a000),
                f32::from_bits(0x453b9000),
                f32::from_bits(0x45375000),
            ),
            b: V3::new(
                f32::from_bits(0xc49b2000),
                f32::from_bits(0x45385000),
                f32::from_bits(0x452b9000),
            ),
            c: V3::new(
                f32::from_bits(0xc485a000),
                f32::from_bits(0x453b0000),
                f32::from_bits(0x45405000),
            ),
            o: V3::new(
                f32::from_bits(0xc48e0929),
                f32::from_bits(0x453a3903),
                f32::from_bits(0x4537b0c3),
            ),
            d: V3::new(
                f32::from_bits(0xc11a7d10),
                f32::from_bits(0xc148d0ad),
                f32::from_bits(0x410d788a),
            ),
            h: V3::new(
                f32::from_bits(0x4069daf6),
                f32::from_bits(0x3fc9a4d8),
                f32::from_bits(0x414b48b4),
            ),
            mt: None,
            tb: true,
            sweep: Some((0x00000000, [0xbf09528d, 0x3f53940c, 0x3e2ef580], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4a7d0f3),
                f32::from_bits(0xc4f43f8b),
                f32::from_bits(0x451ec282),
            ),
            b: V3::new(
                f32::from_bits(0xc4b25ab5),
                f32::from_bits(0xc4fd531d),
                f32::from_bits(0x4520f3e5),
            ),
            c: V3::new(
                f32::from_bits(0xc4b16f29),
                f32::from_bits(0xc4fc8843),
                f32::from_bits(0x4520c2e2),
            ),
            o: V3::new(
                f32::from_bits(0xc4ae2d04),
                f32::from_bits(0xc4fa0eb3),
                f32::from_bits(0x45200f18),
            ),
            d: V3::new(
                f32::from_bits(0xc04e0dd8),
                f32::from_bits(0x4091a1e8),
                f32::from_bits(0xc019eabf),
            ),
            h: V3::new(
                f32::from_bits(0x4113b3db),
                f32::from_bits(0x3e7245b2),
                f32::from_bits(0x408130d3),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc52b2000),
                f32::from_bits(0x44014000),
                f32::from_bits(0xc535a000),
            ),
            b: V3::new(
                f32::from_bits(0xc523f000),
                f32::from_bits(0x43b28000),
                f32::from_bits(0xc5356000),
            ),
            c: V3::new(
                f32::from_bits(0xc525c000),
                f32::from_bits(0x43b50000),
                f32::from_bits(0xc5294000),
            ),
            o: V3::new(
                f32::from_bits(0xc5284faa),
                f32::from_bits(0x43e05265),
                f32::from_bits(0xc533452e),
            ),
            d: V3::new(
                f32::from_bits(0x3e7e1340),
                f32::from_bits(0x4046889c),
                f32::from_bits(0xc0c2e362),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0xbeb4d3ea),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4c3e000),
                f32::from_bits(0x44f54000),
                f32::from_bits(0x451fd000),
            ),
            b: V3::new(
                f32::from_bits(0xc4ca2000),
                f32::from_bits(0x44f9e000),
                f32::from_bits(0x451be000),
            ),
            c: V3::new(
                f32::from_bits(0xc4d2c000),
                f32::from_bits(0x44fa4000),
                f32::from_bits(0x451f3000),
            ),
            o: V3::new(
                f32::from_bits(0xc4c11530),
                f32::from_bits(0x44fb90b6),
                f32::from_bits(0x45194b93),
            ),
            d: V3::new(
                f32::from_bits(0xc09ab8bb),
                f32::from_bits(0x40250848),
                f32::from_bits(0xc020dd1e),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc50e7162),
                f32::from_bits(0x452a7ba8),
                f32::from_bits(0xc35dfca0),
            ),
            b: V3::new(
                f32::from_bits(0xc516cd4a),
                f32::from_bits(0x45307c1a),
                f32::from_bits(0xc35dfca0),
            ),
            c: V3::new(
                f32::from_bits(0xc516d36c),
                f32::from_bits(0x452cbeeb),
                f32::from_bits(0xc35dfca0),
            ),
            o: V3::new(
                f32::from_bits(0xc5144fc4),
                f32::from_bits(0x452d3a4f),
                f32::from_bits(0xc3579395),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x40f86ba0),
                f32::from_bits(0x00000000),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4a94000),
                f32::from_bits(0x451ba000),
                f32::from_bits(0x44a78000),
            ),
            b: V3::new(
                f32::from_bits(0xc4a16000),
                f32::from_bits(0x4518a000),
                f32::from_bits(0x449aa000),
            ),
            c: V3::new(
                f32::from_bits(0xc4b12000),
                f32::from_bits(0x4518b000),
                f32::from_bits(0x44a7c000),
            ),
            o: V3::new(
                f32::from_bits(0xc4aa1e5f),
                f32::from_bits(0x45195ee3),
                f32::from_bits(0x44a4070d),
            ),
            d: V3::new(
                f32::from_bits(0xc068dd48),
                f32::from_bits(0xc03219cf),
                f32::from_bits(0xc134732c),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0x3edab336),
            tb: true,
            sweep: Some((0x00000000, [0x3efbeab8, 0xbf225717, 0x3f18b23c], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x434a34d2),
                f32::from_bits(0x444eab99),
                f32::from_bits(0x45230141),
            ),
            b: V3::new(
                f32::from_bits(0x43b2d1e2),
                f32::from_bits(0x44839e9a),
                f32::from_bits(0x45244d71),
            ),
            c: V3::new(
                f32::from_bits(0x43a57b94),
                f32::from_bits(0x445f138e),
                f32::from_bits(0x45274e69),
            ),
            o: V3::new(
                f32::from_bits(0x43ad3ba6),
                f32::from_bits(0x4481798f),
                f32::from_bits(0x45247055),
            ),
            d: V3::new(
                f32::from_bits(0xc05d2cc1),
                f32::from_bits(0x40289aec),
                f32::from_bits(0xbf9117c4),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0xbfa3b8ee),
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc531a902),
                f32::from_bits(0x4412a266),
                f32::from_bits(0x44d22fda),
            ),
            b: V3::new(
                f32::from_bits(0xc531a902),
                f32::from_bits(0x44079295),
                f32::from_bits(0x44d53211),
            ),
            c: V3::new(
                f32::from_bits(0xc531a902),
                f32::from_bits(0x4402b51a),
                f32::from_bits(0x44cc0fc2),
            ),
            o: V3::new(
                f32::from_bits(0xc53066b0),
                f32::from_bits(0x4419c5b3),
                f32::from_bits(0x44d10ebd),
            ),
            d: V3::new(
                f32::from_bits(0x3f80ef34),
                f32::from_bits(0xc00873b0),
                f32::from_bits(0xc09c3070),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x438189b7),
                f32::from_bits(0xc331b2e6),
                f32::from_bits(0x43e90f5c),
            ),
            b: V3::new(
                f32::from_bits(0x439f7759),
                f32::from_bits(0xc37de5f6),
                f32::from_bits(0x43fb2416),
            ),
            c: V3::new(
                f32::from_bits(0x4376912e),
                f32::from_bits(0xc38e845e),
                f32::from_bits(0x439d5214),
            ),
            o: V3::new(
                f32::from_bits(0x4397ca1d),
                f32::from_bits(0xc37bb94d),
                f32::from_bits(0x43eb0035),
            ),
            d: V3::new(
                f32::from_bits(0xc0d4a091),
                f32::from_bits(0xbf81988c),
                f32::from_bits(0xc0dc51c4),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0x3eaa2fe5),
            tb: true,
            sweep: Some((0x00000000, [0x3f4e3d8d, 0x3ee66104, 0xbec54df0], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45382fae),
                f32::from_bits(0xc41fd9a0),
                f32::from_bits(0xc5270b7a),
            ),
            b: V3::new(
                f32::from_bits(0x452dc2d3),
                f32::from_bits(0xc3d165a1),
                f32::from_bits(0xc52ae1d9),
            ),
            c: V3::new(
                f32::from_bits(0x45317a50),
                f32::from_bits(0xc40a3155),
                f32::from_bits(0xc5240c7c),
            ),
            o: V3::new(
                f32::from_bits(0x45326cf5),
                f32::from_bits(0xc40d9df3),
                f32::from_bits(0xc524a518),
            ),
            d: V3::new(
                f32::from_bits(0x403000fe),
                f32::from_bits(0xc08c75ae),
                f32::from_bits(0xc11f3aec),
            ),
            h: V3::new(
                f32::from_bits(0x4126c869),
                f32::from_bits(0x4123297c),
                f32::from_bits(0x40d02671),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc52c82e3),
                f32::from_bits(0xc52a743d),
                f32::from_bits(0xc462d6e4),
            ),
            b: V3::new(
                f32::from_bits(0xc529dcf9),
                f32::from_bits(0xc52f3688),
                f32::from_bits(0xc462d6e4),
            ),
            c: V3::new(
                f32::from_bits(0xc52937ab),
                f32::from_bits(0xc526fb38),
                f32::from_bits(0xc462d6e4),
            ),
            o: V3::new(
                f32::from_bits(0xc52a6fc4),
                f32::from_bits(0xc52b03e1),
                f32::from_bits(0xc4624753),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x40c1beb8),
                f32::from_bits(0x00000000),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4b74387),
                f32::from_bits(0xc4909b2d),
                f32::from_bits(0x444484dc),
            ),
            b: V3::new(
                f32::from_bits(0xc4c84119),
                f32::from_bits(0xc490434a),
                f32::from_bits(0x44402aa9),
            ),
            c: V3::new(
                f32::from_bits(0xc4a53edb),
                f32::from_bits(0xc467940b),
                f32::from_bits(0x446480a0),
            ),
            o: V3::new(
                f32::from_bits(0xc4b7e3f7),
                f32::from_bits(0xc487fe5a),
                f32::from_bits(0x444d0c73),
            ),
            d: V3::new(
                f32::from_bits(0x410a60ab),
                f32::from_bits(0x411e1c03),
                f32::from_bits(0x40425d43),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3f658865),
            tb: false,
            sweep: Some((0x3f658858, [0xbdfd1bef, 0xbed7b66a, 0x3f660120], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc531251b),
                f32::from_bits(0xc323b25f),
                f32::from_bits(0x420d680a),
            ),
            b: V3::new(
                f32::from_bits(0xc530c384),
                f32::from_bits(0xc3057904),
                f32::from_bits(0x431ca191),
            ),
            c: V3::new(
                f32::from_bits(0xc5310447),
                f32::from_bits(0xc31987fb),
                f32::from_bits(0x42984797),
            ),
            o: V3::new(
                f32::from_bits(0xc52c4a07),
                f32::from_bits(0xc2e2f6b6),
                f32::from_bits(0x41dad20c),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x41902620),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45301cbf),
                f32::from_bits(0x44bec98c),
                f32::from_bits(0xc4ef919a),
            ),
            b: V3::new(
                f32::from_bits(0x4532546a),
                f32::from_bits(0x44c34431),
                f32::from_bits(0xc4f87c84),
            ),
            c: V3::new(
                f32::from_bits(0x452c5493),
                f32::from_bits(0x44c186ff),
                f32::from_bits(0xc4f55cfd),
            ),
            o: V3::new(
                f32::from_bits(0x45314987),
                f32::from_bits(0x44c2ab28),
                f32::from_bits(0xc4f697b8),
            ),
            d: V3::new(
                f32::from_bits(0x418ea0df),
                f32::from_bits(0xc1a8c6a8),
                f32::from_bits(0xbfb749af),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3e09ee3b),
            tb: false,
            sweep: Some((0x3e09ee3e, [0xbc6f8c50, 0x3f657b7c, 0x3ee2cd6a], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x437936ba),
                f32::from_bits(0x440c091d),
                f32::from_bits(0x449863e0),
            ),
            b: V3::new(
                f32::from_bits(0x438b4eb8),
                f32::from_bits(0x43f5fe1c),
                f32::from_bits(0x449932ae),
            ),
            c: V3::new(
                f32::from_bits(0x42aeea3b),
                f32::from_bits(0x44196d25),
                f32::from_bits(0x449a7e06),
            ),
            o: V3::new(
                f32::from_bits(0x42f4d928),
                f32::from_bits(0x440d48d3),
                f32::from_bits(0x44973ca7),
            ),
            d: V3::new(
                f32::from_bits(0xbfe052bc),
                f32::from_bits(0x4098d018),
                f32::from_bits(0xc0714a8f),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc0629f30),
                f32::from_bits(0x451e8918),
                f32::from_bits(0x44f7d3d4),
            ),
            b: V3::new(
                f32::from_bits(0xc0629f30),
                f32::from_bits(0x451e8918),
                f32::from_bits(0x44f7d23d),
            ),
            c: V3::new(
                f32::from_bits(0xc1c030de),
                f32::from_bits(0x451d32e0),
                f32::from_bits(0x44f7d3d4),
            ),
            o: V3::new(
                f32::from_bits(0xc1117db2),
                f32::from_bits(0x451ea081),
                f32::from_bits(0x44f7ff47),
            ),
            d: V3::new(
                f32::from_bits(0x40c14318),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: Some((0x3f00c75b, [0xbf38e456, 0x3f310fe3, 0x80000000], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x44b15525),
                f32::from_bits(0x43fa8af9),
                f32::from_bits(0x44171fd2),
            ),
            b: V3::new(
                f32::from_bits(0x44ab0f2c),
                f32::from_bits(0x4425c99c),
                f32::from_bits(0x4417282c),
            ),
            c: V3::new(
                f32::from_bits(0x4497c6e3),
                f32::from_bits(0x44242ab4),
                f32::from_bits(0x44192d97),
            ),
            o: V3::new(
                f32::from_bits(0x44a7bb99),
                f32::from_bits(0x44161246),
                f32::from_bits(0x441af345),
            ),
            d: V3::new(
                f32::from_bits(0x3eb68160),
                f32::from_bits(0x4072f2b8),
                f32::from_bits(0xc10406d6),
            ),
            h: V3::new(
                f32::from_bits(0x4097f1e4),
                f32::from_bits(0x41226b0f),
                f32::from_bits(0x40b58e4d),
            ),
            mt: Some(0x3fc90240),
            tb: false,
            sweep: Some((0x3f53d1e7, [0x3d53bda5, 0x3c791b57, 0x3f7fa0ca], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x44480000),
                f32::from_bits(0x444f8000),
                f32::from_bits(0xc5395000),
            ),
            b: V3::new(
                f32::from_bits(0x44548000),
                f32::from_bits(0x44390000),
                f32::from_bits(0xc534f000),
            ),
            c: V3::new(
                f32::from_bits(0x444cc000),
                f32::from_bits(0x444b4000),
                f32::from_bits(0xc5348000),
            ),
            o: V3::new(
                f32::from_bits(0x444cb85b),
                f32::from_bits(0x444810cb),
                f32::from_bits(0xc5358eb8),
            ),
            d: V3::new(
                f32::from_bits(0xc088996e),
                f32::from_bits(0x4093d37c),
                f32::from_bits(0xc12e08d4),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc43ee846),
                f32::from_bits(0x4501b8cd),
                f32::from_bits(0xc36151cb),
            ),
            b: V3::new(
                f32::from_bits(0xc43faaf2),
                f32::from_bits(0x450267dc),
                f32::from_bits(0xc35e0d08),
            ),
            c: V3::new(
                f32::from_bits(0xc43f9bb5),
                f32::from_bits(0x45025a2a),
                f32::from_bits(0xc35e4e5a),
            ),
            o: V3::new(
                f32::from_bits(0xc43f4539),
                f32::from_bits(0x45024d1d),
                f32::from_bits(0xc35ee7a3),
            ),
            d: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0xc05eb220),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x440cc000),
                f32::from_bits(0xc4cf4000),
                f32::from_bits(0xc5276000),
            ),
            b: V3::new(
                f32::from_bits(0x44010000),
                f32::from_bits(0xc4d08000),
                f32::from_bits(0xc527b000),
            ),
            c: V3::new(
                f32::from_bits(0x44044000),
                f32::from_bits(0xc4d02000),
                f32::from_bits(0xc5290000),
            ),
            o: V3::new(
                f32::from_bits(0x4404e47c),
                f32::from_bits(0xc4cf515a),
                f32::from_bits(0xc528935f),
            ),
            d: V3::new(
                f32::from_bits(0x3efd6250),
                f32::from_bits(0x409384ee),
                f32::from_bits(0xbfeba174),
            ),
            h: V3::new(
                f32::from_bits(0x3e9e10e3),
                f32::from_bits(0x406faa2b),
                f32::from_bits(0x40e479d3),
            ),
            mt: Some(0xbfaaa50a),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x443c0000),
                f32::from_bits(0xc47d4000),
                f32::from_bits(0x4486a000),
            ),
            b: V3::new(
                f32::from_bits(0x443f0000),
                f32::from_bits(0xc4744000),
                f32::from_bits(0x44680000),
            ),
            c: V3::new(
                f32::from_bits(0x44500000),
                f32::from_bits(0xc4700000),
                f32::from_bits(0x445b4000),
            ),
            o: V3::new(
                f32::from_bits(0x44450d1e),
                f32::from_bits(0xc473f940),
                f32::from_bits(0x446b5c1b),
            ),
            d: V3::new(
                f32::from_bits(0xc0b8cdc2),
                f32::from_bits(0xbfd8049e),
                f32::from_bits(0xc0f868ad),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0x3f4ee746),
            tb: true,
            sweep: Some((0x00000000, [0xbd9192f9, 0x3f788a8a, 0x3e6a56bd], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc52f9a1d),
                f32::from_bits(0xc4cd1432),
                f32::from_bits(0xc512c8ed),
            ),
            b: V3::new(
                f32::from_bits(0xc532b5e3),
                f32::from_bits(0xc4cde1db),
                f32::from_bits(0xc512c8ed),
            ),
            c: V3::new(
                f32::from_bits(0xc5319626),
                f32::from_bits(0xc4cec52c),
                f32::from_bits(0xc512c8ed),
            ),
            o: V3::new(
                f32::from_bits(0xc5310e9c),
                f32::from_bits(0xc4d05245),
                f32::from_bits(0xc5146559),
            ),
            d: V3::new(
                f32::from_bits(0xc00bfb60),
                f32::from_bits(0x412de10e),
                f32::from_bits(0x416860f9),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0x3fe32c16),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x43c404a6),
                f32::from_bits(0x450e26e6),
                f32::from_bits(0x443f11da),
            ),
            b: V3::new(
                f32::from_bits(0x43a64fc8),
                f32::from_bits(0x450f9b30),
                f32::from_bits(0x4455b5a0),
            ),
            c: V3::new(
                f32::from_bits(0x43b35a6a),
                f32::from_bits(0x451000a5),
                f32::from_bits(0x4439aabd),
            ),
            o: V3::new(
                f32::from_bits(0x43b339d1),
                f32::from_bits(0x450f6a48),
                f32::from_bits(0x44452404),
            ),
            d: V3::new(
                f32::from_bits(0xc05ce321),
                f32::from_bits(0xc08f2938),
                f32::from_bits(0xc0307169),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3df2816a),
            tb: false,
            sweep: Some((0x3df28166, [0x3f15c98c, 0x3f4a61f7, 0x3e39159a], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc51fe000),
                f32::from_bits(0xc53b7000),
                f32::from_bits(0xc42c0000),
            ),
            b: V3::new(
                f32::from_bits(0xc51fc000),
                f32::from_bits(0xc53b0000),
                f32::from_bits(0xc42c0000),
            ),
            c: V3::new(
                f32::from_bits(0xc51f5000),
                f32::from_bits(0xc53a6000),
                f32::from_bits(0xc42bc000),
            ),
            o: V3::new(
                f32::from_bits(0xc51f803f),
                f32::from_bits(0xc53b5ad5),
                f32::from_bits(0xc42ad517),
            ),
            d: V3::new(
                f32::from_bits(0x3f4001f9),
                f32::from_bits(0x40bac02e),
                f32::from_bits(0x410e8e60),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x444ca0a6),
                f32::from_bits(0xc4b51672),
                f32::from_bits(0xc51c0ec0),
            ),
            b: V3::new(
                f32::from_bits(0x4450f93d),
                f32::from_bits(0xc4b97d32),
                f32::from_bits(0xc51cf9bb),
            ),
            c: V3::new(
                f32::from_bits(0x4450a7c4),
                f32::from_bits(0xc4b92abc),
                f32::from_bits(0xc51ce887),
            ),
            o: V3::new(
                f32::from_bits(0x44500013),
                f32::from_bits(0xc4b8253e),
                f32::from_bits(0xc51c5852),
            ),
            d: V3::new(
                f32::from_bits(0xc0430283),
                f32::from_bits(0x401ae4f1),
                f32::from_bits(0xc0caabad),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0x3f429016),
            tb: false,
            sweep: Some((0x3ec12e80, [0x00000000, 0xbec5172b, 0x3f6c45d6], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45368028),
                f32::from_bits(0xc4e335dc),
                f32::from_bits(0xc50153b5),
            ),
            b: V3::new(
                f32::from_bits(0x45368e67),
                f32::from_bits(0xc4e457d4),
                f32::from_bits(0xc5026375),
            ),
            c: V3::new(
                f32::from_bits(0x45368da0),
                f32::from_bits(0xc4e447bb),
                f32::from_bits(0xc5025462),
            ),
            o: V3::new(
                f32::from_bits(0x4536a12c),
                f32::from_bits(0xc4e47bdd),
                f32::from_bits(0xc5021d77),
            ),
            d: V3::new(
                f32::from_bits(0x41a8ba70),
                f32::from_bits(0xc12cd271),
                f32::from_bits(0xc132570f),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc45d9a90),
                f32::from_bits(0xc49506c4),
                f32::from_bits(0xc4cc4dde),
            ),
            b: V3::new(
                f32::from_bits(0xc48654a0),
                f32::from_bits(0xc4ac8e34),
                f32::from_bits(0xc4d9e3a2),
            ),
            c: V3::new(
                f32::from_bits(0xc47b1184),
                f32::from_bits(0xc49cee92),
                f32::from_bits(0xc4cb5381),
            ),
            o: V3::new(
                f32::from_bits(0xc4818bd4),
                f32::from_bits(0xc4a6522a),
                f32::from_bits(0xc4d4f9f6),
            ),
            d: V3::new(
                f32::from_bits(0xc0787738),
                f32::from_bits(0xbe277df5),
                f32::from_bits(0xc0cad9da),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3df06373),
            tb: false,
            sweep: Some((0x3df062df, [0x3edb68d9, 0xbf3bb964, 0x3f0721a3], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45286e92),
                f32::from_bits(0xc3e18410),
                f32::from_bits(0xc4b776de),
            ),
            b: V3::new(
                f32::from_bits(0x4529adf8),
                f32::from_bits(0xc3d29503),
                f32::from_bits(0xc4b7c6ba),
            ),
            c: V3::new(
                f32::from_bits(0x45291298),
                f32::from_bits(0xc3d9d8a1),
                f32::from_bits(0xc4b79fe4),
            ),
            o: V3::new(
                f32::from_bits(0x4528c14f),
                f32::from_bits(0xc3de9aad),
                f32::from_bits(0xc4b7cc27),
            ),
            d: V3::new(
                f32::from_bits(0xbeb56b46),
                f32::from_bits(0x3fb0673d),
                f32::from_bits(0xbfdbc656),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc42a1a66),
                f32::from_bits(0x45223ef5),
                f32::from_bits(0x441a342f),
            ),
            b: V3::new(
                f32::from_bits(0xc405f003),
                f32::from_bits(0x4525241c),
                f32::from_bits(0x4402a769),
            ),
            c: V3::new(
                f32::from_bits(0xc41be762),
                f32::from_bits(0x452b8c6f),
                f32::from_bits(0x4416b468),
            ),
            o: V3::new(
                f32::from_bits(0xc415c843),
                f32::from_bits(0x4525a25b),
                f32::from_bits(0x440f85f0),
            ),
            d: V3::new(
                f32::from_bits(0xc1d582b2),
                f32::from_bits(0x41a3d42d),
                f32::from_bits(0x41492f07),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3f0789cb),
            tb: false,
            sweep: Some((0x3f0789c2, [0x3f125b07, 0xbe11955d, 0x3f4edc5d], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc52d6ade),
                f32::from_bits(0xc40edcad),
                f32::from_bits(0xc5298d0c),
            ),
            b: V3::new(
                f32::from_bits(0xc52f7e5f),
                f32::from_bits(0xc3fdde78),
                f32::from_bits(0xc52c32a3),
            ),
            c: V3::new(
                f32::from_bits(0xc52d7f2e),
                f32::from_bits(0xc40e40c3),
                f32::from_bits(0xc529a6f3),
            ),
            o: V3::new(
                f32::from_bits(0xc52e5605),
                f32::from_bits(0xc407a781),
                f32::from_bits(0xc52b1f2f),
            ),
            d: V3::new(
                f32::from_bits(0x4040ec69),
                f32::from_bits(0xc0cc5dee),
                f32::from_bits(0x411ddb86),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: Some((0x3ef071d5, [0x00000000, 0xbf0db745, 0xbf553238], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc5361f4e),
                f32::from_bits(0x44b20d17),
                f32::from_bits(0x448139b8),
            ),
            b: V3::new(
                f32::from_bits(0xc538b7db),
                f32::from_bits(0x44b808ae),
                f32::from_bits(0x44826900),
            ),
            c: V3::new(
                f32::from_bits(0xc538177d),
                f32::from_bits(0x44b8368e),
                f32::from_bits(0x4480ade7),
            ),
            o: V3::new(
                f32::from_bits(0xc5379dde),
                f32::from_bits(0x44b31f81),
                f32::from_bits(0x44828f6e),
            ),
            d: V3::new(
                f32::from_bits(0xbe399040),
                f32::from_bits(0x408617ae),
                f32::from_bits(0xc0f5b07e),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc532ac23),
                f32::from_bits(0xc53510f7),
                f32::from_bits(0xc4a7c5e5),
            ),
            b: V3::new(
                f32::from_bits(0xc5325067),
                f32::from_bits(0xc53510f7),
                f32::from_bits(0xc4a85306),
            ),
            c: V3::new(
                f32::from_bits(0xc53213d5),
                f32::from_bits(0xc53510f7),
                f32::from_bits(0xc4a7b499),
            ),
            o: V3::new(
                f32::from_bits(0xc5325a6f),
                f32::from_bits(0xc53518eb),
                f32::from_bits(0xc4a86e30),
            ),
            d: V3::new(
                f32::from_bits(0xbd068794),
                f32::from_bits(0x3f3a132e),
                f32::from_bits(0x40b9bb71),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3f2f11bc),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x44d66040),
                f32::from_bits(0xc4d60f0f),
                f32::from_bits(0x448bdadc),
            ),
            b: V3::new(
                f32::from_bits(0x44d77a51),
                f32::from_bits(0xc4d549d9),
                f32::from_bits(0x448bbcb6),
            ),
            c: V3::new(
                f32::from_bits(0x44d61941),
                f32::from_bits(0xc4d6a702),
                f32::from_bits(0x4487e5b4),
            ),
            o: V3::new(
                f32::from_bits(0x44d74b4c),
                f32::from_bits(0xc4d60dac),
                f32::from_bits(0x448cfaef),
            ),
            d: V3::new(
                f32::from_bits(0xc0ca29ef),
                f32::from_bits(0x3f06916e),
                f32::from_bits(0xc1c32125),
            ),
            h: V3::new(
                f32::from_bits(0x40e81c6d),
                f32::from_bits(0x3ffd6abc),
                f32::from_bits(0x4130dd13),
            ),
            mt: Some(0x3f508b77),
            tb: true,
            sweep: Some((0x00000000, [0x3f13b1d2, 0xbf500e68, 0x3da6d4c4], true)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x4535eafc),
                f32::from_bits(0xc47daefa),
                f32::from_bits(0xc52fd8d9),
            ),
            b: V3::new(
                f32::from_bits(0x4535eafc),
                f32::from_bits(0xc47daefa),
                f32::from_bits(0xc538c4ca),
            ),
            c: V3::new(
                f32::from_bits(0x453df0ec),
                f32::from_bits(0xc46947f8),
                f32::from_bits(0xc52fd8d9),
            ),
            o: V3::new(
                f32::from_bits(0x452ec025),
                f32::from_bits(0xc486ebb6),
                f32::from_bits(0xc532e01e),
            ),
            d: V3::new(
                f32::from_bits(0xc069b2cc),
                f32::from_bits(0x409c83ec),
                f32::from_bits(0xc0f0246b),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x440ac2e0),
                f32::from_bits(0xc42a8b3a),
                f32::from_bits(0x453a4513),
            ),
            b: V3::new(
                f32::from_bits(0x441174b3),
                f32::from_bits(0xc429944c),
                f32::from_bits(0x4539e439),
            ),
            c: V3::new(
                f32::from_bits(0x4414225a),
                f32::from_bits(0xc4236350),
                f32::from_bits(0x4538b167),
            ),
            o: V3::new(
                f32::from_bits(0x44111724),
                f32::from_bits(0xc426dc72),
                f32::from_bits(0x45394dcc),
            ),
            d: V3::new(
                f32::from_bits(0x4134b3c5),
                f32::from_bits(0xc0a0aa2b),
                f32::from_bits(0x3fbc8cd4),
            ),
            h: V3::new(
                f32::from_bits(0x40b94ea4),
                f32::from_bits(0x4146e267),
                f32::from_bits(0x3faa6382),
            ),
            mt: None,
            tb: true,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc5118908),
                f32::from_bits(0x44eb40c4),
                f32::from_bits(0xc4cd5b91),
            ),
            b: V3::new(
                f32::from_bits(0xc51a2e2c),
                f32::from_bits(0x44f10dd5),
                f32::from_bits(0xc4c3124c),
            ),
            c: V3::new(
                f32::from_bits(0xc5153355),
                f32::from_bits(0x44edb65f),
                f32::from_bits(0xc4c8ff2b),
            ),
            o: V3::new(
                f32::from_bits(0xc5103940),
                f32::from_bits(0x44ff6a02),
                f32::from_bits(0xc4d29872),
            ),
            d: V3::new(
                f32::from_bits(0xc3228461),
                f32::from_bits(0xc3829beb),
                f32::from_bits(0x4317dcfc),
            ),
            h: V3::new(
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
                f32::from_bits(0x00000000),
            ),
            mt: Some(0x3f080723),
            tb: false,
            sweep: Some((0x3f087aa5, [0x80000000, 0x3f5efbda, 0xbefb825b], false)),
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc4fce000),
                f32::from_bits(0x44d80000),
                f32::from_bits(0xc4778000),
            ),
            b: V3::new(
                f32::from_bits(0xc50eb000),
                f32::from_bits(0x44eca000),
                f32::from_bits(0xc491e000),
            ),
            c: V3::new(
                f32::from_bits(0xc4f88000),
                f32::from_bits(0x44ea8000),
                f32::from_bits(0xc449c000),
            ),
            o: V3::new(
                f32::from_bits(0xc5033d02),
                f32::from_bits(0x44e53f51),
                f32::from_bits(0xc478eecb),
            ),
            d: V3::new(
                f32::from_bits(0x3f1b6090),
                f32::from_bits(0x3fbbfb4c),
                f32::from_bits(0xc100ded2),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0xbf2ea6ff),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x45078ae0),
                f32::from_bits(0xc4ce4ce3),
                f32::from_bits(0x4474b9b8),
            ),
            b: V3::new(
                f32::from_bits(0x4506b19f),
                f32::from_bits(0xc4cdcfe5),
                f32::from_bits(0x4474b9b8),
            ),
            c: V3::new(
                f32::from_bits(0x45054cd7),
                f32::from_bits(0xc4d17e8f),
                f32::from_bits(0x4474b9b8),
            ),
            o: V3::new(
                f32::from_bits(0x45073a9d),
                f32::from_bits(0xc4cd9741),
                f32::from_bits(0x4470083c),
            ),
            d: V3::new(
                f32::from_bits(0xc0892baa),
                f32::from_bits(0x3f8342f0),
                f32::from_bits(0xbfb8a4a4),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc5122ee9),
                f32::from_bits(0x4464a509),
                f32::from_bits(0x447812c5),
            ),
            b: V3::new(
                f32::from_bits(0xc5122ee9),
                f32::from_bits(0x4464a509),
                f32::from_bits(0x447d5f11),
            ),
            c: V3::new(
                f32::from_bits(0xc5124059),
                f32::from_bits(0x444d34c8),
                f32::from_bits(0x447812c5),
            ),
            o: V3::new(
                f32::from_bits(0xc50ee26b),
                f32::from_bits(0x4446060c),
                f32::from_bits(0x44669e2d),
            ),
            d: V3::new(
                f32::from_bits(0xbfd5b649),
                f32::from_bits(0x3efcf627),
                f32::from_bits(0x3eb2ae5f),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0x44822000),
                f32::from_bits(0x4410c000),
                f32::from_bits(0xc5213000),
            ),
            b: V3::new(
                f32::from_bits(0x4445c000),
                f32::from_bits(0x43b38000),
                f32::from_bits(0xc5237000),
            ),
            c: V3::new(
                f32::from_bits(0x44750000),
                f32::from_bits(0x43c70000),
                f32::from_bits(0xc5242000),
            ),
            o: V3::new(
                f32::from_bits(0x4480b609),
                f32::from_bits(0x440a85db),
                f32::from_bits(0xc51bbd5e),
            ),
            d: V3::new(
                f32::from_bits(0x412db747),
                f32::from_bits(0x410ad22b),
                f32::from_bits(0x4105ca84),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: Some(0xc15357b1),
            tb: false,
            sweep: None,
        },
        GoldenCase {
            a: V3::new(
                f32::from_bits(0xc440bd7d),
                f32::from_bits(0x44981924),
                f32::from_bits(0xc5416cb6),
            ),
            b: V3::new(
                f32::from_bits(0xc44ad330),
                f32::from_bits(0x449481bf),
                f32::from_bits(0xc5376b9d),
            ),
            c: V3::new(
                f32::from_bits(0xc4489253),
                f32::from_bits(0x44956b30),
                f32::from_bits(0xc534a927),
            ),
            o: V3::new(
                f32::from_bits(0xc437057c),
                f32::from_bits(0x44a04016),
                f32::from_bits(0xc538ca1f),
            ),
            d: V3::new(
                f32::from_bits(0x408e0636),
                f32::from_bits(0xc054ff67),
                f32::from_bits(0x4027de90),
            ),
            h: V3::new(
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
                f32::from_bits(0x40000000),
            ),
            mt: None,
            tb: false,
            sweep: None,
        },
    ];

    #[test]
    fn golden_table_matches_reference_harness() {
        for (i, case) in CASES.iter().enumerate() {
            let mt = moller_trumbore(case.o, case.d, case.a, case.b, case.c);
            match (mt, case.mt) {
                (Some(t), Some(bits)) => assert_eq!(t.to_bits(), bits, "case {i}: MT t mismatch"),
                (None, None) => {}
                (got, want) => panic!("case {i}: MT hit/miss mismatch: got={got:?} want={want:?}"),
            }

            let tb = tri_box_overlap(case.o, case.h, case.a, case.b, case.c);
            assert_eq!(tb, case.tb, "case {i}: tri_box_overlap mismatch");

            let sw = swept_box_triangle(case.o, case.d, case.h, case.a, case.b, case.c);
            match (sw, case.sweep) {
                (Some(hit), Some((t_bits, n_bits, face))) => {
                    assert_eq!(hit.t.to_bits(), t_bits, "case {i}: sweep t mismatch");
                    assert_eq!(
                        hit.normal.to_array().map(f32::to_bits),
                        n_bits,
                        "case {i}: sweep normal mismatch"
                    );
                    assert_eq!(hit.face, face, "case {i}: sweep face mismatch");
                }
                (None, None) => {}
                (got, want) => {
                    panic!("case {i}: sweep hit/miss mismatch: got={got:?} want={want:?}")
                }
            }
        }
    }
}
