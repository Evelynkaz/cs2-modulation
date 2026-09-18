//! Entity lumps (`*.vents_c`): KV3 and legacy key-value forms.
//!
//! `docs/FORMATS.md` section 7; `VRF/Resource/ResourceTypes/EntityLump.cs`,
//! `EntityTransformHelper.cs`, `Resource/Enums/EntityFieldType.cs`.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::hash::string_token;
use crate::kv3::{self, Value};
use crate::util::Reader;

/// Keys this project resolves for hashed (unnamed) legacy fields, instead of leaving them as
/// `hash_{:08x}`. This is a small, project-scoped subset of VRF's ~6000-entry
/// `EntityLumpKnownKeys.cs` table (every key below is present, lowercase, in that file) — just
/// enough to keep `classname`/`origin`/etc. readable when the compiler chose the hashed-only
/// encoding for them (`docs/FORMATS.md` section 7).
const KNOWN_KEYS: &[&str] = &[
    "classname",
    "targetname",
    "origin",
    "angles",
    "scales",
    "model",
    "hammeruniqueid",
    "spawnflags",
    "startdisabled",
    "health",
    "solid",
    "parentname",
    "entitylumpname",
    "place_name",
    "disableshadows",
    "skin",
    "rendermode",
    "renderamt",
    "rendercolor",
    "globalname",
    "disablereceiveshadows",
    "fademindist",
    "fademaxdist",
    "team",
    "enabled",
    "speed",
    "spawnpos",
    "priority",
    "rotation",
];

/// `string_token(key) -> key` for every entry in [`KNOWN_KEYS`], built once on first use.
fn known_keys() -> &'static HashMap<u32, &'static str> {
    static TABLE: OnceLock<HashMap<u32, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| KNOWN_KEYS.iter().map(|&k| (string_token(k), k)).collect())
}

/// A decoded entity property value. Legacy-blob and KV3 forms both funnel into this; numeric
/// variants preserve the source's signedness the way [`kv3::Value`] does.
#[derive(Debug, Clone, PartialEq)]
pub enum EntityValue {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(String),
    /// Legacy `Vector`/`QAngle` fields (`EntityFieldType.Vector`/`QAngle`, type codes 0x03/0x27).
    Vector([f64; 3]),
    /// Legacy `Color32` fields (type code 0x09): raw RGBA bytes.
    Color([u8; 4]),
    Array(Vec<EntityValue>),
    /// A KV3 value with no direct mapping above (object, blob, null).
    Other(kv3::Value),
}

impl EntityValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            EntityValue::Int(i) => Some(*i as f64),
            EntityValue::UInt(u) => Some(*u as f64),
            EntityValue::Float(f) => Some(*f),
            EntityValue::Other(v) => v.as_f64(),
            _ => None,
        }
    }
}

/// An entity I/O connection (`m_connections[]`, `EntityLump.cs:157-191`).
#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    pub output_name: String,
    pub target_name: String,
    pub input_name: String,
    pub override_param: String,
    pub delay: f32,
    pub times_to_fire: i32,
    pub target_type: i32,
}

/// A single entity: its properties (keys always lowercase) and I/O connections.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entity {
    pub properties: Vec<(String, EntityValue)>,
    pub connections: Vec<Connection>,
}

