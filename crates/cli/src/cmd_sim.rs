//! `cs2mod throw|smoke|sightline|replay|calibrate`.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use geom::collider::Collider;
use geom::filter::{grenade_mask, parse_filter};
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::CollisionMesh;
use geom::voxel::VoxelGrid;
use sim::{SmokeParams, ThrowSpec, ThrowType, Trace, eye_height, simulate_exact, smoke_fill};

use crate::cmd_extract::load_or_extract_mesh;
use crate::game_path::ensure_write_allowed;

fn parse_throw_type(s: &str) -> anyhow::Result<ThrowType> {
    Ok(match s {
        "stand" => ThrowType::Stand,
        "crouch" => ThrowType::Crouch,
        "jump" => ThrowType::JumpThrow,
        "crouchjump" => ThrowType::CrouchJumpThrow,
        "runjump" => ThrowType::RunJumpThrow,
        other => bail!("unknown --type {other:?} (expected stand|crouch|jump|crouchjump|runjump)"),
    })
}

fn parse_click(s: &str) -> anyhow::Result<f32> {
    Ok(match s {
        "left" => 1.0,
        "both" => 0.5,
        "right" => 0.0,
        other => bail!("unknown --click {other:?} (expected left|both|right)"),
    })
}

fn parse_vec3(s: &str, flag: &str) -> anyhow::Result<V3> {
    let parts: Vec<f32> = s
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("invalid {flag} {s:?}: expected \"x,y,z\""))?;
    anyhow::ensure!(
        parts.len() == 3,
        "invalid {flag} {s:?}: expected 3 comma-separated numbers, got {}",
        parts.len()
    );
    Ok(V3::new(parts[0], parts[1], parts[2]))
}

fn parse_pair(s: &str, flag: &str) -> anyhow::Result<(f32, f32)> {
    let parts: Vec<f32> = s
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("invalid {flag} {s:?}: expected \"a,b\""))?;
    anyhow::ensure!(
        parts.len() == 2,
        "invalid {flag} {s:?}: expected 2 comma-separated numbers, got {}",
        parts.len()
    );
    Ok((parts[0], parts[1]))
}

use crate::constants::resolve_constants;

/// Mirrors `geom::filter::grenade_mask`'s per-attribute predicate
/// (`CollisionMesh.cs:GrenadeSolidFilter`), for building a second collider
/// that also excludes a named group (glass-gone).
fn grenade_solid_predicate(a: &geom::mesh::CollisionAttribute) -> bool {
    let any_ci =
        |layers: &[String], name: &str| layers.iter().any(|l| l.eq_ignore_ascii_case(name));
    !any_ci(&a.interact_exclude, "csgo_thrown_grenade")
        && !any_ci(&a.interact_as, "playerclip")
        && !any_ci(&a.interact_as, "npcclip")
        && !any_ci(&a.interact_as, "sky")
}

fn grenade_collider(mesh: &CollisionMesh) -> anyhow::Result<UniformGrid> {
    let mask = grenade_mask(mesh);
    Ok(UniformGrid::build(mesh, &mask, None, 128.0)?)
}

