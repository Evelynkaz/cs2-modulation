//! Serializable extraction outputs: entities, nav areas, cache metadata, and the policy report.

use serde::{Deserialize, Serialize};

use s2fmt::entities::EntityValue;
use s2fmt::kv3;
use s2fmt::nav::NavMesh;

/// One entity, as dumped for `entities.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRecord {
    pub classname: String,
    pub targetname: Option<String>,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub model: Option<String>,
    pub hammer_id: Option<String>,
    pub properties: serde_json::Map<String, serde_json::Value>,
    /// The name of the child entity lump (`m_childLumps`) this entity came from, if any
    /// (`docs/FORMATS.md` §7; point_template child lumps are not applied -- geometry-wise -- but
    /// their entities are still dumped for inspection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_child_lump: Option<String>,
}

/// Converts a decoded [`EntityValue`] to JSON, for the entity dump.
pub fn entity_value_to_json(v: &EntityValue) -> serde_json::Value {
    match v {
        EntityValue::Bool(b) => serde_json::Value::Bool(*b),
        EntityValue::Int(i) => serde_json::Value::from(*i),
        EntityValue::UInt(u) => serde_json::Value::from(*u),
        EntityValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        EntityValue::String(s) => serde_json::Value::String(s.clone()),
        EntityValue::Vector(v) => serde_json::json!([v[0], v[1], v[2]]),
        EntityValue::Color(c) => serde_json::json!([c[0], c[1], c[2], c[3]]),
        EntityValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(entity_value_to_json).collect())
        }
        EntityValue::Other(v) => kv3::to_text(&kv3::Document {
            format: kv3::FORMAT_GENERIC,
            encoding: Some(kv3::ENCODING_TEXT),
            version: kv3::Kv3Version::Text,
            root: v.clone(),
        })
        .into(),
    }
}

