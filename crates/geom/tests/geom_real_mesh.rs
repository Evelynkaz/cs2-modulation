//! Real-mesh differential and sanity checks against the actual mirage
//! collision mesh, nav mesh and entity dump. Ignored by default: set
//! `CS2MOD_CGEO=<path to world.cgeo>` (with `nav.json`/`entities.json` next
//! to it, as `extract` writes them) and run with `cargo test --release`.

use std::path::PathBuf;
use std::time::Instant;

use rayon::prelude::*;
use serde::Deserialize;

use geom::bvh::Bvh;
use geom::cgeo;
use geom::collider::{Collider, ColliderTriangles, RayHit};
use geom::filter::{AttributeMask, all_mask, grenade_mask, player_mask};
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::CollisionMesh;
use geom::tri::{RayWindow, moller_trumbore, swept_box_triangle, tri_box_overlap};
use geom::voxel::VoxelGrid;

#[derive(Debug, Deserialize)]
struct EntityRecord {
    classname: String,
    origin: [f32; 3],
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NavArea {
    id: u32,
    hull_index: u8,
    corners: Vec<[f32; 3]>,
}

#[derive(Debug, Deserialize)]
struct NavAreasDump {
    areas: Vec<NavArea>,
}

/// The subset of `extract::report::SkippedEntity` this test needs. Note:
/// no `origin` field exists here (see `classify_failure`'s "skipped entity"
/// rule) - origins are recovered by matching `(classname, model)` against
/// `entities.json`.
#[derive(Debug, Deserialize)]
struct SkippedEntityRecord {
    classname: String,
    model: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ExtractReportPartial {
    #[serde(default)]
    skipped_entities: Vec<SkippedEntityRecord>,
}

/// Fixed-seed xorshift32 PRNG (no `rand` dependency, matching the rest of
/// the crate's differential tests).
struct Rng(u32);
impl Rng {
    fn new(seed: u32) -> Self {
        Rng(seed | 1)
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn f32(&mut self, lo: f32, hi: f32) -> f32 {
        let t = self.next_u32() as f32 / u32::MAX as f32;
        lo + t * (hi - lo)
    }
    fn v3(&mut self, lo: f32, hi: f32) -> V3 {
        V3::new(self.f32(lo, hi), self.f32(lo, hi), self.f32(lo, hi))
    }
    fn unit_v3(&mut self) -> V3 {
        loop {
            let v = self.v3(-1.0, 1.0);
            let len2 = v.length_squared();
            if len2 > 1e-6 && len2 <= 1.0 {
                return v / len2.sqrt();
            }
        }
    }
}

struct Loaded {
    mesh: CollisionMesh,
    entities: Vec<EntityRecord>,
    nav: NavAreasDump,
    skipped_entities: Vec<SkippedEntityRecord>,
}

fn load() -> Loaded {
    let cgeo_path = std::env::var("CS2MOD_CGEO")
        .expect("set CS2MOD_CGEO=<path to world.cgeo> to run this test (nav.json/entities.json must sit alongside it)");
    let cgeo_path = PathBuf::from(cgeo_path);
    let dir = cgeo_path
        .parent()
        .expect("world.cgeo path has a parent dir");
    let (mesh, _meta) = cgeo::load_cgeo(&cgeo_path).expect("failed to load world.cgeo");
    let entities_text =
        std::fs::read_to_string(dir.join("entities.json")).expect("failed to read entities.json");
    let entities: Vec<EntityRecord> =
        serde_json::from_str(&entities_text).expect("failed to parse entities.json");
    let nav_text = std::fs::read_to_string(dir.join("nav.json")).expect("failed to read nav.json");
    let nav: NavAreasDump = serde_json::from_str(&nav_text).expect("failed to parse nav.json");
    let report_text =
        std::fs::read_to_string(dir.join("report.json")).expect("failed to read report.json");
    let report: ExtractReportPartial =
        serde_json::from_str(&report_text).expect("failed to parse report.json");
    Loaded {
        mesh,
        entities,
        nav,
        skipped_entities: report.skipped_entities,
    }
}

fn lerp_aabb(bounds: &Aabb, rng: &mut Rng) -> V3 {
    V3::new(
        rng.f32(bounds.min.x, bounds.max.x),
        rng.f32(bounds.min.y, bounds.max.y),
        rng.f32(bounds.min.z, bounds.max.z),
    )
}

/// `UniformGrid` resolves exact-`t` ties in reference cell-visitation order
/// (matching the C# original), while `Bvh` resolves them by lowest original
/// triangle index. A triangle-index mismatch between the two is only
/// acceptable when it's a *genuine* tie: independently re-testing each
/// triangle against the same query reproduces the exact same reported `t`.
fn confirm_ray_tie(grid: &UniformGrid, from: V3, direction: V3, ta: u32, tb: u32, t: f32) -> bool {
    let [a0, b0, c0] = grid.triangle(ta);
    let [a1, b1, c1] = grid.triangle(tb);
    moller_trumbore(from, direction, a0, b0, c0) == Some(t)
        && moller_trumbore(from, direction, a1, b1, c1) == Some(t)
}

fn confirm_hull_tie(
    grid: &UniformGrid,
    from: V3,
    direction: V3,
    half: V3,
    ta: u32,
    tb: u32,
    t: f32,
) -> bool {
    let [a0, b0, c0] = grid.triangle(ta);
    let [a1, b1, c1] = grid.triangle(tb);
    swept_box_triangle(from, direction, half, a0, b0, c0).map(|h| h.t) == Some(t)
        && swept_box_triangle(from, direction, half, a1, b1, c1).map(|h| h.t) == Some(t)
}

/// A rayon brute-force oracle over every triangle in `triangles` (no
/// acceleration structure at all), used to adjudicate BVH-vs-grid seam
/// mismatches independently of either structure's traversal order.
fn oracle_hit(
    triangles: &ColliderTriangles,
    from: V3,
    to: V3,
    window: RayWindow,
) -> Option<(f32, u32)> {
    let direction = to - from;
    (0..triangles.len())
        .into_par_iter()
        .fold(
            || None::<(f32, u32)>,
            |acc, local| {
                let [a, b, c] = triangles.vertices(local);
                match moller_trumbore(from, direction, a, b, c) {
                    Some(t) if window.accepts(t) => {
                        let cand = (t, triangles.original_index(local));
                        match acc {
                            Some(best) if best.0 <= cand.0 => Some(best),
                            _ => Some(cand),
                        }
                    }
                    _ => acc,
                }
            },
        )
        .reduce(
            || None,
            |a, b| match (a, b) {
                (Some(x), Some(y)) => Some(if x.0 <= y.0 { x } else { y }),
                (Some(x), None) => Some(x),
                (None, Some(y)) => Some(y),
                (None, None) => None,
            },
        )
}

/// Whether `original`'s triangle AABB lies flush with (or past) the grid's
/// own upper bound on some axis - `UniformGrid::build`'s documented
/// reference-exact one-sided covered-cell clamp (`grid.rs`) then drops that
/// triangle from every cell of the grid, even though it's still part of the
/// grid's own triangle set (`grid.triangle`/`local_index` still know it).
/// `region` is recomputed here the same way `UniformGrid::build` does for
/// `region: None` (the union of `triangles`' own AABBs), so this needs no
/// access to the grid's private fields.
fn is_grid_boundary_drop(triangles: &ColliderTriangles, cell_size: f32, original: u32) -> bool {
    let Some(local) = triangles.local_index(original) else {
        return false;
    };
    let Some(region) = triangles.bounds() else {
        return false;
    };
    let aabb = triangles.aabb(local);
    let n = |lo: f32, hi: f32| (((hi - lo) / cell_size).ceil()).max(1.0);
    let grid_max = V3::new(
        region.min.x + n(region.min.x, region.max.x) * cell_size,
        region.min.y + n(region.min.y, region.max.y) * cell_size,
        region.min.z + n(region.min.z, region.max.z) * cell_size,
    );
    aabb.max.x >= grid_max.x - 1e-3
        || aabb.max.y >= grid_max.y - 1e-3
        || aabb.max.z >= grid_max.z - 1e-3
}

/// Brute-force (no acceleration structure) SAT overlap scan, used to
/// adjudicate `box_intersects` BVH/grid mismatches: the query returns no
/// triangle index, so the boundary-drop check needs its own candidate set.
fn oracle_box_overlap_triangles(triangles: &ColliderTriangles, center: V3, half: V3) -> Vec<u32> {
    (0..triangles.len())
        .filter(|&local| {
            let [a, b, c] = triangles.vertices(local);
            tri_box_overlap(center, half, a, b, c)
        })
        .map(|local| triangles.original_index(local))
        .collect()
}

/// Whether `original`'s triangle plane is nearly parallel to `direction`
/// (`|cos(direction, normal)| < 1e-5`) - the documented `moller_trumbore`
/// in-plane-ray exception (`bvh.rs` module doc) that both `UniformGrid` and
/// `Bvh` inherit from the shared primitive.
fn is_in_plane_mismatch(triangles: &ColliderTriangles, direction: V3, original: u32) -> bool {
    let Some(local) = triangles.local_index(original) else {
        return false;
    };
    let [a, b, c] = triangles.vertices(local);
    let n = (b - a).cross(c - a).normalize();
    let len = direction.length();
    if len < 1e-9 {
        return false;
    }
    (direction.dot(n) / len).abs() < 1e-5
}

fn mesh_bounds(mesh: &CollisionMesh) -> Aabb {
    let (min, max) = mesh.bounds().expect("mesh has geometry");
    Aabb {
        min: V3::from_array(min),
        max: V3::from_array(max),
    }
}

#[test]
#[ignore = "needs CS2MOD_CGEO"]
fn real_mesh_bvh_grid_differential_and_reports() {
    // Assertions are deferred to the end (accumulated here) so every section
    // finishes and prints its numbers even if an earlier one finds a
    // failure - the whole point of this test is the printed report.
    let mut failures: Vec<String> = Vec::new();
    let Loaded {
        mesh,
        entities,
        nav,
        skipped_entities,
    } = load();
    println!(
        "mesh: {} triangles, {} vertices, {} attributes",
        mesh.triangle_count(),
        mesh.vertices.len(),
        mesh.attributes.len()
    );
    let bounds = mesh_bounds(&mesh);
    println!("mesh bounds: {:?} .. {:?}", bounds.min, bounds.max);

    // ---- 1. BVH vs grid differential ----
    for (mask_name, mask) in [
        ("grenade", grenade_mask(&mesh)),
        ("player", player_mask(&mesh)),
    ] {
        let t0 = Instant::now();
        let triangles = geom::collider::ColliderTriangles::build(&mesh, &mask, None).unwrap();
        let oracle_triangles = triangles.clone();
        let grid = UniformGrid::build(&mesh, &mask, None, 16.0).unwrap();
        let bvh = Bvh::from_triangles(triangles);
        println!(
            "[{mask_name}] grid+bvh build: {:?}, triangles={}",
            t0.elapsed(),
            grid.triangle_count()
        );

        let mut rng = Rng::new(0xD1FF_0001 ^ (mask_name.len() as u32));
        let mut hull_hits = 0u64;
        let mut hull_mismatches = 0u64;
        let mut hull_explained_boundary = 0u64;
        let mut hull_ties = 0u64;
        let mut hull_ties_changed_normal = 0u64;
        for _ in 0..200_000 {
            let from = lerp_aabb(&bounds, &mut rng);
            let dir = rng.unit_v3();
            let len = rng.f32(1.0, 12.0);
            let to = from + dir * len;
            let half = V3::new(2.0, 2.0, 2.0);
            let g = grid.first_hit_hull(from, to, half, -2.0, None);
            let b = bvh.first_hit_hull(from, to, half, -2.0, None);
            if g.is_some() {
                hull_hits += 1;
            }
            match (g, b) {
                (Some(g), Some(b)) if g.t == b.t && g.triangle == b.triangle => {
                    assert_eq!(g.face, b.face);
                }
                (Some(g), Some(b))
                    if g.t == b.t
                        && confirm_hull_tie(
                            &grid,
                            from,
                            to - from,
                            half,
                            g.triangle,
                            b.triangle,
                            g.t,
                        ) =>
                {
                    hull_ties += 1;
                    if g.normal.to_array() != b.normal.to_array() {
                        hull_ties_changed_normal += 1;
                    }
                }
                (None, None) => {}
                (g, b) => {
                    let explained = b.is_some_and(|hit| {
                        is_grid_boundary_drop(&oracle_triangles, 16.0, hit.triangle)
                    });
                    if explained {
                        hull_explained_boundary += 1;
                    } else {
                        hull_mismatches += 1;
                        if hull_mismatches <= 5 {
                            println!(
                                "  hull mismatch: from={from:?} to={to:?} grid={g:?} bvh={b:?}"
                            );
                        }
                    }
                }
            }
        }
        println!(
            "[{mask_name}] hull sweeps: 200000, hits={hull_hits}, mismatches={hull_mismatches}, explained(grid drops boundary-flush triangle)={hull_explained_boundary}, ties={hull_ties} (normal changed: {hull_ties_changed_normal})"
        );
        if hull_mismatches != 0 {
            failures.push(format!(
                "{mask_name} hull sweep BVH/grid mismatch: {hull_mismatches}"
            ));
        }

        let mut ray_hits = 0u64;
        let mut ray_mismatches = 0u64;
        let mut ray_explained_boundary = 0u64;
        let mut ray_ties = 0u64;
        let mut ray_ties_changed_normal = 0u64;
        for _ in 0..100_000 {
            let from = lerp_aabb(&bounds, &mut rng);
            let dir = rng.unit_v3();
            let len = rng.f32(1.0, 3000.0);
            let to = from + dir * len;
            let g = grid.first_hit_ray(from, to);
            let b = bvh.first_hit_ray(from, to);
            if g.is_some() {
                ray_hits += 1;
            }
            match (g, b) {
                (Some(g), Some(b)) if g.t == b.t && g.triangle == b.triangle => {}
                (Some(g), Some(b))
                    if g.t == b.t
                        && confirm_ray_tie(&grid, from, to - from, g.triangle, b.triangle, g.t) =>
                {
                    ray_ties += 1;
                    if g.normal.to_array() != b.normal.to_array() {
                        ray_ties_changed_normal += 1;
                    }
                }
                (None, None) => {}
                (g, b) => {
                    let explained = b.is_some_and(|hit| {
                        is_grid_boundary_drop(&oracle_triangles, 16.0, hit.triangle)
                    });
                    if explained {
                        ray_explained_boundary += 1;
                    } else {
                        ray_mismatches += 1;
                        if ray_mismatches <= 5 {
                            println!(
                                "  ray mismatch: from={from:?} to={to:?} grid={g:?} bvh={b:?}"
                            );
                        }
                    }
                }
            }
        }
        println!(
            "[{mask_name}] long rays: 100000, hits={ray_hits}, mismatches={ray_mismatches}, explained(grid drops boundary-flush triangle)={ray_explained_boundary}, ties={ray_ties} (normal changed: {ray_ties_changed_normal})"
        );
        if ray_mismatches != 0 {
            failures.push(format!(
                "{mask_name} ray BVH/grid mismatch: {ray_mismatches}"
            ));
        }

        // `blocked` over the same kind of random long rays. Shares
        // `moller_trumbore` with `first_hit_ray`, so a mismatch gets the same
        // oracle-adjudicated in-plane-ray tolerance rather than a hard fail.
        let mut blocked_total = 0u64;
        let mut blocked_mismatches = 0u64;
        let mut blocked_unexplained = 0u64;
        let mut blocked_explained_boundary = 0u64;
        for _ in 0..100_000 {
            let from = lerp_aabb(&bounds, &mut rng);
            let dir = rng.unit_v3();
            let len = rng.f32(1.0, 3000.0);
            let to = from + dir * len;
            blocked_total += 1;
            let g = grid.blocked(from, to);
            let b = bvh.blocked(from, to);
            if g == b {
                continue;
            }
            blocked_mismatches += 1;
            let oracle = oracle_hit(&oracle_triangles, from, to, RayWindow::Sightline);
            let direction = to - from;
            let candidate = oracle
                .map(|(_, tri)| tri)
                .or_else(|| grid.first_hit_ray(from, to).map(|h| h.triangle))
                .or_else(|| bvh.first_hit_ray(from, to).map(|h| h.triangle));
            let explained_in_plane = candidate
                .is_some_and(|tri| is_in_plane_mismatch(&oracle_triangles, direction, tri));
            let explained_boundary =
                candidate.is_some_and(|tri| is_grid_boundary_drop(&oracle_triangles, 16.0, tri));
            if explained_boundary {
                blocked_explained_boundary += 1;
            } else if !explained_in_plane {
                blocked_unexplained += 1;
                if blocked_unexplained <= 5 {
                    println!(
                        "  unexplained blocked mismatch: from={from:?} to={to:?} grid={g} bvh={b}"
                    );
                }
            }
        }
        println!(
            "[{mask_name}] blocked: {blocked_total}, mismatches={blocked_mismatches}, unexplained={blocked_unexplained}, explained(grid drops boundary-flush triangle)={blocked_explained_boundary}"
        );
        if blocked_unexplained != 0 {
            failures.push(format!(
                "{mask_name} blocked BVH/grid unexplained mismatch: {blocked_unexplained}/{blocked_mismatches}"
            ));
        }

        // `box_intersects`: exact SAT test per triangle on both sides, no
        // ray-in-plane ambiguity possible, so this must match exactly.
        let mut box_total = 0u64;
        let mut box_mismatches = 0u64;
        let mut box_explained_boundary = 0u64;
        for _ in 0..50_000 {
            let center = lerp_aabb(&bounds, &mut rng);
            let half = V3::new(rng.f32(0.5, 5.0), rng.f32(0.5, 5.0), rng.f32(0.5, 5.0));
            box_total += 1;
            let g = grid.box_intersects(center, half);
            let b = bvh.box_intersects(center, half);
            if g != b {
                let overlapping = oracle_box_overlap_triangles(&oracle_triangles, center, half);
                let explained = !overlapping.is_empty()
                    && overlapping
                        .iter()
                        .all(|&tri| is_grid_boundary_drop(&oracle_triangles, 16.0, tri));
                if explained {
                    box_explained_boundary += 1;
                } else {
                    box_mismatches += 1;
                    if box_mismatches <= 5 {
                        println!(
                            "  box_intersects mismatch: center={center:?} half={half:?} grid={g} bvh={b}"
                        );
                    }
                }
            }
        }
        println!(
            "[{mask_name}] box_intersects: {box_total}, mismatches={box_mismatches}, explained(grid drops boundary-flush triangle)={box_explained_boundary}"
        );
        if box_mismatches != 0 {
            failures.push(format!(
                "{mask_name} box_intersects BVH/grid mismatch: {box_mismatches}"
            ));
        }

        // Seam probes: rays through triangle vertices/edge midpoints
        // (vertical and random directions) - the case the BVH's node-bound
        // `PAD` fix targets. Rays are collected up front so a rayon
        // brute-force oracle (no acceleration structure at all) can
        // adjudicate every BVH-vs-grid mismatch independently: a BVH
        // mismatch is only acceptable when the oracle's own candidate
        // triangle is a documented in-plane-ray case (`bvh.rs` module doc).
        // The grid-vs-oracle rate is reported for visibility but not
        // asserted, since the grid's own reference-exact cell-corner misses
        // are a separate, already-documented class (`collider.rs`).
        let seam_rays: Vec<(V3, V3)> = (0..3000)
            .flat_map(|_| {
                let ti = rng.next_u32() as usize % mesh.triangles.len();
                let tri = mesh.triangles[ti];
                let a = V3::from_array(mesh.vertices[tri[0] as usize]);
                let b = V3::from_array(mesh.vertices[tri[1] as usize]);
                let c = V3::from_array(mesh.vertices[tri[2] as usize]);
                let candidates = [a, b, c, (a + b) * 0.5, (b + c) * 0.5, (c + a) * 0.5];
                let p = candidates[rng.next_u32() as usize % candidates.len()];
                [V3::new(0.0, 0.0, 1.0), rng.unit_v3()]
                    .into_iter()
                    .map(move |d| (p + d * 2000.0, p - d * 2000.0))
            })
            .collect();
        let seam_total = seam_rays.len() as u64;

        type SeamResult = (Option<RayHit>, Option<RayHit>, Option<(f32, u32)>);
        let seam_results: Vec<SeamResult> = seam_rays
            .par_iter()
            .map(|&(from, to)| {
                let g = grid.first_hit_ray(from, to);
                let b = bvh.first_hit_ray(from, to);
                let o = oracle_hit(&oracle_triangles, from, to, RayWindow::Physics);
                (g, b, o)
            })
            .collect();

        let mut seam_grid_vs_oracle_mismatches = 0u64;
        let mut seam_bvh_unexplained = 0u64;
        for (&(from, to), (g, b, o)) in seam_rays.iter().zip(&seam_results) {
            let direction = to - from;
            let grid_ok = match (*g, *o) {
                (Some(g), Some(o)) => g.t == o.0 && g.triangle == o.1,
                (None, None) => true,
                _ => false,
            };
            if !grid_ok {
                seam_grid_vs_oracle_mismatches += 1;
            }
            let bvh_ok = match (*b, *o) {
                (Some(b), Some(o)) => b.t == o.0 && b.triangle == o.1,
                (None, None) => true,
                _ => false,
            };
            if !bvh_ok {
                let candidate = o.map(|(_, tri)| tri).or_else(|| b.map(|h| h.triangle));
                let explained = candidate
                    .is_some_and(|tri| is_in_plane_mismatch(&oracle_triangles, direction, tri));
                if !explained {
                    seam_bvh_unexplained += 1;
                    if seam_bvh_unexplained <= 5 {
                        println!(
                            "  unexplained bvh/oracle seam mismatch: from={from:?} to={to:?} bvh={b:?} oracle={o:?}"
                        );
                    }
                }
            }
        }
        println!(
            "[{mask_name}] seam probes (rays): {seam_total}, grid-vs-oracle mismatches={seam_grid_vs_oracle_mismatches} (not asserted), bvh-vs-oracle unexplained={seam_bvh_unexplained}"
        );
        if seam_bvh_unexplained != 0 {
            failures.push(format!(
                "{mask_name} seam probe BVH/oracle unexplained mismatch: {seam_bvh_unexplained}/{seam_total}"
            ));
        }

        // Hull sweeps starting exactly on a surface, offset a hair along its
        // own normal on either side. No in-plane ambiguity here (that's a
        // `moller_trumbore` ray property), so this must be exact.
        let mut surf_total = 0u64;
        let mut surf_mismatches = 0u64;
        let mut surf_explained_boundary = 0u64;
        for _ in 0..1000 {
            let ti = rng.next_u32() as usize % mesh.triangles.len();
            let tri = mesh.triangles[ti];
            let a = V3::from_array(mesh.vertices[tri[0] as usize]);
            let b = V3::from_array(mesh.vertices[tri[1] as usize]);
            let c = V3::from_array(mesh.vertices[tri[2] as usize]);
            let centroid = (a + b + c) / 3.0;
            let raw_normal = (b - a).cross(c - a);
            let len2 = raw_normal.length_squared();
            if len2 < 1e-12 {
                continue;
            }
            let normal = raw_normal / len2.sqrt();
            let sign = if rng.next_u32() & 1 == 0 { 1.0 } else { -1.0 };
            let start = centroid + normal * (sign * 0.05);
            let to = start + normal * (-sign * 4.0);
            let half = V3::new(2.0, 2.0, 2.0);
            surf_total += 1;
            let g = grid.first_hit_hull(start, to, half, -2.0, None);
            let b2 = bvh.first_hit_hull(start, to, half, -2.0, None);
            let ok = match (g, b2) {
                (Some(g), Some(b2)) if g.t == b2.t && g.triangle == b2.triangle => true,
                (Some(g), Some(b2))
                    if g.t == b2.t
                        && confirm_hull_tie(
                            &grid,
                            start,
                            to - start,
                            half,
                            g.triangle,
                            b2.triangle,
                            g.t,
                        ) =>
                {
                    true
                }
                (None, None) => true,
                _ => false,
            };
            if !ok {
                let explained = b2.is_some_and(|hit| {
                    is_grid_boundary_drop(&oracle_triangles, 16.0, hit.triangle)
                });
                if explained {
                    surf_explained_boundary += 1;
                } else {
                    surf_mismatches += 1;
                }
            }
        }
        println!(
            "[{mask_name}] surface-start hull sweeps: {surf_total}, mismatches={surf_mismatches}, explained(grid drops boundary-flush triangle)={surf_explained_boundary}"
        );
        if surf_mismatches != 0 {
            failures.push(format!(
                "{mask_name} surface-start hull sweep mismatches: {surf_mismatches}/{surf_total}"
            ));
        }
    }

    // ---- 2. nav-on-floor ----
    // Tiny nav fans over bumpy/rocky terrain can have corner z varying by
    // tens of units over a ~30u fan (the nav mesh's per-area flat-triangle
    // approximation of genuinely uneven ground), so instead of a single
    // straight-down probe from the point's own z, search for the *nearest*
    // walkable surface (normal.z > 0.7) within +-48u, trying both down and
    // up, and grade by sv_stepsize (18u) rather than a flat 8u.
    let player = player_mask(&mesh);
    let player_bvh = Bvh::build(&mesh, &player, None).unwrap();
    let collider: &dyn Collider = &player_bvh;

    const VERTICAL_WINDOW: f32 = 48.0;
    const STEP_SIZE: f32 = 18.0; // sv_stepsize

    /// Walks along the ray from `from` to `to`, skipping non-walkable hits
    /// (`normal.z <= 0.7`) up to 32 times, and returns the world Z of the
    /// first walkable hit encountered along that direction (i.e. the
    /// nearest one from `from`).
    fn nearest_walkable_hit_z(collider: &dyn Collider, from: V3, to: V3) -> Option<f32> {
        let mut cur_from = from;
        for _ in 0..32 {
            let hit = collider.first_hit_ray(cur_from, to)?;
            let dir = to - cur_from;
            let hit_point = cur_from + dir * hit.t;
            if hit.normal.z > 0.7 {
                return Some(hit_point.z);
            }
            let step = dir.normalize() * 1e-3;
            cur_from = hit_point + step;
        }
        None
    }

    fn nearest_walkable_dz(collider: &dyn Collider, p: V3) -> Option<f32> {
        let down = nearest_walkable_hit_z(
            collider,
            V3::new(p.x, p.y, p.z + VERTICAL_WINDOW),
            V3::new(p.x, p.y, p.z - VERTICAL_WINDOW),
        );
        let up = nearest_walkable_hit_z(
            collider,
            V3::new(p.x, p.y, p.z - VERTICAL_WINDOW),
            V3::new(p.x, p.y, p.z + VERTICAL_WINDOW),
        );
        match (down, up) {
            (Some(d), Some(u)) => Some((d - p.z).abs().min((u - p.z).abs())),
            (Some(d), None) => Some((d - p.z).abs()),
            (None, Some(u)) => Some((u - p.z).abs()),
            (None, None) => None,
        }
    }

    // (0-2, 2-4, 4-8, 8-12, 12-18, 18-32, >32, no-hit)
    const BUCKET_EDGES: [f32; 6] = [2.0, 4.0, 8.0, 12.0, 18.0, 32.0];
    const BUCKET_LABELS: [&str; 8] = [
        "0-2", "2-4", "4-8", "8-12", "12-18", "18-32", ">32", "no-hit",
    ];
    fn bucket_of(dz: Option<f32>) -> usize {
        match dz {
            None => 7,
            Some(dz) => BUCKET_EDGES
                .iter()
                .position(|&edge| dz <= edge)
                .unwrap_or(6),
        }
    }

    // An all-attributes collider, for rules 1 and 2 below (they need to see
    // surfaces the player-mask collider deliberately excludes).
    let all = all_mask(&mesh);
    let all_bvh = Bvh::build(&mesh, &all, None).unwrap();
    let all_collider: &dyn Collider = &all_bvh;

    /// Nearest hit (either direction) within `window` of `p`'s Z, ignoring
    /// normal - used by rules 1/2 which need to see *any* surface, not just
    /// walkable ones.
    fn nearest_hit_any(collider: &dyn Collider, p: V3, window: f32) -> Option<(f32, RayHit)> {
        let mut best: Option<(f32, RayHit)> = None;
        for (from, to) in [
            (
                V3::new(p.x, p.y, p.z + window),
                V3::new(p.x, p.y, p.z - window),
            ),
            (
                V3::new(p.x, p.y, p.z - window),
                V3::new(p.x, p.y, p.z + window),
            ),
        ] {
            if let Some(hit) = collider.first_hit_ray(from, to) {
                let hit_z = from.z + hit.t * (to.z - from.z);
                let dz = (hit_z - p.z).abs();
                if dz <= window && best.as_ref().is_none_or(|(bd, _)| dz < *bd) {
                    best = Some((dz, hit));
                }
            }
        }
        best
    }

    // `entities.json` (classname, model) -> origins, for skipped-entity
    // matching below (`SkippedEntity` itself has no origin field).
    let skipped_entity_origins: Vec<V3> = skipped_entities
        .iter()
        .filter(|s| s.reason != "class not in allowlist")
        .flat_map(|s| {
            entities
                .iter()
                .filter(|e| {
                    e.classname == s.classname && e.model.as_deref() == Some(s.model.as_str())
                })
                .map(|e| V3::from_array(e.origin))
        })
        .collect();

    // A failing point (no-hit, or dz > sv_stepsize) may be explained by
    // known collision-mesh/nav-generation policy rather than a real
    // geometry/query defect. Checked in order:
    // 1. "non-player-solid surface": a surface exists within STEP_SIZE of
    //    the nav Z whose attribute is not player-solid (e.g. mirage's
    //    "Default" attribute excludes `player` on some thin slabs) - nav
    //    generation doesn't consult `interact_exclude`, so it can place
    //    walkable-looking areas on a surface players actually pass through.
    // 2. "steep surface": a surface exists within STEP_SIZE with
    //    0 < normal.z < 0.7 (upward-ish but too steep to stand on) and no
    //    walkable one - nav generation's own slope tolerance can be looser
    //    than 0.7. `normal.z > 0` (not `abs(normal.z) > 0`) on purpose: an
    //    overhang/ceiling normal (negative z) isn't "steep", it's the wrong
    //    side of a surface entirely, and shouldn't be conflated here.
    // 3. "physics prop": `prop_physics*` entities are deliberately excluded
    //    from the collision mesh (reference policy); nav generation can
    //    place areas on top of one. Requires vertical proximity too
    //    (`|p.z - origin.z| <= 64`), not just "somewhere above it forever",
    //    so a prop far below in a pit doesn't explain away an unrelated
    //    point 500u above it.
    // 4. "skipped entity": within 128u horizontally *and* 64u vertically of
    //    the origin of an entity whose `(classname, model)` was skipped by
    //    the extractor (`report.json`'s `skipped_entities`, excluding the
    //    blanket "class not in allowlist" reason) - e.g. retake-only
    //    brushes.
    fn classify_failure(
        all_collider: &dyn Collider,
        mesh: &CollisionMesh,
        player: &AttributeMask,
        entities: &[EntityRecord],
        skipped_entity_origins: &[V3],
        p: V3,
    ) -> Option<&'static str> {
        if let Some((_, hit)) = nearest_hit_any(all_collider, p, STEP_SIZE) {
            let attr = mesh.tri_attribute[hit.triangle as usize];
            if !player.is_solid(attr) {
                return Some("non-player-solid surface");
            }
            if hit.normal.z > 0.0 && hit.normal.z < 0.7 {
                return Some("steep surface");
            }
        }
        for e in entities {
            if e.classname.starts_with("prop_physics") {
                let origin = V3::from_array(e.origin);
                let dx = p.x - origin.x;
                let dy = p.y - origin.y;
                let horiz = (dx * dx + dy * dy).sqrt();
                // The nav point sits on top of the prop: within 64u
                // horizontally, above the prop's own origin height, and
                // within 64u vertically of it.
                if horiz <= 64.0 && p.z > origin.z && (p.z - origin.z).abs() <= 64.0 {
                    return Some("physics prop");
                }
            }
        }
        if skipped_entity_origins.iter().any(|origin| {
            let dx = p.x - origin.x;
            let dy = p.y - origin.y;
            (dx * dx + dy * dy).sqrt() <= 128.0 && (p.z - origin.z).abs() <= 64.0
        }) {
            return Some("skipped entity");
        }
        None
    }

