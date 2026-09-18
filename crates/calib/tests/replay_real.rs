//! Real-corpus replay and launch-model checks; needs `CS2_GAME_DIR` (to
//! resolve the current build/cache) and the reference's validation corpus
//! (`CS2MOD_CORPUS`, default `D:\porject\refs\cs2-smoke-solver\data\validation`).
//! Per `specs/s4b_calib.md` §Tests: if the within-3u/median assertion fails,
//! this test reports the numbers, worst 20, and per-build split instead of
//! loosening the threshold.

use std::path::PathBuf;

use calib::{launch_check, load_corpus, replay};
use extract::cache;
use extract::game::GameInstall;
use geom::filter::grenade_mask;
use geom::grid::UniformGrid;
use sim::ThrowConstants;

fn corpus_dir() -> PathBuf {
    std::env::var_os("CS2MOD_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\porject\refs\cs2-smoke-solver\data\validation"))
}

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

fn replay_one_map(map: &str) {
    let Some(game_dir) = std::env::var_os("CS2_GAME_DIR") else {
        eprintln!("skipping {map}: CS2_GAME_DIR not set");
        return;
    };
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    let cache_root = cache_root();
    let dir = cache::find_cached(&cache_root, &install, map)
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for {map}; run `cs2mod extract {map}` first"));
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");

    let corpus = corpus_dir();
    let rows = load_corpus(&corpus, map).expect("load corpus");
    let rows: Vec<_> = rows.into_iter().filter(|r| r.build == "2000899").collect();
    if rows.is_empty() {
        eprintln!("skipping {map}: no build-2000899 throws under {corpus:?}");
        return;
    }

    let solid = grenade_mask(&mesh);
    let collider = UniformGrid::build(&mesh, &solid, None, 128.0).expect("build collider");
    let k = ThrowConstants::default();
    let report = replay(&collider, None, &rows, &k, 20);

    println!(
        "{map}: {} throws  median {:.2}u  p90 {:.1}u  within 3u {:.1}%  over 8u {} ({:.1}%)",
        report.overall.n,
        report.overall.median,
        report.overall.p90,
        report.overall.within_3_pct,
        report.overall.over_8,
        report.overall.over_8_pct
    );
    for (build, m) in &report.per_build {
        println!(
            "  build {build}: {} throws, within 3u {:.1}%, median {:.2}u",
            m.n, m.within_3_pct, m.median
        );
    }

    let launch = launch_check(&rows, &k);
    println!(
        "{map} launch model: {} rows, {} skipped",
        launch.overall.n, launch.skipped
    );
    for (ty, s) in &launch.per_type {
        println!(
            "  {ty}: n={} mean |dpos|={:.2} max |dpos|={:.2} mean |dvel|={:.2} max |dvel|={:.2}",
            s.n, s.mean_pos, s.max_pos, s.mean_vel, s.max_vel
        );
    }

    if report.overall.within_3_pct < 90.0 || report.overall.median > 0.5 {
        eprintln!("FINDING: {map} did not meet within-3u>=90%/median<=0.5u; worst 20:");
        for w in &report.worst {
            eprintln!(
                "  {:.1}u  {} [{}] launch ({:.0},{:.0},{:.0}) sim ({:.0},{:.0},{:.0}) real ({:.0},{:.0},{:.0})",
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
                w.real_rest.z
            );
        }
        eprintln!("per-build split:");
        for (build, m) in &report.per_build {
            eprintln!(
                "  {build}: n={} within3={:.1}% median={:.2}",
                m.n, m.within_3_pct, m.median
            );
        }
    }
    assert!(
        report.overall.within_3_pct >= 90.0,
        "{map}: within 3u {:.1}% < 90%",
        report.overall.within_3_pct
    );
    assert!(
        report.overall.median <= 0.5,
        "{map}: median {:.2}u > 0.5u",
        report.overall.median
    );
}

/// Per `launch.rs`'s module doc: the corpus's `Pos`/`Vel` are the
/// reference's own `DeriveInitial` output at record time, so residuals here
/// track *reference model revisions* across builds, not engine truth.
/// Printing every build's split (not just an averaged total) is the point:
/// hiding it would bury exactly the older-build jump/run-jump drift
/// `docs/ARCHITECTURE.md` §8 asks a practice-server experiment to settle.
fn launch_model_by_build(map: &str) {
    let corpus = corpus_dir();
    let rows = match load_corpus(&corpus, map) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping {map}: {e}");
            return;
        }
    };
    if rows.is_empty() {
        eprintln!("skipping {map}: no corpus rows under {corpus:?}");
        return;
    }
    let k = ThrowConstants::default();

    let mut builds: Vec<String> = rows.iter().map(|r| r.build.clone()).collect();
    builds.sort();
    builds.dedup();

    println!("{map} launch model, all {} rows, by build:", rows.len());
    for build in &builds {
        let subset: Vec<_> = rows.iter().filter(|r| &r.build == build).cloned().collect();
        let report = launch_check(&subset, &k);
        println!(
            "  build {build}: {} rows, {} skipped",
            report.overall.n, report.skipped
        );
        for (ty, s) in &report.per_type {
            println!(
                "    {ty}: n={} mean |dpos|={:.2} max |dpos|={:.2} mean |dvel|={:.2} max |dvel|={:.2}",
                s.n, s.mean_pos, s.max_pos, s.mean_vel, s.max_vel
            );
        }
    }
}

#[test]
#[ignore = "needs the reference validation corpus"]
fn de_mirage_launch_model_by_build() {
    launch_model_by_build("de_mirage");
}

#[test]
#[ignore = "needs CS2_GAME_DIR and the reference validation corpus"]
fn de_mirage_replay() {
    replay_one_map("de_mirage");
}

#[test]
#[ignore = "needs CS2_GAME_DIR and the reference validation corpus"]
fn de_dust2_replay() {
    replay_one_map("de_dust2");
}
