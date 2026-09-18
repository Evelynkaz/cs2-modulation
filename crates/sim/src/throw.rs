//! Throw specs, per-projectile constants, and initial-state derivation.
//! Ported from `cs2-smoke-solver/src/Sim/GrenadeTrajectory.cs:5-121,358-390`.

use std::fs;
use std::path::Path;

use geom::math::V3;
use serde::{Deserialize, Serialize};

use crate::SimError;

/// `GrenadeTrajectory.cs:5-12` (`ThrowType`). Walk/run-carried throw types are
/// a later stage (4c/7): they need constants calibrated from
/// practice-server experiments, not yet measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThrowType {
    Stand,
    Crouch,
    JumpThrow,
    CrouchJumpThrow,
    RunJumpThrow,
}

/// `GrenadeTrajectory.cs:25` (`ThrowSpec`). `strength` maps the mouse click:
/// `1.0` = left, `0.5` = left+right, `0.0` = right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThrowSpec {
    pub eye: V3,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub throw_type: ThrowType,
    pub strength: f32,
    pub run_yaw_offset_deg: f32,
}

/// `GrenadeTrajectory.cs:47-121` (`ThrowConstants`). All defaults are the
/// reference's measured/calibrated values; JSON field names are `PascalCase`
/// to stay compatible with the reference's `throw-constants.json`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ThrowConstants {
    #[serde(default = "default_throw_speed")]
    pub throw_speed: f32,
    #[serde(default = "default_gravity_scale")]
    pub gravity_scale: f32,
    #[serde(default = "default_elasticity")]
    pub elasticity: f32,
    #[serde(default = "default_stop_speed")]
    pub stop_speed: f32,
    #[serde(default = "default_damp_gate_speed")]
    pub damp_gate_speed: f32,
    #[serde(default = "default_jump_velocity")]
    pub jump_velocity: f32,
    #[serde(default = "default_crouch_jump_velocity")]
    pub crouch_jump_velocity: f32,
    #[serde(default)]
    pub edge_tipping: bool,
    #[serde(default = "default_bounces_per_tick")]
    pub bounces_per_tick: i32,
    #[serde(default = "default_glass_pass_factor")]
    pub glass_pass_factor: f32,
    #[serde(default = "default_run_speed")]
    pub run_speed: f32,
    #[serde(default = "default_release_rise_right")]
    pub release_rise_right: f32,
    #[serde(default = "default_release_rise_both")]
    pub release_rise_both: f32,
    #[serde(default = "default_release_rise_left")]
    pub release_rise_left: f32,
    #[serde(default = "default_right_click_scale")]
    pub right_click_scale: f32,
    #[serde(default = "default_both_click_scale")]
    pub both_click_scale: f32,
}

fn default_throw_speed() -> f32 {
    THROW_SPEED
}
fn default_gravity_scale() -> f32 {
    0.40
}
fn default_elasticity() -> f32 {
    0.45
}
fn default_stop_speed() -> f32 {
    19.685
}
fn default_damp_gate_speed() -> f32 {
    690.0
}
fn default_jump_velocity() -> f32 {
    273.6
}
fn default_crouch_jump_velocity() -> f32 {
    277.5
}
fn default_bounces_per_tick() -> i32 {
    2
}
fn default_glass_pass_factor() -> f32 {
    0.40
}
fn default_run_speed() -> f32 {
    306.0
}
fn default_release_rise_right() -> f32 {
    14.0
}
fn default_release_rise_both() -> f32 {
    20.0
}
fn default_release_rise_left() -> f32 {
    26.1
}
fn default_right_click_scale() -> f32 {
    0.30
}
fn default_both_click_scale() -> f32 {
    0.65
}

impl Default for ThrowConstants {
    fn default() -> Self {
        ThrowConstants {
            throw_speed: default_throw_speed(),
            gravity_scale: default_gravity_scale(),
            elasticity: default_elasticity(),
            stop_speed: default_stop_speed(),
            damp_gate_speed: default_damp_gate_speed(),
            jump_velocity: default_jump_velocity(),
            crouch_jump_velocity: default_crouch_jump_velocity(),
            edge_tipping: false,
            bounces_per_tick: default_bounces_per_tick(),
            glass_pass_factor: default_glass_pass_factor(),
            run_speed: default_run_speed(),
            release_rise_right: default_release_rise_right(),
            release_rise_both: default_release_rise_both(),
            release_rise_left: default_release_rise_left(),
            right_click_scale: default_right_click_scale(),
            both_click_scale: default_both_click_scale(),
        }
    }
}

impl ThrowConstants {
    /// `ThrowConstants.SpeedScale` (`GrenadeTrajectory.cs:116-117`).
    pub fn speed_scale(&self, strength: f32) -> f32 {
        if strength >= 0.99 {
            1.0
        } else if strength >= 0.49 {
            self.both_click_scale
        } else {
            self.right_click_scale
        }
    }

