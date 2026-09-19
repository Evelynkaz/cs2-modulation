//! Measured weak-click throw range, for the sweep's range prunes. Ported
//! from `cs2-smoke-solver/src/Solver/ReachTable.cs`.

use std::sync::{Mutex, OnceLock};

use geom::filter::all_mask;
use geom::grid::UniformGrid;
use geom::math::V3;
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use sim::{ThrowConstants, ThrowSpec, ThrowType, eye_height, simulate_exact};

/// `ReachTable.cs:19-20` (`PlaneLength`, `Margin`).
const PLANE_LENGTH: f32 = 8192.0;
const MARGIN: f32 = 1.3;

const MEASURED_TYPES: [ThrowType; 5] = [
    ThrowType::Stand,
    ThrowType::Crouch,
    ThrowType::JumpThrow,
    ThrowType::CrouchJumpThrow,
    ThrowType::RunJumpThrow,
];
const MEASURED_STRENGTHS: [f32; 2] = [0.5, 0.0];

/// `ReachTable.cs:36-41` (`LeftClickBound`). Loose upper bounds; a real
/// measured jumpthrow covers 2286u, so err generously.
pub fn left_click_bound(t: ThrowType) -> f32 {
    match t {
        ThrowType::Stand | ThrowType::Crouch => 2000.0,
        ThrowType::JumpThrow | ThrowType::CrouchJumpThrow => 2700.0,
        _ => 3100.0,
    }
}

/// One measured `(ThrowType, speed_scale bits)` -> reach entry. Keyed on the
/// speed-scale float's bit pattern rather than the float itself (`f32` is not
/// `Hash`/`Eq`), which is exact here since both sides compute
/// `k.speed_scale(strength)` identically.
type Table = Vec<((ThrowType, u32), f32)>;

/// `ReachTable.cs:22` (`Tables`, a `ConcurrentDictionary<ThrowConstants,
/// ...>`). `ThrowConstants` has no `Hash`/`Eq` (it holds `f32` fields), so
/// this is a small linear-scan cache guarded by a mutex instead; the table
/// has at most a handful of distinct `ThrowConstants` in any process
/// lifetime (one per loaded `throw-constants.json`).
static TABLES: OnceLock<Mutex<Vec<(ThrowConstants, Table)>>> = OnceLock::new();

/// `ReachTable.cs:25-33` (`Bound`): the farthest a throw of this kind lands
/// on flat ground, with margin; the left-click constant at full strength.
pub fn bound(k: &ThrowConstants, t: ThrowType, strength: f32) -> f32 {
    if strength >= 0.99 {
        return left_click_bound(t);
    }
    let scale_bits = k.speed_scale(strength).to_bits();
    let cache = TABLES.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    let table = match guard.iter().find(|(tk, _)| tk == k) {
        Some((_, table)) => table.clone(),
        None => {
            let table = measure(k);
            guard.push((*k, table.clone()));
            table
        }
    };
    drop(guard);
    table
        .iter()
        .find(|((ty, bits), _)| *ty == t && *bits == scale_bits)
        .map(|(_, reach)| *reach)
        .unwrap_or_else(|| left_click_bound(t))
}

/// `ReachTable.cs:43-79` (`Measure`).
fn measure(k: &ThrowConstants) -> Table {
    let mut mesh = CollisionMesh::new();
    let attr = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
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
            [0.0, 0.0, 0.0],
            [PLANE_LENGTH, 0.0, 0.0],
            [PLANE_LENGTH, 2048.0, 0.0],
            [0.0, 2048.0, 0.0],
        ],
        &[[0, 1, 2], [0, 2, 3]],
        attr,
        |_| SurfaceProperty::NONE,
        obj,
    )
    .unwrap();
    let mask = all_mask(&mesh);
    let collider = UniformGrid::build(&mesh, &mask, None, 128.0).expect("build reach plane");

    let feet = V3::new(64.0, 1024.0, 0.0);
    let mut table: Table = Vec::new();
    for &t in &MEASURED_TYPES {
        for &strength in &MEASURED_STRENGTHS {
            let eye = feet + V3::new(0.0, 0.0, eye_height(t));
            let mut farthest = 0f32;
            let mut unbounded = false;
            let mut pitch = -89f32;
            while pitch <= 0.0 {
                let spec = ThrowSpec {
                    eye,
                    yaw_deg: 0.0,
                    pitch_deg: pitch,
                    throw_type: t,
                    strength,
                    run_yaw_offset_deg: 0.0,
                };
                let r = simulate_exact(&collider, &spec, k, sim::Trace::default());
                if r.lost || r.rest.x >= PLANE_LENGTH - 64.0 {
                    unbounded = true;
                    break;
                }
                farthest = farthest.max(r.rest.x - feet.x);
                pitch += 1.0;
            }
            let reach = if unbounded {
                left_click_bound(t)
            } else {
                left_click_bound(t).min(farthest * MARGIN)
            };
            table.push(((t, k.speed_scale(strength).to_bits()), reach));
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_click_run_jump_reach_exceeds_old_click_squared_bound() {
        // The old bound (click scale squared) put a right-click run-jump at
        // ~0.09 * MaxRange(RunJumpThrow), i.e. well under 300u
        // (`ReachTable.cs:9-13`'s doc comment: 279u vs the simulator's
        // measured 1,600u). The measured table must exceed that old bound by
        // a wide margin.
        let k = ThrowConstants::default();
        let old_bound = left_click_bound(ThrowType::RunJumpThrow) * k.speed_scale(0.0).powi(2);
        let measured = bound(&k, ThrowType::RunJumpThrow, 0.0);
        assert!(
            measured > old_bound * 2.0,
            "measured {measured} should far exceed the old squared-scale bound {old_bound}"
        );
    }

    #[test]
    fn bound_is_monotonic_in_click_strength() {
        let k = ThrowConstants::default();
        let right = bound(&k, ThrowType::Stand, 0.0);
        let both = bound(&k, ThrowType::Stand, 0.5);
        let left = bound(&k, ThrowType::Stand, 1.0);
        assert!(right <= both, "right {right} both {both}");
        assert!(both <= left, "both {both} left {left}");
    }

    #[test]
    fn left_click_always_returns_the_constant() {
        let k = ThrowConstants::default();
        assert_eq!(
            bound(&k, ThrowType::Stand, 1.0),
            left_click_bound(ThrowType::Stand)
        );
        assert_eq!(
            bound(&k, ThrowType::RunJumpThrow, 1.0),
            left_click_bound(ThrowType::RunJumpThrow)
        );
    }
}