/// `cs2mod throw <MAP> ...`.
#[allow(clippy::too_many_arguments)]
pub fn throw(
    map: &str,
    pos: Option<&str>,
    eye: Option<&str>,
    ang: Option<&str>,
    throw_type: &str,
    click: &str,
    run_deg: f32,
    getpos: Option<&str>,
    constants: Option<&Path>,
    trace_out: Option<&Path>,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    // Fail fast on a bad output path before doing any real work.
    if let Some(out) = trace_out {
        ensure_write_allowed(out)?;
    }

    let throw_type = parse_throw_type(throw_type)?;
    let strength = parse_click(click)?;

    let (eye_pos, pitch, yaw) = if let Some(gp) = getpos {
        calib::parse_getpos(gp).map_err(|e| anyhow::anyhow!("{e}"))?
    } else if let Some(eye) = eye {
        let e = parse_vec3(eye, "--eye")?;
        let (pitch, yaw) = ang
            .map(|a| parse_pair(a, "--ang"))
            .transpose()?
            .context("--eye requires --ang pitch,yaw")?;
        (e, pitch, yaw)
    } else if let Some(pos) = pos {
        let feet = parse_vec3(pos, "--pos")?;
        let (pitch, yaw) = ang
            .map(|a| parse_pair(a, "--ang"))
            .transpose()?
            .context("--pos requires --ang pitch,yaw")?;
        (feet + V3::new(0.0, 0.0, eye_height(throw_type)), pitch, yaw)
    } else {
        bail!("one of --pos, --eye, or --getpos is required");
    };

    let spec = ThrowSpec {
        eye: eye_pos,
        yaw_deg: yaw,
        pitch_deg: pitch,
        throw_type,
        strength,
        run_yaw_offset_deg: run_deg,
    };
    let k = resolve_constants(constants)?;

    let (mesh, _dir) = load_or_extract_mesh(map, game, cache)?;
    let collider = grenade_collider(&mesh)?;

    let mut ticks: Vec<(V3, V3)> = Vec::new();
    let mut bounces: Vec<sim::BounceRecord> = Vec::new();
    let trace = if trace_out.is_some() {
        Trace {
            ticks: Some(&mut ticks),
            bounces: Some(&mut bounces),
        }
    } else {
        Trace::default()
    };
    let result = simulate_exact(&collider, &spec, &k, trace);

    println!(
        "eye ({:.2},{:.2},{:.2}) pitch {:.1} yaw {:.1}",
        spec.eye.x, spec.eye.y, spec.eye.z, spec.pitch_deg, spec.yaw_deg
    );
    let (launch_pos, launch_vel) = sim::derive_initial(&spec, &k);
    println!(
        "derived launch pos ({:.2},{:.2},{:.2}) vel ({:.2},{:.2},{:.2})",
        launch_pos.x, launch_pos.y, launch_pos.z, launch_vel.x, launch_vel.y, launch_vel.z
    );
    println!(
        "rest ({:.2},{:.2},{:.2})  bounces {}  flight {:.2}s  lost {}  glass breaks {}",
        result.rest.x,
        result.rest.y,
        result.rest.z,
        result.bounces,
        result.flight_time,
        result.lost,
        result.glass_breaks
    );
    // `CliParsing.cs:SetposCommand`'s `feet.Z + 1`: teleporting exactly onto
    // the floor can leave the player embedded in it by float error, so the
    // engine is given one extra unit of headroom to settle down onto it from.
    // Printed to 2 decimals (the reference's `F0`) for reproduction precision.
    let feet = spec.eye - V3::new(0.0, 0.0, eye_height(throw_type));
    println!(
        "setpos {:.2} {:.2} {:.2}; setang {:.1} {:.1} 0",
        feet.x,
        feet.y,
        feet.z + 1.0,
        spec.pitch_deg,
        spec.yaw_deg
    );

    if let Some(out) = trace_out {
        #[derive(serde::Serialize)]
        struct TraceTick {
            pos: [f32; 3],
            vel: [f32; 3],
        }
        #[derive(serde::Serialize)]
        struct TraceBounce {
            tick: u32,
            contact: [f32; 3],
            normal: [f32; 3],
        }
        #[derive(serde::Serialize)]
        struct TraceOut {
            ticks: Vec<TraceTick>,
            bounces: Vec<TraceBounce>,
        }
        let out_data = TraceOut {
            ticks: ticks
                .iter()
                .map(|(p, v)| TraceTick {
                    pos: [p.x, p.y, p.z],
                    vel: [v.x, v.y, v.z],
                })
                .collect(),
            bounces: bounces
                .iter()
                .map(|b| TraceBounce {
                    tick: b.tick,
                    contact: [b.contact.x, b.contact.y, b.contact.z],
                    normal: [b.normal.x, b.normal.y, b.normal.z],
                })
                .collect(),
        };
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, serde_json::to_string_pretty(&out_data)?)?;
        println!("wrote trace to {}", out.display());
    }
    // `ThrowCommand.cs:98`: `result.Lost ? 2 : 0`.
    Ok(if result.lost { 2 } else { 0 })
}

