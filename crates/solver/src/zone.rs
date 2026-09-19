//! Stage 1 of the inverse solver: the set of grenade landing cells whose
//! smoke seals every given sightline at once, plus the point-target zone a
//! two-click query uses instead. Ported from
//! `cs2-smoke-solver/src/Solver/LandingZoneSolver.cs` and the zone-building
//! excerpt of `cs2-smoke-solver/src/Cli/Services/TargetSolver.cs:210-231`.

use std::collections::HashMap;

use geom::collider::Collider;
use geom::math::V3;
use geom::voxel::VoxelGrid;
use rayon::prelude::*;
use sim::{SmokeParams, occlusion, smoke_fill};

/// `LandingZoneSolver.cs:11-34` (`SightlineSpec`). Eye jitter samples
/// parallel rays around both endpoints so a zone cell must cover the whole
/// lane, not just the exact center ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SightlineSpec {
    pub eye_a: V3,
    pub eye_b: V3,
    pub eye_jitter: f32,
}

impl SightlineSpec {
    pub fn new(eye_a: V3, eye_b: V3) -> Self {
        SightlineSpec {
            eye_a,
            eye_b,
            eye_jitter: 16.0,
        }
    }

    /// `LandingZoneSolver.cs:13-33` (`BuildEyePairs`).
    pub fn build_eye_pairs(&self) -> Vec<(V3, V3)> {
        let direction = self.eye_b - self.eye_a;
        let mut horizontal = direction.cross(V3::new(0.0, 0.0, 1.0));
        if horizontal.length_squared() < 1e-6 {
            horizontal = V3::new(1.0, 0.0, 0.0);
        }
        horizontal = horizontal.normalize();

        let offsets = [-self.eye_jitter, 0.0, self.eye_jitter];
        let mut pairs = Vec::with_capacity(9);
        for &offset_a in &offsets {
            for &offset_b in &offsets {
                pairs.push((
                    self.eye_a + horizontal * offset_a,
                    self.eye_b + horizontal * offset_b,
                ));
            }
        }
        pairs
    }
}

/// `LandingZoneSolver.cs:36` (`LandingCell`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LandingCell {
    pub center: V3,
    pub min_crossings: u32,
}

/// `LandingZoneSolver.cs:38` (`SolveResult`).
#[derive(Debug, Clone)]
pub struct SolveResult {
    pub zone: Vec<LandingCell>,
    pub clear_pairs: usize,
    pub total_pairs: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ZoneError {
    #[error(
        "every jittered ray of every sightline is blocked by geometry; the sightline definitions are wrong"
    )]
    EveryRayBlocked,
}

/// `LandingZoneSolver.cs:47-89` (`Solve`). `raycaster` answers the
/// exact-triangle occlusion query (`TriangleRaycaster.Blocked`); a
/// `UniformGrid`/`Bvh` over the same mesh works.
pub fn solve<C: Collider>(
    grid: &VoxelGrid,
    raycaster: &C,
    sightlines: &[SightlineSpec],
    p: &SmokeParams,
    min_smoke_cells: u32,
) -> Result<SolveResult, ZoneError> {
    let all_pairs: Vec<(V3, V3)> = sightlines
        .iter()
        .flat_map(|s| s.build_eye_pairs())
        .collect();
    let pairs: Vec<(V3, V3)> = all_pairs
        .iter()
        .copied()
        .filter(|(a, b)| !raycaster.blocked(*a, *b))
        .collect();
    if pairs.is_empty() {
        return Err(ZoneError::EveryRayBlocked);
    }
    let candidates = find_candidate_rest_cells(grid, sightlines, p);

    let zone: Vec<LandingCell> = candidates
        .into_par_iter()
        .filter_map(|cell| {
            let smoke = smoke_fill(grid, grid.cell_center(cell), p).ok()?;
            if smoke.cells.is_empty() {
                return None;
            }
            let mut min_crossings = u32::MAX;
            for &(a, b) in &pairs {
                let result = occlusion(&smoke, grid, a, b);
                min_crossings = min_crossings.min(result.smoke_cells_crossed);
                if result.smoke_cells_crossed < min_smoke_cells {
                    return None;
                }
            }
            Some(LandingCell {
                center: grid.cell_center(cell),
                min_crossings,
            })
        })
        .collect();
    let mut zone = zone;
    zone.sort_by_key(|c| std::cmp::Reverse(c.min_crossings));

    Ok(SolveResult {
        zone,
        clear_pairs: pairs.len(),
        total_pairs: all_pairs.len(),
    })
}

/// `LandingZoneSolver.cs:95-127` (`FindCandidateRestCells`): a grenade can
/// only rest on top of solid geometry, and its smoke can only reach a
/// sightline if the rest cell is within the fill radius of one of the
/// segments.
fn find_candidate_rest_cells(
    grid: &VoxelGrid,
    sightlines: &[SightlineSpec],
    p: &SmokeParams,
) -> Vec<usize> {
    let half_diagonal = grid.voxel_size() * 0.87;
    let max_distance = p.max_radius * p.contained_stretch + half_diagonal;
    let mut candidates = Vec::new();
    for z in 1..grid.nz {
        for y in 0..grid.ny {
            for x in 0..grid.nx {
                let index = grid.index(x, y, z);
                if grid.is_solid(index) || !grid.is_solid(grid.index(x, y, z - 1)) {
                    continue;
                }
                let center = grid.cell_center(index);
                for s in sightlines {
                    if distance_to_segment(center, s.eye_a, s.eye_b) <= max_distance {
                        candidates.push(index);
                        break;
                    }
                }
            }
        }
    }
    candidates
}

