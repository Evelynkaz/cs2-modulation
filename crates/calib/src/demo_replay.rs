//! Offline replay of `tools/demo/extract_launches.py`'s `launches.json` (real GOTV-demo smoke
//! projectiles, `s6v_demo_replay_and_rim_rest.md`, Part C `s6v_c_pro_demos.md` for pro/tournament
//! demos at scale): re-simulates each projectile from its own fitted launch state and aligns the
//! sim trace with the game's own per-tick positions, tick by tick, to find exactly where and why
//! the two diverge.
//!
//! Alignment is direct index correspondence, not a search: `extract_launches.py` fits the launch
//! instant to be exactly one tick before the game's first recorded sample, and the exact
//! integrator's own tick length (`sim::TIME_STEP`, 1/64s) is the demo's own tick length, so sim
//! tick `i` (0-based, the state after `i+1` physics ticks from the launch instant) lands on
//! exactly the same instant as the game's tick `launch_tick + 1 + i` - looked up by tick number
//! (not raw index), so a gap in the recorded game ticks just skips that one comparison instead of
//! desyncing everything after it.

use geom::collider::Collider;
use geom::math::V3;
use geom::mesh::CollisionMesh;
use serde::{Deserialize, Serialize};
use sim::{BASE_GRAVITY, ThrowConstants, Trace, simulate_exact_raw};

/// One `launches.json` (`extract_launches.py`'s own output).
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchesFile {
    pub map: String,
    /// `s6v_c_pro_demos.md`: the demo header fields `extract_launches.py` records - absent
    /// (`None`) for a `launches.json` written before Part C.
    #[serde(default)]
    pub header: Option<DemoHeader>,
    #[serde(default)]
    pub projectiles: Vec<ProjectileLaunch>,
    #[serde(default)]
    pub skipped: Vec<SkippedProjectile>,
}