fn parse_smoke_params(name: &str) -> anyhow::Result<SmokeParams> {
    Ok(match name {
        "full" => SmokeParams::FULL_REACH,
        "coverage" => SmokeParams::COVERAGE,
        "conservative" => SmokeParams::CONSERVATIVE,
        "uncalibrated" => SmokeParams::UNCALIBRATED_DEFAULT,
        other => {
            bail!("unknown --params {other:?} (expected full|coverage|conservative|uncalibrated)")
        }
    })
}

const SMOKE_VOXEL_SIZE: f32 = 16.0;

/// The reference's single `attributeFilter` (`MeshSetup.cs:LoadCommon`/
/// `BuildAndFill`), used identically for the voxel grid smoke fills into and
/// (for `sightline`) the exact geometry check runs against.
fn vision_mask(mesh: &CollisionMesh, attrs: &str) -> anyhow::Result<geom::filter::AttributeMask> {
    Ok(parse_filter(&format!("attrs:{attrs}"), mesh)?)
}

fn build_local_voxel_grid(
    mesh: &CollisionMesh,
    mask: &geom::filter::AttributeMask,
    center: V3,
    pad: f32,
) -> anyhow::Result<VoxelGrid> {
    let half = V3::new(pad, pad, pad);
    let bounds = Aabb {
        min: center - half,
        max: center + half,
    };
    Ok(VoxelGrid::build(mesh, mask, SMOKE_VOXEL_SIZE, bounds)?)
}

/// `cs2mod smoke <MAP> --rest x,y,z ...`.
pub fn smoke(
    map: &str,
    rest: &str,
    params: &str,
    attrs: &str,
    obj_out: Option<&Path>,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    if let Some(out) = obj_out {
        ensure_write_allowed(out)?;
    }
    let rest = parse_vec3(rest, "--rest")?;
    let p = parse_smoke_params(params)?;
    let (mesh, _dir) = load_or_extract_mesh(map, game, cache)?;
    let mask = vision_mask(&mesh, attrs)?;
    let pad = p.max_radius * p.contained_stretch + 4.0 * SMOKE_VOXEL_SIZE;
    let grid = build_local_voxel_grid(&mesh, &mask, rest, pad)?;
    let volume = smoke_fill(&grid, rest, &p)?;

    let bounds = volume.cells.iter().fold(
        (
            V3::new(f32::MAX, f32::MAX, f32::MAX),
            V3::new(f32::MIN, f32::MIN, f32::MIN),
        ),
        |(min, max), &c| {
            let center = grid.cell_center(c as usize);
            (
                V3::new(
                    min.x.min(center.x),
                    min.y.min(center.y),
                    min.z.min(center.z),
                ),
                V3::new(
                    max.x.max(center.x),
                    max.y.max(center.y),
                    max.z.max(center.z),
                ),
            )
        },
    );
    println!("cells: {}", volume.cells.len());
    if !volume.cells.is_empty() {
        println!(
            "bounds: ({:.0},{:.0},{:.0}) - ({:.0},{:.0},{:.0})",
            bounds.0.x, bounds.0.y, bounds.0.z, bounds.1.x, bounds.1.y, bounds.1.z
        );
    }

    if let Some(out) = obj_out {
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(out)?;
        let h = SMOKE_VOXEL_SIZE / 2.0;
        let mut vcount = 0u32;
        for &c in &volume.cells {
            let center = grid.cell_center(c as usize);
            let corners = [
                center + V3::new(-h, -h, -h),
                center + V3::new(h, -h, -h),
                center + V3::new(h, h, -h),
                center + V3::new(-h, h, -h),
                center + V3::new(-h, -h, h),
                center + V3::new(h, -h, h),
                center + V3::new(h, h, h),
                center + V3::new(-h, h, h),
            ];
            for v in &corners {
                writeln!(f, "v {} {} {}", v.x, v.y, v.z)?;
            }
            let b = vcount + 1;
            let faces: [[u32; 4]; 6] = [
                [b, b + 1, b + 2, b + 3],
                [b + 4, b + 7, b + 6, b + 5],
                [b, b + 4, b + 5, b + 1],
                [b + 3, b + 2, b + 6, b + 7],
                [b, b + 3, b + 7, b + 4],
                [b + 1, b + 5, b + 6, b + 2],
            ];
            for face in faces {
                writeln!(f, "f {} {} {} {}", face[0], face[1], face[2], face[3])?;
            }
            vcount += 8;
        }
        println!("wrote {} ({} cells)", out.display(), volume.cells.len());
    }
    // `SmokeCommand.cs:24`: `smoke.Cells.Length > 0 ? 0 : 2`.
    Ok(if volume.cells.is_empty() { 2 } else { 0 })
}