/// `LandingZoneSolver.cs:129-134` (`DistanceToSegment`).
fn distance_to_segment(p: V3, a: V3, b: V3) -> f32 {
    let ab = b - a;
    let denom = ab.length_squared();
    let t = if denom > 0.0 {
        ((p - a).dot(ab) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p - (a + ab * t)).length()
}

/// `TargetSolver.cs:210-231`: the zone for a two-click (point-target) query
/// is simply resting close enough to `target`. Cells are keyed by voxel-grid
/// index; the value is always `1` (the reference's `zoneCrossings[..] = 1`).
/// Deliberately literal: unlike `LandingZoneSolver`'s smoke-sealed zone, the
/// reference does NOT additionally require the below-neighbour to be solid
/// here (that's `FindCandidateRestCells`'s rule, a different zone); a cell
/// only has to be open and within `tolerance + voxelSize` of the target.
///
/// Returned in insertion order, like the reference's own
/// `Dictionary<int, int>` (which enumerates in insertion order absent
/// removals, and none happen here): callers that iterate the whole zone -
/// centroid sums in `sweep`/`verify` - must use this order, not an
/// unordered map, to match the reference bit-for-bit. Use
/// [`zone_lookup`] for point membership queries.
pub fn point_target_zone(grid: &VoxelGrid, target: V3, tolerance: f32) -> Vec<(usize, i32)> {
    let voxel_size = grid.voxel_size();
    let mut seen: HashMap<usize, i32> = HashMap::new();
    let mut zone_crossings: Vec<(usize, i32)> = Vec::new();
    let cell_range = (tolerance / voxel_size).ceil() as i32;
    let (cx, cy, cz) = grid.cell_of(target);
    for dz in -1..=cell_range {
        for dy in -cell_range..=cell_range {
            for dx in -cell_range..=cell_range {
                let (x, y, z) = (cx + dx, cy + dy, cz + dz);
                if !grid.in_bounds(x, y, z) {
                    continue;
                }
                let index = grid.index(x, y, z);
                if grid.is_solid(index) {
                    continue;
                }
                if (grid.cell_center(index) - target).length() <= tolerance + voxel_size
                    && seen.insert(index, 1).is_none()
                {
                    zone_crossings.push((index, 1));
                }
            }
        }
    }
    zone_crossings
}

/// A `Dictionary<int, int>`-equivalent lookup over an insertion-ordered zone
/// (from [`point_target_zone`] or `LandingZoneSolver::solve`'s zone), for
/// point membership queries that don't care about order.
pub fn zone_lookup(zone: &[(usize, i32)]) -> HashMap<usize, i32> {
    zone.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_eye_pairs_produces_nine_jittered_pairs() {
        let s = SightlineSpec::new(V3::new(0.0, 0.0, 0.0), V3::new(100.0, 0.0, 0.0));
        let pairs = s.build_eye_pairs();
        assert_eq!(pairs.len(), 9);
        // Center pair is unjittered.
        assert!(pairs.contains(&(V3::new(0.0, 0.0, 0.0), V3::new(100.0, 0.0, 0.0))));
    }

    #[test]
    fn distance_to_segment_matches_hand_computed() {
        // Point directly "above" the segment midpoint.
        let d = distance_to_segment(
            V3::new(50.0, 10.0, 0.0),
            V3::new(0.0, 0.0, 0.0),
            V3::new(100.0, 0.0, 0.0),
        );
        assert!((d - 10.0).abs() < 1e-4);
        // Point past the segment end clamps to the endpoint distance.
        let d = distance_to_segment(
            V3::new(150.0, 0.0, 0.0),
            V3::new(0.0, 0.0, 0.0),
            V3::new(100.0, 0.0, 0.0),
        );
        assert!((d - 50.0).abs() < 1e-4);
    }

    #[test]
    fn point_target_zone_includes_only_open_cells_within_tolerance() {
        use geom::filter::all_mask;
        use geom::mesh::{
            CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty,
        };
        let mut mesh = CollisionMesh::new();
        let attr = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let obj = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldMesh,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        });
        mesh.push_triangles(
            &[
                [-200.0, -200.0, -8.0],
                [200.0, -200.0, -8.0],
                [200.0, 200.0, -8.0],
                [-200.0, 200.0, -8.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-200.0, -200.0, -8.0),
            max: V3::new(200.0, 200.0, 128.0),
        };
        let grid = VoxelGrid::build(&mesh, &mask, 16.0, bounds).unwrap();
        let target = V3::new(0.0, 0.0, 8.0);
        let zone = point_target_zone(&grid, target, 16.0);
        assert!(!zone.is_empty());
        for &(index, _) in &zone {
            assert!(!grid.is_solid(index));
            assert!((grid.cell_center(index) - target).length() <= 16.0 + grid.voxel_size());
        }
    }
}