    /// `ThrowConstants.ReleaseRise` (`GrenadeTrajectory.cs:119-120`).
    pub fn release_rise(&self, strength: f32) -> f32 {
        if strength >= 0.99 {
            self.release_rise_left
        } else if strength >= 0.49 {
            self.release_rise_both
        } else {
            self.release_rise_right
        }
    }

    /// Loads `ThrowConstants` from a reference-compatible `throw-constants.json`
    /// file, or the reference defaults when `path` is `None`.
    pub fn load_or_default(path: Option<&Path>) -> Result<Self, SimError> {
        match path {
            None => Ok(ThrowConstants::default()),
            Some(path) => {
                let text = fs::read_to_string(path)?;
                Ok(serde_json::from_str(&text)?)
            }
        }
    }
}

/// `GrenadeTrajectory.cs:131` (`ThrowSpeed`).
pub const THROW_SPEED: f32 = 675.0;
/// `GrenadeTrajectory.cs:132` (`MaxFlightSeconds`).
pub const MAX_FLIGHT_SECONDS: f32 = 10.0;
/// `GrenadeTrajectory.cs:137` (`StandEyeHeight`).
pub const STAND_EYE_HEIGHT: f32 = 64.06;
/// `GrenadeTrajectory.cs:138` (`CrouchEyeHeight`).
pub const CROUCH_EYE_HEIGHT: f32 = 46.04;
/// `GrenadeTrajectory.cs:275` (`GrenadeRadius`); the hull is a +-2 unit box.
pub const GRENADE_HALF: f32 = 2.0;
/// `GrenadeTrajectory.cs:160` (`FloorNormalZ`).
pub const FLOOR_NORMAL_Z: f32 = 0.7;
/// `GrenadeTrajectory.cs:144` (`TimeStep`): the server ticks grenades at 64Hz.
pub const TIME_STEP: f32 = 1.0 / 64.0;
/// `GrenadeTrajectory.cs:284` (`PhysicsSubsteps`): physics sub-steps per tick.
pub const PHYSICS_SUBSTEPS: i32 = 2;
/// `GrenadeTrajectory.cs:147` (`MaxVelocityPerAxis`, `sv_maxvelocity`).
pub const MAX_VELOCITY_PER_AXIS: f32 = 3500.0;
/// `GrenadeTrajectory.cs:150` (`StopEpsilon`).
pub const STOP_EPSILON: f32 = 0.1;
/// `GrenadeTrajectory.cs:154` (`BaseGravity`, `sv_gravity` default).
pub const BASE_GRAVITY: f32 = 800.0;
/// `GrenadeTrajectory.cs:278` (`BrokenPaneReach`).
pub const BROKEN_PANE_REACH: f32 = 96.0;

/// `GrenadeTrajectory.cs:140-141` (`EyeHeight`).
pub fn eye_height(t: ThrowType) -> f32 {
    match t {
        ThrowType::Crouch | ThrowType::CrouchJumpThrow => CROUCH_EYE_HEIGHT,
        _ => STAND_EYE_HEIGHT,
    }
}

/// `GrenadeTrajectory.cs:358-366` (`ForwardFromAngles`). Source's angle
/// convention: yaw around Z, negative pitch aims up.
pub fn forward_from_angles(pitch_deg: f32, yaw_deg: f32) -> V3 {
    let pitch = pitch_deg * std::f32::consts::PI / 180.0;
    let yaw = yaw_deg * std::f32::consts::PI / 180.0;
    V3::new(
        pitch.cos() * yaw.cos(),
        pitch.cos() * yaw.sin(),
        -pitch.sin(),
    )
}

