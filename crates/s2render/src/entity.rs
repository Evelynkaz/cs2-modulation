//! Entity visibility, tint and skin rules (§3). Entity walking/transform itself is
//! `s2fmt::entities` (`Entity`, `entity_transform`) -- used directly by `export.rs`, not
//! wrapped here.
//!
//! Ground truth: `IO/Gltf/GltfModelExporter.World.cs:90-192 LoadEntityMeshes`,
//! `GltfModelExporter.World.cs:194-203 GetSkinPathFromModel`; entity table cross-checked against
//! `REPORT.md` §1 / `entity_table.md`.

use std::collections::HashMap;

use s2fmt::entities::{Entity, EntityValue};

use crate::model::Model;

fn entity_num(v: Option<&EntityValue>) -> Option<f64> {
    match v? {
        EntityValue::Int(i) => Some(*i as f64),
        EntityValue::UInt(u) => Some(*u as f64),
        EntityValue::Float(f) => Some(*f),
        EntityValue::String(s) => s.trim().parse::<f64>().ok(),
        EntityValue::Other(v) => v.as_f64(),
        _ => None,
    }
}

fn rendermode_is_none(entity: &Entity) -> bool {
    match entity.get("rendermode") {
        Some(EntityValue::String(s)) => {
            s.eq_ignore_ascii_case("kRenderNone")
                || entity_num(entity.get("rendermode")) == Some(10.0)
        }
        Some(other) => entity_num(Some(other)) == Some(10.0),
        None => false,
    }
}

/// Whether this entity's model should be drawn at all (§3): `startdisabled` true, `enabled`
/// false, `renderamt == 0`, or `rendermode` 10/`kRenderNone` all hide it. Trigger-like classes
/// need no separate list -- their models simply have no render mesh (checked by the caller once
/// the model is loaded), so this only decides visibility, not "does it have geometry".
pub fn should_render(entity: &Entity) -> bool {
    if entity.get_bool("startdisabled") == Some(true) {
        return false;
    }
    if entity.get_bool("enabled") == Some(false) {
        return false;
    }
    if entity_num(entity.get("renderamt")) == Some(0.0) {
        return false;
    }
    if rendermode_is_none(entity) {
        return false;
    }
    true
}

/// This entity's combined tint (`rendercolor` x `renderamt`), as a 0..1 RGBA multiplier
/// (`GltfModelExporter.World.cs:157-167`). Values that already look like 0..255 bytes (any
/// component > 1.5) are rescaled; both `renderamt` forms (a 0..1 float and a 0..255 byte) show up
/// across this codebase's entity dumps, so the same tolerant check is applied to both. White
/// (`[1,1,1,1]`) when absent, matching Mirage's own data (§1: rendercolor white everywhere,
/// renderamt 255 everywhere).
pub fn entity_tint(entity: &Entity) -> [f32; 4] {
    let mut rendercolor = entity.get_vec3("rendercolor").unwrap_or([1.0, 1.0, 1.0]);
    if rendercolor.iter().any(|&c| c > 1.5) {
        rendercolor = rendercolor.map(|c| c / 255.0);
    }
    let mut renderamt = entity_num(entity.get("renderamt"))
        .map(|v| v as f32)
        .unwrap_or(1.0);
    if renderamt > 1.5 {
        renderamt /= 255.0;
    }
    [rendercolor[0], rendercolor[1], rendercolor[2], renderamt]
}

/// The entity's `skin` property, normalised to `None` for "no skin" (`"0"`/`"default"`, or
/// absent) -- `GltfModelExporter.World.cs:151-155`.
pub fn skin_name(entity: &Entity) -> Option<&str> {
    let s = entity.get_str("skin")?;
    if s == "0" || s.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(s)
    }
}

