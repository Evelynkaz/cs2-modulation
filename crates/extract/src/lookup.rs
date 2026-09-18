//! Model lookup: finds a model by path in the map VPK first, then the shared VPKs, and caches
//! its decoded PHYS (and `m_modelInfo.m_keyValueText`) so a model placed hundreds of times (a
//! repeated static prop) is only opened and decoded once (`MapExtractor.cs:964-984 EntryIndex`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use s2fmt::kv3::{self, Value};
use s2fmt::phys::{self, PhysAggregate};
use s2fmt::resource::Resource;
use s2fmt::vpk::{Vpk, VpkEntry};

use crate::ExtractError;

/// Where a model's PHYS came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysSource {
    /// The model's own CTRL block pointed at an embedded PHYS block.
    Embedded,
    /// `m_refPhysicsData[0]` resolved to a separate `.vphys_c` at this path.
    RefPhys(String),
    /// The model has no physics.
    None,
}

/// The result of looking up a model's physics and keyvalues. Never an `Err`: any failure while
/// resolving a model is captured in [`Self::load_error`] (cached alongside a successful result,
/// so a broken model warns exactly once, not once per placement -- see [`ModelLookup`]).
#[derive(Debug, Clone)]
pub struct ModelPhys {
    pub phys: Option<Arc<PhysAggregate>>,
    /// `m_modelInfo.m_keyValueText`, parsed as KV3 text (only when it looks like one; see
    /// [`extract_model_keyvalues`]).
    pub keyvalues: Option<kv3::Document>,
    /// Set when `m_keyValueText` looked like KV3 text (`extract_model_keyvalues`'s gate passed)
    /// but failed to parse; distinct from simply not having keyvalues.
    pub keyvalues_error: Option<String>,
    pub source: PhysSource,
    /// True when the model itself (`<path>_c`) could not be found in the map or any shared VPK.
    pub not_found: bool,
    /// Set when the model was found but reading/parsing it (as a resource, or its PHYS/ref-phys)
    /// failed; distinct from [`Self::not_found`].
    pub load_error: Option<String>,
}

/// One cached lookup result, shared (`Arc`) across every placement of the same model.
struct CacheEntry {
    phys: Option<Arc<PhysAggregate>>,
    keyvalues: Option<kv3::Document>,
    keyvalues_error: Option<String>,
    source: PhysSource,
    not_found: bool,
    load_error: Option<String>,
}

/// Returns the first source (in iteration order) `resolve` succeeds for, paired with its
/// resolved value. Pure and generic so the "map wins over shared, earlier shared wins over
/// later" lookup order can be unit-tested without needing real VPK files (see
/// `tests::first_resolution_prefers_earlier_sources`).
fn first_resolution<'a, S, T>(
    sources: impl IntoIterator<Item = &'a S>,
    resolve: impl Fn(&'a S, &str) -> Option<T>,
    path: &str,
) -> Option<(&'a S, T)> {
    for s in sources {
        if let Some(v) = resolve(s, path) {
            return Some((s, v));
        }
    }
    None
}

/// Opens the map VPK and every shared VPK once, and caches decoded model PHYS by (lowercase)
/// path -- including misses and load failures -- so re-placed props and repeatedly-referenced
/// entity models don't reopen/reparse their compiled resource on every placement, and a broken
/// model doesn't warn once per placement.
pub struct ModelLookup<'a> {
    map_name: String,
    map: &'a Vpk,
    shared: Vec<&'a Vpk>,
    cache: RefCell<HashMap<String, Arc<CacheEntry>>>,
    /// Warnings raised the first (and only) time a model path is resolved; drained by
    /// [`Self::take_warnings`] once extraction for the map is done.
    warnings: RefCell<Vec<String>>,
}

impl<'a> ModelLookup<'a> {
    pub fn new(map_name: &str, map: &'a Vpk, shared: Vec<&'a Vpk>) -> Self {
        ModelLookup {
            map_name: map_name.to_string(),
            map,
            shared,
            cache: RefCell::new(HashMap::new()),
            warnings: RefCell::new(Vec::new()),
        }
    }