    // The spec's original methodology (single straight-down ray from
    // centroid/corner Z + 48 to Z - 48, must hit normal.z > 0.7 within 8u),
    // reported alongside for comparison, not used for any assertion.
    fn original_single_ray_down_ok(collider: &dyn Collider, p: V3) -> bool {
        let from = V3::new(p.x, p.y, p.z + VERTICAL_WINDOW);
        let to = V3::new(p.x, p.y, p.z - VERTICAL_WINDOW);
        match collider.first_hit_ray(from, to) {
            Some(h) if h.normal.z > 0.7 => {
                let hit_z = from.z + h.t * (to.z - from.z);
                (hit_z - p.z).abs() <= 8.0
            }
            _ => false,
        }
    }

    let mut checked = 0u64;
    let mut no_hit_count = 0u64;
    let mut within_step = 0u64;
    let mut centroid_hist = [0u64; 8];
    let mut corner_hist = [0u64; 8];
    let mut explained_non_player_solid = 0u64;
    let mut explained_steep = 0u64;
    let mut explained_physics_prop = 0u64;
    let mut explained_skipped_entity = 0u64;
    let mut unexplained_no_hit: Vec<(u32, V3)> = Vec::new();
    let mut unexplained_over_step: Vec<(u32, V3, f32)> = Vec::new();
    let mut original_method_ok = 0u64;
    let mut areas_total = 0u64;
    let mut areas_all_points_ok = 0u64;
    for area in nav.areas.iter().filter(|a| a.hull_index == 0) {
        let n = area.corners.len() as f32;
        let centroid = area
            .corners
            .iter()
            .fold(V3::ZERO, |acc, c| acc + V3::from_array(*c))
            / n;
        let mut points = vec![(true, centroid)];
        for c in &area.corners {
            let c = V3::from_array(*c);
            let to_centroid = centroid - c;
            let dist = to_centroid.length();
            if dist > 1e-6 {
                points.push((false, c + to_centroid / dist * 2.0));
            }
        }
        areas_total += 1;
        let mut area_all_ok = true;
        for (is_centroid, p) in points {
            checked += 1;
            if original_single_ray_down_ok(collider, p) {
                original_method_ok += 1;
            }
            let dz = nearest_walkable_dz(collider, p);
            let bucket = bucket_of(dz);
            if is_centroid {
                centroid_hist[bucket] += 1;
            } else {
                corner_hist[bucket] += 1;
            }
            let is_failing = match dz {
                None => true,
                Some(dz) => dz > STEP_SIZE,
            };
            if !is_failing {
                within_step += 1;
                continue;
            }
            if dz.is_none() {
                no_hit_count += 1;
            }
            let explanation = classify_failure(
                all_collider,
                &mesh,
                &player,
                &entities,
                &skipped_entity_origins,
                p,
            );
            match explanation {
                Some("non-player-solid surface") => explained_non_player_solid += 1,
                Some("steep surface") => explained_steep += 1,
                Some("physics prop") => explained_physics_prop += 1,
                Some("skipped entity") => explained_skipped_entity += 1,
                Some(_) | None => {
                    area_all_ok = false;
                    match dz {
                        None => unexplained_no_hit.push((area.id, p)),
                        Some(dz) => unexplained_over_step.push((area.id, p, dz)),
                    }
                }
            }
        }
        if area_all_ok {
            areas_all_points_ok += 1;
        }
    }
    println!("nav-on-floor: checked={checked}");
    println!(
        "nav-on-floor original methodology (single ray down, dz<=8u, not asserted): {original_method_ok}/{checked} ({:.4}%)",
        100.0 * original_method_ok as f64 / checked as f64
    );
    println!(
        "nav-on-floor per-area (all points unexplained-within-18u): {areas_all_points_ok}/{areas_total} ({:.4}%)",
        100.0 * areas_all_points_ok as f64 / areas_total as f64
    );
    println!("nav-on-floor histogram (centroids): {BUCKET_LABELS:?} = {centroid_hist:?}");
    println!("nav-on-floor histogram (corners):   {BUCKET_LABELS:?} = {corner_hist:?}");
    let no_hit_rate = 100.0 * no_hit_count as f64 / checked as f64;
    let unexplained_no_hit_rate = 100.0 * unexplained_no_hit.len() as f64 / checked as f64;
    let unexplained_total = unexplained_no_hit.len() as u64 + unexplained_over_step.len() as u64;
    let unexplained_within_step_rate =
        100.0 * (checked - unexplained_total) as f64 / checked as f64;
    println!(
        "nav-on-floor classes: within_step={within_step} explained(non-player-solid surface)={explained_non_player_solid} explained(steep surface)={explained_steep} explained(physics prop)={explained_physics_prop} explained(skipped entity)={explained_skipped_entity} unexplained_no_hit={} unexplained_over_step={} (raw no_hit={no_hit_count})",
        unexplained_no_hit.len(),
        unexplained_over_step.len()
    );
    println!(
        "nav-on-floor rates: no_hit={no_hit_rate:.4}% unexplained_no_hit={unexplained_no_hit_rate:.4}% unexplained_within_step={unexplained_within_step_rate:.4}%"
    );
    println!(
        "nav-on-floor unexplained no-hit points ({}):",
        unexplained_no_hit.len()
    );
    for (id, p) in &unexplained_no_hit {
        println!("  area={id} point={p:?}");
    }
    unexplained_over_step.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    println!(
        "nav-on-floor unexplained areas exceeding sv_stepsize (18u), worst 30 of {}:",
        unexplained_over_step.len()
    );
    for (id, p, dz) in unexplained_over_step.iter().take(30) {
        println!("  area={id} point={p:?} dz={dz}");
    }
    if unexplained_no_hit_rate > 0.5 {
        failures.push(format!(
            "nav-on-floor unexplained no-hit rate above 0.5%: {}/{checked} ({unexplained_no_hit_rate:.4}%)",
            unexplained_no_hit.len()
        ));
    }
    if unexplained_within_step_rate < 99.0 {
        failures.push(format!(
            "nav-on-floor unexplained within-sv_stepsize rate below 99%: {}/{checked} ({unexplained_within_step_rate:.4}%)",
            checked - unexplained_total
        ));
    }