/// `s6v_c_pro_demos.md`: the fixed subset of `DemoParser.parse_header()` `extract_launches.py`
/// records - whichever fields the parser did not provide are `None`. `patch_version` (e.g.
/// `"14185"`) is the game's own `steam.inf` `PatchVersion` without dots; a mismatch means the demo
/// was recorded on different map geometry, which this tool cannot fix but can at least explain.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DemoHeader {
    #[serde(default)]
    pub network_protocol: Option<serde_json::Value>,
    #[serde(default)]
    pub demo_version_name: Option<String>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub demo_file_stamp: Option<String>,
    #[serde(default)]
    pub patch_version: Option<String>,
    #[serde(default)]
    pub tick_interval: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkippedProjectile {
    pub entity_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TickSample {
    pub tick: i64,
    pub pos: [f32; 3],
}

/// `s6v_c_pro_demos.md`: one tick a projectile came within `PLAYER_NEAR_DIST` of an alive
/// player's hull (`extract_launches.py`'s own `player_near`).
#[derive(Debug, Clone, Deserialize)]
pub struct PlayerNear {
    pub tick: i64,
    pub distance: f32,
    pub steamid: String,
    pub is_thrower: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectileLaunch {
    pub entity_id: i64,
    pub first_tick: i64,
    pub last_tick: i64,
    pub num_samples: usize,
    #[serde(default)]
    pub missing_ticks: u32,
    pub fit_ticks: u32,
    pub fit_residual_rms: f32,
    pub fit_residual_max: f32,
    pub fit_g: f32,
    pub launch_confident: bool,
    pub launch_tick: i64,
    pub launch_pos: [f32; 3],
    pub launch_vel: [f32; 3],
    pub sample0_pos: [f32; 3],
    pub sample0_vel: [f32; 3],
    pub ticks: Vec<TickSample>,
    pub game_rest: [f32; 3],
    pub rest_source: String,
    pub detonate_tick: Option<i64>,
    /// `s6v_c_pro_demos.md`: the thrower, when `parse_grenades()` provided one.
    #[serde(default)]
    pub thrower_steamid: Option<String>,
    #[serde(default)]
    pub thrower_name: Option<String>,
    /// `s6v_c_pro_demos.md`: ticks where the projectile came within `PLAYER_NEAR_DIST` of an
    /// alive player's hull (thrower excluded during the first 12 ticks after launch).
    #[serde(default)]
    pub player_near: Vec<PlayerNear>,
    #[serde(default)]
    pub player_contact_possible: bool,
    /// `s6v_c_pro_demos.md` addendum, measured: a smoke that flies into a burning
    /// molotov/incendiary detonates at once while still moving.
    #[serde(default)]
    pub early_detonation_fire: bool,
    #[serde(default)]
    pub nearest_fire_distance: Option<f32>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The source entity a divergence's contact triangle came from, when the mesh recorded one
/// (`CollisionMesh::tri_object`; see the module doc on why the mapping is always available here).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntityInfo {
    pub classname: Option<String>,
    pub targetname: Option<String>,
    pub model: Option<String>,
}

/// The sim's own contact at/just before a divergence tick.
#[derive(Debug, Clone, PartialEq)]
pub struct DivergenceContact {
    /// Sim tick index (0-based, same axis as [`ThrowReplay::first_over_1u`]) the contact fired
    /// on.
    pub tick: u32,
    pub point: V3,
    pub normal: V3,
    /// The collision attribute (group) name of the triangle hit.
    pub group: String,
    /// Set for `EntitySolid`/`EntityBreakable` groups whose triangle traces back to a merged
    /// entity (`CollisionMesh::tri_object`/`objects`); `None` for plain world geometry.
    pub entity: Option<EntityInfo>,
}

/// One throw's replay result.
#[derive(Debug, Clone, PartialEq)]
pub struct ThrowReplay {
    pub entity_id: i64,
    pub sim_rest: V3,
    pub game_rest: V3,
    /// `false` when `rest_source == "last_position"` (no `smokegrenade_detonate` matched this
    /// entity): `rest_error` is still computed against the last tracked position, but it is not
    /// a real landing-spot grade, just a flight-so-far comparison.
    pub rest_graded: bool,
    pub rest_error: f32,
    /// How many game ticks were actually compared (ticks present in both the sim trace and the
    /// recorded game samples).
    pub aligned_ticks: usize,
    /// Game tick number of the first compared sample whose deviation exceeds 1u/5u.
    pub first_over_1u: Option<i64>,
    pub first_over_5u: Option<i64>,
    pub divergence_contact: Option<DivergenceContact>,
    /// Whether the game's own recorded positions show a velocity kink within ±2 ticks of
    /// `first_over_1u` (a real bounce there too, not just a sim/game mismatch).
    pub game_also_bounced: bool,
}

const TICK_DT: f32 = sim::TIME_STEP;

/// `s6v_fix1.md`: the game's finite-difference acceleration noise from 1/32u position
/// quantization is up to 443 u/s² in 3-D (worst case: 4 * (1/64u rounding) / `TICK_DT`²,
/// combined over x/y/z) - well above the old `60.0` tolerance, which made
/// [`game_velocity_kink`] fire on ordinary clean flight. Set above that floor.
pub const KINK_ACCEL_TOLERANCE: f32 = 600.0;

/// Per-tick deviation between an aligned sim trace and the game's own recorded positions, keyed
/// by game tick number. Ticks beyond the sim's own trace (it stops recording once the throw
/// settles; the real grenade may sit there for many more recorded game ticks before detonating)
/// are compared against `sim_rest` instead of a nonexistent later sim sample (`s6v_fix1.md`: those
/// ticks used to never be compared at all). Pure/no simulator involved, so unit-testable on
/// synthetic traces.
pub fn align_deviations(
    sim_ticks: &[(V3, V3)],
    launch_tick: i64,
    sim_rest: V3,
    game_ticks: &[TickSample],
) -> Vec<(i64, f32)> {
    let mut out = Vec::new();
    for (i, &(sim_pos, _)) in sim_ticks.iter().enumerate() {
        let want_tick = launch_tick + 1 + i as i64;
        if let Ok(idx) = game_ticks.binary_search_by_key(&want_tick, |t| t.tick) {
            let game_pos = V3::from_array(game_ticks[idx].pos);
            out.push((want_tick, (sim_pos - game_pos).length()));
        }
    }
    let rest_from_tick = launch_tick + 1 + sim_ticks.len() as i64;
    for gt in game_ticks {
        if gt.tick >= rest_from_tick {
            let game_pos = V3::from_array(gt.pos);
            out.push((gt.tick, (sim_rest - game_pos).length()));
        }
    }
    out
}

/// First tick (game tick number) in `deviations` whose error exceeds `threshold`.
pub fn first_over(deviations: &[(i64, f32)], threshold: f32) -> Option<i64> {
    deviations
        .iter()
        .find(|&&(_, e)| e > threshold)
        .map(|&(t, _)| t)
}

/// Every game tick (the middle sample of a consecutive recorded triple) whose acceleration is off
/// the expected constant free-fall (`g_expected` on z, ~0 on x/y) by more than `accel_tolerance` -
/// a real bounce, or the sim/game disagreeing about one. Uses each sample's own recorded tick
/// number for the finite-difference step (`s6v_c_pro_demos.md`: tournament GOTV may record every
/// 2nd tick, `tv_snapshotrate 32` - a fixed 1-tick step would silently compare non-adjacent
/// samples), so consecutive RECORDED samples are used regardless of the tick gap between them, not
/// consecutive tick numbers. Pure, unit-testable on synthetic tick lists.
pub fn game_kink_ticks(
    game_ticks: &[TickSample],
    g_expected: f32,
    accel_tolerance: f32,
) -> Vec<i64> {
    let expected = V3::new(0.0, 0.0, -g_expected);
    let mut out = Vec::new();
    for i in 1..game_ticks.len().saturating_sub(1) {
        let (t0, t1, t2) = (
            game_ticks[i - 1].tick,
            game_ticks[i].tick,
            game_ticks[i + 1].tick,
        );
        let dt1 = (t1 - t0) as f32 * TICK_DT;
        let dt2 = (t2 - t1) as f32 * TICK_DT;
        if dt1 <= 0.0 || dt2 <= 0.0 {
            continue;
        }
        let p0 = V3::from_array(game_ticks[i - 1].pos);
        let p1 = V3::from_array(game_ticks[i].pos);
        let p2 = V3::from_array(game_ticks[i + 1].pos);
        let v0 = (p1 - p0) * (1.0 / dt1);
        let v1 = (p2 - p1) * (1.0 / dt2);
        let accel = (v1 - v0) * (2.0 / (dt1 + dt2));
        if (accel - expected).length() > accel_tolerance {
            out.push(t1);
        }
    }
    out
}

/// Whether the game's own recorded positions show a velocity kink within `window` ticks either
/// side of `center_tick` (see [`game_kink_ticks`]). Pure, unit-testable on synthetic tick lists.
pub fn game_velocity_kink(
    game_ticks: &[TickSample],
    center_tick: i64,
    window: i64,
    g_expected: f32,
    accel_tolerance: f32,
) -> bool {
    game_kink_ticks(game_ticks, g_expected, accel_tolerance)
        .into_iter()
        .any(|t| (t - center_tick).abs() <= window)
}

/// Looks up the group/entity info for a contact triangle (an original `CollisionMesh` triangle
/// index, per `Collider`'s own indexing contract). `pub` for `cmd_replay_demo`'s `--dump-dir`.
pub fn describe_triangle(mesh: &CollisionMesh, triangle: u32) -> (String, Option<EntityInfo>) {
    let attr_idx = mesh.tri_attribute[triangle as usize];
    let group = mesh
        .attributes
        .get(usize::from(attr_idx))
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "?".to_string());
    let entity = if group == "EntitySolid" || group == "EntityBreakable" {
        mesh.tri_object
            .get(triangle as usize)
            .and_then(|&oi| mesh.objects.get(oi as usize))
            .map(|o| EntityInfo {
                classname: o.classname.clone(),
                targetname: o.targetname.clone(),
                model: o.model.clone(),
            })
    } else {
        None
    };
    (group, entity)
}

/// Re-simulates one `ProjectileLaunch` from its own fitted `launch_pos`/`launch_vel` against
/// `collider` (same colliders/attributes as `throw`) and grades it against the game's own
/// recorded ticks/rest.
pub fn replay_throw<C: Collider>(
    mesh: &CollisionMesh,
    collider: &C,
    k: &ThrowConstants,
    launch: &ProjectileLaunch,
) -> ThrowReplay {
    let pos0 = V3::from_array(launch.launch_pos);
    let vel0 = V3::from_array(launch.launch_vel);

    let mut ticks: Vec<(V3, V3)> = Vec::new();
    let mut bounces: Vec<sim::BounceRecord> = Vec::new();
    let trace = Trace {
        ticks: Some(&mut ticks),
        bounces: Some(&mut bounces),
    };
    let result = simulate_exact_raw(collider, pos0, vel0, k, trace);

    let game_rest = V3::from_array(launch.game_rest);
    let rest_error = (result.rest - game_rest).length();
    let rest_graded = launch.rest_source == "detonate_event";

    let deviations = align_deviations(&ticks, launch.launch_tick, result.rest, &launch.ticks);
    let aligned_ticks = deviations.len();
    let first_over_1u = first_over(&deviations, 1.0);
    let first_over_5u = first_over(&deviations, 5.0);

    let divergence_contact = first_over_1u.and_then(|div_tick| {
        let div_index = (div_tick - launch.launch_tick - 1) as u32;
        bounces
            .iter()
            .filter(|b| b.tick <= div_index)
            .max_by_key(|b| b.tick)
            .map(|b| {
                let (group, entity) = describe_triangle(mesh, b.triangle);
                DivergenceContact {
                    tick: b.tick,
                    point: b.contact,
                    normal: b.normal,
                    group,
                    entity,
                }
            })
    });

    // `s6v_fix1.md`: centred on the sim's OWN contact tick when there is one (not the divergence
    // tick, which can be long after the sim came to rest - see `align_deviations`'s post-rest
    // comparison), and on known physics (`BASE_GRAVITY * k.gravity_scale`), not the fitted `g`
    // (a bad fit's `g` is exactly the case this check must stay reliable for).
    let known_gravity = BASE_GRAVITY * k.gravity_scale;
    let game_also_bounced = first_over_1u.is_some_and(|div_tick| {
        let center_tick = divergence_contact
            .as_ref()
            .map(|c| launch.launch_tick + 1 + i64::from(c.tick))
            .unwrap_or(div_tick);
        game_velocity_kink(
            &launch.ticks,
            center_tick,
            2,
            known_gravity,
            KINK_ACCEL_TOLERANCE,
        )
    });

    ThrowReplay {
        entity_id: launch.entity_id,
        sim_rest: result.rest,
        game_rest,
        rest_graded,
        rest_error,
        aligned_ticks,
        first_over_1u,
        first_over_5u,
        divergence_contact,
        game_also_bounced,
    }
}

/// `s6v_c_pro_demos.md`: why a throw's rest error is excluded from the graded counts, in the
/// order [`exclusion_reason`] checks them (a throw can only match one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// `rest_source != "detonate_event"`: no real landing spot to grade against.
    NoDetonation,
    /// `!launch_confident`: the launch state itself is not trusted.
    LowConfidence,
    /// The throw's first >1u divergence traces back to touching a player, not the collision set.
    PlayerContact,
    /// Addendum, measured: the smoke detonated early because it flew into a burning
    /// molotov/incendiary, while still moving - not a physics/collision-set divergence.
    EarlyDetonationFire,
    /// Addendum: the divergence's sim contact is `EntityBreakable` - breakable glass in a pro
    /// demo may already have been broken earlier in the round (dynamic state this tool cannot
    /// know), so the contact might not reflect what the game grenade actually saw.
    GlassStateUnknown,
}

