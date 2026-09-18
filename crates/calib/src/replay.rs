//! Offline corpus replay: re-simulates every graded throw a validation
//! report recorded, straight from the engine's own launch `Pos`/`Vel`, and
//! scores the rest point against the real one. Ported from
//! `cs2-smoke-solver/src/Cli/Commands/ReplayCommand.cs:117-202`.

use geom::collider::Collider;
use geom::math::V3;
use rayon::prelude::*;
use sim::{ThrowConstants, Trace, simulate_exact_raw};

use crate::corpus::CorpusThrow;

/// A percentile/threshold summary over a set of per-throw errors, matching
/// `ReplayCommand.cs:185-188`'s line (`Pct(p) = sorted[min(len-1, floor(p*len))]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub n: usize,
    pub median: f32,
    pub p90: f32,
    pub p99: f32,
    /// Percentage (0-100) of throws with error <= 3u.
    pub within_3_pct: f32,
    pub over_8: usize,
    pub over_8_pct: f32,
}

fn metrics(errors: &[f32]) -> Metrics {
    let mut sorted = errors.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let pct = |p: f64| -> f32 {
        if n == 0 {
            return 0.0;
        }
        sorted[(n - 1).min((p * n as f64) as usize)]
    };
    let within_3 = errors.iter().filter(|&&e| e <= 3.0).count();
    let over_8 = errors.iter().filter(|&&e| e > 8.0).count();
    Metrics {
        n,
        median: pct(0.5),
        p90: pct(0.9),
        p99: pct(0.99),
        within_3_pct: if n == 0 {
            0.0
        } else {
            within_3 as f32 * 100.0 / n as f32
        },
        over_8,
        over_8_pct: if n == 0 {
            0.0
        } else {
            over_8 as f32 * 100.0 / n as f32
        },
    }
}

/// One of the `--worst N` rows (`ReplayCommand.cs:217-224`).
#[derive(Debug, Clone, PartialEq)]
pub struct WorstEntry {
    pub report: String,
    pub index: i32,
    pub error: f32,
    pub reported: f32,
    pub launch_pos: V3,
    pub sim_rest: V3,
    pub real_rest: V3,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayReport {
    pub overall: Metrics,
    pub per_build: Vec<(String, Metrics)>,
    pub per_divergence_class: Vec<(String, Metrics)>,
    pub worst: Vec<WorstEntry>,
    /// The `ErrPredicted` values as originally reported, summarized the same
    /// way as `overall` (`ReplayCommand.cs:189`'s "as reported at the time" line).
    pub reported: Metrics,
    /// Throws that broke glass and were graded (`ReplayCommand.cs:129-140`).
    pub glass_throws: usize,
    pub glass_corrected: usize,
    /// Throws whose error moved by more than 1u since they were graded
    /// (`ReplayCommand.cs:201-202`).
    pub moved_since_graded: usize,
}

struct Row<'a> {
    t: &'a CorpusThrow,
    pos: V3,
    vel: V3,
    real: V3,
}