    /// Finds an entry by path, map VPK first, then shared VPKs in order
    /// (`MapExtractor.cs:970` `shared.Prepend(map)`). `self.map`/`self.shared` are `&'a Vpk`
    /// copies, so calling `find` through them (rather than through `&self`) ties the result to
    /// `'a` directly.
    fn find_entry(&self, path: &str) -> Option<(&'a Vpk, &'a VpkEntry)> {
        let sources = std::iter::once(self.map).chain(self.shared.iter().copied());
        first_resolution(sources, |vpk: &'a Vpk, p: &str| vpk.find(p), path)
    }

    fn read_resource(&self, path: &str) -> Result<Resource, ExtractError> {
        let (vpk, entry) = self
            .find_entry(path)
            .ok_or_else(|| ExtractError::Other(format!("{path} not found in any VPK")))?;
        let bytes = vpk.read(entry).map_err(|source| ExtractError::Vpk {
            map: self.map_name.clone(),
            path: path.into(),
            source: Box::new(source),
        })?;
        Resource::parse(bytes).map_err(|source| ExtractError::Resource {
            map: self.map_name.clone(),
            path: path.to_string(),
            source: Box::new(source),
        })
    }

    /// Loads a model's PHYS and keyvalues, given its path *without* the trailing `_c`
    /// (e.g. `models/props/de_mirage/wood_pallet01.vmdl`). Cached by lowercase path, including
    /// failures (see [`ModelPhys::load_error`]); a fresh (non-cached) load's warnings are queued
    /// once via [`Self::take_warnings`], never repeated for later placements of the same model.
    pub fn load_model_phys(&self, model_path: &str) -> ModelPhys {
        let key = model_path
            .trim_start_matches(['/', '\\'])
            .to_ascii_lowercase();
        if let Some(entry) = self.cache.borrow().get(&key) {
            return to_model_phys(entry);
        }
        let entry = self.load_uncached(&key);
        if let Some(err) = &entry.load_error {
            self.warnings
                .borrow_mut()
                .push(format!("model {key}: {err}"));
        }
        if let Some(err) = &entry.keyvalues_error {
            self.warnings
                .borrow_mut()
                .push(format!("model {key}: keyvalues unreadable: {err}"));
        }
        let entry = Arc::new(entry);
        self.cache.borrow_mut().insert(key, entry.clone());
        to_model_phys(&entry)
    }

    /// Drains every warning queued by a fresh (cache-miss) [`Self::load_model_phys`] call so far.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.warnings.borrow_mut())
    }

    /// Never fails: any error resolving `key` is captured in [`CacheEntry::load_error`] instead
    /// of propagating, so a broken model is cached (and warned about) exactly once.
    fn load_uncached(&self, key: &str) -> CacheEntry {
        match self.try_load_uncached(key) {
            Ok(entry) => entry,
            Err(e) => CacheEntry {
                phys: None,
                keyvalues: None,
                keyvalues_error: None,
                source: PhysSource::None,
                not_found: false,
                load_error: Some(e.to_string()),
            },
        }
    }

    fn try_load_uncached(&self, key: &str) -> Result<CacheEntry, ExtractError> {
        let full = format!("{key}_c");
        if self.find_entry(&full).is_none() {
            return Ok(CacheEntry {
                phys: None,
                keyvalues: None,
                keyvalues_error: None,
                source: PhysSource::None,
                not_found: true,
                load_error: None,
            });
        }
        let resource = self.read_resource(&full)?;
        let data_doc = resource.data_kv3().ok();
        let (keyvalues, keyvalues_error) = match data_doc
            .as_ref()
            .map(|doc| extract_model_keyvalues(&doc.root))
        {
            Some(Ok(doc)) => (doc, None),
            Some(Err(e)) => (None, Some(e.to_string())),
            None => (None, None),
        };

        if let Some(embedded) =
            resource
                .embedded_phys()
                .map_err(|source| ExtractError::Resource {
                    map: self.map_name.clone(),
                    path: full.clone(),
                    source: Box::new(source),
                })?
        {
            let phys_doc =
                resource
                    .kv3(embedded.block)
                    .map_err(|source| ExtractError::Resource {
                        map: self.map_name.clone(),
                        path: full.clone(),
                        source: Box::new(source),
                    })?;
            let phys = phys::decode(&phys_doc.root).map_err(|source| ExtractError::Phys {
                map: self.map_name.clone(),
                path: full.clone(),
                source: Box::new(source),
            })?;
            return Ok(CacheEntry {
                phys: Some(Arc::new(phys)),
                keyvalues,
                keyvalues_error,
                source: PhysSource::Embedded,
                not_found: false,
                load_error: None,
            });
        }

        if let Some(doc) = &data_doc
            && let Some(ref_name) = doc
                .root
                .get("m_refPhysicsData")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
        {
            let phys_path = format!("{}_c", ref_name.to_ascii_lowercase());
            if self.find_entry(&phys_path).is_some() {
                let phys_resource = self.read_resource(&phys_path)?;
                let phys_doc =
                    phys_resource
                        .data_kv3()
                        .map_err(|source| ExtractError::Resource {
                            map: self.map_name.clone(),
                            path: phys_path.clone(),
                            source: Box::new(source),
                        })?;
                let phys = phys::decode(&phys_doc.root).map_err(|source| ExtractError::Phys {
                    map: self.map_name.clone(),
                    path: phys_path,
                    source: Box::new(source),
                })?;
                return Ok(CacheEntry {
                    phys: Some(Arc::new(phys)),
                    keyvalues,
                    keyvalues_error,
                    source: PhysSource::RefPhys(ref_name.to_string()),
                    not_found: false,
                    load_error: None,
                });
            }
        }

        Ok(CacheEntry {
            phys: None,
            keyvalues,
            keyvalues_error,
            source: PhysSource::None,
            not_found: false,
            load_error: None,
        })
    }
}