/// `s6v_c_pro_demos.md`'s player-contact exclusion rule: a throw with `player_contact_possible`
/// whose first >1u divergence tick is at/after the first `player_near` tick minus 2. Pure,
/// unit-testable.
pub fn player_contact_excludes(launch: &ProjectileLaunch, first_over_1u: Option<i64>) -> bool {
    if !launch.player_contact_possible {
        return false;
    }
    let Some(div_tick) = first_over_1u else {
        return false;
    };
    let Some(first_near_tick) = launch.player_near.iter().map(|p| p.tick).min() else {
        return false;
    };
    div_tick >= first_near_tick - 2
}

/// Why (if at all) `launch`/`replay` is excluded from the graded counts (`s6v_c_pro_demos.md`).
/// Pure, unit-testable.
pub fn exclusion_reason(launch: &ProjectileLaunch, replay: &ThrowReplay) -> Option<Exclusion> {
    if launch.rest_source != "detonate_event" {
        return Some(Exclusion::NoDetonation);
    }
    if !launch.launch_confident {
        return Some(Exclusion::LowConfidence);
    }
    if player_contact_excludes(launch, replay.first_over_1u) {
        return Some(Exclusion::PlayerContact);
    }
    if launch.early_detonation_fire {
        return Some(Exclusion::EarlyDetonationFire);
    }
    if replay
        .divergence_contact
        .as_ref()
        .is_some_and(|c| c.group == "EntityBreakable")
    {
        return Some(Exclusion::GlassStateUnknown);
    }
    None
}

