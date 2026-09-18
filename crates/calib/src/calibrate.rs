//! Ports `CalibrateCommand.cs`'s coordinate-descent sweep. **DECISION**
//! (stage 4b review): every constant is frozen by default, not just the
//! engine-measured ones the reference itself calibrates from server
//! behavior (`GravityScale`, `Elasticity`, `StopSpeed`, `DampGateSpeed`,
//! `MaxVelocity...`) -- `cs2-smoke-solver/docs/physics-sim.md` documents
//! those *and* `ThrowSpeed`/the click scales/`JumpVelocity`/etc. as measured
//! quantities, and a past from-throws fit that touched gravity corrupted it
//! to `0.34`. With nothing unfrozen, `calibrate` only reports the mean
//! residual of `samples` against `k0`; fitting any constant requires naming
//! it with `--unfreeze`. Every sweep grid also includes its own measured
//! default, so unfreezing an axis can't silently walk away from the
//! reference's own measurement without the samples actually preferring a
//! different value.

use geom::collider::Collider;
use sim::{ThrowConstants, Trace, simulate_exact};
use std::collections::HashSet;
use thiserror::Error;

use crate::throws_json::MeasuredThrow;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CalibrateError {
    #[error("no measured throws to calibrate against")]
    NoSamples,
    #[error("unknown --unfreeze constant {0:?} (expected one of: {1})")]
    UnknownAxis(String, String),
}

/// One sweep axis: `name` (as accepted by `--unfreeze`/CLI reporting) and the
/// candidate values tried, plus how to read/write it on `ThrowConstants`.
struct Axis {
    name: &'static str,
    values: Vec<f32>,
    set: fn(ThrowConstants, f32) -> ThrowConstants,
}

/// Reference order (`CalibrateCommand.cs:76-83`): speed, gravity, jumpv,
/// elasticity, rightscale, bothscale; `crouchjumpv` is our own addition (the
/// reference's sweep predates `CrouchJumpVelocity`), appended after it.
/// Every grid includes its own measured default (`ThrowConstants::default`).
fn axes() -> Vec<Axis> {
    vec![
        Axis {
            name: "speed",
            values: vec![600.0, 625.0, 650.0, 675.0, 690.0, 705.0, 725.0, 750.0],
            set: |k, v| ThrowConstants {
                throw_speed: v,
                ..k
            },
        },
        Axis {
            name: "gravity",
            values: vec![0.32, 0.36, 0.4, 0.44, 0.48],
            set: |k, v| ThrowConstants {
                gravity_scale: v,
                ..k
            },
        },
        Axis {
            name: "jumpv",
            values: vec![240.0, 270.0, 273.6, 300.0, 330.0, 360.0],
            set: |k, v| ThrowConstants {
                jump_velocity: v,
                ..k
            },
        },
        Axis {
            name: "elasticity",
            values: vec![0.4, 0.45, 0.5],
            set: |k, v| ThrowConstants { elasticity: v, ..k },
        },
        Axis {
            name: "rightscale",
            values: vec![0.22, 0.26, 0.30, 0.34, 0.38],
            set: |k, v| ThrowConstants {
                right_click_scale: v,
                ..k
            },
        },
        Axis {
            name: "bothscale",
            values: vec![0.6, 0.65, 0.66, 0.7, 0.74, 0.78, 0.85],
            set: |k, v| ThrowConstants {
                both_click_scale: v,
                ..k
            },
        },
        Axis {
            name: "crouchjumpv",
            values: vec![250.0, 260.0, 270.0, 277.5, 280.0, 290.0, 300.0],
            set: |k, v| ThrowConstants {
                crouch_jump_velocity: v,
                ..k
            },
        },
    ]
}

fn axis_names(axes: &[Axis]) -> String {
    axes.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
}