    // ---- 3. spawns ----
    let mut spawn_count = 0u64;
    let mut spawn_misses = 0u64;
    for e in entities.iter().filter(|e| {
        e.classname == "info_player_terrorist" || e.classname == "info_player_counterterrorist"
    }) {
        spawn_count += 1;
        let origin = V3::from_array(e.origin);
        let to = origin - V3::new(0.0, 0.0, 256.0);
        let hit = player_bvh.first_hit_ray(origin, to);
        match hit {
            Some(h) if h.normal.z > 0.7 => {
                println!(
                    "  spawn {} at {:?}: floor at distance {:.2}",
                    e.classname,
                    origin.to_array(),
                    h.t * 256.0
                );
            }
            other => {
                spawn_misses += 1;
                println!(
                    "  spawn {} at {:?}: NO FLOOR HIT (normal.z>0.7) - got {other:?}",
                    e.classname,
                    origin.to_array()
                );
            }
        }
    }
    println!("spawns checked: {spawn_count}, misses: {spawn_misses}");
    if spawn_misses > 0 {
        failures.push(format!(
            "{spawn_misses}/{spawn_count} spawns have no floor within 256u (normal.z>0.7)"
        ));
    }

    // ---- 4. timing report ----
    let all = geom::filter::all_mask(&mesh);
    let t0 = Instant::now();
    let grid_all = UniformGrid::build(&mesh, &all, None, 16.0).unwrap();
    let grid_build = t0.elapsed();
    let t0 = Instant::now();
    let bvh_all = Bvh::build(&mesh, &all, None).unwrap();
    let bvh_build = t0.elapsed();
    let t0 = Instant::now();
    let voxel_all = VoxelGrid::build(&mesh, &all, 16.0, bounds).unwrap();
    let voxel_build = t0.elapsed();
    println!(
        "build times: grid(16u)={grid_build:?} bvh={bvh_build:?} voxel(16u)={voxel_build:?} (all-mask, triangles={})",
        grid_all.triangle_count()
    );
    println!(
        "voxel: {}x{}x{} cells, solid={}",
        voxel_all.nx,
        voxel_all.ny,
        voxel_all.nz,
        voxel_all.solid_count()
    );

