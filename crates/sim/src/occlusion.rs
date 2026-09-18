//! Sightline occlusion through a smoke volume. Ported from
//! `cs2-smoke-solver/src/Sim/Occlusion.cs`, reusing `geom::voxel::VoxelGrid::
//! traverse` for the Amanatides-Woo DDA walk (`Occlusion.cs:16-95`).

use std::ops::ControlFlow;

use geom::math::V3;
use geom::voxel::VoxelGrid;

use crate::smoke::SmokeVolume;

/// `Occlusion.cs:5` (`OcclusionResult`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OcclusionResult {
    pub smoke_cells_crossed: u32,
    pub geometry_blocked: bool,
    pub first_solid_hit: Option<V3>,
}

/// `Occlusion.cs:16-95` (`Test`).
pub fn occlusion(smoke: &SmokeVolume, grid: &VoxelGrid, eye_a: V3, eye_b: V3) -> OcclusionResult {
    let mut smoke_cells_crossed = 0u32;
    let mut geometry_blocked = false;
    let mut first_solid_hit: Option<V3> = None;

    grid.traverse(eye_a, eye_b, |index, (x, y, z)| {
        if grid.is_solid(index) {
            geometry_blocked = true;
            if first_solid_hit.is_none() {
                first_solid_hit = Some(grid.cell_center_xyz(x, y, z));
            }
        } else if smoke.contains(index as u32) {
            smoke_cells_crossed += 1;
        }
        ControlFlow::Continue(())
    });

    OcclusionResult {
        smoke_cells_crossed,
        geometry_blocked,
        first_solid_hit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smoke::{SmokeParams, smoke_fill};
    use geom::filter::all_mask;
    use geom::mesh::CollisionMesh;

    fn open_grid() -> VoxelGrid {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        let bounds = geom::math::Aabb {
            min: V3::new(-128.0, -128.0, -128.0),
            max: V3::new(128.0, 128.0, 128.0),
        };
        VoxelGrid::build(&mesh, &mask, 8.0, bounds).unwrap()
    }

    #[test]
    fn sightline_through_smoke_counts_cells() {
        let grid = open_grid();
        let p = SmokeParams {
            max_radius: 32.0,
            cell_budget: 1_000_000,
            contained_stretch: 1.0,
        };
        let smoke = smoke_fill(&grid, V3::new(0.0, 0.0, 0.0), &p).unwrap();
        let result = occlusion(
            &smoke,
            &grid,
            V3::new(-40.0, 0.0, 0.0),
            V3::new(40.0, 0.0, 0.0),
        );
        assert!(result.smoke_cells_crossed > 0);
        assert!(!result.geometry_blocked);
        assert!(result.first_solid_hit.is_none());
    }

    #[test]
    fn sightline_with_no_smoke_crosses_zero_cells() {
        let grid = open_grid();
        let smoke = smoke_fill(
            &grid,
            V3::new(0.0, 0.0, 0.0),
            &SmokeParams {
                max_radius: 8.0,
                cell_budget: 0,
                contained_stretch: 1.0,
            },
        )
        .unwrap();
        let result = occlusion(
            &smoke,
            &grid,
            V3::new(-40.0, 50.0, 50.0),
            V3::new(40.0, 50.0, 50.0),
        );
        assert_eq!(result.smoke_cells_crossed, 0);
    }
}
