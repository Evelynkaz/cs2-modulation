//! World (`world.vwrld_c`) and world node (`*.vwnod_c`) traversal for rendering: scene objects
//! (props placed by `m_renderableModel`) and aggregates (pre-combined geometry, placed per
//! fragment). This decodes its own copy of the KV3 shape rather than reusing
//! `s2fmt::worldnode` (which only tracks fragment/aggregate *counts*, for the collision path) --
//! same layering choice as `s2render::mesh`/`model` decoding their own KV3 rather than depending
//! on a sibling crate's narrower view (`s6f3a3_map.md`: "s2fmt -- only additions, the collision
//! path is not touched").
//!
//! Ground truth: `Renderer/Renderer/World/WorldNodeLoader.cs`, `IO/Gltf/GltfModelExporter.
//! World.cs:219-270 LoadWorldNodeModels`, `IO/Gltf/GltfModelExporter.Mesh.cs:478-598
//! AggregateCreateFragments`, `Resource/ResourceTypes/ModelData/ModelLodInfo.cs:159-167`.

use s2fmt::kv3::Value;

use crate::error::MeshError;

/// `m_vTransform`: 3 vec4 rows, nested-array form (`docs/FORMATS.md` §6.6).
pub const IDENTITY_TRANSFORM: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

fn parse_nested_transform(v: &Value) -> [[f32; 4]; 3] {
    let Some(rows) = v.as_array() else {
        return IDENTITY_TRANSFORM;
    };
    let mut out = IDENTITY_TRANSFORM;
    for (i, row) in rows.iter().take(3).enumerate() {
        let Some(cols) = row.as_array() else { continue };
        for (j, c) in cols.iter().take(4).enumerate() {
            if let Some(f) = c.as_f32() {
                out[i][j] = f;
            }
        }
    }
    out
}

/// A flat 12-float row-major 3x4 (`m_fragmentTransforms[]` entries): row `i` is
/// `array[4*i..4*i+4]`, translation at indices 3/7/11 (`KVObjectExtensions.cs:432-450
/// ToMatrix4x4(KVObject)`; row-vector `v' = v*M` puts the translation in `M`'s 4th row, which
/// this constructor fills from `array[3]`/`array[7]`/`array[11]` -- worked out in full in
/// `s6f3a3_map.md`'s change item 2).
fn parse_flat_transform(v: &Value) -> Option<[[f32; 4]; 3]> {
    let arr = v.as_array()?;
    if arr.len() < 12 {
        return None;
    }
    let f = |i: usize| arr[i].as_f32().unwrap_or(0.0);
    Some([
        [f(0), f(1), f(2), f(3)],
        [f(4), f(5), f(6), f(7)],
        [f(8), f(9), f(10), f(11)],
    ])
}

