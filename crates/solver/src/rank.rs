//! The API's ranking order for a solve's lineups, and the small per-lineup
//! presentation helpers the CLI/API share. Ported from
//! `cs2-smoke-solver/src/Cli/Services/LineupApi.cs:697-749` (`Rank`,
//! `Ordinal`, `StateDependent`, `Ranked`) and `CliParsing.cs:110-161`
//! (`ClickName`, `RunKeys`, `Describe`, `SetposCommand`).

use std::cmp::Ordering;

use geom::math::V3;
use sim::ThrowType;

use crate::aim_reference::{self, AimReferenceInfo};
use crate::human_error;
use crate::identity;
use crate::lineup::Lineup;
use crate::origins;
use crate::target::TargetSolve;

fn xy(v: V3) -> [f32; 2] {
    [v.x, v.y]
}

fn distance2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

/// `LineupApi.cs:728-729` (`Ordinal`): feet/yaw/pitch/strength/run only - NOT
/// throw type (unlike `sweep::ordinal_cmp`, which the sweep's own bucket
/// resolution needs a type key for).
fn ordinal_cmp(a: &Lineup, b: &Lineup) -> Ordering {
    for (fa, fb) in [
        (a.feet.x, b.feet.x),
        (a.feet.y, b.feet.y),
        (a.feet.z, b.feet.z),
        (a.yaw_deg, b.yaw_deg),
        (a.pitch_deg, b.pitch_deg),
        (a.strength, b.strength),
        (a.run_yaw_offset_deg, b.run_yaw_offset_deg),
    ] {
        match fa.partial_cmp(&fb).unwrap() {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

/// `LineupApi.cs:738-739` (`StateDependent`).
pub fn state_dependent(l: &Lineup) -> bool {
    l.glass_breaks > 0
        && !l
            .rest_if_broken
            .is_some_and(|rb| (rb - l.rest_point).length() <= 8.0)
}

/// `LineupApi.cs:705-729`'s composite sort key, in the reference's own
/// order: state-independence, then reproducibility band, then concealment,
/// then (with a click) closeness/pin/movement or (without) pin, then the
/// ordinal fallback.
#[allow(clippy::too_many_arguments)]
fn cmp_lineups(
    a: &Lineup,
    b: &Lineup,
    error_a: f32,
    error_b: f32,
    pin_a: i32,
    pin_b: i32,
    origin_click: Option<[f32; 2]>,
) -> Ordering {
    let sd = (state_dependent(a) as i32).cmp(&(state_dependent(b) as i32));
    if sd != Ordering::Equal {
        return sd;
    }
    let repro = ((error_a / 8.0) as i32).cmp(&((error_b / 8.0) as i32));
    if repro != Ordering::Equal {
        return repro;
    }
    let los = (a.direct_los as i32).cmp(&(b.direct_los as i32));
    if los != Ordering::Equal {
        return los;
    }
    if let Some(click) = origin_click {
        let da = (distance2(xy(a.feet), click) / 32.0) as i32;
        let db = (distance2(xy(b.feet), click) / 32.0) as i32;
        let dc = da.cmp(&db);
        if dc != Ordering::Equal {
            return dc;
        }
        let pc = pin_b.cmp(&pin_a);
        if pc != Ordering::Equal {
            return pc;
        }
        let tc = (a.throw_type as i32).cmp(&(b.throw_type as i32));
        if tc != Ordering::Equal {
            return tc;
        }
    } else {
        let pc = pin_b.cmp(&pin_a);
        if pc != Ordering::Equal {
            return pc;
        }
        // Confirmed against the reference by the parity harness: the
        // no-click branch also breaks pin ties by throw type before falling
        // to `Ordinal` (`LineupApi.cs:705-727`'s `Rank`, no-click branch).
        let tc = (a.throw_type as i32).cmp(&(b.throw_type as i32));
        if tc != Ordering::Equal {
            return tc;
        }
    }
    ordinal_cmp(a, b)
}

/// `LineupApi.cs:705-727` (`Rank`), generic over caller-supplied human-error
/// and pin lookups (matching the reference's `Func<Lineup, float/int>`
/// parameters).
pub fn rank(
    lineups: &[Lineup],
    origin_click: Option<[f32; 2]>,
    human_error: impl Fn(&Lineup) -> f32,
    pin: impl Fn(&Lineup) -> i32,
) -> Vec<Lineup> {
    let v: Vec<Lineup> = lineups.to_vec();
    let errs: Vec<f32> = v.iter().map(&human_error).collect();
    let pins: Vec<i32> = v.iter().map(&pin).collect();
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&i, &j| {
        cmp_lineups(
            &v[i],
            &v[j],
            errs[i],
            errs[j],
            pins[i],
            pins[j],
            origin_click,
        )
    });
    idx.into_iter().map(|i| v[i]).collect()
}

/// One lineup dressed with everything the API/CLI presents alongside it:
/// durable id, pin class, wall gap, human error, aim reference, and the
/// text helpers from `CliParsing.cs`.
#[derive(Debug, Clone)]
pub struct RankedLineup {
    pub lineup: Lineup,
    pub id: String,
    pub pin: i32,
    pub wall_gap: Option<f32>,
    pub human_error: f32,
    pub aim_ref: AimReferenceInfo,
    pub console: String,
    pub describe: String,
    pub click: &'static str,
}

/// `LineupApi.cs:742-749` (`Ranked`): the API's order for a whole solve,
/// computed from its colliders.
pub fn ranked(solve: &TargetSolve, origin_click: Option<[f32; 2]>) -> Vec<RankedLineup> {
    let n = solve.lineups.len();
    let mut pins = Vec::with_capacity(n);
    let mut wall_gaps = Vec::with_capacity(n);
    let mut aim_refs = Vec::with_capacity(n);
    let mut errors = Vec::with_capacity(n);
    for l in &solve.lineups {
        let (pin, wall_gap) = origins::position_stance(&solve.player_collider, l.feet);
        let aim = aim_reference::analyze(
            &solve.collider,
            l.feet,
            l.throw_type,
            l.pitch_deg,
            l.yaw_deg,
        );
        let err = human_error::estimate_lineup(l, pin, aim.band());
        pins.push(pin);
        wall_gaps.push(wall_gap);
        aim_refs.push(aim);
        errors.push(err);
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&i, &j| {
        cmp_lineups(
            &solve.lineups[i],
            &solve.lineups[j],
            errors[i],
            errors[j],
            pins[i],
            pins[j],
            origin_click,
        )
    });
    idx.into_iter()
        .map(|i| {
            let l = solve.lineups[i];
            RankedLineup {
                id: identity::id_of(&l),
                pin: pins[i],
                wall_gap: wall_gaps[i],
                human_error: errors[i],
                aim_ref: aim_refs[i],
                console: setpos_command(l.feet, l.pitch_deg, l.yaw_deg),
                describe: describe(l.throw_type, l.strength, l.run_yaw_offset_deg),
                click: click_name(l.strength),
                lineup: l,
            }
        })
        .collect()
}

/// `CliParsing.cs:110-111` (`ClickName`).
pub fn click_name(strength: f32) -> &'static str {
    if strength >= 0.99 {
        "left"
    } else if strength >= 0.49 {
        "left+right"
    } else {
        "right"
    }
}

fn buttons_label(strength: f32) -> &'static str {
    if strength >= 0.99 {
        "left click"
    } else if strength >= 0.49 {
        "left+right click"
    } else {
        "right click"
    }
}

