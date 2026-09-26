//! `cs2mod standspots|solve`.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, bail};
use extract::{StandSpotFile, StandSpotJson, StandSpotsState};
use geom::filter::{names_mask, player_mask};
use geom::grid::UniformGrid;
use geom::math::V3;
use sim::ThrowType;
use solver::rank;
use solver::standspots::{self, Stance};
use solver::target::{
    MapData, Phase, SolveHooks, SolveQuery, StandSpotOrigin, Target, solve_for_target,
};

use crate::cmd_extract::load_or_extract_mesh;
use crate::constants::resolve_constants;

/// `MeshSetup.cs:18` (`SingleTargetDefaultAttrs`): the default solid set for
/// single-target commands - `EntitySolid` also implies `EntityDoor`/
/// `EntityBreakable` (`geom::filter::names_mask`).
const SINGLE_TARGET_DEFAULT_ATTRS: [&str; 3] = ["Default", "default", "EntitySolid"];

fn stance_str(s: Stance) -> &'static str {
    match s {
        Stance::Standing => "Standing",
        Stance::Crouching => "Crouching",
        Stance::None => "None",
    }
}

/// `cs2mod standspots <MAP> [--force] [--step N]`. Ported from
/// `cs2-smoke-solver/src/Cli/Commands/StandSpotsCommand.cs`.
pub fn standspots(
    map: &str,
    game: Option<&Path>,
    cache: Option<&Path>,
    force: bool,
    step: f32,
) -> anyhow::Result<()> {
    let (mesh, dir) = load_or_extract_mesh(map, game, cache)?;
    let cache_path = dir.join("standspots.json");

    if !force
        && let StandSpotsState::Loaded(cached) = extract::load_stand_spots(&dir)
        && cached.step == step
    {
        print_summary(map, &cached);
        println!("  (cached; pass --force to recompute)");
        return Ok(());
    }

    let nav_path = dir.join("nav.json");
    if !nav_path.is_file() {
        bail!(
            "{} not found; run `cs2mod extract {map}` first",
            nav_path.display()
        );
    }
    let nav_areas = extract::load_nav_areas(&dir).map_err(|e| match e {
        extract::ExtractError::Io { .. } => anyhow::anyhow!(
            "{} not found; run `cs2mod extract {map}` first",
            nav_path.display()
        ),
        other => other.into(),
    })?;

    let Some((min, max)) = mesh.bounds() else {
        bail!("{map}'s mesh has no triangles");
    };
    let (min, max) = (V3::from_array(min), V3::from_array(max));

    // Player-solid, so the clip brushes that stop a player - and are
    // invisible to grenades - are exactly what bounds the standable set.
    let mask = player_mask(&mesh);
    let collider =
        UniformGrid::build(&mesh, &mask, None, 128.0).context("failed to build player collider")?;

    println!(
        "{map}: scanning {:.0} x {:.0} at {step}u with the 32x32x72 player hull",
        max.x - min.x,
        max.y - min.y
    );
    let start = Instant::now();
    let mut last_percent = -1i32;
    let spots = standspots::compute(
        &collider,
        &nav_areas,
        min,
        max,
        step,
        Some(&mut |done, total| {
            let percent = if total > 0 { done * 100 / total } else { 100 };
            if percent != last_percent && percent % 10 == 0 {
                last_percent = percent;
                print!("\r  columns {percent}%   ");
            }
            true
        }),
    );
    let elapsed = start.elapsed();

    let payload = StandSpotFile {
        version: extract::STANDSPOTS_VERSION,
        map: map.to_string(),
        step,
        spots: spots
            .iter()
            .map(|s| StandSpotJson {
                feet: [
                    (s.feet.x * 100.0).round_ties_even() / 100.0,
                    (s.feet.y * 100.0).round_ties_even() / 100.0,
                    (s.feet.z * 100.0).round_ties_even() / 100.0,
                ],
                stance: stance_str(s.stance).to_string(),
                nav: s.nav_covered,
            })
            .collect(),
    };
    extract::save_stand_spots(&dir, &payload)
        .with_context(|| format!("failed to write {}", cache_path.display()))?;

    println!(
        "\r  {} reachable stand spots in {:.1}s",
        spots.len(),
        elapsed.as_secs_f64()
    );
    print_summary(map, &payload);
    println!("  wrote {}", cache_path.display());
    Ok(())
}