    let mut rng = Rng::new(0x1234_5678);
    let n_queries = 200_000usize;
    let hull_queries: Vec<(V3, V3)> = (0..n_queries)
        .map(|_| {
            let from = lerp_aabb(&bounds, &mut rng);
            let to = from + rng.unit_v3() * rng.f32(1.0, 5.5);
            (from, to)
        })
        .collect();
    let ray_queries: Vec<(V3, V3)> = (0..n_queries)
        .map(|_| {
            let from = lerp_aabb(&bounds, &mut rng);
            let to = from + rng.unit_v3() * rng.f32(1.0, 2000.0);
            (from, to)
        })
        .collect();

    let colliders: [(&str, &dyn Collider); 2] = [
        ("grid", &grid_all as &dyn Collider),
        ("bvh", &bvh_all as &dyn Collider),
    ];
    for (name, collider) in colliders {
        let half = V3::new(2.0, 2.0, 2.0);
        let t0 = Instant::now();
        let hits: u64 = hull_queries
            .iter()
            .filter(|(f, t)| collider.first_hit_hull(*f, *t, half, -2.0, None).is_some())
            .count() as u64;
        let single_elapsed = t0.elapsed();
        let t0 = Instant::now();
        let phits: u64 = hull_queries
            .par_iter()
            .filter(|(f, t)| collider.first_hit_hull(*f, *t, half, -2.0, None).is_some())
            .count() as u64;
        let par_elapsed = t0.elapsed();
        println!(
            "[{name}] hull sweep: single={:.0} q/s ({hits} hits) parallel={:.0} q/s ({phits} hits)",
            n_queries as f64 / single_elapsed.as_secs_f64(),
            n_queries as f64 / par_elapsed.as_secs_f64()
        );

        let t0 = Instant::now();
        let rhits: u64 = ray_queries
            .iter()
            .filter(|(f, t)| collider.first_hit_ray(*f, *t).is_some())
            .count() as u64;
        let single_elapsed = t0.elapsed();
        let t0 = Instant::now();
        let prhits: u64 = ray_queries
            .par_iter()
            .filter(|(f, t)| collider.first_hit_ray(*f, *t).is_some())
            .count() as u64;
        let par_elapsed = t0.elapsed();
        println!(
            "[{name}] long ray: single={:.0} q/s ({rhits} hits) parallel={:.0} q/s ({prhits} hits)",
            n_queries as f64 / single_elapsed.as_secs_f64(),
            n_queries as f64 / par_elapsed.as_secs_f64()
        );

        let t0 = Instant::now();
        let bhits: u64 = ray_queries
            .iter()
            .filter(|(f, t)| collider.blocked(*f, *t))
            .count() as u64;
        let single_elapsed = t0.elapsed();
        println!(
            "[{name}] blocked (sightline reuse): single={:.0} q/s ({bhits} blocked)",
            n_queries as f64 / single_elapsed.as_secs_f64()
        );

        let box_half = V3::new(16.0, 16.0, 36.0);
        let t0 = Instant::now();
        let boxhits: u64 = hull_queries
            .iter()
            .filter(|(f, _)| collider.box_intersects(*f, box_half))
            .count() as u64;
        let single_elapsed = t0.elapsed();
        println!(
            "[{name}] box_intersects (player hull): single={:.0} q/s ({boxhits} hits)",
            n_queries as f64 / single_elapsed.as_secs_f64()
        );
    }

