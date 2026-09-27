//! `cs2mod replay-demo [MAP] --input launches.json... [--json out.json] [--top N] [--solid spec]
//! [--dump-dir dir]` (`s6v_demo_replay_and_rim_rest.md`, Part C `s6v_c_pro_demos.md`): re-simulates
//! every smoke projectile `tools/demo/extract_launches.py` recovered from real GOTV demos against
//! the exact sim and grades it tick-by-tick, one map's mesh loaded once even when several demos
//! (and several maps) are given at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use calib::{
    DemoHeader, Exclusion, GradedThrow, KINK_ACCEL_TOLERANCE, LaunchesFile, ProjectileLaunch,
    ThrowReplay, describe_triangle, exclusion_reason, game_kink_ticks, replay_throw, summarize,
};
use geom::filter::{AttributeMask, grenade_mask};
use geom::grid::UniformGrid;
use geom::math::V3;
use geom::mesh::CollisionMesh;
use sim::ThrowConstants;

use crate::cmd_extract::{install_for, load_or_extract_mesh};
use crate::constants::resolve_constants;
use crate::game_path::ensure_write_allowed;

fn fmt_opt_tick(t: Option<i64>) -> String {
    t.map_or_else(|| "-".to_string(), |t| t.to_string())
}

fn exclusion_label(e: Exclusion) -> &'static str {
    match e {
        Exclusion::NoDetonation => "no detonation",
        Exclusion::LowConfidence => "low confidence",
        Exclusion::PlayerContact => "player contact",
        Exclusion::EarlyDetonationFire => "early detonation (fire)",
        Exclusion::GlassStateUnknown => "glass state unknown",
    }
}

/// `--input` accepts `.launches.json` files and/or directories (each searched, non-recursively,
/// for `*.launches.json` - `extract_launches.py`'s own multi-demo output layout).
fn discover_input_files(inputs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("failed to read directory {}", p.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".launches.json"))
                })
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(p.clone());
        }
    }
    Ok(files)
}

/// `--solid <spec>` (`s6v_c_pro_demos.md`): `export-obj --filter` syntax (`grenade` default,
/// `attrs:Name1,Name2`) plus `grenade-minus:Name1,...` - the grenade mask with the named attribute
/// groups made non-solid, to A/B a hypothesis like "EntityPhysicsClip does not block grenades"
/// without code edits. The `grenade-minus:` predicate mirrors `geom::filter::grenade_mask`'s own
/// (ported from `CollisionMesh.cs:GrenadeSolidFilter`) rather than calling it, since
/// `AttributeMask` has no public way to combine two already-built masks by attribute name.
fn parse_solid_spec(spec: &str, mesh: &CollisionMesh) -> anyhow::Result<AttributeMask> {
    if let Some(rest) = spec.strip_prefix("grenade-minus:") {
        let minus: Vec<String> = rest
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let any_ci =
            |layers: &[String], name: &str| layers.iter().any(|l| l.eq_ignore_ascii_case(name));
        return Ok(geom::filter::from_fn(mesh, |a| {
            let grenade_solid = !any_ci(&a.interact_exclude, "csgo_thrown_grenade")
                && !any_ci(&a.interact_as, "playerclip")
                && !any_ci(&a.interact_as, "npcclip")
                && !any_ci(&a.interact_as, "sky");
            grenade_solid && !minus.iter().any(|n| a.name.eq_ignore_ascii_case(n))
        }));
    }
    geom::filter::parse_filter(spec, mesh)
        .map_err(|e| anyhow::anyhow!("invalid --solid {spec:?}: {e}"))
}

/// The `PatchVersion=` line of `<csgo>/steam.inf`, without dots (`"1.41.8.5"` -> `"14185"`) to
/// match `DemoHeader::patch_version`'s own format.
fn installed_patch_version(csgo_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(csgo_dir.join("steam.inf")).ok()?;
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("PatchVersion="))?;
    Some(line.trim().replace('.', ""))
}

