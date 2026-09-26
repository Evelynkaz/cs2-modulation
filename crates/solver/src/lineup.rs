//! The `Lineup` record and yaw normalization. Ported from
//! `cs2-smoke-solver/src/Solver/LineupSolver.cs:7-42,958-969`.

use geom::math::V3;
use sim::ThrowType;

/// `LineupSolver.cs:7-42` (`Lineup`). Field order and defaults match the
/// reference record exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lineup {
    pub feet: V3,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub throw_type: ThrowType,
    pub rest_point: V3,
    pub bounces: u32,
    pub flight_time: f32,
    pub rest_crossings: i32,
    pub stability: f32,
    /// `s6l_aim_precision.md`: the 5-probe stability at the reference ±0.6° window around the
    /// chosen aim, even when the lineup was found/verified with a finer precise-mode step; equals
    /// `stability` outside precise mode.
    pub stability_wide: f32,
    pub strength: f32,
    /// Movement-key direction of a running jump throw relative to the
    /// facing; 0 for every grounded/W throw.
    pub run_yaw_offset_deg: f32,
    /// Position-chaos score: how far the predicted rest moves when the feet
    /// shift by a single movement-key tick (0.25u).
    pub rest_scatter: f32,
    /// The thrower has a clear line of sight to where the smoke lands.
    /// Filled in by the target solver (part C); defaults to `false`.
    pub direct_los: bool,
    /// Breakable panes the grenade breaks through on the way.
    pub glass_breaks: u32,
    /// Where the same throw comes to rest with every breakable pane gone;
    /// `None` when it does not touch glass, or when it never settles
    /// without it.
    pub rest_if_broken: Option<V3>,
    /// `s6q_robust_aim.md`: precise mode's post-verify robustness - `min(robust_aim, robust_pos,
    /// robust_model)`, not an average (see `verify::robust_center`); `None` outside precise mode
    /// (never computed) or for a lineup past the analysis cap.
    pub robustness: Option<f32>,
    /// `s6q_robust_aim.md`: `robustness`'s own aim-disk sub-fraction.
    pub robust_aim: Option<f32>,
    /// `s6q_robust_aim.md`: `robustness`'s own feet-jitter sub-fraction.
    pub robust_pos: Option<f32>,
    /// `s6q_robust_aim.md`'s real-game addendum: `robustness`'s own launch-model-uncertainty
    /// sub-fraction (throw speed and launch-position perturbations, see
    /// `verify::robust_model_fraction`).
    pub robust_model: Option<f32>,
    /// `s6q_robust_aim.md`: the re-centered aim's clearance (degrees, pitch-weighted) to the
    /// nearest rejected cell in the fine grid; `verify::ROBUST_NO_REJECT_MARGIN_DEG` (reported as
    /// "≥" that) when the whole grid came back accepted.
    pub aim_margin_deg: Option<f32>,
}

impl Lineup {
    /// A `Lineup` with every reference-default field (`Stability: 0f,
    /// Strength: 1f, RunYawOffsetDeg: 0f, RestScatter: 0f, DirectLos: false,
    /// GlassBreaks: 0, RestIfBroken: null`), plus `StabilityWide: 0f`
    /// (`s6l_aim_precision.md`, not in the reference) and `robustness`/
    /// `robust_aim`/`robust_pos`/`aim_margin_deg: None` (`s6q_robust_aim.md`, likewise not in the
    /// reference), except the ones every caller must supply.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        feet: V3,
        yaw_deg: f32,
        pitch_deg: f32,
        throw_type: ThrowType,
        rest_point: V3,
        bounces: u32,
        flight_time: f32,
        rest_crossings: i32,
    ) -> Self {
        Lineup {
            feet,
            yaw_deg,
            pitch_deg,
            throw_type,
            rest_point,
            bounces,
            flight_time,
            rest_crossings,
            stability: 0.0,
            stability_wide: 0.0,
            strength: 1.0,
            run_yaw_offset_deg: 0.0,
            rest_scatter: 0.0,
            direct_los: false,
            glass_breaks: 0,
            rest_if_broken: None,
            robustness: None,
            robust_aim: None,
            robust_pos: None,
            robust_model: None,
            aim_margin_deg: None,
        }
    }
}

/// `LineupSolver.cs:958-969` (`Normalize`): wraps `yaw` into `[-180, 180]`
/// exactly via repeated +/-360 subtraction/addition (not a single `%`), to
/// match the reference's own loop bit-for-bit at the boundary.
pub fn normalize_yaw(mut yaw: f32) -> f32 {
    while yaw > 180.0 {
        yaw -= 360.0;
    }
    while yaw < -180.0 {
        yaw += 360.0;
    }
    yaw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_yaw_wraps_into_range() {
        assert_eq!(normalize_yaw(0.0), 0.0);
        assert_eq!(normalize_yaw(180.0), 180.0);
        assert_eq!(normalize_yaw(180.001), -179.999);
        assert_eq!(normalize_yaw(-180.0), -180.0);
        assert_eq!(normalize_yaw(540.0), 180.0);
        assert_eq!(normalize_yaw(-540.0), -180.0);
        assert_eq!(normalize_yaw(359.5), -0.5);
    }
}