fn print_summary(map: &str, payload: &StandSpotFile) {
    let total = payload.spots.len();
    let nav_covered = payload.spots.iter().filter(|s| s.nav).count();
    let recovered = total - nav_covered;
    let crouching = payload
        .spots
        .iter()
        .filter(|s| s.stance == "Crouching")
        .count();
    println!("{map}: {total} reachable stand spots");
    println!(
        "    {nav_covered} on the nav mesh, {recovered} the nav mesh misses ({:.1}%)",
        if total == 0 {
            0.0
        } else {
            100.0 * recovered as f64 / total as f64
        }
    );
    println!("    {crouching} reachable only crouched");
}

fn parse_vec2_or_3(s: &str) -> anyhow::Result<(V3, bool)> {
    let parts: Vec<f32> = s
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("invalid \"{s}\": expected \"x,y\" or \"x,y,z\""))?;
    anyhow::ensure!(
        parts.len() == 2 || parts.len() == 3,
        "invalid \"{s}\": expected \"x,y\" or \"x,y,z\", got {} values",
        parts.len()
    );
    let z = parts.get(2).copied().unwrap_or(0.0);
    Ok((V3::new(parts[0], parts[1], z), parts.len() > 2))
}

/// `--sightline x1,y1,z1:x2,y2,z2` (`s6r_sightline_target.md`): a pair of exact `x,y,z` eye
/// points, no 2D fallback (there is no nav-ground height to derive an eye position from).
fn parse_sightline_spec(s: &str) -> anyhow::Result<(V3, V3)> {
    let (a, b) = s
        .split_once(':')
        .with_context(|| format!("invalid \"{s}\": expected \"x1,y1,z1:x2,y2,z2\""))?;
    Ok((parse_vec3_exact(a)?, parse_vec3_exact(b)?))
}

fn parse_vec3_exact(s: &str) -> anyhow::Result<V3> {
    let parts: Vec<f32> = s
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("invalid \"{s}\": expected \"x,y,z\""))?;
    anyhow::ensure!(
        parts.len() == 3,
        "invalid \"{s}\": expected \"x,y,z\", got {} values",
        parts.len()
    );
    Ok(V3::new(parts[0], parts[1], parts[2]))
}

fn parse_throw_type(s: &str) -> anyhow::Result<ThrowType> {
    Ok(match s {
        "stand" => ThrowType::Stand,
        "crouch" => ThrowType::Crouch,
        "jump" => ThrowType::JumpThrow,
        "crouchjump" => ThrowType::CrouchJumpThrow,
        "runjump" => ThrowType::RunJumpThrow,
        other => bail!(
            "unknown throw type {other:?} in --types (expected stand|crouch|jump|crouchjump|runjump)"
        ),
    })
}

fn parse_click(s: &str) -> anyhow::Result<f32> {
    Ok(match s {
        "left" => 1.0,
        "both" => 0.5,
        "right" => 0.0,
        other => bail!("unknown click {other:?} in --clicks (expected left|both|right)"),
    })
}

/// A pasted `--getpos` line to an origin click + Z. Viewer convention
/// (`main.js:1940-1953`/`state.js:725`), not `ValidateCommand.cs`'s
/// unconditional `eye - 64`: a line with a `setang` part is the player's EYE
/// (feet = eye - (0,0,64.06), the standing eye height); a bare `setpos` with
/// no `setang` is already feet (a teleport target, not a look direction).
fn getpos_origin(gp: &str) -> anyhow::Result<([f32; 2], f32)> {
    let (eye, _pitch, _yaw) = calib::parse_getpos(gp).map_err(|e| anyhow::anyhow!("{e}"))?;
    let feet = if gp.to_lowercase().contains("setang") {
        eye - V3::new(0.0, 0.0, sim::STAND_EYE_HEIGHT)
    } else {
        eye
    };
    Ok(([feet.x, feet.y], feet.z))
}