/// `GrenadeTrajectory.cs:368-390` (`DeriveInitial`).
pub fn derive_initial(spec: &ThrowSpec, k: &ThrowConstants) -> (V3, V3) {
    // `effectivePitch = spec.PitchDeg - (90f - MathF.Abs(spec.PitchDeg)) / 90f * 10f`.
    let effective_pitch = spec.pitch_deg - (90.0 - spec.pitch_deg.abs()) / 90.0 * 10.0;
    let forward = forward_from_angles(effective_pitch, spec.yaw_deg);

    let mut velocity = forward * (k.throw_speed * k.speed_scale(spec.strength));
    let mut release = spec.eye + forward * 16.0;
    let is_jump = matches!(
        spec.throw_type,
        ThrowType::JumpThrow | ThrowType::CrouchJumpThrow | ThrowType::RunJumpThrow
    );
    if is_jump {
        velocity.z += if spec.throw_type == ThrowType::CrouchJumpThrow {
            k.crouch_jump_velocity
        } else {
            k.jump_velocity
        };
        release.z += k.release_rise(spec.strength);
    }
    if spec.throw_type == ThrowType::RunJumpThrow {
        let run_yaw = (spec.yaw_deg + spec.run_yaw_offset_deg) * std::f32::consts::PI / 180.0;
        velocity = velocity + V3::new(run_yaw.cos(), run_yaw.sin(), 0.0) * k.run_speed;
    }
    (release, velocity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_reference() {
        let k = ThrowConstants::default();
        assert_eq!(k.throw_speed, 675.0);
        assert_eq!(k.gravity_scale, 0.40);
        assert_eq!(k.elasticity, 0.45);
        assert_eq!(k.stop_speed, 19.685);
        assert_eq!(k.damp_gate_speed, 690.0);
        assert_eq!(k.jump_velocity, 273.6);
        assert_eq!(k.crouch_jump_velocity, 277.5);
        assert!(!k.edge_tipping);
        assert_eq!(k.bounces_per_tick, 2);
        assert_eq!(k.glass_pass_factor, 0.40);
        assert_eq!(k.run_speed, 306.0);
        assert_eq!(k.release_rise_right, 14.0);
        assert_eq!(k.release_rise_both, 20.0);
        assert_eq!(k.release_rise_left, 26.1);
        assert_eq!(k.right_click_scale, 0.30);
        assert_eq!(k.both_click_scale, 0.65);
        assert_eq!(STAND_EYE_HEIGHT, 64.06);
        assert_eq!(CROUCH_EYE_HEIGHT, 46.04);
        assert_eq!(GRENADE_HALF, 2.0);
        assert_eq!(MAX_FLIGHT_SECONDS, 10.0);
        assert_eq!(FLOOR_NORMAL_Z, 0.7);
    }

    #[test]
    fn speed_and_release_rise_scale_bands() {
        let k = ThrowConstants::default();
        assert_eq!(k.speed_scale(1.0), 1.0);
        assert_eq!(k.speed_scale(0.99), 1.0);
        assert_eq!(k.speed_scale(0.98), k.both_click_scale);
        assert_eq!(k.speed_scale(0.5), k.both_click_scale);
        assert_eq!(k.speed_scale(0.49), k.both_click_scale);
        assert_eq!(k.speed_scale(0.48), k.right_click_scale);
        assert_eq!(k.speed_scale(0.0), k.right_click_scale);

        assert_eq!(k.release_rise(1.0), k.release_rise_left);
        assert_eq!(k.release_rise(0.5), k.release_rise_both);
        assert_eq!(k.release_rise(0.0), k.release_rise_right);
    }

    #[test]
    fn eye_height_by_type() {
        assert_eq!(eye_height(ThrowType::Stand), STAND_EYE_HEIGHT);
        assert_eq!(eye_height(ThrowType::JumpThrow), STAND_EYE_HEIGHT);
        assert_eq!(eye_height(ThrowType::RunJumpThrow), STAND_EYE_HEIGHT);
        assert_eq!(eye_height(ThrowType::Crouch), CROUCH_EYE_HEIGHT);
        assert_eq!(eye_height(ThrowType::CrouchJumpThrow), CROUCH_EYE_HEIGHT);
    }

    #[test]
    fn forward_from_angles_hand_computed() {
        let f = forward_from_angles(0.0, 0.0);
        assert!((f.x - 1.0).abs() < 1e-6);
        assert!(f.y.abs() < 1e-6);
        assert!(f.z.abs() < 1e-6);

        // Pitch +90 aims straight down (dir.z = -sin(pitch)).
        let f = forward_from_angles(90.0, 0.0);
        assert!(f.x.abs() < 1e-5);
        assert!((f.z + 1.0).abs() < 1e-5);

        // Yaw 90 aims along +Y.
        let f = forward_from_angles(0.0, 90.0);
        assert!(f.x.abs() < 1e-5);
        assert!((f.y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn derive_initial_stand_full_strength() {
        let k = ThrowConstants::default();
        let spec = ThrowSpec {
            eye: V3::new(0.0, 0.0, STAND_EYE_HEIGHT),
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            throw_type: ThrowType::Stand,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let (pos, vel) = derive_initial(&spec, &k);
        // effective pitch at 0 = 0 - (90-0)/90*10 = -10 deg, so forward has a
        // small +Z lift component (negative pitch aims up).
        assert!((vel.x - 675.0 * (-10.0f32.to_radians()).cos()).abs() < 1e-2);
        assert!(vel.z > 0.0);
        assert!((pos.x - 16.0 * (-10.0f32.to_radians()).cos()).abs() < 1e-2);
    }

    #[test]
    fn derive_initial_jump_adds_vertical_velocity_and_release_rise() {
        let k = ThrowConstants::default();
        let spec = ThrowSpec {
            eye: V3::new(0.0, 0.0, STAND_EYE_HEIGHT),
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            throw_type: ThrowType::JumpThrow,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
        };
        let (stand_pos, stand_vel) = derive_initial(
            &ThrowSpec {
                throw_type: ThrowType::Stand,
                ..spec
            },
            &k,
        );
        let (jump_pos, jump_vel) = derive_initial(&spec, &k);
        assert!((jump_vel.z - stand_vel.z - k.jump_velocity).abs() < 1e-3);
        assert!((jump_pos.z - stand_pos.z - k.release_rise_left).abs() < 1e-3);
    }
}