/// Determinant of a row-major 3x4 transform's linear (rotation*scale) part -- zero means the
/// fragment collapses to nothing (`GltfModelExporter.Mesh.cs:559-566`).
pub fn det3(t: &[[f32; 4]; 3]) -> f32 {
    let (a, b, c) = (t[0][0], t[0][1], t[0][2]);
    let (d, e, f) = (t[1][0], t[1][1], t[1][2]);
    let (g, h, i) = (t[2][0], t[2][1], t[2][2]);
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

/// `m_nObjectTypeFlags` bit values (`wnode_survey.py`'s `SN` table, cross-checked against
/// `Renderer/Renderer/SceneNode.cs`'s `ObjectTypeFlags`); only the bit this crate acts on
/// (`Overlay`) needs to be correct, the rest are kept for a faithful string round-trip in
/// diagnostics.
const OBJECT_TYPE_DECAL: i64 = 0x4;
const OBJECT_TYPE_MODEL: i64 = 0x8;
const OBJECT_TYPE_BLOCK_LIGHT: i64 = 0x10;
const OBJECT_TYPE_NO_SHADOWS: i64 = 0x20;
const OBJECT_TYPE_RENDER_TO_CUBEMAPS: i64 = 0x400;
const OBJECT_TYPE_OVERLAY: i64 = 0x2000;

fn flag_name_bit(name: &str) -> i64 {
    match name.trim() {
        "OBJECT_TYPE_DECAL" => OBJECT_TYPE_DECAL,
        "OBJECT_TYPE_MODEL" => OBJECT_TYPE_MODEL,
        "OBJECT_TYPE_BLOCK_LIGHT" => OBJECT_TYPE_BLOCK_LIGHT,
        "OBJECT_TYPE_NO_SHADOWS" => OBJECT_TYPE_NO_SHADOWS,
        "OBJECT_TYPE_RENDER_TO_CUBEMAPS" => OBJECT_TYPE_RENDER_TO_CUBEMAPS,
        "OBJECT_TYPE_OVERLAY" => OBJECT_TYPE_OVERLAY,
        _ => 0,
    }
}

/// `m_nObjectTypeFlags`, which the compiler writes as either a KV3 integer or a `"A|B|C"` string
/// of flag names (`REPORT.md` §9's parsing trap; 300 of Mirage's world objects use the string
/// form). Unrecognised names contribute no bits rather than erroring.
fn parse_object_type_flags(v: Option<&Value>) -> i64 {
    let Some(v) = v else { return 0 };
    if let Some(i) = v.as_i64() {
        return i;
    }
    if let Some(s) = v.as_str() {
        return s.split('|').map(flag_name_bit).fold(0, |a, b| a | b);
    }
    0
}

fn is_overlay_flags(flags: i64) -> bool {
    flags & OBJECT_TYPE_OVERLAY != 0
}

/// A `world.vwrld_c` `DATA` root: entity lumps and world node prefixes, plus the lightmap UV
/// scale baked into `TEXCOORD_1` (§6; `complex.vert.slang:347`, for F3a-4's benefit).
#[derive(Debug, Clone, Default)]
pub struct WorldDoc {
    pub entity_lumps: Vec<String>,
    pub world_node_prefixes: Vec<String>,
    pub lightmap_uv_scale: [f32; 2],
}

/// Turns a stored `m_worldNodePrefix` into the `.vwnod_c` path VPK entries use (same transform as
/// `s2fmt::worldnode::world_node_path`, kept local to avoid a dependency on it for one helper).
pub fn world_node_path(prefix: &str) -> String {
    let mut path = prefix.replace('\\', "/").to_lowercase();
    path.push_str(".vwnod_c");
    path
}

pub fn decode_world(root: &Value) -> WorldDoc {
    let entity_lumps = root
        .get("m_entityLumps")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let world_node_prefixes = root
        .get("m_worldNodes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get("m_worldNodePrefix").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let lightmap_uv_scale = root
        .get("m_vLightmapUvScale")
        .and_then(Value::as_array)
        .filter(|a| a.len() >= 2)
        .map(|a| [a[0].as_f32().unwrap_or(1.0), a[1].as_f32().unwrap_or(1.0)])
        .unwrap_or([1.0, 1.0]);

    WorldDoc {
        entity_lumps,
        world_node_prefixes,
        lightmap_uv_scale,
    }
}

/// One `m_sceneObjects[]` entry: a single prop placement.
#[derive(Debug, Clone)]
pub struct SceneObjectRaw {
    /// `m_renderableModel` (a `.vmdl` path to resolve and load).
    pub renderable_model: Option<String>,
    /// `m_renderable` (a direct mesh reference); no real map exercises this, covered by a
    /// synthetic test only (§2).
    pub renderable: Option<String>,
    pub transform: [[f32; 4]; 3],
    /// `m_vTintColor`, 0..1; white when absent or all-zero (`GltfModelExporter.World.cs:238-243`).
    pub tint: [f32; 4],
    pub is_overlay: bool,
    /// `m_nOverlayRenderOrder` (0 default): paint order among overlapping overlays on the same
    /// surface, written as `extras.overlayOrder` on overlay nodes (§4). `0` means "no explicit
    /// order" (most overlay objects have none) rather than a real order-0, so the caller writes
    /// the field only when this is non-zero, and only for an object-level overlay placement.
    pub overlay_render_order: i64,
    pub layer_index: Option<usize>,
}

/// One `m_aggregateMeshes[]` entry.
#[derive(Debug, Clone)]
pub struct FragmentRaw {
    pub draw_call_index: i64,
    pub has_transform: bool,
    /// `m_vTintColor`, 0..255 (absent -> white; §2).
    pub tint: Option<[f32; 3]>,
    pub lod_group_mask: i64,
    pub lod_setup_index: i64,
}

/// One `m_aggregateSceneObjects[]` entry: pre-combined geometry placed per fragment.
#[derive(Debug, Clone)]
pub struct AggregateRaw {
    pub renderable_model: Option<String>,
    pub fragments: Vec<FragmentRaw>,
    /// Flat per-fragment transforms, consumed in order by fragments with `has_transform` -- see
    /// [`build_fragment_placements`].
    pub fragment_transforms: Vec<[[f32; 4]; 3]>,
}

#[derive(Debug, Clone, Default)]
pub struct WorldNodeRaw {
    pub scene_objects: Vec<SceneObjectRaw>,
    pub aggregates: Vec<AggregateRaw>,
    pub layer_names: Vec<String>,
}

fn parse_tint01(v: Option<&Value>) -> [f32; 4] {
    let Some(arr) = v.and_then(Value::as_array) else {
        return [1.0, 1.0, 1.0, 1.0];
    };
    if arr.len() < 3 {
        return [1.0, 1.0, 1.0, 1.0];
    }
    let t = [
        arr[0].as_f32().unwrap_or(0.0),
        arr[1].as_f32().unwrap_or(0.0),
        arr[2].as_f32().unwrap_or(0.0),
        arr.get(3).and_then(Value::as_f32).unwrap_or(1.0),
    ];
    if t[0] == 0.0 && t[1] == 0.0 && t[2] == 0.0 {
        [1.0, 1.0, 1.0, 1.0]
    } else {
        t
    }
}

fn parse_fragment(v: &Value) -> FragmentRaw {
    let tint = v
        .get("m_vTintColor")
        .and_then(Value::as_array)
        .filter(|a| a.len() >= 3)
        .map(|a| {
            [
                a[0].as_f32().unwrap_or(255.0) / 255.0,
                a[1].as_f32().unwrap_or(255.0) / 255.0,
                a[2].as_f32().unwrap_or(255.0) / 255.0,
            ]
        });
    FragmentRaw {
        draw_call_index: v
            .get("m_nDrawCallIndex")
            .and_then(Value::as_i64)
            .unwrap_or(-1),
        has_transform: v
            .get("m_bHasTransform")
            .and_then(|b| b.as_bool().or_else(|| b.as_i64().map(|i| i != 0)))
            .unwrap_or(false),
        tint,
        lod_group_mask: v
            .get("m_nLODGroupMask")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        lod_setup_index: v
            .get("m_nLODSetupIndex")
            .and_then(Value::as_i64)
            .unwrap_or(-1),
    }
}

pub fn decode_world_node(root: &Value) -> Result<WorldNodeRaw, MeshError> {
    let layer_indices = root
        .get("m_sceneObjectLayerIndices")
        .and_then(Value::as_array);

    let scene_objects = root
        .get("m_sceneObjects")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, obj)| SceneObjectRaw {
                    renderable_model: obj
                        .get("m_renderableModel")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    renderable: obj
                        .get("m_renderable")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    transform: obj
                        .get("m_vTransform")
                        .map(parse_nested_transform)
                        .unwrap_or(IDENTITY_TRANSFORM),
                    tint: parse_tint01(obj.get("m_vTintColor")),
                    is_overlay: is_overlay_flags(parse_object_type_flags(
                        obj.get("m_nObjectTypeFlags"),
                    )),
                    overlay_render_order: obj
                        .get("m_nOverlayRenderOrder")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    layer_index: layer_indices
                        .and_then(|arr| arr.get(i))
                        .and_then(Value::as_i64)
                        .and_then(|i| usize::try_from(i).ok()),
                })
                .collect()
        })
        .unwrap_or_default();

    let aggregates = root
        .get("m_aggregateSceneObjects")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|obj| {
                    let fragments = obj
                        .get("m_aggregateMeshes")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().map(parse_fragment).collect())
                        .unwrap_or_default();
                    let fragment_transforms = obj
                        .get("m_fragmentTransforms")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(parse_flat_transform).collect())
                        .unwrap_or_default();
                    AggregateRaw {
                        renderable_model: obj
                            .get("m_renderableModel")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        fragments,
                        fragment_transforms,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let layer_names = root
        .get("m_layerNames")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(WorldNodeRaw {
        scene_objects,
        aggregates,
        layer_names,
    })
}

