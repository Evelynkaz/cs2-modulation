//! What a lineup has to align its aim against: sky coverage, nearest
//! silhouette, and nearest reticle-arm silhouette. Ported from
//! `cs2-smoke-solver/src/Solver/AimReference.cs`.

use geom::collider::Collider;
use geom::math::V3;
use sim::{ThrowType, eye_height, forward_from_angles};

const PI: f32 = std::f32::consts::PI;

/// `AimReference.cs:13-64` (`AimReferenceInfo`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AimReferenceInfo {
    pub sky_fraction: f32,
    pub nearest_silhouette_deg: f32,
    pub nearest_reticle_deg: f32,
    pub reference_point: Option<V3>,
}

impl AimReferenceInfo {
    /// `AimReference.cs:19-22` (`IsSkyShot`).
    pub fn is_sky_shot(&self) -> bool {
        self.sky_fraction > 0.95 && !self.nearest_reticle_deg.is_finite()
    }

    /// `AimReference.cs:28-32` (`Tier`).
    pub fn tier(&self) -> &'static str {
        if self.is_sky_shot() {
            "sky"
        } else if self.nearest_silhouette_deg.is_finite() {
            "edge"
        } else if self.sky_fraction > 0.95 {
            "reticle"
        } else {
            "flat"
        }
    }

    /// `AimReference.cs:47-55` (`Band`).
    pub fn band(&self) -> i32 {
        if self.is_sky_shot() {
            6
        } else if self.nearest_silhouette_deg.is_finite() {
            if self.nearest_silhouette_deg <= 1.0 {
                0
            } else if self.nearest_silhouette_deg <= 3.0 {
                1
            } else {
                2
            }
        } else if self.sky_fraction > 0.95 {
            if self.nearest_reticle_deg.is_finite() && self.nearest_reticle_deg <= 15.0 {
                3
            } else {
                4
            }
        } else {
            5
        }
    }

    /// `AimReference.cs:60-63` (`MarginDeg`).
    pub fn margin_deg(&self) -> Option<f32> {
        if self.nearest_silhouette_deg.is_finite() {
            Some(self.nearest_silhouette_deg)
        } else if self.nearest_reticle_deg.is_finite() {
            Some(self.nearest_reticle_deg)
        } else {
            None
        }
    }
}

const CONE_HALF_ANGLE_DEG: f32 = 6.0;
const RAYS_PER_AXIS: usize = 9;
const MAX_REFERENCE_RANGE: f32 = 3000.0;
const DEPTH_JUMP_RATIO: f32 = 0.25;
const RETICLE_HALF_WIDTH_DEG: f32 = 45.0;
const RETICLE_HALF_HEIGHT_DEG: f32 = 36.87;
const RETICLE_SAMPLES: usize = 41;

struct CameraBasis {
    forward: V3,
    right: V3,
    up: V3,
}

/// `AimReference.cs:186-194` (`CameraBasis`).
fn camera_basis(pitch_deg: f32, yaw_deg: f32) -> CameraBasis {
    let forward = direction(pitch_deg, yaw_deg);
    let across = forward.cross(V3::new(0.0, 0.0, 1.0));
    let right = if across.length_squared() < 1e-6 {
        V3::new(0.0, 1.0, 0.0)
    } else {
        across.normalize()
    };
    CameraBasis {
        forward,
        right,
        up: right.cross(forward),
    }
}

/// `AimReference.cs:196-202` (`ScreenDirection`).
fn screen_direction(camera: &CameraBasis, dx_deg: f32, dy_deg: f32) -> V3 {
    (camera.forward
        + camera.right * (dx_deg * PI / 180.0).tan()
        + camera.up * (dy_deg * PI / 180.0).tan())
    .normalize()
}

