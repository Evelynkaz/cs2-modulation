//! Compares `derive_initial` against the corpus's recorded `Pos`/`Vel`.
//!
//! **What this actually measures.** The corpus's `Pos`/`Vel` are not
//! something the live game engine derived from a player's aim/click; they
//! are the *reference's own* `DeriveInitial(new ThrowSpec(eye, ...))` output
//! (`ValidateCommand.cs:180-185`: `eye = Feet + (0,0,EyeHeight(Type))`, then
//! `DeriveInitial`), fed straight into a synthetic `SmokeCreate(pos, vel,
//! ...)` spawn by the calibration rig's CounterStrikeSharp plugin
//! (`rig/CalibrationThrower/CalibrationThrowerPlugin.cs:469-475`,
//! `ThrowSynthetic`) so the *engine's* collision physics can be graded
//! without a human throwing every lineup by hand. So `launch_check` here
//! measures this port's `derive_initial` parity with **the reference's own
//! launch-model code**, not with anything the live CS2 engine computed --
//! `crates/calib::replay`'s mesh+collider+integrator check (fed those same
//! recorded `Pos`/`Vel` straight into `simulate_exact_raw`) is the one that
//! validates against real engine behavior. Evidence for the launch model
//! itself is `physics-sim.md`'s measurements plus practice-server
//! experiments (`docs/ARCHITECTURE.md` §8), not this comparison.
//!
//! Consequently a nonzero residual here on an older recorded `build` is not
//! a bug in this port -- it's a *reference model revision*: builds
//! 2000839/2000872 predate calibration changes documented for later builds
//! (an older jump velocity of ~300 u/s vs. the current 273.6, no release-rise
//! offset yet, an older run speed), so `derive_initial` with today's
//! constants legitimately disagrees with `DeriveInitial` calls the reference
//! made under yesterday's constants. `launch_check` reports these per-`Type`
//! (not per-build) for now; the real ignored test groups by build instead so
//! this doesn't get hidden by averaging across it.

use geom::math::V3;
use sim::{ThrowConstants, ThrowSpec, ThrowType, derive_initial, eye_height};

use crate::corpus::CorpusThrow;