/// One placed fragment: which draw call to render, its local transform (identity when
/// `m_bHasTransform` is unset) and tint.
#[derive(Debug, Clone, Copy)]
pub struct FragmentPlacement {
    pub draw_call_index: usize,
    pub transform: [[f32; 4]; 3],
    pub tint: [f32; 3],
}

/// Why a fragment's transform slot couldn't be read: the flat transform list ran out before every
/// `has_transform` fragment claimed its entry (a malformed/truncated aggregate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fragment {fragment} claims transform slot {slot}, but only {available} are present")]
pub struct FragmentTransformShortage {
    pub fragment: usize,
    pub slot: usize,
    pub available: usize,
}

/// Resolves an aggregate's fragments to their final placements, exactly mirroring
/// `AggregateCreateFragments` (`GltfModelExporter.Mesh.cs:524-595`):
/// - `m_fragmentTransforms` is consumed by a running counter that every `m_bHasTransform`
///   fragment advances, *including* fragments later dropped by a zero determinant or LOD --
///   the counter must not skip ahead just because a fragment turns out to be discarded.
/// - LOD is grouped **per `m_nLODSetupIndex`** (`ModelLodInfo.IsInLowestSetLevel`): the combined
///   mask is the union of every fragment's `m_nLODGroupMask` sharing the same setup index, and a
///   fragment survives if its own mask is zero (not LOD-managed) or includes that setup's lowest
///   set level.
pub fn build_fragment_placements(
    agg: &AggregateRaw,
) -> Result<Vec<FragmentPlacement>, FragmentTransformShortage> {
    let mut combined_by_setup: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::new();
    for f in &agg.fragments {
        *combined_by_setup.entry(f.lod_setup_index).or_insert(0) |= f.lod_group_mask;
    }

    let mut transform_index = 0usize;
    let mut out = Vec::with_capacity(agg.fragments.len());
    for (id, f) in agg.fragments.iter().enumerate() {
        let mut transform = IDENTITY_TRANSFORM;
        if f.has_transform {
            let t =
                agg.fragment_transforms
                    .get(transform_index)
                    .ok_or(FragmentTransformShortage {
                        fragment: id,
                        slot: transform_index,
                        available: agg.fragment_transforms.len(),
                    })?;
            transform_index += 1;
            if det3(t) == 0.0 {
                continue; // collapses to nothing (GltfModelExporter.Mesh.cs:559-566)
            }
            transform = *t;
        }

        let combined = combined_by_setup
            .get(&f.lod_setup_index)
            .copied()
            .unwrap_or(0);
        let lowest_set_level = if combined == 0 {
            0
        } else {
            combined.trailing_zeros()
        };
        let in_lowest =
            f.lod_group_mask == 0 || (f.lod_group_mask & (1i64 << lowest_set_level)) != 0;
        if !in_lowest {
            continue;
        }

        if f.draw_call_index < 0 {
            continue; // no m_nDrawCallIndex: nothing to draw, not an error (§2's own draw-call-index note is about the field being absent entirely, handled by the caller)
        }

        out.push(FragmentPlacement {
            draw_call_index: f.draw_call_index as usize,
            transform,
            tint: f.tint.unwrap_or([1.0, 1.0, 1.0]),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2fmt::kv3::Object;

    fn obj(entries: Vec<(&str, Value)>) -> Value {
        let mut o = Object::new();
        for (k, v) in entries {
            o.push(k, v);
        }
        Value::Object(o)
    }

    fn flat12(vals: [f32; 12]) -> Value {
        Value::Array(vals.iter().map(|&v| Value::Double(v as f64)).collect())
    }

    #[test]
    fn flat_transform_reads_rows_and_translation_at_3_7_11() {
        #[rustfmt::skip]
        let v = flat12([
            1.0, 0.0, 0.0, 10.0,
            0.0, 1.0, 0.0, 20.0,
            0.0, 0.0, 1.0, 30.0,
        ]);
        let t = parse_flat_transform(&v).unwrap();
        assert_eq!(t[0], [1.0, 0.0, 0.0, 10.0]);
        assert_eq!(t[1], [0.0, 1.0, 0.0, 20.0]);
        assert_eq!(t[2], [0.0, 0.0, 1.0, 30.0]);
    }

    #[test]
    fn det3_of_identity_is_one_and_zero_scale_is_zero() {
        assert_eq!(det3(&IDENTITY_TRANSFORM), 1.0);
        let mut zero_scale = IDENTITY_TRANSFORM;
        zero_scale[0][0] = 0.0;
        assert_eq!(det3(&zero_scale), 0.0);
    }

    #[test]
    fn object_type_flags_parse_string_combo_and_numeric() {
        assert!(is_overlay_flags(parse_object_type_flags(Some(
            &Value::String("OBJECT_TYPE_OVERLAY".to_string())
        ))));
        assert!(is_overlay_flags(parse_object_type_flags(Some(
            &Value::String("OBJECT_TYPE_NO_SHADOWS | OBJECT_TYPE_OVERLAY".to_string())
        ))));
        assert!(!is_overlay_flags(parse_object_type_flags(Some(
            &Value::String("OBJECT_TYPE_NO_SHADOWS".to_string())
        ))));
        assert!(is_overlay_flags(parse_object_type_flags(Some(
            &Value::Int(0x2000)
        ))));
        assert!(!is_overlay_flags(parse_object_type_flags(None)));
    }

    fn fragment(draw_call: i64, has_transform: bool, lod_mask: i64, lod_setup: i64) -> Value {
        obj(vec![
            ("m_nDrawCallIndex", Value::Int(draw_call)),
            ("m_bHasTransform", Value::Bool(has_transform)),
            ("m_nLODGroupMask", Value::Int(lod_mask)),
            ("m_nLODSetupIndex", Value::Int(lod_setup)),
        ])
    }

    #[test]
    fn fragment_transform_counter_advances_even_for_dropped_fragments() {
        // Fragment 0: has_transform, zero-determinant (dropped, but still consumes slot 0).
        // Fragment 1: has_transform, identity-ish (kept, must consume slot 1, not slot 0).
        #[rustfmt::skip]
        let zero_det = flat12([0.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0]);
        #[rustfmt::skip]
        let translate = flat12([1.0,0.0,0.0,5.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0]);

        let agg_val = obj(vec![
            ("m_renderableModel", Value::String("models/agg.vmdl".into())),
            (
                "m_aggregateMeshes",
                Value::Array(vec![fragment(0, true, 0, -1), fragment(1, true, 0, -1)]),
            ),
            (
                "m_fragmentTransforms",
                Value::Array(vec![zero_det, translate]),
            ),
        ]);
        let root = obj(vec![(
            "m_aggregateSceneObjects",
            Value::Array(vec![agg_val]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let placements = build_fragment_placements(&node.aggregates[0]).unwrap();

        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].draw_call_index, 1);
        assert_eq!(placements[0].transform[0][3], 5.0);
    }

    #[test]
    fn lod_lowest_level_per_setup_index_kept_others_dropped() {
        // Two fragments share setup 0: mask 0b01 (LOD0) and 0b10 (LOD1). Combined = 0b11, lowest
        // set level = 0, so only the LOD0 fragment survives.
        let agg_val = obj(vec![
            ("m_renderableModel", Value::String("models/agg.vmdl".into())),
            (
                "m_aggregateMeshes",
                Value::Array(vec![
                    fragment(0, false, 0b01, 0),
                    fragment(1, false, 0b10, 0),
                ]),
            ),
        ]);
        let root = obj(vec![(
            "m_aggregateSceneObjects",
            Value::Array(vec![agg_val]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let placements = build_fragment_placements(&node.aggregates[0]).unwrap();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].draw_call_index, 0);
    }

    #[test]
    fn zero_mask_fragment_is_always_kept_unmanaged_by_lod() {
        let agg_val = obj(vec![
            ("m_renderableModel", Value::String("models/agg.vmdl".into())),
            (
                "m_aggregateMeshes",
                Value::Array(vec![fragment(0, false, 0, -1)]),
            ),
        ]);
        let root = obj(vec![(
            "m_aggregateSceneObjects",
            Value::Array(vec![agg_val]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let placements = build_fragment_placements(&node.aggregates[0]).unwrap();
        assert_eq!(placements.len(), 1);
    }

    #[test]
    fn missing_transform_slot_errors_rather_than_panicking() {
        let agg_val = obj(vec![
            ("m_renderableModel", Value::String("models/agg.vmdl".into())),
            (
                "m_aggregateMeshes",
                Value::Array(vec![fragment(0, true, 0, -1)]),
            ),
            ("m_fragmentTransforms", Value::Array(vec![])),
        ]);
        let root = obj(vec![(
            "m_aggregateSceneObjects",
            Value::Array(vec![agg_val]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let err = build_fragment_placements(&node.aggregates[0]).unwrap_err();
        assert_eq!(err.fragment, 0);
        assert_eq!(err.available, 0);
    }

    #[test]
    fn scene_object_reads_tint_flags_and_transform() {
        #[rustfmt::skip]
        let transform = Value::Array(vec![
            Value::Array(vec![Value::Double(1.0), Value::Double(0.0), Value::Double(0.0), Value::Double(10.0)]),
            Value::Array(vec![Value::Double(0.0), Value::Double(1.0), Value::Double(0.0), Value::Double(20.0)]),
            Value::Array(vec![Value::Double(0.0), Value::Double(0.0), Value::Double(1.0), Value::Double(30.0)]),
        ]);
        let root = obj(vec![(
            "m_sceneObjects",
            Value::Array(vec![obj(vec![
                ("m_renderableModel", Value::String("models/foo.vmdl".into())),
                ("m_vTransform", transform),
                (
                    "m_nObjectTypeFlags",
                    Value::String("OBJECT_TYPE_OVERLAY".into()),
                ),
            ])]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let so = &node.scene_objects[0];
        assert_eq!(so.renderable_model.as_deref(), Some("models/foo.vmdl"));
        assert_eq!(so.transform[0][3], 10.0);
        assert!(so.is_overlay);
        assert_eq!(so.tint, [1.0, 1.0, 1.0, 1.0]); // absent -> white
        assert_eq!(so.overlay_render_order, 0); // absent -> 0
    }

    #[test]
    fn scene_object_reads_overlay_render_order_and_renderable() {
        let root = obj(vec![(
            "m_sceneObjects",
            Value::Array(vec![obj(vec![
                ("m_renderable", Value::String("models/foo.vmesh".into())),
                ("m_nOverlayRenderOrder", Value::Int(2)),
            ])]),
        )]);
        let node = decode_world_node(&root).unwrap();
        let so = &node.scene_objects[0];
        assert_eq!(so.renderable_model, None);
        assert_eq!(so.renderable.as_deref(), Some("models/foo.vmesh"));
        assert_eq!(so.overlay_render_order, 2);
    }
}
