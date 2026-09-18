//! World (`*.vwrld_c`) and world node (`*.vwnod_c`) scene objects.
//!
//! `docs/FORMATS.md` section 8; `VRF/Resource/ResourceTypes/World.cs`, `WorldNode.cs`,
//! `VRF-R/World/WorldNodeLoader.cs`.

use crate::kv3::Value;

/// A `world.vwrld_c` `DATA` root: which entity lumps and world nodes make up the map
/// (`World.cs:16-17`, `42-45`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct World {
    pub entity_lumps: Vec<String>,
    /// `m_worldNodes[].m_worldNodePrefix`, as stored (backslash-separated; see
    /// [`world_node_path`]).
    pub world_node_prefixes: Vec<String>,
}

/// An error decoding a world or world node. Currently infallible: missing/malformed fields
/// degrade to empty collections rather than erroring, matching the reference's tolerance for
/// older/newer schema shapes (`WorldNode.cs:23-50`, optional keys via `ContainsKey`).
#[derive(Debug, thiserror::Error)]
pub enum WorldError {}

/// Decodes a `world.vwrld_c` `DATA` root (`World.cs:16-45`).
pub fn decode_world(root: &Value) -> Result<World, WorldError> {
    let entity_lumps = root
        .get("m_entityLumps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let world_node_prefixes = root
        .get("m_worldNodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.get("m_worldNodePrefix").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(World {
        entity_lumps,
        world_node_prefixes,
    })
}

/// Turns a stored `m_worldNodePrefix` into the `.vwnod_c` path VPK entries use: backslashes to
/// `/`, lowercased (`docs/FORMATS.md` section 8 "(!)"; `cs2-smoke-solver/MapExtractor.cs:1298`).
pub fn world_node_path(prefix: &str) -> String {
    let mut path = prefix.replace('\\', "/").to_lowercase();
    path.push_str(".vwnod_c");
    path
}

/// A `m_sceneObjects[]` entry (`SceneObject_t`; `WorldNodeLoader.cs:60-144`).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneObject {
    pub object_id: Option<i64>,
    pub renderable_model: Option<String>,
    pub renderable: Option<String>,
    /// Row-major 3x4 placement matrix (`m_vTransform`; `docs/FORMATS.md` section 6.6).
    pub transform: [[f32; 4]; 3],
    pub object_type_flags: i64,
    /// This object's entry in `m_sceneObjectLayerIndices`, if the node has a layer system
    /// (`WorldNode.cs:23-26`).
    pub layer_index: Option<i64>,
}

/// A `m_aggregateSceneObjects[]` entry: a combined draw call with no link back to the original
/// props' models/physics (`docs/FORMATS.md` section 8; `SceneAggregate.cs:145-165`, `300-326`).
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateSceneObject {
    pub renderable_model: Option<String>,
    /// `m_nLayer` (`WorldNodeLoader.cs:162`).
    pub layer: Option<i64>,
    /// Count of fragments with their own transform (`m_bHasTransform`), i.e. the length of
    /// `m_fragmentTransforms` (`SceneAggregate.cs:326`, `340`).
    pub fragment_count: usize,
    /// Total fragment count: the length of `m_aggregateMeshes`, which may exceed
    /// [`Self::fragment_count`] since not every fragment has `m_bHasTransform` set
    /// (`SceneAggregate.cs:288`, `340`).
    pub aggregate_mesh_count: usize,
}

/// A decoded `*.vwnod_c` `DATA` root (`WorldNode.cs`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldNode {
    pub scene_objects: Vec<SceneObject>,
    pub aggregate_scene_objects: Vec<AggregateSceneObject>,
    pub clutter_scene_object_count: usize,
    pub layer_names: Vec<String>,
}

/// Identity placement: no rotation/scale, zero origin.
const IDENTITY_TRANSFORM: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// Reads `m_vTransform`: 3 vec4 rows, nested-array form (`docs/FORMATS.md` section 6.6).
fn parse_transform(v: &Value) -> [[f32; 4]; 3] {
    let Some(rows) = v.as_array() else {
        return IDENTITY_TRANSFORM;
    };
    let mut out = IDENTITY_TRANSFORM;
    for (i, row) in rows.iter().take(3).enumerate() {
        let Some(cols) = row.as_array() else { continue };
        for (j, c) in cols.iter().take(4).enumerate() {
            if let Some(f) = c.as_f64() {
                out[i][j] = f as f32;
            }
        }
    }
    out
}