fn is_silhouette(a: f32, b: f32) -> bool {
    if a.is_infinite() != b.is_infinite() {
        return true;
    }
    if a.is_infinite() {
        return false;
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    (hi - lo) / hi > DEPTH_JUMP_RATIO
}

fn angle_from_center(i: usize, j: usize, center: usize, step: f32) -> f32 {
    let dx = (i as f32 - center as f32) * step;
    let dy = (j as f32 - center as f32) * step;
    (dx * dx + dy * dy).sqrt()
}

/// `AimReference.cs:157-178` (`NearestArmSilhouette`).
fn nearest_arm_silhouette<C: Collider>(
    collider: &C,
    eye: V3,
    camera: &CameraBasis,
    half_angle_deg: f32,
    horizontal: bool,
) -> f32 {
    let mut nearest = f32::INFINITY;
    let mut previous_depth = f32::NAN;
    let mut previous_angle = 0f32;
    for i in 0..RETICLE_SAMPLES {
        let angle =
            -half_angle_deg + 2.0 * half_angle_deg * i as f32 / (RETICLE_SAMPLES - 1) as f32;
        let dir = if horizontal {
            screen_direction(camera, angle, 0.0)
        } else {
            screen_direction(camera, 0.0, angle)
        };
        let hit = collider.first_hit_ray(eye, eye + dir * MAX_REFERENCE_RANGE);
        let depth = hit
            .map(|h| h.t * MAX_REFERENCE_RANGE)
            .unwrap_or(f32::INFINITY);
        if !previous_depth.is_nan() && is_silhouette(previous_depth, depth) {
            nearest = nearest.min(previous_angle.abs().min(angle.abs()));
        }
        previous_depth = depth;
        previous_angle = angle;
    }
    nearest
}

/// `AimReference.cs:225-228` (`Direction`): raw pitch, not `derive_initial`'s
/// release bias - this models what the player's camera looks at.
fn direction(pitch_deg: f32, yaw_deg: f32) -> V3 {
    forward_from_angles(pitch_deg, yaw_deg)
}

/// `AimReference.cs:97-150` (`Analyze`).
pub fn analyze<C: Collider>(
    collider: &C,
    feet: V3,
    t: ThrowType,
    pitch_deg: f32,
    yaw_deg: f32,
) -> AimReferenceInfo {
    let eye = feet + V3::new(0.0, 0.0, eye_height(t));
    let camera = camera_basis(pitch_deg, yaw_deg);
    let step = 2.0 * CONE_HALF_ANGLE_DEG / (RAYS_PER_AXIS - 1) as f32;

    let mut depths = [[0f32; RAYS_PER_AXIS]; RAYS_PER_AXIS];
    let mut sky = 0u32;
    let center = (RAYS_PER_AXIS - 1) / 2;
    let mut reference_point: Option<V3> = None;
    for (i, row) in depths.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            let dir = screen_direction(
                &camera,
                -CONE_HALF_ANGLE_DEG + i as f32 * step,
                -CONE_HALF_ANGLE_DEG + j as f32 * step,
            );
            let hit = collider.first_hit_ray(eye, eye + dir * MAX_REFERENCE_RANGE);
            let depth = hit
                .map(|h| h.t * MAX_REFERENCE_RANGE)
                .unwrap_or(f32::INFINITY);
            *cell = depth;
            if depth.is_infinite() {
                sky += 1;
            } else if i == center && j == center {
                reference_point = Some(eye + dir * depth);
            }
        }
    }

    let mut nearest = f32::INFINITY;
    for i in 0..RAYS_PER_AXIS {
        for j in 0..RAYS_PER_AXIS {
            for (ni, nj) in [(i + 1, j), (i, j + 1)] {
                if ni >= RAYS_PER_AXIS
                    || nj >= RAYS_PER_AXIS
                    || !is_silhouette(depths[i][j], depths[ni][nj])
                {
                    continue;
                }
                let di = angle_from_center(i, j, center, step)
                    .min(angle_from_center(ni, nj, center, step));
                nearest = nearest.min(di);
            }
        }
    }

    let reticle = nearest_arm_silhouette(collider, eye, &camera, RETICLE_HALF_WIDTH_DEG, true).min(
        nearest_arm_silhouette(collider, eye, &camera, RETICLE_HALF_HEIGHT_DEG, false),
    );

    AimReferenceInfo {
        sky_fraction: sky as f32 / (RAYS_PER_AXIS * RAYS_PER_AXIS) as f32,
        nearest_silhouette_deg: nearest,
        nearest_reticle_deg: reticle,
        reference_point,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    fn empty_collider() -> UniformGrid {
        let mesh = CollisionMesh::new();
        let mask = all_mask(&mesh);
        UniformGrid::build(&mesh, &mask, None, 128.0).unwrap()
    }

    fn wall_collider() -> UniformGrid {
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
        // A wide wall facing -X, well within reference range.
        mesh.push_triangles(
            &[
                [500.0, -1000.0, -1000.0],
                [500.0, 1000.0, -1000.0],
                [500.0, 1000.0, 1000.0],
                [500.0, -1000.0, 1000.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        let mask = all_mask(&mesh);
        UniformGrid::build(&mesh, &mask, None, 128.0).unwrap()
    }

    #[test]
    fn sky_shot_when_nothing_around() {
        let collider = empty_collider();
        let info = analyze(
            &collider,
            V3::new(0.0, 0.0, 0.0),
            ThrowType::Stand,
            0.0,
            0.0,
        );
        assert_eq!(info.sky_fraction, 1.0);
        assert!(info.is_sky_shot());
        assert_eq!(info.tier(), "sky");
        assert_eq!(info.band(), 6);
    }

    #[test]
    fn flat_wall_gives_a_low_band_with_no_silhouette() {
        let collider = wall_collider();
        // Aim straight at the flat wall, far from its edges: no silhouette
        // inside the 6-degree cone, but not sky either.
        let info = analyze(
            &collider,
            V3::new(0.0, 0.0, 0.0),
            ThrowType::Stand,
            0.0,
            0.0,
        );
        assert_eq!(info.sky_fraction, 0.0);
        assert!(!info.nearest_silhouette_deg.is_finite());
        assert_eq!(info.tier(), "flat");
        assert_eq!(info.band(), 5);
    }
}
