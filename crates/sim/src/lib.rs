//! Tick-accurate grenade simulation (64 tick, two physics sub-steps),
//! per-grenade detonation rules, smoke volume and sightline occlusion.
//!
//! Ported from `cs2-smoke-solver/src/Sim/` (`GrenadeTrajectory.cs`,
//! `SmokeParams.cs`, `SmokeFloodFill.cs`, `Occlusion.cs`); see each module's
//! doc comment for exact file:line citations. Grenade-kind detonation logic
//! (flash/HE/molotov/decoy) is not implemented yet (stage 7); the
//! [`trajectory::Detonation`] trait is the seam it hangs off, threaded
//! through [`simulate_exact_with`]/[`simulate_exact_raw_with`] without
//! reworking the tick loop.

pub mod occlusion;
pub mod smoke;
pub mod throw;
pub mod trajectory;

pub use occlusion::{OcclusionResult, occlusion};
pub use smoke::{SmokeParams, SmokeVolume, smoke_fill};
pub use throw::{
    BASE_GRAVITY, BROKEN_PANE_REACH, CROUCH_EYE_HEIGHT, FLOOR_NORMAL_Z, GRENADE_HALF,
    MAX_FLIGHT_SECONDS, MAX_VELOCITY_PER_AXIS, PHYSICS_SUBSTEPS, STAND_EYE_HEIGHT, STOP_EPSILON,
    THROW_SPEED, TIME_STEP, ThrowConstants, ThrowSpec, ThrowType, derive_initial, eye_height,
    forward_from_angles,
};
pub use trajectory::{
    BounceRecord, Detonation, EndReason, SmokeRest, Trace, TrajectoryResult, bounce,
    simulate_exact, simulate_exact_raw, simulate_exact_raw_with, simulate_exact_with,
    simulate_voxel,
};

use geom::math::V3;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SimError {
    #[error("failed to read throw-constants file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse throw-constants JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rest point {rest:?} is outside the voxel grid")]
    RestPointOutOfBounds { rest: V3 },
}