    assert!(
        failures.is_empty(),
        "real-mesh checks failed:\n{}",
        failures.join("\n")
    );
}

/// Three seam rays on the real mirage mesh (grenade mask), pinned to fixed
/// coordinates so a regression in either `UniformGrid` or `Bvh` is caught
/// even if `real_mesh_bvh_grid_differential_and_reports`'s random sampling
/// happens not to hit them.
#[test]
#[ignore = "needs CS2MOD_CGEO"]
fn pinned_seam_rays_grenade_mask() {
    let Loaded { mesh, .. } = load();
    let mask = grenade_mask(&mesh);
    let grid = UniformGrid::build(&mesh, &mask, None, 16.0).unwrap();
    let bvh = Bvh::build(&mesh, &mask, None).unwrap();

    // A: t ~= 0.84181774, triangle 8713.
    let a_from = V3::new(-1142.4836, 173.99998, -300.2086);
    let a_to = V3::new(-1142.4836, 173.99998, -351.6444);
    let g = grid.first_hit_ray(a_from, a_to).expect("A: grid must hit");
    let b = bvh.first_hit_ray(a_from, a_to).expect("A: bvh must hit");
    assert_eq!(g.t, b.t, "A: grid/bvh t must match exactly");
    assert_eq!(g.triangle, b.triangle, "A: grid/bvh triangle must match");
    assert!((g.t - 0.841_817_74).abs() < 1e-4, "A: t={}", g.t);
    assert_eq!(g.triangle, 8713, "A: triangle={}", g.triangle);

    // B: blocked true, triangle 102915.
    let b_from = V3::new(626.30096, -1462.3456, 7.9390383);
    let b_to = V3::new(569.69904, -1405.6544, 20.060963);
    assert!(grid.blocked(b_from, b_to), "B: grid must be blocked");
    assert!(bvh.blocked(b_from, b_to), "B: bvh must be blocked");
    let g = grid.first_hit_ray(b_from, b_to).expect("B: grid must hit");
    let b2 = bvh.first_hit_ray(b_from, b_to).expect("B: bvh must hit");
    assert_eq!(g.triangle, b2.triangle, "B: grid/bvh triangle must match");
    assert_eq!(g.triangle, 102915, "B: triangle={}", g.triangle);

    // C: t=0.5, triangle 122895.
    let c_from = V3::new(1144.5874, 510.35806, -65.11733);
    let c_to = V3::new(1155.4126, 541.6419, -78.88267);
    let g = grid.first_hit_ray(c_from, c_to).expect("C: grid must hit");
    let b2 = bvh.first_hit_ray(c_from, c_to).expect("C: bvh must hit");
    assert_eq!(g.t, b2.t, "C: grid/bvh t must match exactly");
    assert_eq!(g.triangle, b2.triangle, "C: grid/bvh triangle must match");
    assert_eq!(g.t, 0.5, "C: t={}", g.t);
    assert_eq!(g.triangle, 122895, "C: triangle={}", g.triangle);
}
