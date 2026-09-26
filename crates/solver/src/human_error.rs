//! How far from its landing a lineup can be expected to miss when a person,
//! not the solver, throws it. Ported from
//! `cs2-smoke-solver/src/Solver/HumanError.cs`.

use sim::ThrowType;

use crate::lineup::Lineup;

/// `HumanError.cs:29` (`PositionError`): foot placement error by how the
/// geometry pins the spot. 2 = corner, 1 = wall (free to slide along it), 0
/// = open ground.
pub fn position_error(pin: i32) -> f32 {
    match pin {
        2 => 2.0,
        1 => 8.0,
        _ => 24.0,
    }
}

/// `HumanError.cs:33-40` (`AimErrorDeg`): aim error in degrees by aim-
/// reference band (`AimReference::band`). 0 is a silhouette on the
/// crosshair, 6 is sky.
pub fn aim_error_deg(band: i32) -> f32 {
    match band {
        0 => 0.5,
        1 | 2 => 1.0,
        3 => 1.5,
        4 | 5 => 2.5,
        _ => 5.0,
    }
}

/// `HumanError.cs:44-49` (`MovementError`): extra landing error from the
/// movement itself.
pub fn movement_error(t: ThrowType) -> f32 {
    match t {
        ThrowType::JumpThrow | ThrowType::CrouchJumpThrow => 6.0,
        ThrowType::RunJumpThrow => 16.0,
        _ => 0.0,
    }
}

/// `HumanError.cs:53` (`ChaosScatter`).
pub const CHAOS_SCATTER: f32 = 16.0;
/// `HumanError.cs:59` (`FragilityError`).
pub const FRAGILITY_ERROR: f32 = 24.0;

/// `HumanError.cs:62-67` (`Estimate`): expected landing miss, in units, for
/// a person throwing this lineup.
pub fn estimate(
    pin: i32,
    band: i32,
    horizontal_distance: f32,
    t: ThrowType,
    rest_scatter: f32,
    stability: f32,
) -> f32 {
    position_error(pin)
        + horizontal_distance * (aim_error_deg(band) * std::f32::consts::PI / 180.0).tan()
        + movement_error(t)
        + if rest_scatter > CHAOS_SCATTER {
            rest_scatter
        } else {
            0.0
        }
        + (1.0 - stability.clamp(0.0, 1.0)) * FRAGILITY_ERROR
}

/// `HumanError.cs:69-70` (`Estimate(Lineup, pin, band)`). Uses `stability_wide`, not `stability`
/// (`s6l_aim_precision.md`, not in the reference): in precise mode `stability` is measured at the
/// narrower ±0.2° probe, so ranking by it would reward a lineup for surviving a tighter window it
/// was only ever asked to clear because precise mode was on, not because it is actually more
/// fragile to throw by hand - `stability_wide` is always the same ±0.6° reference-window measure
/// regardless of mode, and equals `stability` outside precise mode, so normal-mode ranking is
/// unaffected.
pub fn estimate_lineup(l: &Lineup, pin: i32, band: i32) -> f32 {
    let dx = l.feet.x - l.rest_point.x;
    let dy = l.feet.y - l.rest_point.y;
    let horizontal_distance = (dx * dx + dy * dy).sqrt();
    estimate(
        pin,
        band,
        horizontal_distance,
        l.throw_type,
        l.rest_scatter,
        l.stability_wide,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::math::V3;

    #[test]
    fn position_error_by_pin() {
        assert_eq!(position_error(2), 2.0);
        assert_eq!(position_error(1), 8.0);
        assert_eq!(position_error(0), 24.0);
        assert_eq!(position_error(-1), 24.0);
    }

    #[test]
    fn aim_error_by_band() {
        assert_eq!(aim_error_deg(0), 0.5);
        assert_eq!(aim_error_deg(1), 1.0);
        assert_eq!(aim_error_deg(2), 1.0);
        assert_eq!(aim_error_deg(3), 1.5);
        assert_eq!(aim_error_deg(4), 2.5);
        assert_eq!(aim_error_deg(5), 2.5);
        assert_eq!(aim_error_deg(6), 5.0);
    }

    #[test]
    fn movement_error_by_type() {
        assert_eq!(movement_error(ThrowType::Stand), 0.0);
        assert_eq!(movement_error(ThrowType::Crouch), 0.0);
        assert_eq!(movement_error(ThrowType::JumpThrow), 6.0);
        assert_eq!(movement_error(ThrowType::CrouchJumpThrow), 6.0);
        assert_eq!(movement_error(ThrowType::RunJumpThrow), 16.0);
    }

    #[test]
    fn estimate_adds_every_term() {
        let pin = 0; // 24
        let band = 6; // 5 deg
        let horizontal = 100.0;
        let scatter = 20.0; // > CHAOS_SCATTER(16) so counted
        let stability = 0.5; // (1-0.5)*24 = 12
        let e = estimate(
            pin,
            band,
            horizontal,
            ThrowType::RunJumpThrow,
            scatter,
            stability,
        );
        let expected = 24.0 + 100.0 * (5.0f32.to_radians()).tan() + 16.0 + 20.0 + 12.0;
        assert!((e - expected).abs() < 1e-4);
    }

    /// `s6l_aim_precision.md`: `estimate_lineup` must key off `stability_wide`, not `stability` -
    /// a lineup that only clears precise mode's own narrower probe (`stability` 1.0) but is
    /// fragile at the reference ±0.6° window (`stability_wide` 0.4) must rank as more error-prone
    /// than one that is stable at both.
    #[test]
    fn estimate_lineup_keys_off_stability_wide_not_stability() {
        let mut fragile = Lineup::new(V3::ZERO, 0.0, 0.0, ThrowType::Stand, V3::ZERO, 0, 0.0, 1);
        fragile.stability = 1.0;
        fragile.stability_wide = 0.4;

        let mut sturdy = fragile;
        sturdy.stability = 1.0;
        sturdy.stability_wide = 1.0;

        let fragile_err = estimate_lineup(&fragile, 0, 0);
        let sturdy_err = estimate_lineup(&sturdy, 0, 0);
        assert!(
            fragile_err > sturdy_err,
            "stability_wide 0.4 should cost more than 1.0, got {fragile_err} vs {sturdy_err}"
        );
    }

    #[test]
    fn estimate_ignores_scatter_under_chaos_threshold() {
        let e = estimate(2, 0, 0.0, ThrowType::Stand, 10.0, 1.0);
        assert_eq!(e, 2.0);
    }
}