/// Builds a `default[i] -> skin[i]` material path remap for a model's `skin` material group
/// (§3, `REPORT.md` §5 item 8: the reference's own exporter gets this wrong, taking the skin
/// group's *first* material for every draw call). `None` if the model has no `"default"` group or
/// no group named `skin`.
pub fn skin_remap(model: &Model, skin: &str) -> Option<HashMap<String, String>> {
    let default_group = model.material_groups.iter().find(|g| g.name == "default")?;
    let skin_group = model.material_groups.iter().find(|g| g.name == skin)?;
    let mut map = HashMap::with_capacity(default_group.materials.len());
    for (i, default_mat) in default_group.materials.iter().enumerate() {
        if let Some(skin_mat) = skin_group.materials.get(i) {
            map.insert(default_mat.clone(), skin_mat.clone());
        }
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MaterialGroup;

    fn entity_with(props: Vec<(&str, EntityValue)>) -> Entity {
        Entity {
            properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            connections: Vec::new(),
        }
    }

    #[test]
    fn startdisabled_hides() {
        let e = entity_with(vec![("startdisabled", EntityValue::Bool(true))]);
        assert!(!should_render(&e));
    }

    #[test]
    fn enabled_false_hides() {
        let e = entity_with(vec![("enabled", EntityValue::Bool(false))]);
        assert!(!should_render(&e));
    }

    #[test]
    fn renderamt_zero_hides() {
        let e = entity_with(vec![("renderamt", EntityValue::Int(0))]);
        assert!(!should_render(&e));
    }

    #[test]
    fn rendermode_10_or_krendernone_hides() {
        let numeric = entity_with(vec![("rendermode", EntityValue::String("10".into()))]);
        assert!(!should_render(&numeric));
        let named = entity_with(vec![(
            "rendermode",
            EntityValue::String("kRenderNone".into()),
        )]);
        assert!(!should_render(&named));
        let normal = entity_with(vec![(
            "rendermode",
            EntityValue::String("kRenderNormal".into()),
        )]);
        assert!(should_render(&normal));
        let zero = entity_with(vec![("rendermode", EntityValue::String("0".into()))]);
        assert!(should_render(&zero));
    }

    #[test]
    fn default_entity_renders() {
        assert!(should_render(&Entity::default()));
    }

    #[test]
    fn tint_defaults_to_white() {
        assert_eq!(entity_tint(&Entity::default()), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn tint_rescales_byte_range_rendercolor_and_renderamt() {
        let e = entity_with(vec![
            ("rendercolor", EntityValue::Vector([171.0, 254.0, 243.0])),
            ("renderamt", EntityValue::Int(128)),
        ]);
        let t = entity_tint(&e);
        assert!((t[0] - 171.0 / 255.0).abs() < 1e-5);
        assert!((t[1] - 254.0 / 255.0).abs() < 1e-5);
        assert!((t[2] - 243.0 / 255.0).abs() < 1e-5);
        assert!((t[3] - 128.0 / 255.0).abs() < 1e-5);
    }

    #[test]
    fn skin_name_normalizes_zero_and_default() {
        assert_eq!(
            skin_name(&entity_with(vec![(
                "skin",
                EntityValue::String("0".into())
            )])),
            None
        );
        assert_eq!(
            skin_name(&entity_with(vec![(
                "skin",
                EntityValue::String("default".into())
            )])),
            None
        );
        assert_eq!(
            skin_name(&entity_with(vec![(
                "skin",
                EntityValue::String("1".into())
            )])),
            Some("1")
        );
        assert_eq!(skin_name(&Entity::default()), None);
    }

    #[test]
    fn skin_remap_maps_by_index_not_first_material() {
        let model = Model {
            material_groups: vec![
                MaterialGroup {
                    name: "default".to_string(),
                    materials: vec![
                        "materials/tv_off.vmat".to_string(),
                        "materials/frame.vmat".to_string(),
                    ],
                },
                MaterialGroup {
                    name: "1".to_string(),
                    materials: vec![
                        "materials/tv_channel1.vmat".to_string(),
                        "materials/frame.vmat".to_string(),
                    ],
                },
            ],
            ..Default::default()
        };
        let map = skin_remap(&model, "1").expect("remap");
        assert_eq!(
            map.get("materials/tv_off.vmat").map(String::as_str),
            Some("materials/tv_channel1.vmat")
        );
        // Same material at index 1 in both groups: remaps to itself, not folded away.
        assert_eq!(
            map.get("materials/frame.vmat").map(String::as_str),
            Some("materials/frame.vmat")
        );
    }

    #[test]
    fn skin_remap_none_without_default_or_named_group() {
        let model = Model::default();
        assert!(skin_remap(&model, "1").is_none());
    }
}