/// One nav area, as dumped for `nav.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavAreaDump {
    pub id: u32,
    pub hull_index: u8,
    pub attribute_flags: u64,
    pub corners: Vec<[f32; 3]>,
    /// Distinct area ids this area connects to, across all its edges.
    pub connections: Vec<u32>,
    pub ladders_above: Vec<u32>,
    pub ladders_below: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavLadderDump {
    pub id: u32,
    pub width: f32,
    pub top: [f32; 3],
    pub bottom: [f32; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationHullDump {
    pub enabled: bool,
    pub radius: f32,
    pub height: f32,
}

/// A full parsed `.nav` dump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavAreasDump {
    pub version: u32,
    pub sub_version: u32,
    pub generation_hulls: Vec<GenerationHullDump>,
    pub areas: Vec<NavAreaDump>,
    pub ladders: Vec<NavLadderDump>,
}

impl NavAreasDump {
    pub fn from_nav_mesh(nav: &NavMesh) -> Self {
        let generation_hulls = nav
            .generation_params
            .as_ref()
            .map(|p| {
                p.hulls
                    .iter()
                    .map(|h| GenerationHullDump {
                        enabled: h.enabled,
                        radius: h.radius,
                        height: h.height,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let areas = nav
            .areas
            .iter()
            .map(|a| {
                let mut connections: Vec<u32> = a
                    .connections
                    .iter()
                    .flat_map(|edge| edge.iter().map(|c| c.area_id))
                    .collect();
                connections.sort_unstable();
                connections.dedup();
                NavAreaDump {
                    id: a.id,
                    hull_index: a.hull_index,
                    attribute_flags: a.attribute_flags,
                    corners: a.corners.clone(),
                    connections,
                    ladders_above: a.ladders_above.clone(),
                    ladders_below: a.ladders_below.clone(),
                }
            })
            .collect();

        let ladders = nav
            .ladders
            .iter()
            .map(|l| NavLadderDump {
                id: l.id,
                width: l.width,
                top: l.top,
                bottom: l.bottom,
            })
            .collect();

        NavAreasDump {
            version: nav.version,
            sub_version: nav.sub_version,
            generation_hulls,
            areas,
            ladders,
        }
    }
}

/// Extraction cache metadata, written into `manifest.json` alongside the cache file list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractMeta {
    pub map: String,
    pub game_build: String,
    pub extractor_version: u32,
    pub map_vpk_sha256: String,
    /// `(vpk path, sha256)` for every shared `*_dir.vpk` used.
    pub shared_vpk_sha256: Vec<(String, String)>,
    /// RFC3339 UTC timestamp, e.g. `2026-09-17T12:34:56Z`.
    pub created_utc: String,
    pub timing_ms: u64,
}

/// Per-attribute triangle count, for `report.json`'s `triangles_per_attribute`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributeTriangleCount {
    /// Index into the mesh's attribute table.
    pub index: u16,
    pub name: String,
    pub interact_as: Vec<String>,
    pub interact_exclude: Vec<String>,
    pub synthetic: bool,
    pub triangles: usize,
}

/// One entry merged into the mesh from an entity's own PHYS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergedEntity {
    pub classname: String,
    pub targetname: Option<String>,
    pub model: String,
    pub attribute: String,
    pub triangles: usize,
    /// Distinct `(group, interact_as)` pairs this entity's own PHYS collision attributes carried
    /// (diagnostic only: entity attribute tables are not merged into the mesh's own).
    pub own_attribute_groups: Vec<String>,
}

/// One entity that had a model in [`crate::policy::SOLID_ENTITY_CLASSES`] but was not merged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedEntity {
    pub classname: String,
    pub targetname: Option<String>,
    pub model: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassableGlassEntity {
    pub classname: String,
    pub targetname: Option<String>,
    pub model: String,
}

/// Every policy decision made while building the mesh, for `report.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractReport {
    pub triangles_per_attribute: Vec<AttributeTriangleCount>,
    /// `(object kind name, triangle count)`.
    pub triangles_per_kind: Vec<(String, usize)>,
    /// World hull `m_nFlags` histogram: `(flags, count)`.
    pub world_hull_flags_histogram: Vec<(u32, usize)>,
    /// `(source, count)`, source one of "world"/"entity"/"static_prop".
    pub spheres_by_source: Vec<(String, usize)>,
    pub capsules_by_source: Vec<(String, usize)>,
    /// Number of parts (across every phys aggregate processed) whose bind pose was not identity.
    pub bind_pose_non_identity_count: usize,
    pub degenerate_triangles_skipped: u64,

    pub entities_per_class: Vec<(String, usize)>,
    pub merged_entities: Vec<MergedEntity>,
    pub skipped_entities: Vec<SkippedEntity>,
    pub skipped_by_reason: Vec<(String, usize)>,
    pub passable_glass: Vec<PassableGlassEntity>,

    pub static_props_total: usize,
    pub static_props_with_phys: usize,
    pub static_props_without_phys: usize,
    pub static_props_models_not_found: usize,
    /// Unique models that failed to load (distinct from [`Self::static_props_models_not_found`]:
    /// the model was found but reading/parsing it failed).
    pub static_props_load_errors: usize,
    pub aggregates_skipped: usize,

    pub point_template_count: usize,
    pub lumps_with_child_lumps: usize,

    pub surface_indices_out_of_range: u64,

    pub warnings: Vec<String>,
}

impl ExtractReport {
    pub fn record_skip(
        &mut self,
        classname: &str,
        targetname: Option<&str>,
        model: &str,
        reason: &str,
    ) {
        self.skipped_entities.push(SkippedEntity {
            classname: classname.to_string(),
            targetname: targetname.map(str::to_string),
            model: model.to_string(),
            reason: reason.to_string(),
        });
        match self.skipped_by_reason.iter_mut().find(|(r, _)| r == reason) {
            Some((_, c)) => *c += 1,
            None => self.skipped_by_reason.push((reason.to_string(), 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_report_round_trips_through_json() {
        let mut report = ExtractReport::default();
        report.record_skip("func_brush", Some("retake_a"), "models/x.vmdl", "retake");
        report.record_skip("func_brush", Some("retake_b"), "models/y.vmdl", "retake");
        report.merged_entities.push(MergedEntity {
            classname: "func_clip_vphysics".to_string(),
            targetname: None,
            model: "models/clip.vmdl".to_string(),
            attribute: "EntityPhysicsClip".to_string(),
            triangles: 12,
            own_attribute_groups: vec!["Default:[]".to_string()],
        });
        report.triangles_per_attribute.push(AttributeTriangleCount {
            index: 0,
            name: "Default".to_string(),
            interact_as: Vec::new(),
            interact_exclude: Vec::new(),
            synthetic: false,
            triangles: 100,
        });
        report.warnings.push("something".to_string());

        let text = serde_json::to_string(&report).unwrap();
        let back: ExtractReport = serde_json::from_str(&text).unwrap();
        assert_eq!(report, back);
        assert_eq!(back.skipped_by_reason, vec![("retake".to_string(), 2)]);
    }

    #[test]
    fn entity_value_to_json_variants() {
        assert_eq!(
            entity_value_to_json(&EntityValue::Bool(true)),
            serde_json::json!(true)
        );
        assert_eq!(
            entity_value_to_json(&EntityValue::Int(-5)),
            serde_json::json!(-5)
        );
        assert_eq!(
            entity_value_to_json(&EntityValue::UInt(5)),
            serde_json::json!(5)
        );
        assert_eq!(
            entity_value_to_json(&EntityValue::String("x".into())),
            serde_json::json!("x")
        );
        assert_eq!(
            entity_value_to_json(&EntityValue::Vector([1.0, 2.0, 3.0])),
            serde_json::json!([1.0, 2.0, 3.0])
        );
        assert_eq!(
            entity_value_to_json(&EntityValue::Array(vec![
                EntityValue::Int(1),
                EntityValue::Int(2)
            ])),
            serde_json::json!([1, 2])
        );
    }
}
