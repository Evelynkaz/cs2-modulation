//! A durable name for a lineup: the same physical throw gets the same id
//! whichever solve produced it. Ported from
//! `cs2-smoke-solver/src/Solver/LineupIdentity.cs`.

use geom::math::V3;
use sha2::{Digest, Sha256};
use sim::ThrowType;

use crate::lineup::Lineup;

/// `LineupIdentity.cs:36` (`IdLength`).
const ID_LENGTH: usize = 16;

/// `LineupIdentity.cs:38-49` (`Canonical`). Yaw is periodic; a throw at
/// 179.95 and one at -180.05 are the same aim and must not hash apart. Feet
/// coordinates round half-to-even (`MathF.Round`'s default), matching the
/// reference; other fields use plain fixed-decimal formatting.
pub fn canonical(
    t: ThrowType,
    strength: f32,
    run_yaw_offset_deg: f32,
    feet: V3,
    yaw_deg: f32,
    pitch_deg: f32,
) -> String {
    let mut yaw = (yaw_deg % 360.0 + 360.0) % 360.0;
    if yaw >= 359.95 {
        yaw = 0.0;
    }
    format!(
        "{:?}|{:.2}|{:.0}|{:.0}|{:.0}|{:.0}|{:.1}|{:.1}",
        t,
        strength,
        run_yaw_offset_deg,
        feet.x.round_ties_even(),
        feet.y.round_ties_even(),
        feet.z.round_ties_even(),
        yaw,
        pitch_deg
    )
}

/// `LineupIdentity.cs:51-56` (`Id`).
pub fn id(
    t: ThrowType,
    strength: f32,
    run_yaw_offset_deg: f32,
    feet: V3,
    yaw_deg: f32,
    pitch_deg: f32,
) -> String {
    let canonical = canonical(t, strength, run_yaw_offset_deg, feet, yaw_deg, pitch_deg);
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..ID_LENGTH].to_string()
}

/// `LineupIdentity.cs:58-59` (`Id(Lineup)`).
pub fn id_of(l: &Lineup) -> String {
    id(
        l.throw_type,
        l.strength,
        l.run_yaw_offset_deg,
        l.feet,
        l.yaw_deg,
        l.pitch_deg,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_matches_hand_computed_format() {
        let feet = V3::new(12.4, -7.6, 0.5);
        let s = canonical(ThrowType::JumpThrow, 1.0, 0.0, feet, 45.12, -10.03);
        // Round(12.4)=12, Round(-7.6)=-8, Round(0.5)=0 (ties-to-even).
        assert_eq!(s, "JumpThrow|1.00|0|12|-8|0|45.1|-10.0");
    }

    #[test]
    fn canonical_wraps_yaw_periodically() {
        let feet = V3::new(0.0, 0.0, 0.0);
        let a = canonical(ThrowType::Stand, 1.0, 0.0, feet, 179.96, 0.0);
        let b = canonical(ThrowType::Stand, 1.0, 0.0, feet, -180.04, 0.0);
        assert_eq!(a, b);
    }

    #[test]
    fn id_is_deterministic_and_16_hex_chars() {
        let feet = V3::new(0.0, 0.0, 0.0);
        let a = id(ThrowType::Stand, 1.0, 0.0, feet, 0.0, -10.0);
        let b = id(ThrowType::Stand, 1.0, 0.0, feet, 0.0, -10.0);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        let c = id(ThrowType::Stand, 1.0, 0.0, feet, 0.0, -10.1);
        assert_ne!(a, c);
    }
}