/// `cs2mod sightline <MAP> --from x,y,z --to x,y,z [--rest x,y,z] [--params ...]`.
#[allow(clippy::too_many_arguments)]
pub fn sightline(
    map: &str,
    from: &str,
    to: &str,
    rest: Option<&str>,
    params: &str,
    attrs: &str,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    let from = parse_vec3(from, "--from")?;
    let to = parse_vec3(to, "--to")?;
    let (mesh, _dir) = load_or_extract_mesh(map, game, cache)?;
    let mask = vision_mask(&mesh, attrs)?;

    let bvh = geom::bvh::Bvh::build(&mesh, &mask, None)?;
    let geometry_blocked = bvh.blocked(from, to);
    println!("geometry blocked: {geometry_blocked}");

    let Some(rest) = rest else {
        // `--rest` is our own addition (the reference always requires it);
        // with no smoke to check, the geometry check alone decides the
        // exit code: 0 clear, 2 blocked.
        return Ok(if geometry_blocked { 2 } else { 0 });
    };
    let rest = parse_vec3(rest, "--rest")?;
    let p = parse_smoke_params(params)?;
    let pad = p.max_radius * p.contained_stretch + 4.0 * SMOKE_VOXEL_SIZE;
    let extent = pad.max((from - rest).length()).max((to - rest).length()) + SMOKE_VOXEL_SIZE;
    let grid = build_local_voxel_grid(&mesh, &mask, rest, extent)?;
    let result = sim::smoke_blocks_sightline(&grid, from, to, rest, &p)?;
    println!("smoke cells crossed: {}", result.smoke_cells_crossed);
    println!("voxel geometry blocked: {}", result.geometry_blocked);
    // `Occlusion.cs:SmokeBlocked`: `SmokeCellsCrossed >= minSmokeCells`, with
    // no dependence on `geometry_blocked` (a sightline through smoke that
    // also grazes solid geometry is still graded purely on smoke coverage).
    let blocked = result.smoke_cells_crossed >= sim::MIN_SMOKE_CELLS_BLOCKED;
    println!(
        "{}",
        if blocked {
            "BLOCKED by smoke"
        } else {
            "NOT blocked by smoke"
        }
    );
    Ok(if blocked { 0 } else { 2 })
}

