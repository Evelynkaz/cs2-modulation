//! CS2 map-extraction gameplay policy: which entities are solid, which props are breakable
//! glass, and how a solid class maps to a collision-attribute name. Each function documents its
//! source in `cs2-smoke-solver/src/Extraction/MapExtractor.cs`.

use s2fmt::entities::{Entity, EntityValue};
use s2fmt::kv3::Value;

/// Brush/prop classes whose compiled models are solid to physics objects at round start
/// (`MapExtractor.cs:997`). `prop_dynamic_override` is deliberately absent: entities of that
/// class are reported as skipped ("class not in allowlist") so the policy can be revisited.
pub const SOLID_ENTITY_CLASSES: &[&str] = &[
    "func_brush",
    "func_clip_vphysics",
    "func_door",
    "func_door_rotating",
    "func_breakable",
    "prop_door_rotating",
    "prop_dynamic",
];

/// True if `targetname` contains "retake" or `model` contains "/retake_" (case-insensitive):
/// Retake-only geometry that is not solid in Defusal (`MapExtractor.cs:1075-1077`).
pub fn is_retake_only(targetname: &str, model: &str) -> bool {
    targetname.to_ascii_lowercase().contains("retake")
        || model.to_ascii_lowercase().contains("/retake_")
}

/// True if the entity's `startdisabled` is a boolean true, an integer `1`, or one of the strings
/// `"1"`/`"true"`/`"True"` (`MapExtractor.cs:1086-1088`; [`Entity::get_bool`] already implements
/// this exact set of accepted spellings).
pub fn starts_disabled(entity: &Entity) -> bool {
    entity.get_bool("startdisabled").unwrap_or(false)
}

/// Reads an integer-ish property as an `i64`, accepting an `Int`, a `UInt` that fits, or a
/// numeric string -- the same encoding accepted by [`starts_disabled`] for booleans, needed
/// because real compiled entity lumps disagree on how they store small integer keyvalues (e.g.
/// `de_mirage` stores `func_brush`'s `solidity` as an int; several other maps store it as a
/// string).
fn property_int(entity: &Entity, key: &str) -> Option<i64> {
    match entity.get(key)? {
        EntityValue::Int(i) => Some(*i),
        EntityValue::UInt(u) => i64::try_from(*u).ok(),
        EntityValue::String(s) => s.parse().ok(),
        EntityValue::Other(v) => v.as_i64().or_else(|| v.as_str()?.parse().ok()),
        _ => None,
    }
}

/// True if a `func_brush`'s `solidity` is `1`: Hammer's FGD `Solidity` enum is `0` = Toggle
/// (solid unless start-disabled, already handled by [`starts_disabled`]), `1` = Never Solid,
/// `2` = Always Solid. A never-solid `func_brush` merged as solid was confirmed, against a GOTV
/// demo on de_mirage, to be the cause of two of five simulated smoke rests missing the game's by
/// hundreds of units (see the ground-truth regression test).
pub fn never_solid_func_brush(entity: &Entity) -> bool {
    property_int(entity, "solidity") == Some(1)
}

/// True if a `prop_dynamic`'s `solid` is `0`, i.e. `SOLID_NONE` in the Source engine's
/// `solid_t` enum (`0` None, `1` BSP, `2` BBox, `3` OBB, `4` OBB yaw, `5` Custom, `6` VPhysics,
/// `7` Bounds): the model has no collision at all. Seen on ~106 of 689 `prop_dynamic` entities
/// across the 23 cached maps (most of the rest are `6`, VPhysics).
pub fn not_solid_prop_dynamic(entity: &Entity) -> bool {
    property_int(entity, "solid") == Some(0)
}

/// True if the model's KV3 keyvalues have a top-level `break_list` key and no
/// `break_command_list` key: the model shatters into pieces when damaged, rather than just
/// switching to an already-open state (`MapExtractor.cs:1047-1066`).
pub fn breakable_model(keyvalues: Option<&Value>) -> bool {
    let Some(root) = keyvalues.and_then(Value::as_object) else {
        return false;
    };
    root.get("break_list").is_some() && root.get("break_command_list").is_none()
}