/// One sample's residual against the fitted (or frozen) constants,
/// for the reference's per-sample report (`CalibrateCommand.cs:110-116`).
#[derive(Debug, Clone, PartialEq)]
pub struct SampleResidual {
    /// `"rest"` or `"impact"`, matching the reference's `kind`.
    pub kind: &'static str,
    pub predicted: [f32; 3],
    pub measured: [f32; 3],
    /// Horizontal (X/Y) distance between `predicted` and `measured`.
    pub residual: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationResult {
    pub constants: ThrowConstants,
    pub error_before: f32,
    pub error_after: f32,
    pub n_samples: usize,
    pub samples: Vec<SampleResidual>,
}

/// `CalibrateCommand.cs:110-116`: for `impact` samples, prefer `first_touch`,
/// falling back to `rest` when the sim never registers a touch.
fn sample_residual(
    collider: &impl Collider,
    s: &MeasuredThrow,
    k: &ThrowConstants,
) -> SampleResidual {
    let result = simulate_exact(collider, &s.spec, k, Trace::default());
    let (kind, point) = if s.impact {
        ("impact", result.first_touch.unwrap_or(result.rest))
    } else {
        ("rest", result.rest)
    };
    let dx = point.x - s.landing.x;
    let dy = point.y - s.landing.y;
    SampleResidual {
        kind,
        predicted: [point.x, point.y, point.z],
        measured: [s.landing.x, s.landing.y, s.landing.z],
        residual: (dx * dx + dy * dy).sqrt(),
    }
}

/// `CalibrateCommand.cs:51-69` (`Error`).
fn error(collider: &impl Collider, samples: &[MeasuredThrow], k: &ThrowConstants) -> f32 {
    let mut total = 0.0f32;
    for s in samples {
        let result = simulate_exact(collider, &s.spec, k, Trace::default());
        if s.impact {
            total += match result.first_touch {
                Some(touch) => {
                    let dx = touch.x - s.landing.x;
                    let dy = touch.y - s.landing.y;
                    (dx * dx + dy * dy).sqrt()
                }
                None => 10000.0,
            };
            continue;
        }
        if result.lost {
            total += 10000.0;
            continue;
        }
        let d = result.rest - s.landing;
        total += (d.x * d.x + d.y * d.y).sqrt() + d.z.abs() * 0.5;
    }
    total / samples.len() as f32
}

/// Sweeps every axis named in `unfreeze`, 3 passes, coordinate-descent style
/// (`CalibrateCommand.cs:75-106`); with `unfreeze` empty (the default), this
/// just scores `k0` against `samples` and returns it unchanged. Errors if
/// `samples` is empty or `unfreeze` names an axis [`axes`] doesn't have.
pub fn calibrate(
    samples: &[MeasuredThrow],
    collider: &impl Collider,
    k0: ThrowConstants,
    unfreeze: &[&str],
) -> Result<CalibrationResult, CalibrateError> {
    if samples.is_empty() {
        return Err(CalibrateError::NoSamples);
    }
    let all_axes = axes();
    for name in unfreeze {
        if !all_axes.iter().any(|a| a.name == *name) {
            return Err(CalibrateError::UnknownAxis(
                name.to_string(),
                axis_names(&all_axes),
            ));
        }
    }
    let unfrozen: HashSet<&str> = unfreeze.iter().copied().collect();
    let active: Vec<Axis> = all_axes
        .into_iter()
        .filter(|a| unfrozen.contains(a.name))
        .collect();

    let mut best_k = k0;
    let mut best_error = error(collider, samples, &best_k);
    let error_before = best_error;

    for _pass in 0..3 {
        for axis in &active {
            for &v in &axis.values {
                let candidate = (axis.set)(best_k, v);
                let e = error(collider, samples, &candidate);
                if e < best_error {
                    best_k = candidate;
                    best_error = e;
                }
            }
        }
    }

    let sample_residuals = samples
        .iter()
        .map(|s| sample_residual(collider, s, &best_k))
        .collect();

    Ok(CalibrationResult {
        constants: best_k,
        error_before,
        error_after: best_error,
        n_samples: samples.len(),
        samples: sample_residuals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::math::V3;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
    use sim::{ThrowSpec, ThrowType};

    fn flat_plane_grid(half: f32) -> UniformGrid {
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
                [-half, -half, 0.0],
                [half, -half, 0.0],
                [half, half, 0.0],
                [-half, half, 0.0],
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

    fn speed_705_samples(grid: &UniformGrid) -> (Vec<MeasuredThrow>, ThrowConstants) {
        let true_k = ThrowConstants {
            throw_speed: 705.0,
            ..ThrowConstants::default()
        };
        let mut samples = Vec::new();
        for yaw in [0.0, 45.0, 90.0, 180.0] {
            let spec = ThrowSpec {
                eye: V3::new(0.0, 0.0, 64.06),
                yaw_deg: yaw,
                pitch_deg: 0.0,
                throw_type: ThrowType::Stand,
                strength: 1.0,
                run_yaw_offset_deg: 0.0,
            };
            let result = simulate_exact(grid, &spec, &true_k, Trace::default());
            samples.push(MeasuredThrow {
                spec,
                landing: result.rest,
                impact: false,
            });
        }
        (samples, true_k)
    }

    #[test]
    fn recovers_perturbed_throw_speed_when_unfrozen() {
        let grid = flat_plane_grid(4000.0);
        let (samples, _) = speed_705_samples(&grid);
        let wrong_k = ThrowConstants::default();
        let fitted = calibrate(&samples, &grid, wrong_k, &["speed"]).unwrap();
        assert!(
            (fitted.constants.throw_speed - 705.0).abs() <= 25.0,
            "fitted speed {} (expected near 705)",
            fitted.constants.throw_speed
        );
        assert!(fitted.error_after <= fitted.error_before);
    }

    #[test]
    fn everything_frozen_by_default_only_reports_residual() {
        let grid = flat_plane_grid(4000.0);
        let (samples, _) = speed_705_samples(&grid);
        let k0 = ThrowConstants::default();
        let result = calibrate(&samples, &grid, k0, &[]).unwrap();
        assert_eq!(result.constants, k0, "nothing may move with no --unfreeze");
        assert_eq!(result.error_before, result.error_after);
    }

    #[test]
    fn unknown_unfreeze_name_errors() {
        let grid = flat_plane_grid(4000.0);
        let (samples, _) = speed_705_samples(&grid);
        let err = calibrate(
            &samples,
            &grid,
            ThrowConstants::default(),
            &["not-a-constant"],
        )
        .unwrap_err();
        assert_eq!(
            err,
            CalibrateError::UnknownAxis("not-a-constant".to_string(), axis_names(&axes()))
        );
    }

    #[test]
    fn per_sample_residuals_reported_for_every_sample() {
        let grid = flat_plane_grid(4000.0);
        let (samples, _) = speed_705_samples(&grid);
        let k0 = ThrowConstants::default();
        let result = calibrate(&samples, &grid, k0, &[]).unwrap();
        assert_eq!(result.samples.len(), samples.len());
        for (sample, residual) in samples.iter().zip(&result.samples) {
            assert_eq!(residual.kind, if sample.impact { "impact" } else { "rest" });
            assert_eq!(
                residual.measured,
                [sample.landing.x, sample.landing.y, sample.landing.z]
            );
            assert!(residual.residual.is_finite());
        }
    }

    #[test]
    fn empty_samples_errors() {
        let grid = flat_plane_grid(4000.0);
        let err = calibrate(&[], &grid, ThrowConstants::default(), &[]).unwrap_err();
        assert_eq!(err, CalibrateError::NoSamples);
    }
}