/// Parses the corpus's `Type` string into a [`ThrowType`], or `None` for an
/// unrecognized string (skipped by [`launch_check`], not an error: some
/// older reports may use a type this port doesn't model yet).
pub fn parse_corpus_type(s: &str) -> Option<ThrowType> {
    match s {
        "Stand" => Some(ThrowType::Stand),
        "Crouch" => Some(ThrowType::Crouch),
        "JumpThrow" => Some(ThrowType::JumpThrow),
        "CrouchJumpThrow" => Some(ThrowType::CrouchJumpThrow),
        "RunJumpThrow" => Some(ThrowType::RunJumpThrow),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ErrorStats {
    pub n: usize,
    pub mean_pos: f32,
    pub max_pos: f32,
    pub mean_vel: f32,
    pub max_vel: f32,
}

#[derive(Debug, Clone)]
pub struct LaunchReport {
    /// `(ThrowType`'s corpus name`, stats)`, one per distinct type seen.
    pub per_type: Vec<(String, ErrorStats)>,
    pub overall: ErrorStats,
    /// Rows skipped because `Type`/`Pos`/`Vel` were missing or unrecognized.
    pub skipped: usize,
}

/// Compares `derive_initial(spec, k)` against each row's recorded `Pos`/
/// `Vel`, grouped by `Type`.
pub fn launch_check(throws: &[CorpusThrow], k: &ThrowConstants) -> LaunchReport {
    let mut per_type: Vec<(String, Vec<(f32, f32)>)> = Vec::new();
    let mut skipped = 0usize;

    for t in throws {
        let r = &t.result;
        let (Some(throw_type), Some(pos), Some(vel)) =
            (parse_corpus_type(&r.throw_type), r.pos, r.vel)
        else {
            skipped += 1;
            continue;
        };
        let feet = V3::new(r.feet[0], r.feet[1], r.feet[2]);
        let eye = feet + V3::new(0.0, 0.0, eye_height(throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: r.yaw,
            pitch_deg: r.pitch,
            throw_type,
            strength: r.strength,
            run_yaw_offset_deg: r.run_deg,
        };
        let (derived_pos, derived_vel) = derive_initial(&spec, k);
        let recorded_pos = V3::new(pos[0], pos[1], pos[2]);
        let recorded_vel = V3::new(vel[0], vel[1], vel[2]);
        let dpos = (derived_pos - recorded_pos).length();
        let dvel = (derived_vel - recorded_vel).length();

        match per_type.iter_mut().find(|(name, _)| name == &r.throw_type) {
            Some((_, v)) => v.push((dpos, dvel)),
            None => per_type.push((r.throw_type.clone(), vec![(dpos, dvel)])),
        }
    }

    let stats = |v: &[(f32, f32)]| -> ErrorStats {
        let n = v.len();
        if n == 0 {
            return ErrorStats::default();
        }
        let mean_pos = v.iter().map(|(p, _)| p).sum::<f32>() / n as f32;
        let mean_vel = v.iter().map(|(_, v)| v).sum::<f32>() / n as f32;
        let max_pos = v.iter().map(|(p, _)| *p).fold(0.0f32, f32::max);
        let max_vel = v.iter().map(|(_, v)| *v).fold(0.0f32, f32::max);
        ErrorStats {
            n,
            mean_pos,
            max_pos,
            mean_vel,
            max_vel,
        }
    };

    let all: Vec<(f32, f32)> = per_type
        .iter()
        .flat_map(|(_, v)| v.iter().copied())
        .collect();
    let overall = stats(&all);
    let per_type = per_type
        .into_iter()
        .map(|(name, v)| (name, stats(&v)))
        .collect();

    LaunchReport {
        per_type,
        overall,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CorpusResult;

    fn throw(feet: [f32; 3], yaw: f32, pitch: f32, ty: &str, strength: f32) -> CorpusThrow {
        let throw_type = parse_corpus_type(ty).unwrap();
        let spec = ThrowSpec {
            eye: V3::new(feet[0], feet[1], feet[2]) + V3::new(0.0, 0.0, eye_height(throw_type)),
            yaw_deg: yaw,
            pitch_deg: pitch,
            throw_type,
            strength,
            run_yaw_offset_deg: 0.0,
        };
        let k = ThrowConstants::default();
        let (pos, vel) = derive_initial(&spec, &k);
        CorpusThrow {
            report: "synthetic".to_string(),
            build: "0".to_string(),
            result: CorpusResult {
                index: 0,
                throw_type: ty.to_string(),
                strength,
                stability: 0.0,
                feet,
                yaw,
                pitch,
                run_deg: 0.0,
                perturb_u: 0.0,
                scatter: 0.0,
                predicted_bounces: 0,
                real_bounces: 0,
                pos: Some([pos.x, pos.y, pos.z]),
                vel: Some([vel.x, vel.y, vel.z]),
                predicted_rest: None,
                real_rest: None,
                detonated: true,
                err_predicted: 0.0,
                err_target: 0.0,
                divergence_tick: -1,
                divergence_class: None,
                glass_state: None,
            },
        }
    }

    #[test]
    fn recovers_exact_launch_state_from_its_own_model() {
        let rows = vec![
            throw([0.0, 0.0, 0.0], 30.0, -10.0, "Stand", 1.0),
            throw([100.0, -50.0, 20.0], 90.0, 0.0, "JumpThrow", 0.5),
        ];
        let k = ThrowConstants::default();
        let report = launch_check(&rows, &k);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.overall.n, 2);
        assert!(report.overall.max_pos < 1e-3);
        assert!(report.overall.max_vel < 1e-3);
        assert_eq!(report.per_type.len(), 2);
    }
}