/// Re-simulates every graded throw in `throws` (rows without `Pos`/`Vel`/
/// `RealRest`, or with `Detonated == false`, are skipped, exactly like
/// `ReplayCommand.cs:101-105`) against `collider`, correcting for glass state
/// with `collider_glass_gone` when given (`ReplayCommand.cs:129-140`), and
/// summarizes overall, per-build and per-`DivergenceClass` metrics plus the
/// worst `worst_n` throws.
pub fn replay<C: Collider>(
    collider: &C,
    collider_glass_gone: Option<&C>,
    throws: &[CorpusThrow],
    k: &ThrowConstants,
    worst_n: usize,
) -> ReplayReport {
    let rows: Vec<Row> = throws
        .iter()
        .filter_map(|t| {
            let r = &t.result;
            if !r.detonated {
                return None;
            }
            let pos = r.pos?;
            let vel = r.vel?;
            let real = r.real_rest?;
            Some(Row {
                t,
                pos: V3::new(pos[0], pos[1], pos[2]),
                vel: V3::new(vel[0], vel[1], vel[2]),
                real: V3::new(real[0], real[1], real[2]),
            })
        })
        .collect();

    struct Outcome {
        rest: V3,
        error: f32,
        glass_eligible: bool,
        glass_corrected: bool,
    }

    let outcomes: Vec<Outcome> = rows
        .par_iter()
        .map(|row| {
            let result = simulate_exact_raw(collider, row.pos, row.vel, k, Trace::default());
            let mut rest = result.rest;
            let mut error = (rest - row.real).length();
            let mut glass_eligible = false;
            let mut glass_corrected = false;
            if result.glass_breaks > 0
                && let Some(gone) = collider_glass_gone
                && row.t.result.glass_state.as_deref() != Some("intact")
            {
                glass_eligible = true;
                let gone_result = simulate_exact_raw(gone, row.pos, row.vel, k, Trace::default());
                let err_gone = (gone_result.rest - row.real).length();
                if row.t.result.glass_state.as_deref() == Some("gone") || err_gone < error {
                    rest = gone_result.rest;
                    error = err_gone;
                    glass_corrected = true;
                }
            }
            Outcome {
                rest,
                error,
                glass_eligible,
                glass_corrected,
            }
        })
        .collect();

    let errors: Vec<f32> = outcomes.iter().map(|o| o.error).collect();
    let overall = metrics(&errors);
    let reported_errors: Vec<f32> = rows.iter().map(|r| r.t.result.err_predicted).collect();
    let reported = metrics(&reported_errors);

    let mut builds: Vec<String> = rows.iter().map(|r| r.t.build.clone()).collect();
    builds.sort();
    builds.dedup();
    let per_build = builds
        .into_iter()
        .map(|b| {
            let e: Vec<f32> = rows
                .iter()
                .zip(&errors)
                .filter(|(r, _)| r.t.build == b)
                .map(|(_, &e)| e)
                .collect();
            (b, metrics(&e))
        })
        .collect();

    let mut classes: Vec<String> = rows
        .iter()
        .filter_map(|r| r.t.result.divergence_class.clone())
        .collect();
    classes.sort();
    classes.dedup();
    let per_divergence_class = classes
        .into_iter()
        .map(|cls| {
            let e: Vec<f32> = rows
                .iter()
                .zip(&errors)
                .filter(|(r, _)| r.t.result.divergence_class.as_deref() == Some(cls.as_str()))
                .map(|(_, &e)| e)
                .collect();
            (cls, metrics(&e))
        })
        .collect();

    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| errors[b].partial_cmp(&errors[a]).unwrap());
    let worst = order
        .into_iter()
        .take(worst_n)
        .map(|i| WorstEntry {
            report: rows[i].t.report.clone(),
            index: rows[i].t.result.index,
            error: errors[i],
            reported: rows[i].t.result.err_predicted,
            launch_pos: rows[i].pos,
            sim_rest: outcomes[i].rest,
            real_rest: rows[i].real,
        })
        .collect();

    let glass_throws = outcomes.iter().filter(|o| o.glass_eligible).count();
    let glass_corrected = outcomes.iter().filter(|o| o.glass_corrected).count();

    let moved_since_graded = rows
        .iter()
        .zip(&errors)
        .filter(|(r, e)| (*e - r.t.result.err_predicted).abs() > 1.0)
        .count();

    ReplayReport {
        overall,
        per_build,
        per_divergence_class,
        worst,
        reported,
        glass_throws,
        glass_corrected,
        moved_since_graded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CorpusResult;
    use geom::filter::all_mask;
    use geom::grid::UniformGrid;
    use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};

    #[test]
    fn metrics_percentiles_and_thresholds_on_ten_values() {
        // `Pct(p) = sorted[min(len-1, floor(p*len))]` (`ReplayCommand.cs:186`).
        let errors = [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 8.5, 9.0];
        let m = metrics(&errors);
        assert_eq!(m.n, 10);
        assert_eq!(m.median, errors[5]); // floor(0.5*10) = 5
        assert_eq!(m.p90, errors[9]); // floor(0.9*10) = 9
        assert_eq!(m.p99, errors[9]); // floor(0.99*10) = 9, clamped to len-1 anyway
        assert_eq!(m.within_3_pct, 60.0); // 6 of 10 are <= 3.0
        assert_eq!(m.over_8, 2);
        assert_eq!(m.over_8_pct, 20.0);
    }

    /// A breakable vertical pane at x=50 (spanning the flight's y/z) standing
    /// in front of a floor plane at z=0: with the pane present, a throw
    /// aimed through it loses most of its speed to `glass_pass_factor` and
    /// lands short; with the pane already gone, it keeps its speed and lands
    /// much further out. `GlassState` picks which of those two outcomes a
    /// row is graded against (`ReplayCommand.cs:129-140`).
    fn glass_test_colliders() -> (UniformGrid, UniformGrid, V3, V3) {
        let mut mesh = CollisionMesh::new();
        let floor_attr = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let glass_attr = mesh
            .add_attribute(CollisionAttribute {
                name: "EntityBreakable".to_string(),
                interact_as: vec![],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
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
        mesh.push_triangles(
            &[
                [-2000.0, -2000.0, 0.0],
                [2000.0, -2000.0, 0.0],
                [2000.0, 2000.0, 0.0],
                [-2000.0, 2000.0, 0.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            floor_attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();
        mesh.push_triangles(
            &[
                [50.0, -200.0, 0.0],
                [50.0, 200.0, 0.0],
                [50.0, 200.0, 200.0],
                [50.0, -200.0, 200.0],
            ],
            &[[0, 1, 2], [0, 2, 3]],
            glass_attr,
            |_| SurfaceProperty::NONE,
            obj,
        )
        .unwrap();

        let full_mask = all_mask(&mesh);
        let full = UniformGrid::build(&mesh, &full_mask, None, 128.0).unwrap();
        let glass_mask = geom::filter::from_fn(&mesh, |a| a.name != "EntityBreakable");
        let gone = UniformGrid::build(&mesh, &glass_mask, None, 128.0).unwrap();

        let pos = V3::new(0.0, 0.0, 40.0);
        let vel = V3::new(400.0, 0.0, 60.0);
        (full, gone, pos, vel)
    }

    fn glass_row(index: i32, glass_state: Option<&str>, real: V3) -> CorpusThrow {
        CorpusThrow {
            report: "synthetic".to_string(),
            build: "1".to_string(),
            result: CorpusResult {
                index,
                throw_type: "Stand".to_string(),
                strength: 1.0,
                stability: 0.0,
                feet: [0.0, 0.0, 0.0],
                yaw: 0.0,
                pitch: 0.0,
                run_deg: 0.0,
                perturb_u: 0.0,
                scatter: 0.0,
                predicted_bounces: 0,
                real_bounces: 0,
                pos: Some([0.0, 0.0, 40.0]),
                vel: Some([400.0, 0.0, 60.0]),
                predicted_rest: None,
                real_rest: Some([real.x, real.y, real.z]),
                detonated: true,
                err_predicted: 0.0,
                err_target: 0.0,
                divergence_tick: -1,
                divergence_class: None,
                glass_state: glass_state.map(str::to_string),
            },
        }
    }

    #[test]
    fn glass_state_gates_the_pane_gone_correction() {
        let (full, gone, pos, vel) = glass_test_colliders();
        let k = ThrowConstants::default();
        let full_result = simulate_exact_raw(&full, pos, vel, &k, Trace::default());
        let gone_result = simulate_exact_raw(&gone, pos, vel, &k, Trace::default());
        assert_eq!(full_result.glass_breaks, 1, "must break the pane once");
        assert!(
            (full_result.rest - gone_result.rest).length() > 20.0,
            "the pane must make a real difference: full {:?} vs gone {:?}",
            full_result.rest,
            gone_result.rest
        );
        // The "real" rest always matches the pane-already-gone outcome; only
        // `GlassState` differs, so each row exercises a different branch of
        // `ReplayCommand.cs:129-140`.
        let real = gone_result.rest;
        let rows = vec![
            glass_row(0, Some("intact"), real), // never corrected
            glass_row(1, None, real),           // corrected because it improves
            glass_row(2, Some("gone"), real),   // corrected unconditionally
        ];

        let report = replay(&full, Some(&gone), &rows, &k, 3);
        assert_eq!(report.glass_throws, 2, "intact row must not be eligible");
        assert_eq!(report.glass_corrected, 2);

        let by_index = |i: i32| report.worst.iter().find(|w| w.index == i).unwrap();
        assert!(
            by_index(0).error > 20.0,
            "intact row keeps the full-collider error"
        );
        assert!(
            by_index(1).error < 1.0,
            "null-GlassState row is corrected because it improves"
        );
        assert!(
            by_index(2).error < 1.0,
            "gone row is corrected unconditionally"
        );
    }
}