impl Entity {
    /// Looks up a property by key, case-insensitively.
    pub fn get(&self, key: &str) -> Option<&EntityValue> {
        let key = key.to_ascii_lowercase();
        self.properties
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// Looks up a string property by key, case-insensitively.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            EntityValue::String(s) => Some(s.as_str()),
            EntityValue::Other(v) => v.as_str(),
            _ => None,
        }
    }

    /// The entity's `classname`. Entities without one are dropped by
    /// [`decode_entity_lump`], so this is always non-empty for a decoded entity.
    pub fn classname(&self) -> &str {
        self.get_str("classname").unwrap_or("")
    }

    /// The entity's `targetname`, if any (may carry a `[PR#]` point-template prefix;
    /// `EntityLump.cs:607-622` strips it, this does not).
    pub fn targetname(&self) -> Option<&str> {
        self.get_str("targetname")
    }

    /// Reads a 3-vector property: a KV3/legacy array of 3 numbers, a legacy
    /// `Vector`/`QAngle` value, or a `"x y z"` string (`EntityTransformHelper.cs:318-339`).
    pub fn get_vec3(&self, key: &str) -> Option<[f32; 3]> {
        match self.get(key)? {
            EntityValue::Vector(v) => Some([v[0] as f32, v[1] as f32, v[2] as f32]),
            EntityValue::Array(items) if items.len() == 3 => {
                let mut out = [0f32; 3];
                for (i, item) in items.iter().enumerate() {
                    out[i] = item.as_f64()? as f32;
                }
                Some(out)
            }
            EntityValue::Other(v) => {
                let arr = v.as_array()?;
                if arr.len() != 3 {
                    return None;
                }
                let mut out = [0f32; 3];
                for (i, item) in arr.iter().enumerate() {
                    out[i] = item.as_f64()? as f32;
                }
                Some(out)
            }
            EntityValue::String(s) => parse_vec3_str(s),
            _ => None,
        }
    }

    /// `origin` (`EntityTransformHelper.cs:32`), default `(0, 0, 0)`.
    pub fn origin(&self) -> [f32; 3] {
        self.get_vec3("origin").unwrap_or([0.0, 0.0, 0.0])
    }

    /// `angles` as (pitch, yaw, roll) degrees (`EntityTransformHelper.cs:33`), default
    /// `(0, 0, 0)`.
    pub fn angles(&self) -> [f32; 3] {
        self.get_vec3("angles").unwrap_or([0.0, 0.0, 0.0])
    }

    /// `scales` (`EntityTransformHelper.cs:31`), default `(1, 1, 1)`.
    pub fn scales(&self) -> [f32; 3] {
        self.get_vec3("scales").unwrap_or([1.0, 1.0, 1.0])
    }

    /// Reads a boolean-ish property, matching the reference's `startdisabled`-style semantics:
    /// `Bool`, integer `0`/`1` exactly (any other integer is not a boolean), or the strings
    /// `"1"`/`"true"`/`"True"` (true) / `"0"`/`"false"`/`"False"` (false) exactly.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            EntityValue::Bool(b) => Some(*b),
            EntityValue::Int(i) => int_to_bool(*i),
            EntityValue::UInt(u) => int_to_bool(*u as i64),
            EntityValue::Other(v) => {
                if let Some(b) = v.as_bool() {
                    Some(b)
                } else if let Some(i) = v.as_i64() {
                    int_to_bool(i)
                } else {
                    v.as_str().and_then(parse_bool_str)
                }
            }
            EntityValue::String(s) => parse_bool_str(s),
            _ => None,
        }
    }
}