/// `cs2mod solve <MAP> --target x,y[,z] ...`. Ported from
/// `cs2-smoke-solver/src/Cli/Commands/ValidateCommand.cs`'s target/origin
/// parsing and `LineupApi.cs:497-694,705-749` (`RunTargetQuery`/`Rank`).
#[allow(clippy::too_many_arguments)]
pub fn solve(
    map: &str,
    target_spec: Option<&str>,
    tolerance: f32,
    sightline_spec: Option<&str>,
    from_spec: Option<&str>,
    getpos_spec: Option<&str>,
    reach: Option<f32>,
    exact: bool,
    fine: bool,
    types_spec: Option<&str>,
    clicks_spec: Option<&str>,
    broken_spec: Option<&str>,
    spawns_spec: Option<&str>,
    pin_spec: Option<&str>,
    precise_aim: bool,
    referee: bool,
    top: usize,
    json_out: Option<&Path>,
    constants_path: Option<&Path>,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<()> {
    if let Some(out) = json_out {
        crate::game_path::ensure_write_allowed(out)?;
    }

    let (mesh, dir) = load_or_extract_mesh(map, game, cache)?;

    let nav_areas = extract::load_nav_areas(&dir)?;

    let standspots_path = dir.join("standspots.json");
    let stand_spots: Option<Vec<StandSpotOrigin>> = match extract::load_stand_spots(&dir) {
        StandSpotsState::Loaded(cached) => Some(
            cached
                .spots
                .iter()
                .map(|s| StandSpotOrigin {
                    feet: V3::new(s.feet[0], s.feet[1], s.feet[2]),
                    crouched: s.stance == "Crouching",
                })
                .collect(),
        ),
        StandSpotsState::Stale { .. } => {
            println!(
                "hint: {} is from an older standspots version - run `cs2mod standspots {map} --force` to refresh it; falling back to nav-mesh origins for now",
                standspots_path.display()
            );
            None
        }
        // Unreadable or unparsable (a truncated/corrupt file, a format from
        // before this port existed, ...): the same "treat it as missing"
        // fallback as a stale version, not a hard failure - the command
        // still has a working answer via nav-mesh origins.
        StandSpotsState::Unreadable => {
            println!(
                "hint: {} is unreadable or not valid JSON - run `cs2mod standspots {map} --force` to rebuild it; falling back to nav-mesh origins for now",
                standspots_path.display()
            );
            None
        }
        StandSpotsState::Missing => {
            println!(
                "hint: no {} - run `cs2mod standspots {map}` for the precomputed hull-checked origin set; falling back to nav-mesh origins",
                standspots_path.display()
            );
            None
        }
    };

    let spawns = extract::load_spawns(&dir)?;
    let t_spawns = spawns.t;
    let ct_spawns = spawns.ct;
    let mut all_spawns = t_spawns.clone();
    all_spawns.extend(ct_spawns.iter().copied());

    // `MapRegistry.cs:299-317` (`SpawnFronts`): one representative point per
    // side - T first, then CT - skipping an empty side, not every spawn.
    let mut spawn_fronts = Vec::new();
    if !t_spawns.is_empty() {
        spawn_fronts.push(t_spawns[t_spawns.len() / 2]);
    }
    if !ct_spawns.is_empty() {
        spawn_fronts.push(ct_spawns[ct_spawns.len() / 2]);
    }

    // `s6r_sightline_target.md`: `--sightline` builds `Target::Sightline` instead of the usual
    // `Target::Point`; mutually exclusive with `--target` (`required_unless_present`/
    // `conflicts_with` on the clap args already enforce this for real CLI use, but `solve` is
    // also a plain function, so both ends are checked here too).
    let target_kind = match (target_spec, sightline_spec) {
        (Some(_), Some(_)) => bail!("--target and --sightline are mutually exclusive"),
        (None, None) => bail!("either --target or --sightline is required"),
        (Some(t), None) => {
            let (pos, has_z) = parse_vec2_or_3(t)?;
            Target::Point {
                pos,
                has_z,
                tolerance,
            }
        }
        (None, Some(sl)) => {
            let (from, to) = parse_sightline_spec(sl)?;
            Target::Sightline { from, to }
        }
    };

    let (origin_click, origin_z) = if let Some(gp) = getpos_spec {
        let (click, z) = getpos_origin(gp)?;
        (Some(click), Some(z))
    } else if let Some(spec) = from_spec {
        let (v, has_z) = parse_vec2_or_3(spec)?;
        (Some([v.x, v.y]), if has_z { Some(v.z) } else { None })
    } else {
        (None, None)
    };

    let types = match types_spec {
        Some(s) => Some(
            s.split(',')
                .map(str::trim)
                .map(parse_throw_type)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let strengths = match clicks_spec {
        Some(s) => Some(
            s.split(',')
                .map(str::trim)
                .map(parse_click)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let broken_groups: Vec<String> = match broken_spec {
        Some(s) => {
            let mut groups: Vec<String> = s
                .split(',')
                .map(str::trim)
                .map(|g| match g {
                    "glass" => Ok("EntityBreakable".to_string()),
                    "doors" => Ok("EntityDoor".to_string()),
                    other => bail!("unknown group {other:?} in --broken (expected glass|doors)"),
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            groups.sort();
            groups.dedup();
            groups
        }
        None => Vec::new(),
    };
    let (spawns_only, spawn_points) = match spawns_spec {
        Some("t") => (true, t_spawns.clone()),
        Some("ct") => (true, ct_spawns.clone()),
        Some("all") => (true, all_spawns.clone()),
        Some(other) => bail!("unknown --spawns {other:?} (expected t|ct|all)"),
        None => (false, Vec::new()),
    };
    if spawns_only && spawn_points.is_empty() {
        bail!(
            "--spawns {} matched no spawn entities in {map}'s entities.json (checked info_player_terrorist/info_player_counterterrorist, excluding 2v2 spawns) - run `cs2mod extract {map}` first if this looks wrong",
            spawns_spec.unwrap()
        );
    }
    let origin_pin_min: u8 = match pin_spec {
        Some("corner") => 2,
        Some("wall") => 1,
        Some(other) => bail!("unknown --pin {other:?} (expected corner|wall)"),
        None => 0,
    };

    // `MeshSetup.cs:18,53-75` (`SingleTargetDefaultAttrs`).
    let attribute_filter = Some(names_mask(&mesh, &SINGLE_TARGET_DEFAULT_ATTRS));
    let map_data = MapData {
        mesh,
        nav_areas,
        stand_spots,
        spawns: all_spawns.clone(),
        attribute_filter,
    };
    // `LineupApi.cs:527-529`: 300u with an origin click, 3100u map-wide.
    let reach = reach.unwrap_or(if origin_click.is_some() {
        300.0
    } else {
        3100.0
    });
    let query = SolveQuery {
        target: target_kind,
        origin_click,
        origin_z,
        origin_reach: reach,
        origin_area: None,
        min_stability: 0.4,
        fine_scan: fine,
        types,
        strengths,
        broken_groups,
        spawn_fronts,
        spawn_points,
        spawn_scope_radius: 0.0,
        spawns_only,
        exact_origin: exact,
        referee,
        origin_pin_min,
        precise_aim,
    };
    let constants = resolve_constants(constants_path)?;
    let cancel = AtomicBool::new(false);

    match &query.target {
        Target::Point { pos, has_z, .. } => println!(
            "solving target ({:.0},{:.0}{}) tolerance {tolerance:.0}u ...",
            pos.x,
            pos.y,
            if *has_z {
                format!(",{:.0}", pos.z)
            } else {
                String::new()
            }
        ),
        Target::Sightline { from, to } => println!(
            "solving sightline ({:.0},{:.0},{:.0}) -> ({:.0},{:.0},{:.0}) ...",
            from.x, from.y, from.z, to.x, to.y, to.z
        ),
        Target::Area(_) => println!("solving target area ..."),
    }

    let started = Instant::now();
    let last: Mutex<(Instant, Option<Phase>)> = Mutex::new((started, None));
    let progress = |phase: Phase, count: usize| {
        let mut last = last.lock().unwrap();
        let (last_time, last_phase) = *last;
        if let Some(prev) = last_phase {
            println!("  {prev:?} took {:.3?}", last_time.elapsed());
        }
        println!("  -> {phase:?} ({count})");
        *last = (Instant::now(), Some(phase));
    };
    let hooks = SolveHooks {
        progress: &progress,
        on_origin: None,
        on_candidate: None,
    };
    let solve = solve_for_target(&map_data, &query, &constants, &hooks, &cancel);
    let total = started.elapsed();
    {
        let (last_time, last_phase) = *last.lock().unwrap();
        if let Some(prev) = last_phase {
            println!("  {prev:?} took {:.3?}", last_time.elapsed());
        }
    }

    // `s6r_sightline_target.md`'s own check: `SolveCommand.cs`'s "eye rays: X/Y geometry-clear" /
    // "landing zone: N cells" console lines, ported to whichever count `zone::solve` produced.
    if let Some(z) = &solve.sightline_zone {
        println!(
            "eye rays: {}/{} geometry-clear",
            z.clear_pairs, z.total_pairs
        );
        println!("landing zone: {} cells", z.zone_cells);
    }
    println!(
        "target resolved to ({:.0},{:.0},{:.0}); {} origins; {} lineups in {:.2}s",
        solve.target.x,
        solve.target.y,
        solve.target.z,
        solve.origins,
        solve.lineups.len(),
        total.as_secs_f64()
    );
    if let Some(why) = &solve.empty_reason {
        println!("  {why}");
    }
    if referee {
        match (&solve.referee, &solve.referee_notes) {
            (Some(rf), Some(notes)) => {
                println!(
                    "referee: {} exhaustive lineups, {} miss note(s)",
                    rf.len(),
                    notes.len()
                );
                for n in notes {
                    println!("  - {n}");
                }
            }
            _ => println!("referee: not run (only applies to exact-origin solves, --exact)"),
        }
    }

    // `s6q_robust_aim.md`: "-" for a field precise mode never computed (also every field, outside
    // precise mode), a fixed 2-decimal value otherwise.
    let fmt_opt = |v: Option<f32>| match v {
        Some(x) => format!("{x:.2}"),
        None => "-".to_string(),
    };
    let ranked_list = rank::ranked(&solve, origin_click);
    for (i, rl) in ranked_list.iter().take(top).enumerate() {
        let l = &rl.lineup;
        let dist = ((l.rest_point.x - solve.target.x).powi(2)
            + (l.rest_point.y - solve.target.y).powi(2))
        .sqrt();
        println!(
            "{:>3}. {}  [{}]  rest ({:.0},{:.0},{:.0}) dist {:.0}u  bounces {} flight {:.2}s  stability {:.2} stability_wide {:.2} scatter {:.0}u  human_error {:.0}u  pin {}  exposed {}  robustness {} robust_aim {} robust_pos {} robust_model {} aim_margin {}  exact: {}{}",
            i + 1,
            rl.console,
            rl.describe,
            l.rest_point.x,
            l.rest_point.y,
            l.rest_point.z,
            dist,
            l.bounces,
            l.flight_time,
            l.stability,
            l.stability_wide,
            l.rest_scatter,
            rl.human_error,
            rl.pin,
            l.direct_los,
            fmt_opt(l.robustness),
            fmt_opt(l.robust_aim),
            fmt_opt(l.robust_pos),
            fmt_opt(l.robust_model),
            fmt_opt(l.aim_margin_deg),
            rl.console_exact.as_deref().unwrap_or("-"),
            match (l.blocks_sightline, l.smoke_cells_crossed) {
                (Some(b), Some(c)) => format!("  blocks_sightline {b} smoke_cells_crossed {c}"),
                _ => String::new(),
            },
        );
    }

    if let Some(out) = json_out {
        let json = server::solve::json_payload(&solve, &ranked_list, None);
        let tmp = out.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&json)?)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, out).with_context(|| format!("failed to write {}", out.display()))?;
        println!("wrote {}", out.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vec2_or_3_accepts_2d_and_3d() {
        let (v, has_z) = parse_vec2_or_3("-662.5,-1612.5").unwrap();
        assert_eq!((v.x, v.y, v.z, has_z), (-662.5, -1612.5, 0.0, false));
        let (v, has_z) = parse_vec2_or_3("-662.5,-1612.5,-120").unwrap();
        assert_eq!((v.x, v.y, v.z, has_z), (-662.5, -1612.5, -120.0, true));
        assert!(parse_vec2_or_3("1,2,3,4").is_err());
        assert!(parse_vec2_or_3("1").is_err());
    }

    #[test]
    fn getpos_origin_with_setang_subtracts_stand_eye_height() {
        let (click, z) = getpos_origin("setpos -104.5 -1796 -104; setang -10 90").unwrap();
        assert_eq!(click, [-104.5, -1796.0]);
        assert!((z - (-104.0 - sim::STAND_EYE_HEIGHT)).abs() < 1e-3);
    }

    #[test]
    fn getpos_origin_without_setang_is_already_feet() {
        let (click, z) = getpos_origin("setpos 32 -1696 -168").unwrap();
        assert_eq!(click, [32.0, -1696.0]);
        assert_eq!(z, -168.0);
    }

    #[test]
    fn parse_sightline_spec_reads_both_eye_points() {
        let (from, to) = parse_sightline_spec("-1100,-640,64:-250,-600,64").unwrap();
        assert_eq!((from.x, from.y, from.z), (-1100.0, -640.0, 64.0));
        assert_eq!((to.x, to.y, to.z), (-250.0, -600.0, 64.0));
    }

    #[test]
    fn parse_sightline_spec_rejects_missing_colon_or_bad_component_count() {
        assert!(parse_sightline_spec("-1100,-640,64").is_err());
        assert!(parse_sightline_spec("-1100,-640:-250,-600,64").is_err());
    }
}