/// One throw plus the map it came from - `s6v_c_pro_demos.md`'s multi-file/multi-map
/// `summarize()`: grouped per map for the per-map report and concatenated for the total.
#[derive(Debug, Clone, Copy)]
pub struct GradedThrow<'a> {
    pub launch: &'a ProjectileLaunch,
    pub replay: &'a ThrowReplay,
    pub map: &'a str,
}

/// `replay-demo`'s own summary aggregate: counts within 1u/3u/8u of the graded rest error
/// (`exclusion_reason` throws are excluded, and counted separately), plus a ranked list of
/// divergence surfaces (group + map + a coarse location cluster) by how many non-excluded throws'
/// first >1u deviation traces back to it.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoReplaySummary {
    pub n_graded: usize,
    pub within_1u: usize,
    pub within_3u: usize,
    pub within_8u: usize,
    pub excluded_no_detonation: usize,
    pub excluded_low_confidence: usize,
    pub excluded_player_contact: usize,
    pub excluded_early_detonation_fire: usize,
    pub excluded_glass_state_unknown: usize,
    /// `(group, map, cluster center, throw count)`, sorted by count descending.
    pub surfaces: Vec<(String, String, V3, usize)>,
}

/// The location-cluster grid size (units): divergence contacts within the same 64u cell (and
/// the same attribute group and map) are counted as the same surface.
const CLUSTER_SIZE: f32 = 64.0;