/// Decodes a `*.vwnod_c` `DATA` root (`WorldNode.cs:15-51`).
pub fn decode_world_node(root: &Value) -> Result<WorldNode, WorldError> {
    let layer_indices = root
        .get("m_sceneObjectLayerIndices")
        .and_then(|v| v.as_array());

    let scene_objects = root
        .get("m_sceneObjects")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, obj)| SceneObject {
                    // Real de_mirage world nodes store this as `m_nObjectID` (capital ID);
                    // accept the schema-cased `m_nObjectId` too in case older/other files
                    // use it.
                    object_id: obj
                        .get("m_nObjectID")
                        .or_else(|| obj.get("m_nObjectId"))
                        .and_then(|v| v.as_i64()),
                    renderable_model: obj
                        .get("m_renderableModel")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    renderable: obj
                        .get("m_renderable")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    transform: obj
                        .get("m_vTransform")
                        .map(parse_transform)
                        .unwrap_or(IDENTITY_TRANSFORM),
                    object_type_flags: obj
                        .get("m_nObjectTypeFlags")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                    layer_index: layer_indices
                        .and_then(|arr| arr.get(i))
                        .and_then(|v| v.as_i64()),
                })
                .collect()
        })
        .unwrap_or_default();

    let aggregate_scene_objects = root
        .get("m_aggregateSceneObjects")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|obj| AggregateSceneObject {
                    renderable_model: obj
                        .get("m_renderableModel")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    layer: obj.get("m_nLayer").and_then(|v| v.as_i64()),
                    fragment_count: obj
                        .get("m_fragmentTransforms")
                        .and_then(|v| v.as_array())
                        .map(<[Value]>::len)
                        .unwrap_or(0),
                    aggregate_mesh_count: obj
                        .get("m_aggregateMeshes")
                        .and_then(|v| v.as_array())
                        .map(<[Value]>::len)
                        .unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();

    let clutter_scene_object_count = root
        .get("m_clutterSceneObjects")
        .and_then(|v| v.as_array())
        .map(<[Value]>::len)
        .unwrap_or(0);

    let layer_names = root
        .get("m_layerNames")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(WorldNode {
        scene_objects,
        aggregate_scene_objects,
        clutter_scene_object_count,
        layer_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv3::Object;

    fn obj(entries: Vec<(&str, Value)>) -> Value {
        let mut o = Object::new();
        for (k, v) in entries {
            o.push(k, v);
        }
        Value::Object(o)
    }

    fn num_array(nums: &[f64]) -> Value {
        Value::Array(nums.iter().map(|&n| Value::Double(n)).collect())
    }

    #[test]
    fn world_node_path_normalises_backslashes_and_case() {
        assert_eq!(
            world_node_path("Maps\\de_mirage\\worldnodes\\N0"),
            "maps/de_mirage/worldnodes/n0.vwnod_c"
        );
        assert_eq!(world_node_path("already/lower"), "already/lower.vwnod_c");
    }

    #[test]
    fn decode_world_reads_entity_lumps_and_node_prefixes() {
        let root = obj(vec![
            (
                "m_entityLumps",
                Value::Array(vec![Value::String("worlds/entities".into())]),
            ),
            (
                "m_worldNodes",
                Value::Array(vec![obj(vec![(
                    "m_worldNodePrefix",
                    Value::String("worldnodes\\n0".into()),
                )])]),
            ),
        ]);

        let world = decode_world(&root).unwrap();
        assert_eq!(world.entity_lumps, vec!["worlds/entities".to_string()]);
        assert_eq!(
            world.world_node_prefixes,
            vec!["worldnodes\\n0".to_string()]
        );
    }

    #[test]
    fn decode_world_node_reads_scene_objects_aggregates_and_layers() {
        let transform = Value::Array(vec![
            num_array(&[1.0, 0.0, 0.0, 10.0]),
            num_array(&[0.0, 1.0, 0.0, 20.0]),
            num_array(&[0.0, 0.0, 1.0, 30.0]),
        ]);
        let root = obj(vec![
            (
                "m_sceneObjects",
                Value::Array(vec![obj(vec![
                    (
                        "m_renderableModel",
                        Value::String("models/foo.vmdl_c".into()),
                    ),
                    ("m_vTransform", transform),
                    ("m_nObjectTypeFlags", Value::Int(2)),
                ])]),
            ),
            (
                "m_sceneObjectLayerIndices",
                Value::Array(vec![Value::Int(1)]),
            ),
            (
                "m_aggregateSceneObjects",
                Value::Array(vec![obj(vec![
                    (
                        "m_renderableModel",
                        Value::String("models/agg.vmdl_c".into()),
                    ),
                    ("m_nLayer", Value::Int(0)),
                    (
                        "m_fragmentTransforms",
                        Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
                    ),
                    (
                        "m_aggregateMeshes",
                        Value::Array(vec![
                            Value::Int(0),
                            Value::Int(0),
                            Value::Int(0),
                            Value::Int(0),
                        ]),
                    ),
                ])]),
            ),
            (
                "m_clutterSceneObjects",
                Value::Array(vec![Value::Int(0), Value::Int(0)]),
            ),
            (
                "m_layerNames",
                Value::Array(vec![Value::String("Layer0".into())]),
            ),
        ]);

        let node = decode_world_node(&root).unwrap();
        assert_eq!(node.scene_objects.len(), 1);
        let so = &node.scene_objects[0];
        assert_eq!(so.renderable_model.as_deref(), Some("models/foo.vmdl_c"));
        assert_eq!(so.object_type_flags, 2);
        assert_eq!(so.layer_index, Some(1));
        assert_eq!(
            so.transform,
            [
                [1.0, 0.0, 0.0, 10.0],
                [0.0, 1.0, 0.0, 20.0],
                [0.0, 0.0, 1.0, 30.0],
            ]
        );

        assert_eq!(node.aggregate_scene_objects.len(), 1);
        let agg = &node.aggregate_scene_objects[0];
        assert_eq!(agg.renderable_model.as_deref(), Some("models/agg.vmdl_c"));
        assert_eq!(agg.layer, Some(0));
        assert_eq!(agg.fragment_count, 3);
        assert_eq!(agg.aggregate_mesh_count, 4);

        assert_eq!(node.clutter_scene_object_count, 2);
        assert_eq!(node.layer_names, vec!["Layer0".to_string()]);
    }

    #[test]
    fn decode_world_node_missing_fields_default_gracefully() {
        let root = obj(vec![]);
        let node = decode_world_node(&root).unwrap();
        assert!(node.scene_objects.is_empty());
        assert!(node.aggregate_scene_objects.is_empty());
        assert_eq!(node.clutter_scene_object_count, 0);
        assert!(node.layer_names.is_empty());
    }
}