fn check_patch_version(demo: &str, map: &str, header: &DemoHeader, csgo_dir: &Path) {
    let Some(demo_patch) = &header.patch_version else {
        return;
    };
    let Some(installed) = installed_patch_version(csgo_dir) else {
        return;
    };
    if demo_patch != &installed {
        println!(
            "warning: {demo} ({map}): demo patch_version {demo_patch:?} != installed game \
             {installed:?} - this demo may have been recorded on different map geometry"
        );
    }
}

struct LoadedFile {
    demo: String,
    map: String,
    launches: LaunchesFile,
}

struct MapContext {
    mesh: CollisionMesh,
    collider: UniformGrid,
}

fn write_dump(
    dir: &Path,
    demo: &str,
    launch: &ProjectileLaunch,
    replay: &ThrowReplay,
    ctx: &MapContext,
    k: &ThrowConstants,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let pos0 = V3::from_array(launch.launch_pos);
    let vel0 = V3::from_array(launch.launch_vel);
    let mut ticks: Vec<(V3, V3)> = Vec::new();
    let mut bounces: Vec<sim::BounceRecord> = Vec::new();
    let trace = sim::Trace {
        ticks: Some(&mut ticks),
        bounces: Some(&mut bounces),
    };
    let _ = sim::simulate_exact_raw(&ctx.collider, pos0, vel0, k, trace);

    let known_gravity = sim::BASE_GRAVITY * k.gravity_scale;
    let game_bounce_ticks = game_kink_ticks(&launch.ticks, known_gravity, KINK_ACCEL_TOLERANCE);

    let sim_trace: Vec<_> = ticks
        .iter()
        .enumerate()
        .map(|(i, (p, v))| {
            serde_json::json!({
                "tick": launch.launch_tick + 1 + i as i64,
                "pos": [p.x, p.y, p.z],
                "vel": [v.x, v.y, v.z],
            })
        })
        .collect();
    let sim_contacts: Vec<_> = bounces
        .iter()
        .map(|b| {
            let (group, entity) = describe_triangle(&ctx.mesh, b.triangle);
            serde_json::json!({
                "tick": launch.launch_tick + 1 + i64::from(b.tick),
                "point": [b.contact.x, b.contact.y, b.contact.z],
                "normal": [b.normal.x, b.normal.y, b.normal.z],
                "group": group,
                "entity": entity.map(|e| serde_json::json!({
                    "classname": e.classname, "targetname": e.targetname, "model": e.model,
                })),
            })
        })
        .collect();
    let game_samples: Vec<_> = launch
        .ticks
        .iter()
        .map(|t| serde_json::json!({"tick": t.tick, "pos": t.pos}))
        .collect();

    let json = serde_json::json!({
        "demo": demo,
        "entity_id": launch.entity_id,
        "first_tick": launch.first_tick,
        "rest_error": replay.rest_error,
        "first_over_1u": replay.first_over_1u,
        "game_samples": game_samples,
        "sim_trace": sim_trace,
        "sim_contacts": sim_contacts,
        "game_bounce_ticks": game_bounce_ticks,
    });
    let path = dir.join(format!(
        "{demo}_{}_{}.json",
        launch.entity_id, launch.first_tick
    ));
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn print_replay_line(demo: &str, l: &ProjectileLaunch, r: &ThrowReplay, multi: bool) {
    let prefix = if multi {
        format!("{demo} entity {}", l.entity_id)
    } else {
        format!("entity {}", l.entity_id)
    };
    let graded = if r.rest_graded {
        ""
    } else {
        " (ungraded: no detonation event)"
    };
    let confidence = if l.launch_confident {
        ""
    } else {
        "  LOW-CONFIDENCE LAUNCH FIT"
    };
    let excl = exclusion_reason(l, r)
        .map(|e| format!("  [excluded: {}]", exclusion_label(e)))
        .unwrap_or_default();
    println!(
        "{prefix}: rest error {:.2}u{graded}{confidence}{excl}  sim ({:.1},{:.1},{:.1}) game \
         ({:.1},{:.1},{:.1})  aligned {} tick(s)  first >1u @ {}  first >5u @ {}",
        r.rest_error,
        r.sim_rest.x,
        r.sim_rest.y,
        r.sim_rest.z,
        r.game_rest.x,
        r.game_rest.y,
        r.game_rest.z,
        r.aligned_ticks,
        fmt_opt_tick(r.first_over_1u),
        fmt_opt_tick(r.first_over_5u),
    );
    if r.first_over_1u.is_some() && r.divergence_contact.is_none() {
        println!(
            "  no sim contact recorded before this tick - diverges in pure free flight \
             (likely a launch-state mismatch, not a collision)"
        );
    }
    if let Some(c) = &r.divergence_contact {
        let entity_desc = c
            .entity
            .as_ref()
            .map(|e| {
                format!(
                    " entity(classname={} targetname={} model={})",
                    e.classname.as_deref().unwrap_or("-"),
                    e.targetname.as_deref().unwrap_or("-"),
                    e.model.as_deref().unwrap_or("-"),
                )
            })
            .unwrap_or_default();
        println!(
            "  sim contact @ sim tick {}: point ({:.1},{:.1},{:.1}) normal ({:.2},{:.2},{:.2}) \
             group={}{}  game also bounced within 2 ticks: {}",
            c.tick,
            c.point.x,
            c.point.y,
            c.point.z,
            c.normal.x,
            c.normal.y,
            c.normal.z,
            c.group,
            entity_desc,
            r.game_also_bounced,
        );
    }
    if l.player_contact_possible && !l.player_near.is_empty() {
        println!("  player_near: {} tick(s) recorded", l.player_near.len());
    }
    if l.early_detonation_fire {
        println!(
            "  early_detonation_fire: true (nearest fire {:.0}u)",
            l.nearest_fire_distance.unwrap_or(f32::NAN)
        );
    }
}

fn print_summary_block(label: &str, summary: &calib::DemoReplaySummary, top: usize) {
    println!(
        "{label}: {} graded throw(s), within 1u {} within 3u {} within 8u {}  \
         excluded: no_detonation={} low_confidence={} player_contact={} \
         early_detonation_fire={} glass_state_unknown={}",
        summary.n_graded,
        summary.within_1u,
        summary.within_3u,
        summary.within_8u,
        summary.excluded_no_detonation,
        summary.excluded_low_confidence,
        summary.excluded_player_contact,
        summary.excluded_early_detonation_fire,
        summary.excluded_glass_state_unknown,
    );
    println!("  divergence surfaces (group, map, location cluster, throws explained):");
    for (group, map, center, n) in summary.surfaces.iter().take(top.max(1)) {
        println!(
            "    {group} @ {map} ({:.0},{:.0},{:.0})  {n} throw(s)",
            center.x, center.y, center.z
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn replay_demo(
    map: Option<&str>,
    input: &[PathBuf],
    json_out: Option<&Path>,
    top: usize,
    constants: Option<&Path>,
    game: Option<&Path>,
    cache: Option<&Path>,
    solid: Option<&str>,
    dump_dir: Option<&Path>,
) -> anyhow::Result<u8> {
    if let Some(out) = json_out {
        ensure_write_allowed(out)?;
    }
    if let Some(dir) = dump_dir {
        ensure_write_allowed(dir)?;
    }

    let files = discover_input_files(input)?;
    if files.is_empty() {
        anyhow::bail!("no .launches.json file found in the given --input");
    }
    let multi = files.len() > 1;

    let mut loaded = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let launches: LaunchesFile = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let demo = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("demo")
            .trim_end_matches(".launches")
            .to_string();

        let effective_map = if !multi && let Some(m) = map {
            if !launches.map.is_empty() && launches.map != m {
                println!(
                    "warning: {} was extracted from map {:?}, replaying against {:?} instead",
                    path.display(),
                    launches.map,
                    m
                );
            }
            m.to_string()
        } else if !launches.map.is_empty() {
            launches.map.clone()
        } else if let Some(m) = map {
            m.to_string()
        } else {
            println!(
                "skipping {}: no map recorded in the file and no map given",
                path.display()
            );
            continue;
        };

        if launches.projectiles.is_empty() {
            println!(
                "no smoke grenade projectiles in {} - nothing to replay ({} skipped)",
                path.display(),
                launches.skipped.len()
            );
            for s in &launches.skipped {
                println!("  skipped entity {}: {}", s.entity_id, s.reason);
            }
        }

        loaded.push(LoadedFile {
            demo,
            map: effective_map,
            launches,
        });
    }

    if loaded.is_empty() {
        return Ok(2);
    }

    let k = resolve_constants(constants)?;
    let install = install_for(game)?;

    for f in &loaded {
        if let Some(h) = &f.launches.header {
            check_patch_version(&f.demo, &f.map, h, &install.csgo_dir);
        }
    }

    let mut map_ctx: BTreeMap<String, MapContext> = BTreeMap::new();
    for f in &loaded {
        if map_ctx.contains_key(&f.map) {
            continue;
        }
        let (mesh, _dir) = load_or_extract_mesh(&f.map, game, cache)?;
        let mask = match solid {
            Some(spec) => parse_solid_spec(spec, &mesh)?,
            None => grenade_mask(&mesh),
        };
        let collider = UniformGrid::build(&mesh, &mask, None, 128.0)?;
        map_ctx.insert(f.map.clone(), MapContext { mesh, collider });
    }

    struct Record<'a> {
        demo: &'a str,
        map: &'a str,
        launch: &'a ProjectileLaunch,
        replay: ThrowReplay,
    }

    let mut records: Vec<Record> = Vec::new();
    for f in &loaded {
        let ctx = &map_ctx[&f.map];
        for l in &f.launches.projectiles {
            let r = replay_throw(&ctx.mesh, &ctx.collider, &k, l);
            print_replay_line(&f.demo, l, &r, multi);
            if let Some(dir) = dump_dir
                && (r.rest_error > 3.0 || r.first_over_1u.is_some())
            {
                write_dump(dir, &f.demo, l, &r, ctx, &k)?;
            }
            records.push(Record {
                demo: &f.demo,
                map: &f.map,
                launch: l,
                replay: r,
            });
        }
    }

    let mut by_map: BTreeMap<&str, Vec<GradedThrow>> = BTreeMap::new();
    for rec in &records {
        by_map.entry(rec.map).or_default().push(GradedThrow {
            launch: rec.launch,
            replay: &rec.replay,
            map: rec.map,
        });
    }

    if by_map.len() > 1 {
        for (map_name, throws) in &by_map {
            print_summary_block(&format!("summary [{map_name}]"), &summarize(throws), top);
        }
    }
    let all_throws: Vec<GradedThrow> = records
        .iter()
        .map(|rec| GradedThrow {
            launch: rec.launch,
            replay: &rec.replay,
            map: rec.map,
        })
        .collect();
    let total_summary = summarize(&all_throws);
    print_summary_block("summary [total]", &total_summary, top);

    if let Some(out) = json_out {
        let throws_json: Vec<_> = records
            .iter()
            .map(|rec| {
                let r = &rec.replay;
                let l = rec.launch;
                serde_json::json!({
                    "map": rec.map,
                    "demo": rec.demo,
                    "entity_id": r.entity_id,
                    "rest_error": r.rest_error,
                    "rest_graded": r.rest_graded,
                    "sim_rest": [r.sim_rest.x, r.sim_rest.y, r.sim_rest.z],
                    "game_rest": [r.game_rest.x, r.game_rest.y, r.game_rest.z],
                    "aligned_ticks": r.aligned_ticks,
                    "first_over_1u": r.first_over_1u,
                    "first_over_5u": r.first_over_5u,
                    "launch_confident": l.launch_confident,
                    "rest_source": l.rest_source,
                    "game_also_bounced": r.game_also_bounced,
                    "player_contact_possible": l.player_contact_possible,
                    "excluded": exclusion_reason(l, r).map(exclusion_label),
                    "divergence_contact": r.divergence_contact.as_ref().map(|c| serde_json::json!({
                        "tick": c.tick,
                        "point": [c.point.x, c.point.y, c.point.z],
                        "normal": [c.normal.x, c.normal.y, c.normal.z],
                        "group": c.group,
                        "entity": c.entity.as_ref().map(|e| serde_json::json!({
                            "classname": e.classname,
                            "targetname": e.targetname,
                            "model": e.model,
                        })),
                    })),
                })
            })
            .collect();
        let summary_json = |s: &calib::DemoReplaySummary| {
            serde_json::json!({
                "n_graded": s.n_graded,
                "within_1u": s.within_1u,
                "within_3u": s.within_3u,
                "within_8u": s.within_8u,
                "excluded_no_detonation": s.excluded_no_detonation,
                "excluded_low_confidence": s.excluded_low_confidence,
                "excluded_player_contact": s.excluded_player_contact,
                "excluded_early_detonation_fire": s.excluded_early_detonation_fire,
                "excluded_glass_state_unknown": s.excluded_glass_state_unknown,
                "surfaces": s.surfaces.iter().map(|(g, map, c, n)| serde_json::json!({
                    "group": g, "map": map, "center": [c.x, c.y, c.z], "throws": n,
                })).collect::<Vec<_>>(),
            })
        };
        let by_map_json: serde_json::Map<String, serde_json::Value> = by_map
            .iter()
            .map(|(m, throws)| (m.to_string(), summary_json(&summarize(throws))))
            .collect();
        let json = serde_json::json!({
            "headers": loaded.iter().map(|f| serde_json::json!({
                "demo": f.demo,
                "map": f.map,
                "header": f.launches.header,
            })).collect::<Vec<_>>(),
            "throws": throws_json,
            "summary_by_map": by_map_json,
            "summary": summary_json(&total_summary),
        });
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("failed to write {}", out.display()))?;
        println!("wrote {}", out.display());
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::mesh::{CollisionAttribute, MeshObject, ObjectKind, SurfaceProperty};

    fn two_attr_mesh() -> CollisionMesh {
        let mut mesh = CollisionMesh::new();
        let default_attr = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let clip_attr = mesh
            .add_attribute(CollisionAttribute {
                name: "EntityPhysicsClip".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: true,
            })
            .unwrap();
        let obj = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldMesh,
            classname: None,
            targetname: None,
            model: None,
            hammer_id: None,
            source_index: 0,
            hull_flags: None,
        });
        for attr in [default_attr, clip_attr] {
            mesh.push_triangles(
                &[[-10.0, -10.0, 0.0], [10.0, -10.0, 0.0], [10.0, 10.0, 0.0]],
                &[[0, 1, 2]],
                attr,
                |_| SurfaceProperty::NONE,
                obj,
            )
            .unwrap();
        }
        mesh
    }

    #[test]
    fn solid_spec_grenade_minus_clears_only_the_named_group() {
        let mesh = two_attr_mesh();
        let mask = parse_solid_spec("grenade-minus:EntityPhysicsClip", &mesh).unwrap();
        assert!(mask.is_solid(0)); // Default
        assert!(!mask.is_solid(1)); // EntityPhysicsClip made non-solid
    }

    #[test]
    fn solid_spec_delegates_plain_filters_to_geom_filter() {
        let mesh = two_attr_mesh();
        let grenade = parse_solid_spec("grenade", &mesh).unwrap();
        assert!(grenade.is_solid(0));
        assert!(grenade.is_solid(1));
        let attrs = parse_solid_spec("attrs:Default", &mesh).unwrap();
        assert!(attrs.is_solid(0));
        assert!(!attrs.is_solid(1));
    }

    #[test]
    fn solid_spec_rejects_an_unknown_spec() {
        let mesh = two_attr_mesh();
        assert!(parse_solid_spec("bogus", &mesh).is_err());
    }

    #[test]
    fn discover_input_files_expands_a_directory_to_its_launches_json_files() {
        let dir = std::env::temp_dir().join(format!(
            "cs2mod-replay-demo-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.launches.json"), "{}").unwrap();
        std::fs::write(dir.join("b.launches.json"), "{}").unwrap();
        std::fs::write(dir.join("ignore.txt"), "").unwrap();
        let files = discover_input_files(std::slice::from_ref(&dir)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".launches.json"))
        }));
    }
}