/// `cs2mod replay <MAP>... [--corpus DIR] [--build ID] [--worst N] [--json out.json]`.
#[allow(clippy::too_many_arguments)]
pub fn replay(
    maps: &[String],
    all: bool,
    corpus_dir: Option<&Path>,
    build: Option<&str>,
    worst: usize,
    json_out: Option<&Path>,
    constants: Option<&Path>,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    if let Some(out) = json_out {
        ensure_write_allowed(out)?;
    }
    let k = resolve_constants(constants)?;

    let corpus_dir = corpus_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CS2MOD_CORPUS").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(r"D:\porject\refs\cs2-smoke-solver\data\validation"));

    let maps: Vec<String> = if all {
        let mut names: Vec<String> = std::fs::read_dir(&corpus_dir)
            .with_context(|| format!("failed to read corpus dir {}", corpus_dir.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if !name.ends_with(".json") {
                    return None;
                }
                let stem = name.strip_suffix(".json")?;
                // Map names have no hyphens (`de_mirage`, `cs_italy`); a
                // report's own `<timestamp>` (everything after the first
                // hyphen) is discarded here. `campaign-report.md` (a real
                // file in the reference corpus) never reaches this split:
                // it's already excluded by the `.json` filter above.
                let map = stem.split_once('-')?.0.to_string();
                Some(map)
            })
            .collect();
        names.sort();
        names.dedup();
        names
    } else {
        maps.to_vec()
    };
    anyhow::ensure!(!maps.is_empty(), "no maps given (use --all or name maps)");

    #[derive(serde::Serialize)]
    struct JsonOut {
        maps: Vec<serde_json::Value>,
    }
    let mut json_maps = Vec::new();

    let started = std::time::Instant::now();
    let mut overall_n = 0usize;
    let mut overall_within3 = 0.0f64;

    for map in &maps {
        let mut rows = match calib::load_corpus(&corpus_dir, map) {
            Ok(rows) => rows,
            Err(e) if all => {
                println!("{map}: warning: skipping, corpus failed to parse: {e}");
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("failed to load corpus for {map}"));
            }
        };
        if let Some(b) = build {
            rows.retain(|r| r.build == b);
        }
        if rows.is_empty() {
            println!("{map}: no graded throws under {}", corpus_dir.display());
            continue;
        }

        let (mesh, dir) = load_or_extract_mesh(map, game, cache)?;
        let manifest = extract::cache::load_manifest(&dir)?;
        let solid = grenade_mask(&mesh);
        let collider = UniformGrid::build(&mesh, &solid, None, 128.0)?;
        let has_glass = mesh.attributes.iter().any(|a| a.name == "EntityBreakable");
        let glass_gone = if has_glass {
            let mask = geom::filter::from_fn(&mesh, |a| {
                a.name != "EntityBreakable" && grenade_solid_predicate(a)
            });
            Some(UniformGrid::build(&mesh, &mask, None, 128.0)?)
        } else {
            None
        };

        let report = calib::replay(&collider, glass_gone.as_ref(), &rows, &k, worst);

        overall_n += report.overall.n;
        overall_within3 += report.overall.within_3_pct as f64 * report.overall.n as f64;

        println!(
            "{map}: {} throws  median {:.2}u  p90 {:.1}u  p99 {:.0}u  within 3u {:.1}%  over 8u {} ({:.1}%)",
            report.overall.n,
            report.overall.median,
            report.overall.p90,
            report.overall.p99,
            report.overall.within_3_pct,
            report.overall.over_8,
            report.overall.over_8_pct
        );
        // `ReplayCommand.cs:189`.
        println!(
            "  as reported at the time: median {:.2}u  over 8u {}",
            report.reported.median, report.reported.over_8
        );
        println!("  mesh build {}", manifest.meta.game_build);
        for (b, m) in &report.per_build {
            println!(
                "  build {b}: {} throws, within 3u {:.1}%, over 8u {} ({:.1}%)",
                m.n, m.within_3_pct, m.over_8, m.over_8_pct
            );
        }
        for (cls, m) in &report.per_divergence_class {
            println!(
                "  {cls}: {} throws, within 3u {:.1}%, median {:.2}u",
                m.n, m.within_3_pct, m.median
            );
        }
        if report.glass_throws > 0 {
            println!(
                "  throws that break glass: {}; graded against pane gone: {}",
                report.glass_throws, report.glass_corrected
            );
        }
        println!(
            "  throws whose error moved by more than 1u since graded: {}",
            report.moved_since_graded
        );
        for w in &report.worst {
            println!(
                "  {:.0}u  {} [{}] launch ({:.0},{:.0},{:.0}) sim rest ({:.0},{:.0},{:.0}) real ({:.0},{:.0},{:.0})  (was {:.0}u)",
                w.error,
                w.report,
                w.index,
                w.launch_pos.x,
                w.launch_pos.y,
                w.launch_pos.z,
                w.sim_rest.x,
                w.sim_rest.y,
                w.sim_rest.z,
                w.real_rest.x,
                w.real_rest.y,
                w.real_rest.z,
                w.reported
            );
        }

        let metrics_json = |m: &calib::Metrics| {
            serde_json::json!({
                "n": m.n, "median": m.median, "p90": m.p90, "p99": m.p99,
                "within_3_pct": m.within_3_pct, "over_8": m.over_8, "over_8_pct": m.over_8_pct,
            })
        };
        json_maps.push(serde_json::json!({
            "map": map,
            "overall": metrics_json(&report.overall),
            "reported": metrics_json(&report.reported),
            "per_build": report.per_build.iter().map(|(b, m)| serde_json::json!({"build": b, "metrics": metrics_json(m)})).collect::<Vec<_>>(),
            "per_divergence_class": report.per_divergence_class.iter().map(|(c, m)| serde_json::json!({"class": c, "metrics": metrics_json(m)})).collect::<Vec<_>>(),
            "worst": report.worst.iter().map(|w| serde_json::json!({
                "report": w.report, "index": w.index, "error": w.error, "reported": w.reported,
                "launch_pos": [w.launch_pos.x, w.launch_pos.y, w.launch_pos.z],
                "sim_rest": [w.sim_rest.x, w.sim_rest.y, w.sim_rest.z],
                "real_rest": [w.real_rest.x, w.real_rest.y, w.real_rest.z],
            })).collect::<Vec<_>>(),
            "glass_throws": report.glass_throws,
            "glass_corrected": report.glass_corrected,
            "moved_since_graded": report.moved_since_graded,
        }));
    }

    anyhow::ensure!(
        overall_n > 0,
        "no graded throws found under {}",
        corpus_dir.display()
    );
    println!(
        "overall: {overall_n} throws  within 3u {:.1}%",
        overall_within3 / overall_n as f64
    );
    println!("time: {:.1}s", started.elapsed().as_secs_f64());

    if let Some(out) = json_out {
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            out,
            serde_json::to_string_pretty(&JsonOut { maps: json_maps })?,
        )?;
    }
    Ok(0)
}