fn to_model_phys(entry: &CacheEntry) -> ModelPhys {
    ModelPhys {
        phys: entry.phys.clone(),
        keyvalues: entry.keyvalues.clone(),
        keyvalues_error: entry.keyvalues_error.clone(),
        source: entry.source.clone(),
        not_found: entry.not_found,
        load_error: entry.load_error.clone(),
    }
}

/// `m_modelInfo.m_keyValueText`, parsed as KV3 text only if it starts with `<!-- kv3 ` and its
/// length is >= 140 (`Model.cs:577-591`). `Ok(None)` means the field is absent or doesn't look
/// like KV3 text (nothing to parse); `Err` means it looked like KV3 text but failed to parse.
fn extract_model_keyvalues(data_root: &Value) -> Result<Option<kv3::Document>, kv3::Kv3Error> {
    let Some(text) = data_root
        .get("m_modelInfo")
        .and_then(|v| v.get("m_keyValueText"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    if !text.starts_with("<!-- kv3 ") || text.len() < 140 {
        return Ok(None);
    }
    kv3::parse_text(text).map(Some)
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

    const VALID_HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n";

    #[test]
    fn extract_model_keyvalues_absent_field_is_ok_none() {
        let root = obj(vec![]);
        assert!(matches!(extract_model_keyvalues(&root), Ok(None)));
    }

    #[test]
    fn extract_model_keyvalues_wrong_prefix_is_ok_none() {
        let text = format!(
            "not kv3 text at all, padded to be long enough to pass the length gate on its own merits {}",
            "x".repeat(60)
        );
        let root = obj(vec![(
            "m_modelInfo",
            obj(vec![("m_keyValueText", Value::String(text))]),
        )]);
        assert!(matches!(extract_model_keyvalues(&root), Ok(None)));
    }

    #[test]
    fn extract_model_keyvalues_below_140_chars_is_ok_none_even_with_valid_header() {
        // A well-formed but short document (header + "{}\n") is well under 140 chars.
        let text = format!("{VALID_HEADER}{{}}\n");
        assert!(
            text.len() < 140,
            "test fixture must be short: {}",
            text.len()
        );
        let root = obj(vec![(
            "m_modelInfo",
            obj(vec![("m_keyValueText", Value::String(text))]),
        )]);
        assert!(matches!(extract_model_keyvalues(&root), Ok(None)));
    }

    #[test]
    fn extract_model_keyvalues_parses_valid_long_enough_text() {
        let padding = "// padding to reach the 140-char gate\n".repeat(3);
        let text =
            format!("{VALID_HEADER}{{\n{padding}prop_data = {{ base = \"Glass.Window\" }}\n}}\n");
        assert!(
            text.len() >= 140,
            "test fixture must be long enough: {}",
            text.len()
        );
        let root = obj(vec![(
            "m_modelInfo",
            obj(vec![("m_keyValueText", Value::String(text))]),
        )]);
        let doc = extract_model_keyvalues(&root).unwrap().unwrap();
        let base = doc
            .root
            .get("prop_data")
            .and_then(|v| v.get("base"))
            .and_then(Value::as_str);
        assert_eq!(base, Some("Glass.Window"));
    }

    #[test]
    fn extract_model_keyvalues_long_enough_but_malformed_is_err() {
        // Valid header, long enough, but the body is not valid KV3 (unbalanced braces).
        let padding =
            "// padding to reach the 140-char gate for this malformed-body test\n".repeat(2);
        let text = format!("{VALID_HEADER}{{\n{padding}this is not valid kv3 {{{{{{");
        assert!(text.len() >= 140);
        let root = obj(vec![(
            "m_modelInfo",
            obj(vec![("m_keyValueText", Value::String(text))]),
        )]);
        assert!(extract_model_keyvalues(&root).is_err());
    }

    struct FakeSource {
        name: &'static str,
        files: &'static [&'static str],
    }

    #[test]
    fn first_resolution_prefers_earlier_sources() {
        let map = FakeSource {
            name: "map",
            files: &["a.vmdl_c"],
        };
        let pak01 = FakeSource {
            name: "pak01",
            files: &["a.vmdl_c", "b.vmdl_c"],
        };
        let addon = FakeSource {
            name: "addon",
            files: &["b.vmdl_c", "c.vmdl_c"],
        };
        let resolve = |s: &FakeSource, path: &str| s.files.contains(&path).then_some(s.name);

        // Present in both map and pak01: map (first) wins.
        let (_, found) = first_resolution([&map, &pak01, &addon], resolve, "a.vmdl_c").unwrap();
        assert_eq!(found, "map");

        // Present in pak01 and addon, not map: pak01 (earlier shared) wins.
        let (_, found) = first_resolution([&map, &pak01, &addon], resolve, "b.vmdl_c").unwrap();
        assert_eq!(found, "pak01");

        // Only in addon.
        let (_, found) = first_resolution([&map, &pak01, &addon], resolve, "c.vmdl_c").unwrap();
        assert_eq!(found, "addon");

        // Nowhere.
        assert!(first_resolution([&map, &pak01, &addon], resolve, "missing.vmdl_c").is_none());
    }
}