/// `CliParsing.cs:133-138` (`RunKeys`).
pub fn run_keys(run_yaw_offset_deg: f32) -> &'static str {
    if run_yaw_offset_deg > 67.5 {
        "left (A)"
    } else if run_yaw_offset_deg > 22.5 {
        "forward-left (W+A)"
    } else if run_yaw_offset_deg < -67.5 {
        "right (D)"
    } else if run_yaw_offset_deg < -22.5 {
        "forward-right (W+D)"
    } else {
        "forward (W)"
    }
}

/// `CliParsing.cs:140-161` (`Describe`).
pub fn describe(t: ThrowType, strength: f32, run_yaw_offset_deg: f32) -> String {
    let movement = match t {
        ThrowType::Stand => "stand still".to_string(),
        ThrowType::Crouch => "crouch (hold ctrl)".to_string(),
        ThrowType::JumpThrow => "hold click, tap jump, release".to_string(),
        ThrowType::CrouchJumpThrow => "crouch + hold click, tap jump, release".to_string(),
        ThrowType::RunJumpThrow => {
            format!(
                "run {} + hold click, tap jump, release",
                run_keys(run_yaw_offset_deg)
            )
        }
    };
    format!("{movement}, {}", buttons_label(strength))
}

/// `CliParsing.cs:127-128` (`SetposCommand`).
pub fn setpos_command(feet: V3, pitch_deg: f32, yaw_deg: f32) -> String {
    format!(
        "setpos {:.0} {:.0} {:.0}; setang {:.1} {:.1} 0",
        feet.x,
        feet.y,
        feet.z + 1.0,
        pitch_deg,
        yaw_deg
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineup(feet: V3, t: ThrowType, direct_los: bool) -> Lineup {
        let mut l = Lineup::new(feet, 0.0, -10.0, t, V3::ZERO, 0, 1.0, 1);
        l.direct_los = direct_los;
        l
    }

    #[test]
    fn state_dependent_true_when_broken_rest_moves() {
        let mut l = Lineup::new(V3::ZERO, 0.0, -10.0, ThrowType::Stand, V3::ZERO, 0, 1.0, 1);
        l.glass_breaks = 1;
        l.rest_if_broken = Some(V3::new(100.0, 0.0, 0.0));
        assert!(state_dependent(&l));
        l.rest_if_broken = Some(V3::new(1.0, 0.0, 0.0));
        assert!(!state_dependent(&l));
        l.glass_breaks = 0;
        assert!(!state_dependent(&l));
    }

    #[test]
    fn rank_prefers_state_independent_then_lower_error_then_concealed() {
        let a = lineup(V3::new(0.0, 0.0, 0.0), ThrowType::Stand, true);
        let b = lineup(V3::new(10.0, 0.0, 0.0), ThrowType::Stand, false);
        // a: exposed but otherwise identical to b (concealed) -> b first.
        let ranked = rank(&[a, b], None, |_| 0.0, |_| 0);
        assert_eq!(ranked[0].feet, b.feet);
        assert_eq!(ranked[1].feet, a.feet);
    }

    #[test]
    fn rank_falls_back_to_ordinal_on_full_tie() {
        let a = lineup(V3::new(2.0, 0.0, 0.0), ThrowType::Stand, false);
        let b = lineup(V3::new(1.0, 0.0, 0.0), ThrowType::Stand, false);
        let ranked = rank(&[a, b], None, |_| 0.0, |_| 0);
        assert_eq!(ranked[0].feet.x, 1.0);
        assert_eq!(ranked[1].feet.x, 2.0);
    }

    #[test]
    fn rank_no_click_branch_breaks_ties_by_throw_type() {
        // Identical feet/yaw/pitch/strength/run (full `Ordinal` tie); only
        // `throw_type` differs, so the no-click branch's type key must
        // decide the order (parity-harness-confirmed fix, `rank.rs`'s
        // `cmp_lineups`).
        let feet = V3::new(10.0, 20.0, 0.0);
        let run_jump = lineup(feet, ThrowType::RunJumpThrow, false);
        let stand = lineup(feet, ThrowType::Stand, false);
        let ranked = rank(&[run_jump, stand], None, |_| 0.0, |_| 0);
        assert_eq!(ranked[0].throw_type, ThrowType::Stand);
        assert_eq!(ranked[1].throw_type, ThrowType::RunJumpThrow);
    }

    #[test]
    fn click_name_and_describe_match_bands() {
        assert_eq!(click_name(1.0), "left");
        assert_eq!(click_name(0.5), "left+right");
        assert_eq!(click_name(0.0), "right");
        assert_eq!(
            describe(ThrowType::Stand, 1.0, 0.0),
            "stand still, left click"
        );
        assert_eq!(
            describe(ThrowType::RunJumpThrow, 0.0, 90.0),
            "run left (A) + hold click, tap jump, release, right click"
        );
    }

    #[test]
    fn setpos_command_lifts_feet_by_one() {
        let s = setpos_command(V3::new(1.0, 2.0, 3.0), -10.0, 90.0);
        assert_eq!(s, "setpos 1 2 4; setang -10.0 90.0 0");
    }
}