fn int_to_bool(i: i64) -> Option<bool> {
    match i {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn parse_bool_str(s: &str) -> Option<bool> {
    match s {
        "1" | "true" | "True" => Some(true),
        "0" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Parses a `"x y z"` string into 3 invariant floats (`EntityTransformHelper.cs:318-339`).
fn parse_vec3_str(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split(' ');
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some([x, y, z])
}

/// An entity lump (`*.vents_c` `DATA` block).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntityLump {
    pub name: String,
    pub child_lumps: Vec<String>,
    pub entities: Vec<Entity>,
}

/// An error decoding an entity lump.
#[derive(Debug, thiserror::Error)]
pub enum EntityError {
    #[error("unsupported entity keyvalues version {version} (expected 1)")]
    UnsupportedVersion { version: i64 },
    #[error(
        "legacy entity key '{name}' hash {hash:#010x} does not match computed hash {computed:#010x}"
    )]
    KeyHashMismatch {
        name: String,
        hash: u32,
        computed: u32,
    },
    #[error("unknown legacy entity field type {type_id:#04x}")]
    UnknownFieldType { type_id: u32 },
    #[error("truncated legacy entity keyvalues data: {0}")]
    Truncated(#[from] crate::util::ReadError),
}

/// Decodes an entity lump's `DATA` root (`EntityLump.cs:196-215`).
pub fn decode_entity_lump(root: &Value) -> Result<EntityLump, EntityError> {
    let name = root
        .get("m_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let child_lumps = root
        .get("m_childLumps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut entities = Vec::new();
    if let Some(arr) = root.get("m_entityKeyValues").and_then(|v| v.as_array()) {
        for entity_kv in arr {
            if let Some(entity) = parse_entity(entity_kv)? {
                entities.push(entity);
            }
        }
    }

    Ok(EntityLump {
        name,
        child_lumps,
        entities,
    })
}

/// Parses one `m_entityKeyValues[]` element; `Ok(None)` if it has no `classname`
/// (`EntityLump.cs:217-235`, "are there any kinds of valid entities which don't contain a
/// classname?").
fn parse_entity(entity_kv: &Value) -> Result<Option<Entity>, EntityError> {
    let mut properties = Vec::new();

    if let Some(kv3_data) = entity_kv.get("keyValues3Data") {
        parse_kv3_form(kv3_data, &mut properties)?;
    } else {
        let blob = entity_kv
            .get("m_keyValuesData")
            .and_then(|v| v.as_blob())
            .unwrap_or(&[]);
        parse_legacy_form(blob, &mut properties)?;
    }

    if !properties.iter().any(|(k, _)| k == "classname") {
        return Ok(None);
    }

    let connections = entity_kv
        .get("m_connections")
        .and_then(|v| v.as_array())
        .unwrap_or(&[])
        .iter()
        .map(parse_connection)
        .collect();

    Ok(Some(Entity {
        properties,
        connections,
    }))
}

/// `Connection` fields, `EntityLump.cs:239-249`.
fn parse_connection(kv: &Value) -> Connection {
    Connection {
        output_name: kv
            .get("m_outputName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        target_name: kv
            .get("m_targetName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        input_name: kv
            .get("m_inputName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        override_param: kv
            .get("m_overrideParam")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        delay: kv.get("m_flDelay").and_then(|v| v.as_f32()).unwrap_or(0.0),
        times_to_fire: kv
            .get("m_nTimesToFire")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        target_type: kv.get("m_targetType").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
    }
}

/// `keyValues3Data { version, values{}, attributes{} }` (`EntityLump.cs:255-273`, `278-298`).
fn parse_kv3_form(
    kv3_data: &Value,
    out: &mut Vec<(String, EntityValue)>,
) -> Result<(), EntityError> {
    let version = kv3_data
        .get("version")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if version != 1 {
        return Err(EntityError::UnsupportedVersion { version });
    }

    for section in ["values", "attributes"] {
        if let Some(obj) = kv3_data.get(section).and_then(|v| v.as_object()) {
            for (key, value) in obj.iter() {
                out.push((key.to_ascii_lowercase(), kv3_value_to_entity_value(value)));
            }
        }
    }
    Ok(())
}

/// Converts a KV3 leaf into an [`EntityValue`], preserving array/number shape.
fn kv3_value_to_entity_value(v: &Value) -> EntityValue {
    match v.unflagged() {
        kv3::Value::Bool(b) => EntityValue::Bool(*b),
        kv3::Value::Int(i) => EntityValue::Int(*i),
        kv3::Value::UInt(u) => EntityValue::UInt(*u),
        kv3::Value::Double(d) => EntityValue::Float(*d),
        kv3::Value::String(s) => EntityValue::String(s.clone()),
        kv3::Value::Array(items) => {
            EntityValue::Array(items.iter().map(kv3_value_to_entity_value).collect())
        }
        kv3::Value::Null
        | kv3::Value::Blob(_)
        | kv3::Value::Object(_)
        | kv3::Value::Flagged(..) => EntityValue::Other(v.clone()),
    }
}

/// Legacy `m_keyValuesData` blob layout (`docs/FORMATS.md` section 7, `EntityLump.cs:300-371`).
fn parse_legacy_form(
    bytes: &[u8],
    out: &mut Vec<(String, EntityValue)>,
) -> Result<(), EntityError> {
    if bytes.is_empty() {
        return Ok(());
    }

    let mut r = Reader::new(bytes);
    let version = r.u32()?;
    if version != 1 {
        return Err(EntityError::UnsupportedVersion {
            version: version as i64,
        });
    }
    let hashed_count = r.u32()?;
    let string_count = r.u32()?;

    for _ in 0..hashed_count {
        let hash = r.u32()?;
        let value = read_legacy_value(&mut r)?;
        let key = match known_keys().get(&hash) {
            Some(&name) => name.to_string(),
            None => format!("hash_{hash:08x}"),
        };
        out.push((key, value));
    }

    for _ in 0..string_count {
        let hash = r.u32()?;
        let name = read_cstr_lossy(&mut r)?;
        let computed = string_token(&name);
        if computed != hash {
            return Err(EntityError::KeyHashMismatch {
                name,
                hash,
                computed,
            });
        }
        let value = read_legacy_value(&mut r)?;
        out.push((name.to_ascii_lowercase(), value));
    }

    Ok(())
}

fn read_f32(r: &mut Reader) -> Result<f32, EntityError> {
    let bytes = r.bytes(4)?;
    Ok(f32::from_le_bytes(
        bytes.try_into().expect("checked length"),
    ))
}

/// Reads a NUL-terminated string leniently, like `BinaryReader.ReadNullTermString(Encoding.UTF8)`
/// as .NET actually behaves on invalid bytes: replaces them rather than throwing
/// (`EntityLump.cs:331`, `365`, VRF-like decoding for legacy key names and `CString` values).
/// Still requires a NUL terminator; only invalid UTF-8 is tolerated.
fn read_cstr_lossy(r: &mut Reader) -> Result<String, EntityError> {
    let start = r.pos();
    let remaining = r.remaining();
    let slice = r.bytes(remaining)?;
    match slice.iter().position(|&b| b == 0) {
        Some(nul) => {
            let s = String::from_utf8_lossy(&slice[..nul]).into_owned();
            r.set_pos(start + nul + 1);
            Ok(s)
        }
        None => Err(crate::util::ReadError::MissingNul { offset: start }.into()),
    }
}

/// One legacy field's `u32 type` tag plus its typed payload (`docs/FORMATS.md` section 7).
fn read_legacy_value(r: &mut Reader) -> Result<EntityValue, EntityError> {
    let type_id = r.u32()?;
    match type_id {
        0x06 => Ok(EntityValue::Bool(r.u8()? != 0)),
        0x01 => Ok(EntityValue::Float(read_f32(r)? as f64)),
        0x22 => Ok(EntityValue::Float(r.f64()?)),
        0x09 => {
            let b = r.bytes(4)?;
            Ok(EntityValue::Color([b[0], b[1], b[2], b[3]]))
        }
        0x05 => Ok(EntityValue::Int(r.i32()? as i64)),
        0x25 => Ok(EntityValue::UInt(r.u32()? as u64)),
        0x1a => Ok(EntityValue::Int(r.i64()?)),
        0x21 => Ok(EntityValue::UInt(r.u64()?)),
        0x03 | 0x27 => {
            let x = read_f32(r)?;
            let y = read_f32(r)?;
            let z = read_f32(r)?;
            Ok(EntityValue::Vector([x as f64, y as f64, z as f64]))
        }
        0x1e => Ok(EntityValue::String(read_cstr_lossy(r)?)),
        other => Err(EntityError::UnknownFieldType { type_id: other }),
    }
}

/// Euler angles (pitch, yaw, roll) in degrees to a rotation matrix, matching
/// `EntityTransformHelper.cs:52-59`/`132-140` and `MapExtractor.cs:1364-1374`
/// (`R = Rz(yaw)*Ry(pitch)*Rx(roll)`, column vectors; `forward = R*(1,0,0)^T`).
/// Returned as row-major rows: `matrix[row][col]`.
pub fn angles_to_matrix(angles: [f32; 3]) -> [[f32; 3]; 3] {
    let [pitch, yaw, roll] = angles;
    let (sp, cp) = pitch.to_radians().sin_cos();
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sr, cr) = roll.to_radians().sin_cos();

    [
        [cy * cp, cy * sp * sr - sy * cr, cy * sp * cr + sy * sr],
        [sy * cp, sy * sp * sr + cy * cr, sy * sp * cr - cy * sr],
        [-sp, cp * sr, cp * cr],
    ]
}

/// The full entity placement transform, `v' = R(angles)*(v * scales) + origin`
/// (`docs/FORMATS.md` section 7). Returned as 3 rows of `[Rx*sx, Ry*sy, Rz*sz, t]`
/// (`docs/FORMATS.md` section 6.6 flat 3x4 convention).
pub fn entity_transform(origin: [f32; 3], angles: [f32; 3], scales: [f32; 3]) -> [[f32; 4]; 3] {
    let r = angles_to_matrix(angles);
    let mut out = [[0f32; 4]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = r[i][j] * scales[j];
        }
        out[i][3] = origin[i];
    }
    out
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

    fn approx3(a: [f32; 3], b: [f32; 3]) {
        for i in 0..3 {
            assert!((a[i] - b[i]).abs() < 1e-5, "{a:?} != {b:?} at {i}");
        }
    }

    #[test]
    fn kv3_form_decoding_and_missing_classname_dropped() {
        let root = obj(vec![
            ("m_name", Value::String("test_lump".into())),
            (
                "m_childLumps",
                Value::Array(vec![Value::String("child1".into())]),
            ),
            (
                "m_entityKeyValues",
                Value::Array(vec![
                    obj(vec![(
                        "keyValues3Data",
                        obj(vec![
                            ("version", Value::Int(1)),
                            (
                                "values",
                                obj(vec![
                                    ("classname", Value::String("info_player_terrorist".into())),
                                    (
                                        "origin",
                                        Value::Array(vec![
                                            Value::Double(1.0),
                                            Value::Double(2.0),
                                            Value::Double(3.0),
                                        ]),
                                    ),
                                ]),
                            ),
                            (
                                "attributes",
                                obj(vec![("StartDisabled", Value::Bool(true))]),
                            ),
                        ]),
                    )]),
                    // No classname: dropped.
                    obj(vec![(
                        "keyValues3Data",
                        obj(vec![
                            ("version", Value::Int(1)),
                            ("values", obj(vec![("foo", Value::Int(1))])),
                        ]),
                    )]),
                ]),
            ),
        ]);

        let lump = decode_entity_lump(&root).unwrap();
        assert_eq!(lump.name, "test_lump");
        assert_eq!(lump.child_lumps, vec!["child1".to_string()]);
        assert_eq!(lump.entities.len(), 1);

        let e = &lump.entities[0];
        assert_eq!(e.classname(), "info_player_terrorist");
        assert_eq!(e.origin(), [1.0, 2.0, 3.0]);
        assert_eq!(e.get_bool("startdisabled"), Some(true));
        assert_eq!(e.get_bool("STARTDISABLED"), Some(true));
    }

    #[test]
    fn unsupported_kv3_version_errors() {
        let root = obj(vec![("version", Value::Int(2)), ("values", obj(vec![]))]);
        let mut out = Vec::new();
        assert!(matches!(
            parse_kv3_form(&root, &mut out),
            Err(EntityError::UnsupportedVersion { version: 2 })
        ));
    }

    #[test]
    fn legacy_blob_round_trip() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes()); // version
        bytes.extend_from_slice(&1u32.to_le_bytes()); // hashedCount
        bytes.extend_from_slice(&1u32.to_le_bytes()); // stringCount

        // Hashed-only field (no name in the file): bool type.
        let hashed_hash = 0xdead_beefu32;
        bytes.extend_from_slice(&hashed_hash.to_le_bytes());
        bytes.extend_from_slice(&0x06u32.to_le_bytes());
        bytes.push(1);

        // String-keyed field: "classname" -> cstring.
        bytes.extend_from_slice(&string_token("classname").to_le_bytes());
        bytes.extend_from_slice(b"classname\0");
        bytes.extend_from_slice(&0x1eu32.to_le_bytes());
        bytes.extend_from_slice(b"prop_dynamic\0");

        let mut props = Vec::new();
        parse_legacy_form(&bytes, &mut props).unwrap();
        assert_eq!(
            props,
            vec![
                ("hash_deadbeef".to_string(), EntityValue::Bool(true)),
                (
                    "classname".to_string(),
                    EntityValue::String("prop_dynamic".to_string())
                ),
            ]
        );
    }

    #[test]
    fn legacy_blob_hashed_known_key_is_resolved_to_its_name() {
        // classname stored hashed-only (no name in the file), as real compiled entity lumps do
        // for common keys (`docs/FORMATS.md` section 7; confirmed against
        // `default_ents_kv3_v0.vents_c`/`ascent_speedup_switch_template_ents.vents_c`).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes()); // version
        bytes.extend_from_slice(&2u32.to_le_bytes()); // hashedCount
        bytes.extend_from_slice(&0u32.to_le_bytes()); // stringCount

        bytes.extend_from_slice(&string_token("classname").to_le_bytes());
        bytes.extend_from_slice(&0x1eu32.to_le_bytes());
        bytes.extend_from_slice(b"func_physbox\0");

        // An unknown hash still falls back to the hash_{:08x} placeholder.
        bytes.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        bytes.extend_from_slice(&0x06u32.to_le_bytes());
        bytes.push(1);

        let mut props = Vec::new();
        parse_legacy_form(&bytes, &mut props).unwrap();
        assert_eq!(
            props,
            vec![
                (
                    "classname".to_string(),
                    EntityValue::String("func_physbox".to_string())
                ),
                ("hash_deadbeef".to_string(), EntityValue::Bool(true)),
            ]
        );
    }

    #[test]
    fn legacy_blob_all_value_types() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());

        let mut push_field = |name: &str, type_id: u32, payload: &[u8]| {
            bytes.extend_from_slice(&string_token(name).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&type_id.to_le_bytes());
            bytes.extend_from_slice(payload);
        };
        let mut vec_payload = Vec::new();
        vec_payload.extend_from_slice(&1.0f32.to_le_bytes());
        vec_payload.extend_from_slice(&2.0f32.to_le_bytes());
        vec_payload.extend_from_slice(&3.0f32.to_le_bytes());

        push_field("f32val", 0x01, &1.5f32.to_le_bytes());
        push_field("f64val", 0x22, &2.5f64.to_le_bytes());
        push_field("colorval", 0x09, &[1, 2, 3, 4]);
        push_field("i64val", 0x1a, &(-7i64).to_le_bytes());
        push_field("vecval", 0x03, &vec_payload);

        let mut props = Vec::new();
        parse_legacy_form(&bytes, &mut props).unwrap();
        assert_eq!(props[0].1, EntityValue::Float(1.5));
        assert_eq!(props[1].1, EntityValue::Float(2.5));
        assert_eq!(props[2].1, EntityValue::Color([1, 2, 3, 4]));
        assert_eq!(props[3].1, EntityValue::Int(-7));
        assert_eq!(props[4].1, EntityValue::Vector([1.0, 2.0, 3.0]));
    }

    #[test]
    fn legacy_blob_key_hash_mismatch_errors() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // wrong hash
        bytes.extend_from_slice(b"classname\0");
        bytes.extend_from_slice(&0x1eu32.to_le_bytes());
        bytes.extend_from_slice(b"x\0");

        let mut props = Vec::new();
        assert!(matches!(
            parse_legacy_form(&bytes, &mut props),
            Err(EntityError::KeyHashMismatch { .. })
        ));
    }

    #[test]
    fn legacy_blob_cstring_with_invalid_utf8_is_lossy_not_an_error() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());

        let name = b"classname";
        bytes.extend_from_slice(&string_token(std::str::from_utf8(name).unwrap()).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(&0x1eu32.to_le_bytes());
        bytes.extend_from_slice(&[b'p', b'r', b'o', b'p', 0xff, 0xfe, 0]); // invalid UTF-8 + NUL

        let mut props = Vec::new();
        parse_legacy_form(&bytes, &mut props).unwrap();
        assert_eq!(
            props[0].1,
            EntityValue::String("prop\u{fffd}\u{fffd}".to_string())
        );
    }

    #[test]
    fn legacy_blob_empty_is_no_properties() {
        let mut props = Vec::new();
        parse_legacy_form(&[], &mut props).unwrap();
        assert!(props.is_empty());
    }

    #[test]
    fn missing_classname_is_dropped_legacy_form() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let entity_kv = obj(vec![("m_keyValuesData", Value::Blob(bytes))]);
        assert_eq!(parse_entity(&entity_kv).unwrap(), None);
    }

    #[test]
    fn get_vec3_from_array_legacy_vector_and_string_forms() {
        let mut e = Entity::default();
        e.properties.push((
            "arr".into(),
            EntityValue::Array(vec![
                EntityValue::Int(1),
                EntityValue::Int(2),
                EntityValue::Int(3),
            ]),
        ));
        e.properties
            .push(("vec".into(), EntityValue::Vector([4.0, 5.0, 6.0])));
        e.properties
            .push(("str".into(), EntityValue::String("7 8 9".into())));

        assert_eq!(e.get_vec3("arr"), Some([1.0, 2.0, 3.0]));
        assert_eq!(e.get_vec3("vec"), Some([4.0, 5.0, 6.0]));
        assert_eq!(e.get_vec3("str"), Some([7.0, 8.0, 9.0]));
        assert_eq!(e.get_vec3("missing"), None);
    }

    #[test]
    fn get_bool_variants() {
        let mut e = Entity::default();
        e.properties.push(("a".into(), EntityValue::Bool(true)));
        e.properties.push(("b".into(), EntityValue::Int(0)));
        e.properties.push(("c".into(), EntityValue::UInt(1)));
        e.properties
            .push(("d".into(), EntityValue::String("True".into())));
        e.properties
            .push(("e".into(), EntityValue::String("false".into())));
        e.properties
            .push(("f".into(), EntityValue::String("0".into())));
        e.properties
            .push(("g".into(), EntityValue::String("bogus".into())));
        // Case not in the exact accepted set: not a boolean.
        e.properties
            .push(("h".into(), EntityValue::String("TRUE".into())));
        // Any integer other than 0/1 is not a boolean.
        e.properties.push(("i".into(), EntityValue::Int(2)));

        assert_eq!(e.get_bool("a"), Some(true));
        assert_eq!(e.get_bool("b"), Some(false));
        assert_eq!(e.get_bool("c"), Some(true));
        assert_eq!(e.get_bool("d"), Some(true));
        assert_eq!(e.get_bool("e"), Some(false));
        assert_eq!(e.get_bool("f"), Some(false));
        assert_eq!(e.get_bool("g"), None);
        assert_eq!(e.get_bool("h"), None);
        assert_eq!(e.get_bool("i"), None);
        assert_eq!(e.get_bool("missing"), None);
    }

    #[test]
    fn defaults_origin_angles_scales() {
        let e = Entity::default();
        assert_eq!(e.origin(), [0.0, 0.0, 0.0]);
        assert_eq!(e.angles(), [0.0, 0.0, 0.0]);
        assert_eq!(e.scales(), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn angles_matrix_matches_reference_values() {
        fn columns(m: [[f32; 3]; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
            (
                [m[0][0], m[1][0], m[2][0]],
                [m[0][1], m[1][1], m[2][1]],
                [m[0][2], m[1][2], m[2][2]],
            )
        }
        fn approx3_tol(a: [f32; 3], b: [f32; 3], tol: f32) {
            for i in 0..3 {
                assert!((a[i] - b[i]).abs() < tol, "{a:?} != {b:?} at {i}");
            }
        }

        let (ex, ey, ez) = columns(angles_to_matrix([30.0, 45.0, 60.0]));
        approx3_tol(ex, [0.61237, 0.61237, -0.5], 1e-4);
        approx3_tol(ey, [-0.04737, 0.65974, 0.75], 1e-4);
        approx3_tol(ez, [0.78915, -0.43560, 0.43301], 1e-4);

        let (_, ey, ez) = columns(angles_to_matrix([0.0, 0.0, 90.0]));
        approx3_tol(ey, [0.0, 0.0, 1.0], 1e-4);
        approx3_tol(ez, [0.0, -1.0, 0.0], 1e-4);
    }

    #[test]
    fn angles_matrix_forward_vectors() {
        let forward = |angles: [f32; 3]| {
            let r = angles_to_matrix(angles);
            [r[0][0], r[1][0], r[2][0]]
        };
        // Yaw 90 -> forward +Y.
        approx3(forward([0.0, 90.0, 0.0]), [0.0, 1.0, 0.0]);
        // Pitch 90 -> forward -Z (pitch is positive downwards).
        approx3(forward([90.0, 0.0, 0.0]), [0.0, 0.0, -1.0]);
        // Roll turns about the forward axis, so it cannot move it.
        approx3(forward([0.0, 0.0, 90.0]), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn entity_transform_applies_scale_rotation_and_translation() {
        let rows = entity_transform([10.0, 20.0, 30.0], [0.0, 90.0, 0.0], [2.0, 3.0, 4.0]);
        // Rotation (yaw 90) maps local X to +Y, local Y to -X, local Z to Z; then scaled and
        // translated.
        approx3([rows[0][3], rows[1][3], rows[2][3]], [10.0, 20.0, 30.0]);
        assert!((rows[1][0] - 2.0).abs() < 1e-5, "{rows:?}");
    }
}