/// True if the model's `prop_data.base` starts with `"Glass"` (case-insensitive) when
/// non-empty, else if `model_path` contains `"window"` or `"glass"` (case-insensitive)
/// (`MapExtractor.cs:1014-1022`).
pub fn passable_glass(keyvalues: Option<&Value>, model_path: &str) -> bool {
    let base = keyvalues
        .and_then(|v| v.get("prop_data"))
        .and_then(|v| v.get("base"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(base) = base {
        return base.to_ascii_lowercase().starts_with("glass");
    }
    let lower = model_path.to_ascii_lowercase();
    lower.contains("window") || lower.contains("glass")
}

/// The synthetic collision-attribute name a solid entity's geometry is merged under
/// (`MapExtractor.cs:1203-1212`): each gets its own group so a consumer (grenade vs. player vs.
/// sightline filters) can exclude clips/doors/breakables independently, rather than merging
/// everything into "Default".
pub fn entity_attribute_name(classname: &str, passable_glass: bool) -> &'static str {
    match classname {
        "func_clip_vphysics" => "EntityPhysicsClip",
        "func_door" | "func_door_rotating" | "prop_door_rotating" => "EntityDoor",
        "func_breakable" => "EntityBreakable",
        "prop_dynamic" if passable_glass => "EntityBreakable",
        "prop_dynamic" => "EntitySolid",
        _ => "EntitySolid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2fmt::entities::EntityValue;
    use s2fmt::kv3::Object;

    fn obj(entries: Vec<(&str, Value)>) -> Value {
        let mut o = Object::new();
        for (k, v) in entries {
            o.push(k, v);
        }
        Value::Object(o)
    }

    #[test]
    fn retake_by_targetname_or_model_path() {
        assert!(is_retake_only("[PR#]retake.asite", "models/foo.vmdl"));
        assert!(is_retake_only("[PR#]RETAKE.asite", "models/foo.vmdl"));
        assert!(is_retake_only(
            "brush01",
            "maps/de_mirage/entities/retake_asite.vmdl"
        ));
        assert!(!is_retake_only(
            "brush01",
            "maps/de_mirage/entities/asite.vmdl"
        ));
    }

    #[test]
    fn starts_disabled_variants() {
        let mut e = Entity::default();
        e.properties
            .push(("startdisabled".into(), EntityValue::String("1".into())));
        assert!(starts_disabled(&e));

        let mut e = Entity::default();
        e.properties
            .push(("startdisabled".into(), EntityValue::String("true".into())));
        assert!(starts_disabled(&e));

        let mut e = Entity::default();
        e.properties
            .push(("startdisabled".into(), EntityValue::Bool(true)));
        assert!(starts_disabled(&e));

        let e = Entity::default();
        assert!(!starts_disabled(&e));

        let mut e = Entity::default();
        e.properties
            .push(("startdisabled".into(), EntityValue::String("0".into())));
        assert!(!starts_disabled(&e));
    }

    #[test]
    fn never_solid_func_brush_accepts_int_or_string() {
        let mut e = Entity::default();
        e.properties.push(("solidity".into(), EntityValue::Int(1)));
        assert!(never_solid_func_brush(&e));

        let mut e = Entity::default();
        e.properties
            .push(("solidity".into(), EntityValue::String("1".into())));
        assert!(never_solid_func_brush(&e));

        let mut e = Entity::default();
        e.properties.push(("solidity".into(), EntityValue::Int(0)));
        assert!(!never_solid_func_brush(&e));

        let mut e = Entity::default();
        e.properties
            .push(("solidity".into(), EntityValue::String("0".into())));
        assert!(!never_solid_func_brush(&e));

        let mut e = Entity::default();
        e.properties.push(("solidity".into(), EntityValue::Int(2)));
        assert!(!never_solid_func_brush(&e));

        let e = Entity::default();
        assert!(!never_solid_func_brush(&e));
    }

    #[test]
    fn not_solid_prop_dynamic_accepts_int_or_string() {
        let mut e = Entity::default();
        e.properties.push(("solid".into(), EntityValue::Int(0)));
        assert!(not_solid_prop_dynamic(&e));

        let mut e = Entity::default();
        e.properties
            .push(("solid".into(), EntityValue::String("0".into())));
        assert!(not_solid_prop_dynamic(&e));

        let mut e = Entity::default();
        e.properties.push(("solid".into(), EntityValue::Int(6)));
        assert!(!not_solid_prop_dynamic(&e));

        let e = Entity::default();
        assert!(!not_solid_prop_dynamic(&e));
    }

    #[test]
    fn breakable_model_needs_break_list_without_break_command_list() {
        let kv = obj(vec![("break_list", Value::Array(vec![]))]);
        assert!(breakable_model(Some(&kv)));

        let kv = obj(vec![
            ("break_list", Value::Array(vec![])),
            ("break_command_list", Value::Array(vec![])),
        ]);
        assert!(!breakable_model(Some(&kv)));

        let kv = obj(vec![("other", Value::Int(1))]);
        assert!(!breakable_model(Some(&kv)));
        assert!(!breakable_model(None));
    }

    #[test]
    fn passable_glass_from_prop_data_base() {
        let kv = obj(vec![(
            "prop_data",
            obj(vec![("base", Value::String("Glass.Window".into()))]),
        )]);
        assert!(passable_glass(Some(&kv), "models/whatever.vmdl"));

        let kv = obj(vec![(
            "prop_data",
            obj(vec![("base", Value::String("Wooden.Medium".into()))]),
        )]);
        assert!(!passable_glass(Some(&kv), "models/window_pane.vmdl"));
    }

    #[test]
    fn passable_glass_falls_back_to_model_path() {
        assert!(passable_glass(None, "models/props/de_office/window01.vmdl"));
        assert!(passable_glass(None, "models/props/de_nuke/glass_pane.vmdl"));
        assert!(!passable_glass(None, "models/props/de_dust/crate01.vmdl"));
    }

    #[test]
    fn attribute_name_mapping() {
        assert_eq!(
            entity_attribute_name("func_clip_vphysics", false),
            "EntityPhysicsClip"
        );
        assert_eq!(entity_attribute_name("func_door", false), "EntityDoor");
        assert_eq!(
            entity_attribute_name("func_door_rotating", false),
            "EntityDoor"
        );
        assert_eq!(
            entity_attribute_name("prop_door_rotating", false),
            "EntityDoor"
        );
        assert_eq!(
            entity_attribute_name("func_breakable", false),
            "EntityBreakable"
        );
        assert_eq!(
            entity_attribute_name("prop_dynamic", true),
            "EntityBreakable"
        );
        assert_eq!(entity_attribute_name("prop_dynamic", false), "EntitySolid");
        assert_eq!(entity_attribute_name("func_brush", false), "EntitySolid");
    }
}
