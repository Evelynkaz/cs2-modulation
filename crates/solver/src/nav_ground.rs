//! Nav-mesh ground-Z helpers, ported from the `NavGroundZ*` functions in
//! `cs2-smoke-solver/src/Solver/LineupSolver.Origins.cs`.

use geom::math::V3;

use crate::standspots::point_in_polygon;

/// How far outside a nav area a point may sit and still take that area's
/// height. `LineupSolver.Origins.cs:827`.
pub const NAV_GAP_REACH: f32 = 96.0;

fn distance_to_polygon(corners: &[V3], x: f32, y: f32) -> f32 {
    let mut best = f32::MAX;
    let n = corners.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (ax, ay) = (corners[j].x, corners[j].y);
        let (bx, by) = (corners[i].x, corners[i].y);
        let (ex, ey) = (bx - ax, by - ay);
        let len_sq = ex * ex + ey * ey;
        let t = if len_sq > 0.0 {
            (((x - ax) * ex + (y - ay) * ey) / len_sq).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (px, py) = (ax + ex * t, ay + ey * t);
        best = best.min(((x - px) * (x - px) + (y - py) * (y - py)).sqrt());
        j = i;
    }
    best
}

fn area_z(corners: &[V3]) -> f32 {
    (corners.iter().map(|c| c.z as f64).sum::<f64>() / corners.len() as f64) as f32
}

/// Ground z for a 2D point from the nav mesh: the walkable surface a player
/// would be on. With stacked walkable areas the lowest wins.
/// `LineupSolver.Origins.cs:834-849`.
pub fn nav_ground_z(area_corners: &[Vec<V3>], x: f32, y: f32) -> Option<f32> {
    let mut best: Option<f32> = None;
    for corners in area_corners {
        if point_in_polygon(corners, x, y) {
            let z = area_z(corners);
            if best.is_none_or(|b| z < b) {
                best = Some(z);
            }
        }
    }
    best
}

/// Ground z for a CLICKED point, tolerating the slivers between nav areas
/// (bridging up to [`NAV_GAP_REACH`]). `LineupSolver.Origins.cs:703-722`.
pub fn nav_ground_z_nearby(area_corners: &[Vec<V3>], x: f32, y: f32) -> Option<f32> {
    if let Some(inside) = nav_ground_z(area_corners, x, y) {
        return Some(inside);
    }
    let mut best: Option<f32> = None;
    let mut best_distance = NAV_GAP_REACH;
    for corners in area_corners {
        let d = distance_to_polygon(corners, x, y);
        if d < best_distance {
            best_distance = d;
            best = Some(area_z(corners));
        }
    }
    best
}

/// Walkable height at a point, accepting an area within `max_distance`
/// rather than the default [`NAV_GAP_REACH`]. `LineupSolver.Origins.cs:731-750`.
pub fn nav_ground_z_within(
    area_corners: &[Vec<V3>],
    x: f32,
    y: f32,
    max_distance: f32,
) -> Option<f32> {
    if let Some(inside) = nav_ground_z(area_corners, x, y) {
        return Some(inside);
    }
    let mut best: Option<f32> = None;
    let mut best_distance = max_distance;
    for corners in area_corners {
        let d = distance_to_polygon(corners, x, y);
        if d < best_distance {
            best_distance = d;
            best = Some(area_z(corners));
        }
    }
    best
}

/// Every distinct walkable height stacked over one 2D point, lowest first.
/// `LineupSolver.Origins.cs:765-802`.
pub fn nav_ground_levels(
    area_corners: &[Vec<V3>],
    x: f32,
    y: f32,
    separation: f32,
    strict: bool,
) -> Vec<f32> {
    let mut heights: Vec<f32> = Vec::new();
    for corners in area_corners {
        if point_in_polygon(corners, x, y) {
            heights.push(area_z(corners));
        }
    }
    if !strict || heights.is_empty() {
        for corners in area_corners {
            if !point_in_polygon(corners, x, y)
                && distance_to_polygon(corners, x, y) < NAV_GAP_REACH
            {
                heights.push(area_z(corners));
            }
        }
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut levels: Vec<f32> = Vec::new();
    for z in heights {
        if levels.last().is_none_or(|&last| z - last > separation) {
            levels.push(z);
        }
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(min: (f32, f32), max: (f32, f32), z: f32) -> Vec<V3> {
        vec![
            V3::new(min.0, min.1, z),
            V3::new(max.0, min.1, z),
            V3::new(max.0, max.1, z),
            V3::new(min.0, max.1, z),
        ]
    }

    #[test]
    fn nav_ground_z_picks_lowest_containing_area() {
        let areas = vec![
            square((-10.0, -10.0), (10.0, 10.0), 0.0),
            square((-10.0, -10.0), (10.0, 10.0), -50.0),
        ];
        assert_eq!(nav_ground_z(&areas, 0.0, 0.0), Some(-50.0));
        assert_eq!(nav_ground_z(&areas, 100.0, 100.0), None);
    }

    #[test]
    fn nav_ground_z_nearby_bridges_sliver() {
        let areas = vec![square((0.0, 0.0), (100.0, 100.0), 10.0)];
        assert_eq!(nav_ground_z_nearby(&areas, 105.0, 50.0), Some(10.0));
        assert_eq!(nav_ground_z_nearby(&areas, 500.0, 50.0), None);
    }

    #[test]
    fn nav_ground_levels_separates_stacked_floors() {
        let areas = vec![
            square((-10.0, -10.0), (10.0, 10.0), 0.0),
            square((-10.0, -10.0), (10.0, 10.0), 200.0),
        ];
        let levels = nav_ground_levels(&areas, 0.0, 0.0, 128.0, false);
        assert_eq!(levels, vec![0.0, 200.0]);
    }
}