fn cluster_center(p: V3) -> V3 {
    let snap = |v: f32| (v / CLUSTER_SIZE).round() * CLUSTER_SIZE;
    V3::new(snap(p.x), snap(p.y), snap(p.z))
}

/// (group, map, cluster center) - a divergence surface bucket key, before the throw count is
/// attached (`clippy::type_complexity`).
type SurfaceKey = (String, String, [i32; 3]);

pub fn summarize(throws: &[GradedThrow]) -> DemoReplaySummary {
    let mut s = DemoReplaySummary {
        n_graded: 0,
        within_1u: 0,
        within_3u: 0,
        within_8u: 0,
        excluded_no_detonation: 0,
        excluded_low_confidence: 0,
        excluded_player_contact: 0,
        excluded_early_detonation_fire: 0,
        excluded_glass_state_unknown: 0,
        surfaces: Vec::new(),
    };

    let mut buckets: Vec<(SurfaceKey, usize)> = Vec::new();
    for t in throws {
        let reason = exclusion_reason(t.launch, t.replay);
        match reason {
            None => {
                s.n_graded += 1;
                let e = t.replay.rest_error;
                if e <= 1.0 {
                    s.within_1u += 1;
                }
                if e <= 3.0 {
                    s.within_3u += 1;
                }
                if e <= 8.0 {
                    s.within_8u += 1;
                }
            }
            Some(Exclusion::NoDetonation) => s.excluded_no_detonation += 1,
            Some(Exclusion::LowConfidence) => s.excluded_low_confidence += 1,
            Some(Exclusion::PlayerContact) => s.excluded_player_contact += 1,
            Some(Exclusion::EarlyDetonationFire) => s.excluded_early_detonation_fire += 1,
            Some(Exclusion::GlassStateUnknown) => s.excluded_glass_state_unknown += 1,
        }

        // Surfaces: informative even for a throw whose rest itself isn't graded (no detonation /
        // low confidence) - only skip the ones a NON-mesh reason already explains away.
        if matches!(
            reason,
            Some(Exclusion::PlayerContact)
                | Some(Exclusion::EarlyDetonationFire)
                | Some(Exclusion::GlassStateUnknown)
        ) {
            continue;
        }
        let Some(c) = &t.replay.divergence_contact else {
            continue;
        };
        let center = cluster_center(c.point);
        let key = (
            c.group.clone(),
            t.map.to_string(),
            [center.x as i32, center.y as i32, center.z as i32],
        );
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => buckets.push((key, 1)),
        }
    }
    buckets.sort_by_key(|a| std::cmp::Reverse(a.1));
    s.surfaces = buckets
        .into_iter()
        .map(|((group, map, c), n)| {
            (
                group,
                map,
                V3::new(c[0] as f32, c[1] as f32, c[2] as f32),
                n,
            )
        })
        .collect();

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(t: i64, pos: [f32; 3]) -> TickSample {
        TickSample { tick: t, pos }
    }

    #[test]
    fn align_deviations_matches_by_tick_number_and_skips_gaps() {
        let sim_ticks = vec![
            (V3::new(0.0, 0.0, 0.0), V3::ZERO),
            (V3::new(1.0, 0.0, 0.0), V3::ZERO),
            (V3::new(2.0, 0.0, 0.0), V3::ZERO),
        ];
        // launch_tick 99 -> sim tick i corresponds to game tick 100+i; tick 101 missing.
        let game_ticks = vec![tick(100, [0.0, 0.0, 0.0]), tick(102, [2.5, 0.0, 0.0])];
        let dev = align_deviations(&sim_ticks, 99, V3::new(2.0, 0.0, 0.0), &game_ticks);
        assert_eq!(dev, vec![(100, 0.0), (102, 0.5)]);
    }

    #[test]
    fn align_deviations_compares_post_rest_game_ticks_against_the_sim_rest() {
        // `s6v_fix1.md`: the sim trace ends once the throw settles (2 ticks here), but the game
        // keeps recording the grenade sitting there for longer before it detonates - those later
        // ticks used to never be compared against anything at all.
        let sim_ticks = vec![
            (V3::new(0.0, 0.0, 0.0), V3::ZERO),
            (V3::new(1.0, 0.0, 0.0), V3::ZERO),
        ];
        let sim_rest = V3::new(1.0, 0.0, 0.0);
        let game_ticks = vec![
            tick(100, [0.0, 0.0, 0.0]),
            tick(101, [1.0, 0.0, 0.0]),
            tick(102, [1.0, 0.0, 0.0]),
            tick(103, [1.5, 0.0, 0.0]),
        ];
        let dev = align_deviations(&sim_ticks, 99, sim_rest, &game_ticks);
        assert_eq!(dev, vec![(100, 0.0), (101, 0.0), (102, 0.0), (103, 0.5)]);
    }

    #[test]
    fn first_over_finds_the_first_exceeding_tick() {
        let dev = vec![(10, 0.1), (11, 0.9), (12, 1.5), (13, 6.0)];
        assert_eq!(first_over(&dev, 1.0), Some(12));
        assert_eq!(first_over(&dev, 5.0), Some(13));
        assert_eq!(first_over(&dev, 100.0), None);
    }

    #[test]
    fn game_velocity_kink_detects_a_synthetic_bounce() {
        let dt = TICK_DT;
        let g = 320.0f32;
        // Clean free fall x=100*t, z = -0.5*g*t^2, for ticks 0..10, then a bounce at tick 6:
        // velocity.z snaps from falling to rising, breaking the constant-acceleration model.
        let mut ticks = Vec::new();
        let mut vz = 0.0f32;
        let mut z = 0.0f32;
        let mut x = 0.0f32;
        for i in 0..12i64 {
            ticks.push(tick(i, [x, 0.0, z]));
            if i == 6 {
                vz = -vz * 0.6; // bounce: reflect and damp.
            }
            x += 100.0 * dt;
            vz -= g * dt;
            z += vz * dt;
        }
        assert!(game_velocity_kink(&ticks, 6, 2, g, KINK_ACCEL_TOLERANCE));
        assert!(!game_velocity_kink(&ticks, 1, 0, g, KINK_ACCEL_TOLERANCE));
    }

    #[test]
    fn game_velocity_kink_ignores_1_32_quantization_noise_on_a_smooth_parabola() {
        // `s6v_fix1.md`: positions rounded to 1/32u (game network quantization) must not read as
        // a kink - the raw finite-difference acceleration noise from that rounding can reach
        // ~443u/s^2 in 3-D, and `KINK_ACCEL_TOLERANCE` sits above that floor.
        let dt = TICK_DT;
        let g = 320.0f32;
        let round32 = |v: f32| (v * 32.0).round() / 32.0;
        let mut ticks = Vec::new();
        let mut vz = 0.0f32;
        let mut z = 0.0f32;
        let mut x = 0.0f32;
        for i in 0..20i64 {
            ticks.push(tick(i, [round32(x), 0.0, round32(z)]));
            x += 100.0 * dt;
            vz -= g * dt;
            z += vz * dt;
        }
        assert!(!game_velocity_kink(&ticks, 10, 2, g, KINK_ACCEL_TOLERANCE));
    }

    #[test]
    fn game_velocity_kink_uses_the_real_tick_delta_for_a_2_tick_snapshot_rate() {
        // `s6v_c_pro_demos.md`: tournament GOTV may record every 2nd tick - a fixed-1-tick-step
        // formula would compute a wildly wrong acceleration here and false-positive on ordinary
        // clean flight.
        let dt = TICK_DT;
        let g = 320.0f32;
        let mut vz = 0.0f32;
        let mut z = 0.0f32;
        let mut x = 0.0f32;
        let mut ticks = Vec::new();
        for i in 0..20i64 {
            if i % 2 == 0 {
                ticks.push(tick(i, [x, 0.0, z]));
            }
            x += 100.0 * dt;
            vz -= g * dt;
            z += vz * dt;
        }
        assert!(!game_velocity_kink(&ticks, 10, 4, g, 5.0));
    }

    fn launch_fixture() -> ProjectileLaunch {
        ProjectileLaunch {
            entity_id: 0,
            first_tick: 0,
            last_tick: 0,
            num_samples: 0,
            missing_ticks: 0,
            fit_ticks: 0,
            fit_residual_rms: 0.0,
            fit_residual_max: 0.0,
            fit_g: 320.0,
            launch_confident: true,
            launch_tick: 0,
            launch_pos: [0.0; 3],
            launch_vel: [0.0; 3],
            sample0_pos: [0.0; 3],
            sample0_vel: [0.0; 3],
            ticks: Vec::new(),
            game_rest: [0.0; 3],
            rest_source: "detonate_event".to_string(),
            detonate_tick: Some(0),
            thrower_steamid: None,
            thrower_name: None,
            player_near: Vec::new(),
            player_contact_possible: false,
            early_detonation_fire: false,
            nearest_fire_distance: None,
            notes: Vec::new(),
        }
    }

    fn replay_fixture(err: f32, contact: Option<(&str, V3)>) -> ThrowReplay {
        replay_fixture_with_tick(err, contact.is_some().then_some(10), contact)
    }

    fn replay_fixture_with_tick(
        err: f32,
        first_over_1u: Option<i64>,
        contact: Option<(&str, V3)>,
    ) -> ThrowReplay {
        ThrowReplay {
            entity_id: 0,
            sim_rest: V3::ZERO,
            game_rest: V3::ZERO,
            rest_graded: true,
            rest_error: err,
            aligned_ticks: 0,
            first_over_1u,
            first_over_5u: None,
            divergence_contact: contact.map(|(g, p)| DivergenceContact {
                tick: 0,
                point: p,
                normal: V3::new(0.0, 0.0, 1.0),
                group: g.to_string(),
                entity: None,
            }),
            game_also_bounced: false,
        }
    }

    #[test]
    fn player_contact_excludes_only_at_or_after_the_first_near_tick_minus_2() {
        let mut l = launch_fixture();
        l.player_contact_possible = true;
        l.player_near = vec![PlayerNear {
            tick: 100,
            distance: 3.0,
            steamid: "1".to_string(),
            is_thrower: false,
        }];
        assert!(player_contact_excludes(&l, Some(98)));
        assert!(player_contact_excludes(&l, Some(105)));
        assert!(!player_contact_excludes(&l, Some(90)));
        assert!(!player_contact_excludes(&l, None));

        let l2 = launch_fixture();
        assert!(!player_contact_excludes(&l2, Some(200)));
    }

    #[test]
    fn exclusion_reason_checks_in_priority_order() {
        let mut no_det = launch_fixture();
        no_det.rest_source = "last_position".to_string();
        assert_eq!(
            exclusion_reason(&no_det, &replay_fixture(0.0, None)),
            Some(Exclusion::NoDetonation)
        );

        let mut low_conf = launch_fixture();
        low_conf.launch_confident = false;
        assert_eq!(
            exclusion_reason(&low_conf, &replay_fixture(0.0, None)),
            Some(Exclusion::LowConfidence)
        );

        let mut player = launch_fixture();
        player.player_contact_possible = true;
        player.player_near = vec![PlayerNear {
            tick: 9,
            distance: 3.0,
            steamid: "1".to_string(),
            is_thrower: false,
        }];
        assert_eq!(
            exclusion_reason(&player, &replay_fixture_with_tick(5.0, Some(10), None)),
            Some(Exclusion::PlayerContact)
        );

        let mut fire = launch_fixture();
        fire.early_detonation_fire = true;
        assert_eq!(
            exclusion_reason(&fire, &replay_fixture(5.0, None)),
            Some(Exclusion::EarlyDetonationFire)
        );

        let glass = launch_fixture();
        assert_eq!(
            exclusion_reason(
                &glass,
                &replay_fixture(5.0, Some(("EntityBreakable", V3::ZERO)))
            ),
            Some(Exclusion::GlassStateUnknown)
        );

        let clean = launch_fixture();
        assert_eq!(
            exclusion_reason(&clean, &replay_fixture(5.0, Some(("Default", V3::ZERO)))),
            None
        );
    }

    #[test]
    fn summarize_counts_thresholds_and_ranks_surfaces() {
        let l_ok = launch_fixture();
        let mut l_no_det = launch_fixture();
        l_no_det.rest_source = "last_position".to_string();

        let r1 = replay_fixture(0.5, None);
        let r2 = replay_fixture(2.0, Some(("Default", V3::new(10.0, 10.0, 0.0))));
        let r3 = replay_fixture(9.0, Some(("Default", V3::new(20.0, 10.0, 0.0))));
        let r4 = replay_fixture(100.0, Some(("EntitySolid", V3::new(500.0, 0.0, 0.0))));

        let throws = vec![
            GradedThrow {
                launch: &l_ok,
                replay: &r1,
                map: "de_dust2",
            },
            GradedThrow {
                launch: &l_ok,
                replay: &r2,
                map: "de_dust2",
            },
            GradedThrow {
                launch: &l_ok,
                replay: &r3,
                map: "de_dust2",
            },
            GradedThrow {
                launch: &l_no_det,
                replay: &r4,
                map: "de_dust2",
            },
        ];
        let s = summarize(&throws);
        assert_eq!(s.n_graded, 3);
        assert_eq!(s.within_1u, 1);
        assert_eq!(s.within_3u, 2);
        assert_eq!(s.within_8u, 2);
        assert_eq!(s.excluded_no_detonation, 1);
        assert_eq!(s.surfaces[0].0, "Default");
        assert_eq!(s.surfaces[0].1, "de_dust2");
        assert_eq!(s.surfaces[0].3, 2);
    }

    #[test]
    fn summarize_excludes_player_contact_fire_and_glass_from_surfaces_and_grading() {
        let mut player = launch_fixture();
        player.player_contact_possible = true;
        player.player_near = vec![PlayerNear {
            tick: 9,
            distance: 3.0,
            steamid: "1".to_string(),
            is_thrower: false,
        }];
        let r_player = replay_fixture(50.0, Some(("Default", V3::new(1000.0, 0.0, 0.0))));

        let mut fire = launch_fixture();
        fire.early_detonation_fire = true;
        let r_fire = replay_fixture(50.0, Some(("Default", V3::new(2000.0, 0.0, 0.0))));

        let glass = launch_fixture();
        let r_glass = replay_fixture(50.0, Some(("EntityBreakable", V3::new(3000.0, 0.0, 0.0))));

        let throws = vec![
            GradedThrow {
                launch: &player,
                replay: &r_player,
                map: "de_mirage",
            },
            GradedThrow {
                launch: &fire,
                replay: &r_fire,
                map: "de_mirage",
            },
            GradedThrow {
                launch: &glass,
                replay: &r_glass,
                map: "de_mirage",
            },
        ];
        let s = summarize(&throws);
        assert_eq!(s.n_graded, 0);
        assert_eq!(s.excluded_player_contact, 1);
        assert_eq!(s.excluded_early_detonation_fire, 1);
        assert_eq!(s.excluded_glass_state_unknown, 1);
        assert!(s.surfaces.is_empty());
    }
}
