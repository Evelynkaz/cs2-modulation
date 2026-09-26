//! S6s ground-truth regression: five real smokes from a GOTV demo on de_mirage
//! (`specs/s6s_never_solid_brushes.md`), each thrown from an exact `setpos`/`setang` (CS2 is
//! fully deterministic). Asserts our simulated rest lands within 3u of the game's own
//! `smokegrenade_detonate` position -- proving that a never-solid `func_brush` merged as solid
//! (fixed by this change) was the root cause of two of the five misses. Needs `CS2_GAME_DIR`;
//! extracts de_mirage into a temp dir, never the shared cache.

use std::path::PathBuf;

use extract::build::{ExtractOptions, extract_map};
use extract::cache;
use extract::game::GameInstall;
use geom::collider::Collider;
use geom::filter::{grenade_mask, player_mask};
use geom::grid::UniformGrid;
use geom::math::V3;
use serde::Deserialize;
use sim::{ThrowConstants, ThrowSpec, ThrowType, Trace, eye_height, simulate_exact};
use solver::origins::floor_under_hull;

#[derive(Debug, Deserialize)]
struct GroundTruthThrow {
    id: String,
    setpos: [f32; 3],
    setang: [f32; 3],
    /// Whether `setpos` is the player's eye (before falling) or already feet, per the demo's own
    /// analysis (not the general "setang present -> eye" heuristic other pasted-getpos callers
    /// use: these are admin `setpos`/`setang` commands, annotated by hand against the recording).
    position_is_eye: bool,
    /// The player fell from `setpos` to the actual floor before throwing.
    falls_to_floor: bool,
    /// Overrides both `position_is_eye`/`falls_to_floor` derivation: the demo shows the player
    /// standing on a lower floor than a naive downward probe from `setpos` would find (case C).
    #[serde(default)]
    feet_z_override: Option<f32>,
    /// A feet Z independently known to be correct (case A), to check the floor-drop derivation
    /// against, not just the final simulated rest.
    #[serde(default)]
    known_feet_z: Option<f32>,
    throw_type: String,
    strength: f32,
    game_rest: [f32; 3],
}

#[derive(Debug, Deserialize)]
struct GroundTruthFixture {
    map: String,
    throws: Vec<GroundTruthThrow>,
}

fn parse_throw_type(s: &str) -> ThrowType {
    match s {
        "stand" => ThrowType::Stand,
        "crouch" => ThrowType::Crouch,
        "jump" => ThrowType::JumpThrow,
        "crouchjump" => ThrowType::CrouchJumpThrow,
        "runjump" => ThrowType::RunJumpThrow,
        other => panic!("fixture: unknown throw_type {other:?}"),
    }
}

/// Removes `path` recursively on drop, even on panic (the temp extraction dir must never linger
/// or be mistaken for a real cache).
struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_ground_truth_rests_within_3u() {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");

    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ground_truth_de_mirage.json");
    let fixture_text = std::fs::read_to_string(&fixture_path).expect("read fixture");
    let fixture: GroundTruthFixture = serde_json::from_str(&fixture_text).expect("parse fixture");

    let extraction =
        extract_map(&install, &fixture.map, &ExtractOptions::default()).expect("extract de_mirage");

    // Into a temp dir, never the shared cache (`cache::save_extraction` keys by content hash +
    // extractor version, so this is fully isolated from `D:\porject\modulator\cache`).
    let temp_root = std::env::temp_dir().join(format!("s6s-ground-truth-{}", std::process::id()));
    let _cleanup = TempDirGuard(temp_root.clone());
    let dir = cache::save_extraction(&temp_root, &extraction, true).expect("save to temp dir");
    let mesh = cache::load_mesh(&dir).expect("load mesh");

    let grenade_solid = grenade_mask(&mesh);
    let grenade_collider =
        UniformGrid::build(&mesh, &grenade_solid, None, 128.0).expect("grenade collider");
    let player_solid = player_mask(&mesh);
    let player_collider =
        UniformGrid::build(&mesh, &player_solid, None, 128.0).expect("player collider");
    let player_dyn: &dyn Collider = &player_collider;

    let k = ThrowConstants::default();
    let mut errors: Vec<(String, f32)> = Vec::new();

    for t in &fixture.throws {
        let feet_z = if let Some(z) = t.feet_z_override {
            z
        } else if t.falls_to_floor {
            let probe_z = if t.position_is_eye {
                t.setpos[2] - sim::STAND_EYE_HEIGHT
            } else {
                t.setpos[2]
            };
            let at = V3::new(t.setpos[0], t.setpos[1], probe_z);
            // Same primitive `TargetSolver.cs:24-27`'s `DropToFloor` uses for a spawn point that
            // falls before landing: probe up a stand-eye-height, down 512u, take the highest hit.
            floor_under_hull(player_dyn, at, sim::STAND_EYE_HEIGHT, 512.0)
                .unwrap_or_else(|| panic!("{}: no floor found under {at:?}", t.id))
        } else if t.position_is_eye {
            t.setpos[2] - sim::STAND_EYE_HEIGHT
        } else {
            t.setpos[2]
        };

        if let Some(known) = t.known_feet_z {
            assert!(
                (feet_z - known).abs() < 2.0,
                "{}: derived feet z {feet_z:.2} should match the known-correct {known:.2}",
                t.id
            );
        }

        let throw_type = parse_throw_type(&t.throw_type);
        let eye =
            V3::new(t.setpos[0], t.setpos[1], feet_z) + V3::new(0.0, 0.0, eye_height(throw_type));
        let spec = ThrowSpec {
            eye,
            yaw_deg: t.setang[1],
            pitch_deg: t.setang[0],
            throw_type,
            strength: t.strength,
            run_yaw_offset_deg: 0.0,
        };
        let result = simulate_exact(&grenade_collider, &spec, &k, Trace::default());
        let game_rest = V3::from_array(t.game_rest);
        let error = (result.rest - game_rest).length();
        println!(
            "{}: feet ({:.2},{:.2},{:.2})  sim rest ({:.2},{:.2},{:.2})  game rest ({:.2},{:.2},{:.2})  error {:.2}u",
            t.id,
            t.setpos[0],
            t.setpos[1],
            feet_z,
            result.rest.x,
            result.rest.y,
            result.rest.z,
            game_rest.x,
            game_rest.y,
            game_rest.z,
            error
        );
        errors.push((t.id.clone(), error));
    }

    for (id, error) in &errors {
        assert!(
            *error <= 3.0,
            "{id}: simulated rest should be within 3u of the game rest, got {error:.2}u"
        );
    }
}