/// `cs2mod calibrate <MAP> --throws data/throws.json [--out ...] [--unfreeze a,b]`.
pub fn calibrate(
    map: &str,
    throws_path: &Path,
    out_path: &Path,
    unfreeze: &[String],
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    ensure_write_allowed(out_path)?;
    let samples = calib::load_throws_json(throws_path, 64.0).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{} measured throw(s)", samples.len());
    let (mesh, _dir) = load_or_extract_mesh(map, game, cache)?;
    let collider = grenade_collider(&mesh)?;

    let unfreeze_refs: Vec<&str> = unfreeze.iter().map(String::as_str).collect();
    let k0 = resolve_constants(None)?;
    let result = calib::calibrate(&samples, &collider, k0, &unfreeze_refs)?;
    println!(
        "error before: {:.1}u  after: {:.1}u  over {} sample(s)",
        result.error_before, result.error_after, result.n_samples
    );
    // `CalibrateCommand.cs:110-116`.
    for s in &result.samples {
        println!(
            "  {} residual {:.0}u  predicted ({:.0},{:.0},{:.0}) vs measured ({:.0},{:.0},{:.0})",
            s.kind,
            s.residual,
            s.predicted[0],
            s.predicted[1],
            s.predicted[2],
            s.measured[0],
            s.measured[1],
            s.measured[2]
        );
    }

    if unfreeze.is_empty() {
        println!("nothing unfrozen; {} not written", out_path.display());
        return Ok(0);
    }

    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, serde_json::to_string_pretty(&result.constants)?)?;
    println!("wrote {}", out_path.display());
    Ok(0)
}
