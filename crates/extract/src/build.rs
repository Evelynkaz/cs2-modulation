//! Top-level map extraction: merges world physics, solid entity geometry, static props, entity
//! metadata and the nav mesh into one [`Extraction`].

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind};
use s2fmt::entities::{self, Entity, entity_transform};
use s2fmt::nav;
use s2fmt::phys::{self, Mat3x4, PhysAggregate};
use s2fmt::resource::Resource;
use s2fmt::vpk::{Vpk, VpkEntry};
use s2fmt::worldnode;

use crate::ExtractError;
use crate::game::GameInstall;
use crate::lookup::ModelLookup;
use crate::mesh_build::{self, AttributeMode, ObjectMode, Source};
use crate::policy;
use crate::report::{
    AttributeTriangleCount, EntityRecord, ExtractMeta, ExtractReport, MergedEntity, NavAreasDump,
    PassableGlassEntity, entity_value_to_json,
};

/// Bumped whenever the extraction policy or output shape changes in a way that should
/// invalidate old caches.
///
/// `2` (S6s): never-solid `func_brush` (`solidity == 1`) and non-solid `prop_dynamic`
/// (`solid == 0`) are no longer merged as solid geometry -- see `policy::never_solid_func_brush`
/// / `policy::not_solid_prop_dynamic`. Re-extraction changes world geometry, so cached stand
/// spots (built against the player collider and the old mesh) must be recomputed too.
pub const EXTRACTOR_VERSION: u32 = 2;

/// Extraction knobs. Currently empty; kept as a struct (rather than `()`) so options can be
/// added later without breaking [`extract_map`]'s signature.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct ExtractOptions {}

/// One map's fully-built extraction.
pub struct Extraction {
    pub mesh: CollisionMesh,
    pub entities: Vec<EntityRecord>,
    pub nav: Option<NavAreasDump>,
    pub report: ExtractReport,
    pub meta: ExtractMeta,
}

fn find_entry_ending_with<'a>(vpk: &'a Vpk, suffix: &str) -> Option<&'a VpkEntry> {
    vpk.entries()
        .find(|e| e.path.to_ascii_lowercase().ends_with(suffix))
}

fn read_resource(vpk: &Vpk, entry: &VpkEntry, map: &str) -> Result<Resource, ExtractError> {
    let bytes = vpk.read(entry).map_err(|source| ExtractError::Vpk {
        map: map.to_string(),
        path: PathBuf::from(&entry.path),
        source: Box::new(source),
    })?;
    Resource::parse(bytes).map_err(|source| ExtractError::Resource {
        map: map.to_string(),
        path: entry.path.clone(),
        source: Box::new(source),
    })
}

/// Strips a model/lump path's trailing `_c`, if any (our lookups take the uncompiled path and
/// add it back themselves).
fn strip_c(path: &str) -> &str {
    path.strip_suffix("_c").unwrap_or(path)
}

/// Turns a `m_childLumps`-style lump name into the compiled `.vents_c` VPK path it resolves to
/// (same transform as `m_entityLumps`: backslashes to `/`, lowercased, `_c` appended).
fn child_lump_path(name: &str) -> String {
    format!("{}_c", name.replace('\\', "/").to_ascii_lowercase())
}

/// World physics lives embedded in `world_physics.vmdl_c` in the map's own VPK
/// (`docs/FORMATS.md` 6.1; `MapExtractor.cs:100-115`).
fn load_world_physics(map_vpk: &Vpk, map: &str) -> Result<PhysAggregate, ExtractError> {
    let entry = find_entry_ending_with(map_vpk, "world_physics.vmdl_c")
        .ok_or_else(|| ExtractError::Other(format!("{map}: no world_physics.vmdl_c in map VPK")))?;
    let resource = read_resource(map_vpk, entry, map)?;
    let embedded = resource
        .embedded_phys()
        .map_err(|source| ExtractError::Resource {
            map: map.to_string(),
            path: entry.path.clone(),
            source: Box::new(source),
        })?
        .ok_or_else(|| {
            ExtractError::Other(format!("{map}: {} has no embedded PHYS block", entry.path))
        })?;
    let doc = resource
        .kv3(embedded.block)
        .map_err(|source| ExtractError::Resource {
            map: map.to_string(),
            path: entry.path.clone(),
            source: Box::new(source),
        })?;
    phys::decode(&doc.root).map_err(|source| ExtractError::Phys {
        map: map.to_string(),
        path: entry.path.clone(),
        source: Box::new(source),
    })
}

fn load_world(map_vpk: &Vpk, map: &str, warnings: &mut Vec<String>) -> Option<worldnode::World> {
    let entry = find_entry_ending_with(map_vpk, "world.vwrld_c")?;
    let doc = match read_resource(map_vpk, entry, map).and_then(|r| {
        r.data_kv3().map_err(|source| ExtractError::Resource {
            map: map.to_string(),
            path: entry.path.clone(),
            source: Box::new(source),
        })
    }) {
        Ok(doc) => doc,
        Err(e) => {
            warnings.push(format!("failed to read world.vwrld_c: {e}"));
            return None;
        }
    };
    Some(worldnode::decode_world(&doc.root).expect("WorldError is uninhabited"))
}

/// Resolves the world's entity lumps (`m_entityLumps` + `_c`); if the world listed none, or a
/// listed lump could not be found/decoded, falls back to every `*.vents_c` entry in the map VPK
/// (`docs/FORMATS.md` §7 "референс проще: перебирает все vents_c в VPK карты").
fn resolve_entity_lumps(
    map_vpk: &Vpk,
    map: &str,
    listed: &[String],
    warnings: &mut Vec<String>,
) -> Vec<(String, entities::EntityLump)> {
    if !listed.is_empty() {
        let mut out = Vec::with_capacity(listed.len());
        let mut ok = true;
        for name in listed {
            let path = child_lump_path(name);
            let Some(entry) = map_vpk.find(&path) else {
                ok = false;
                break;
            };
            match read_resource(map_vpk, entry, map)
                .and_then(|r| {
                    r.data_kv3().map_err(|source| ExtractError::Resource {
                        map: map.to_string(),
                        path: path.clone(),
                        source: Box::new(source),
                    })
                })
                .map(|doc| entities::decode_entity_lump(&doc.root))
            {
                Ok(Ok(lump)) => out.push((path, lump)),
                Ok(Err(e)) => {
                    warnings.push(format!("failed to decode entity lump {path}: {e}"));
                    ok = false;
                    break;
                }
                Err(e) => {
                    warnings.push(format!("failed to read entity lump {path}: {e}"));
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return out;
        }
        warnings.push(
            "world listed entity lumps that could not all be resolved; falling back to every \
             *.vents_c entry in the map VPK"
                .to_string(),
        );
    } else {
        warnings.push(
            "world listed no entity lumps; falling back to every *.vents_c entry in the map VPK"
                .to_string(),
        );
    }

    let mut out = Vec::new();
    for entry in map_vpk.entries() {
        if !entry.extension().eq_ignore_ascii_case("vents_c") {
            continue;
        }
        match read_resource(map_vpk, entry, map).and_then(|r| {
            r.data_kv3().map_err(|source| ExtractError::Resource {
                map: map.to_string(),
                path: entry.path.clone(),
                source: Box::new(source),
            })
        }) {
            Ok(doc) => match entities::decode_entity_lump(&doc.root) {
                Ok(lump) => out.push((entry.path.clone(), lump)),
                Err(e) => {
                    warnings.push(format!("failed to decode entity lump {}: {e}", entry.path))
                }
            },
            Err(e) => warnings.push(format!("failed to read entity lump {}: {e}", entry.path)),
        }
    }

    exclude_child_lumps(out)
}

/// A `*.vents_c` file that some other lump lists in `m_childLumps` isn't a top-level lump (it's
/// a point_template's instance data) even though it's a standalone file the map VPK happens to
/// contain; excluding it here means it's only in `entities.json` once, via
/// `resolve_child_lumps`, tagged with `from_child_lump`, not duplicated as if it were also a
/// regular top-level lump.
fn exclude_child_lumps(
    lumps: Vec<(String, entities::EntityLump)>,
) -> Vec<(String, entities::EntityLump)> {
    let child_paths: HashSet<String> = lumps
        .iter()
        .flat_map(|(_, lump)| lump.child_lumps.iter())
        .map(|name| child_lump_path(name))
        .collect();
    lumps
        .into_iter()
        .filter(|(path, _)| !child_paths.contains(&path.to_ascii_lowercase()))
        .collect()
}

fn bump(list: &mut Vec<(String, usize)>, key: &str) {
    match list.iter_mut().find(|(k, _)| k == key) {
        Some((_, c)) => *c += 1,
        None => list.push((key.to_string(), 1)),
    }
}

fn own_attribute_groups(phys: &PhysAggregate) -> Vec<String> {
    let mut out = Vec::new();
    for a in &phys.collision_attributes {
        let s = format!(
            "{}:as=[{}]:with=[{}]:exclude=[{}]",
            a.group.as_deref().unwrap_or("Default"),
            a.interact_as.join(","),
            a.interact_with.join(","),
            a.interact_exclude.join(","),
        );
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Pure BFS over child-lump names (`m_childLumps`), transitively -- a child lump can itself list
/// grandchildren -- deduplicated by name via `seen`. `load` resolves one name to its decoded
/// lump (or `None`, e.g. not found/undecodable; the caller is expected to have already recorded
/// why). Factored out from [`resolve_child_lumps`] so the traversal itself (queue/seen bookkeeping,
/// no duplicate visits even in a diamond `A -> {B, C} -> D` shape) can be unit-tested against an
/// in-memory map instead of a real VPK (see `tests::child_lump_worklist_*`).
fn child_lump_worklist(
    entity_lumps: &[(String, entities::EntityLump)],
    mut load: impl FnMut(&str) -> Option<entities::EntityLump>,
) -> Vec<(String, entities::EntityLump)> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for (_, lump) in entity_lumps {
        for child in &lump.child_lumps {
            if seen.insert(child.clone()) {
                queue.push_back(child.clone());
            }
        }
    }

    let mut out = Vec::new();
    while let Some(name) = queue.pop_front() {
        if let Some(lump) = load(&name) {
            for grandchild in &lump.child_lumps {
                if seen.insert(grandchild.clone()) {
                    queue.push_back(grandchild.clone());
                }
            }
            out.push((name, lump));
        }
    }
    out
}

/// Resolves every child entity lump (`m_childLumps`) referenced by the top-level entity lumps,
/// transitively (see [`child_lump_worklist`]). Not applied to the mesh (`docs/FORMATS.md` §7:
/// point_template child lumps are template instances, not placed geometry); their entities are
/// only dumped for inspection and reported as skipped when they'd otherwise have been merged.
fn resolve_child_lumps(
    map_vpk: &Vpk,
    map: &str,
    entity_lumps: &[(String, entities::EntityLump)],
    warnings: &mut Vec<String>,
) -> Vec<(String, entities::EntityLump)> {
    child_lump_worklist(entity_lumps, |child| {
        let path = child_lump_path(child);
        let Some(entry) = map_vpk.find(&path) else {
            warnings.push(format!("child entity lump {path} not found"));
            return None;
        };
        match read_resource(map_vpk, entry, map).and_then(|r| {
            r.data_kv3().map_err(|source| ExtractError::Resource {
                map: map.to_string(),
                path: path.clone(),
                source: Box::new(source),
            })
        }) {
            Ok(doc) => match entities::decode_entity_lump(&doc.root) {
                Ok(lump) => Some(lump),
                Err(e) => {
                    warnings.push(format!("failed to decode child entity lump {path}: {e}"));
                    None
                }
            },
            Err(e) => {
                warnings.push(format!("failed to read child entity lump {path}: {e}"));
                None
            }
        }
    })
}

/// Extracts one map into a collision mesh, entity dump, nav mesh and policy report.
pub fn extract_map(
    install: &GameInstall,
    map: &str,
    _opts: &ExtractOptions,
) -> Result<Extraction, ExtractError> {
    let start = Instant::now();

    let map_vpk_path = install.map_vpk(map);
    let map_vpk = Vpk::open(&map_vpk_path).map_err(|source| ExtractError::Vpk {
        map: map.to_string(),
        path: map_vpk_path.clone(),
        source: Box::new(source),
    })?;
    let shared_paths = install.shared_vpks(map);
    let shared_vpks: Vec<Vpk> = shared_paths
        .iter()
        .map(|p| {
            Vpk::open(p).map_err(|source| ExtractError::Vpk {
                map: map.to_string(),
                path: p.clone(),
                source: Box::new(source),
            })
        })
        .collect::<Result<_, _>>()?;
    let lookup = ModelLookup::new(map, &map_vpk, shared_vpks.iter().collect());

    let mut mesh = CollisionMesh::new();
    let mut report = ExtractReport::default();
    let mut warnings: Vec<String> = Vec::new();

    // 1. World physics.
    let world_phys = load_world_physics(&map_vpk, map)?;
    {
        let mut warn = |s: String| warnings.push(s);
        mesh_build::append_phys(
            &mut mesh,
            &world_phys,
            &Mat3x4::IDENTITY,
            AttributeMode::PerDescriptor,
            ObjectMode::PerWorldShape,
            Source::World,
            &mut report,
            &mut warn,
        );
    }

    // 2. World -> entity lumps + static prop world nodes.
    let world = load_world(&map_vpk, map, &mut warnings);
    let entity_lumps = resolve_entity_lumps(
        &map_vpk,
        map,
        world
            .as_ref()
            .map(|w| w.entity_lumps.as_slice())
            .unwrap_or(&[]),
        &mut warnings,
    );

    // 3. Entities: dump every entity, and merge solid-class geometry.
    let mut entity_records = Vec::new();
    let mut entity_index: u32 = 0;
    for (lump_path, lump) in &entity_lumps {
        if !lump.child_lumps.is_empty() {
            report.lumps_with_child_lumps += 1;
        }
        for entity in &lump.entities {
            let classname = entity.classname().to_string();
            let targetname = entity.targetname().map(str::to_string);
            let model = entity.get_str("model").unwrap_or("").to_string();
            let hammer_id = entity.get_str("hammeruniqueid").map(str::to_string);

            bump(&mut report.entities_per_class, &classname);
            if classname == "point_template" {
                report.point_template_count += 1;
            }

            let mut properties = serde_json::Map::new();
            for (k, v) in &entity.properties {
                properties.insert(k.clone(), entity_value_to_json(v));
            }
            entity_records.push(EntityRecord {
                classname: classname.clone(),
                targetname: targetname.clone(),
                origin: entity.origin(),
                angles: entity.angles(),
                model: (!model.is_empty()).then(|| model.clone()),
                hammer_id: hammer_id.clone(),
                properties,
                from_child_lump: None,
            });

            merge_solid_entity(
                &lookup,
                &mut mesh,
                &mut report,
                &mut warnings,
                &classname,
                targetname.as_deref(),
                &model,
                entity,
                entity_index,
                lump_path,
            );
            entity_index += 1;
        }
    }

    // 3b. Child entity lumps (`m_childLumps`, e.g. point_template instances): dumped for
    // inspection, not applied to geometry (`docs/FORMATS.md` §7).
    let child_lumps = resolve_child_lumps(&map_vpk, map, &entity_lumps, &mut warnings);
    for (lump_name, lump) in &child_lumps {
        for entity in &lump.entities {
            let classname = entity.classname().to_string();
            let targetname = entity.targetname().map(str::to_string);
            let model = entity.get_str("model").unwrap_or("").to_string();
            let hammer_id = entity.get_str("hammeruniqueid").map(str::to_string);

            let mut properties = serde_json::Map::new();
            for (k, v) in &entity.properties {
                properties.insert(k.clone(), entity_value_to_json(v));
            }
            entity_records.push(EntityRecord {
                classname: classname.clone(),
                targetname: targetname.clone(),
                origin: entity.origin(),
                angles: entity.angles(),
                model: (!model.is_empty()).then(|| model.clone()),
                hammer_id,
                properties,
                from_child_lump: Some(lump_name.clone()),
            });

            if !model.is_empty() && policy::SOLID_ENTITY_CLASSES.contains(&classname.as_str()) {
                report.record_skip(
                    &classname,
                    targetname.as_deref(),
                    &model,
                    "child lump (point_template)",
                );
            }
        }
    }

    // 4. Static props (world node scene objects).
    if let Some(world) = &world {
        build_static_props(
            &map_vpk,
            &lookup,
            world,
            &mut mesh,
            &mut report,
            &mut warnings,
            map,
        );
    } else {
        warnings.push("no world.vwrld_c: skipping static props".to_string());
    }

    // Every model lookup warning queued so far (entities + static props), each raised exactly
    // once per model path regardless of how many times it was placed.
    warnings.extend(lookup.take_warnings());

    // 5. Nav mesh.
    let nav = load_nav(&map_vpk, map, &mut warnings)?;

    let stats = mesh.stats();
    report.triangles_per_attribute = mesh
        .attributes
        .iter()
        .zip(stats.triangles_per_attribute)
        .enumerate()
        .map(|(index, (attr, (_, triangles)))| AttributeTriangleCount {
            index: index as u16,
            name: attr.name.clone(),
            interact_as: attr.interact_as.clone(),
            interact_exclude: attr.interact_exclude.clone(),
            synthetic: attr.synthetic,
            triangles,
        })
        .collect();
    report.triangles_per_kind = stats
        .triangles_per_kind
        .into_iter()
        .map(|(kind, count)| (format!("{kind:?}"), count))
        .collect();
    report.degenerate_triangles_skipped = mesh.degenerate_skipped;

    let total_spheres: usize = report.spheres_by_source.iter().map(|(_, n)| n).sum();
    let total_capsules: usize = report.capsules_by_source.iter().map(|(_, n)| n).sum();
    if total_spheres > 0 || total_capsules > 0 {
        warnings.push(format!(
            "{total_spheres} spheres / {total_capsules} capsules not triangulated (reference behaviour)"
        ));
    }

    report.warnings = warnings;

    // 6. Metadata.
    let game_build = install.build_id()?;
    let map_vpk_sha256 =
        crate::cache::sha256_file(&map_vpk_path).map_err(|source| ExtractError::Io {
            path: map_vpk_path.clone(),
            source,
        })?;
    let mut shared_vpk_sha256 = Vec::with_capacity(shared_paths.len());
    for p in &shared_paths {
        let hash = crate::cache::sha256_file(p).map_err(|source| ExtractError::Io {
            path: p.clone(),
            source,
        })?;
        shared_vpk_sha256.push((p.display().to_string(), hash));
    }

    let meta = ExtractMeta {
        map: map.to_string(),
        game_build,
        extractor_version: EXTRACTOR_VERSION,
        map_vpk_sha256,
        shared_vpk_sha256,
        created_utc: crate::cache::now_utc_rfc3339(),
        timing_ms: start.elapsed().as_millis() as u64,
    };

    Ok(Extraction {
        mesh,
        entities: entity_records,
        nav,
        report,
        meta,
    })
}

#[allow(clippy::too_many_arguments)]
fn merge_solid_entity(
    lookup: &ModelLookup,
    mesh: &mut CollisionMesh,
    report: &mut ExtractReport,
    warnings: &mut Vec<String>,
    classname: &str,
    targetname: Option<&str>,
    model: &str,
    entity: &Entity,
    entity_index: u32,
    _lump_path: &str,
) {
    if model.is_empty() {
        return;
    }
    if !policy::SOLID_ENTITY_CLASSES.contains(&classname) {
        report.record_skip(classname, targetname, model, "class not in allowlist");
        return;
    }
    if policy::is_retake_only(targetname.unwrap_or(""), model) {
        report.record_skip(classname, targetname, model, "retake");
        return;
    }
    if policy::starts_disabled(entity) {
        report.record_skip(classname, targetname, model, "startdisabled");
        return;
    }
    if classname == "func_brush" && policy::never_solid_func_brush(entity) {
        report.record_skip(classname, targetname, model, "never solid");
        return;
    }

    let model_phys = lookup.load_model_phys(strip_c(model));
    if model_phys.not_found {
        report.record_skip(classname, targetname, model, "model not found");
        return;
    }
    if model_phys.load_error.is_some() {
        // Warned once (per model path) by `ModelLookup` itself; see `lookup.take_warnings()`.
        report.record_skip(classname, targetname, model, "model load error");
        return;
    }

    let mut passable = true;
    if classname == "prop_dynamic" {
        if model_phys.keyvalues_error.is_some() {
            report.record_skip(classname, targetname, model, "model keyvalues unreadable");
            return;
        }
        let kv_root = model_phys.keyvalues.as_ref().map(|d| &d.root);
        if !policy::breakable_model(kv_root) {
            report.record_skip(classname, targetname, model, "prop_dynamic not breakable");
            return;
        }
        if policy::not_solid_prop_dynamic(entity) {
            report.record_skip(classname, targetname, model, "prop_dynamic not solid");
            return;
        }
        passable = policy::passable_glass(kv_root, model);
    }

    let Some(phys) = model_phys.phys else {
        report.record_skip(classname, targetname, model, "no physics");
        return;
    };

    if classname == "prop_dynamic" && passable {
        report.passable_glass.push(PassableGlassEntity {
            classname: classname.to_string(),
            targetname: targetname.map(str::to_string),
            model: model.to_string(),
        });
    }

    let attr_name = policy::entity_attribute_name(classname, passable);
    let attr_id = mesh
        .add_attribute(CollisionAttribute {
            name: attr_name.to_string(),
            interact_as: Vec::new(),
            interact_with: Vec::new(),
            interact_exclude: Vec::new(),
            synthetic: true,
        })
        .unwrap_or(0);

    let origin = entity.origin();
    let angles = entity.angles();
    let scales = entity.scales();
    let transform = Mat3x4 {
        rows: entity_transform(origin, angles, scales),
    };

    let object = mesh.add_object(MeshObject {
        kind: ObjectKind::Entity,
        classname: Some(classname.to_string()),
        targetname: targetname.map(str::to_string),
        model: Some(model.to_string()),
        hammer_id: entity.get_str("hammeruniqueid").map(str::to_string),
        source_index: entity_index,
        hull_flags: None,
    });

    let before = mesh.triangle_count();
    {
        let mut warn = |s: String| warnings.push(s);
        mesh_build::append_phys(
            mesh,
            &phys,
            &transform,
            AttributeMode::Fixed(attr_id),
            ObjectMode::Fixed(object),
            Source::Entity,
            report,
            &mut warn,
        );
    }
    let triangles = mesh.triangle_count() - before;

    report.merged_entities.push(MergedEntity {
        classname: classname.to_string(),
        targetname: targetname.map(str::to_string),
        model: model.to_string(),
        attribute: attr_name.to_string(),
        triangles,
        own_attribute_groups: own_attribute_groups(&phys),
    });
}

#[allow(clippy::too_many_arguments)]
fn build_static_props(
    map_vpk: &Vpk,
    lookup: &ModelLookup,
    world: &worldnode::World,
    mesh: &mut CollisionMesh,
    report: &mut ExtractReport,
    warnings: &mut Vec<String>,
    map: &str,
) {
    let mut without_phys_models: HashSet<String> = HashSet::new();
    let mut missing_models: HashSet<String> = HashSet::new();
    let mut load_error_models: HashSet<String> = HashSet::new();

    for prefix in &world.world_node_prefixes {
        let node_path = worldnode::world_node_path(prefix);
        let Some(entry) = map_vpk.find(&node_path) else {
            warnings.push(format!("world node {node_path} not found in map VPK"));
            continue;
        };
        let doc = match read_resource(map_vpk, entry, map).and_then(|r| {
            r.data_kv3().map_err(|source| ExtractError::Resource {
                map: map.to_string(),
                path: node_path.clone(),
                source: Box::new(source),
            })
        }) {
            Ok(doc) => doc,
            Err(e) => {
                warnings.push(format!("failed to read world node {node_path}: {e}"));
                continue;
            }
        };
        let node = worldnode::decode_world_node(&doc.root).expect("WorldError is uninhabited");

        report.aggregates_skipped += node.aggregate_scene_objects.len();

        for (i, scene_object) in node.scene_objects.iter().enumerate() {
            let Some(model) = &scene_object.renderable_model else {
                continue;
            };
            report.static_props_total += 1;

            let model_phys = lookup.load_model_phys(strip_c(model));
            if model_phys.not_found {
                missing_models.insert(model.clone());
                continue;
            }
            if model_phys.load_error.is_some() {
                // Warned once (per model path) by `ModelLookup` itself.
                load_error_models.insert(model.clone());
                continue;
            }
            let Some(phys) = model_phys.phys else {
                without_phys_models.insert(model.clone());
                continue;
            };

            report.static_props_with_phys += 1;
            let attr_id = mesh
                .add_attribute(CollisionAttribute {
                    name: "EntitySolid".to_string(),
                    interact_as: Vec::new(),
                    interact_with: Vec::new(),
                    interact_exclude: Vec::new(),
                    synthetic: true,
                })
                .unwrap_or(0);
            let transform = Mat3x4 {
                rows: scene_object.transform,
            };
            let object = mesh.add_object(MeshObject {
                kind: ObjectKind::StaticProp,
                classname: None,
                targetname: None,
                model: Some(model.clone()),
                hammer_id: None,
                source_index: i as u32,
                hull_flags: None,
            });
            let mut warn = |s: String| warnings.push(s);
            mesh_build::append_phys(
                mesh,
                &phys,
                &transform,
                AttributeMode::Fixed(attr_id),
                ObjectMode::Fixed(object),
                Source::StaticProp,
                report,
                &mut warn,
            );
        }
    }

    report.static_props_models_not_found = missing_models.len();
    report.static_props_without_phys = without_phys_models.len();
    report.static_props_load_errors = load_error_models.len();
}

fn load_nav(
    map_vpk: &Vpk,
    map: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<NavAreasDump>, ExtractError> {
    let path = format!("maps/{map}.nav");
    let Some(entry) = map_vpk.find(&path) else {
        warnings.push(format!("{path} not found in map VPK"));
        return Ok(None);
    };
    let bytes = map_vpk.read(entry).map_err(|source| ExtractError::Vpk {
        map: map.to_string(),
        path: PathBuf::from(&entry.path),
        source: Box::new(source),
    })?;
    let parsed = nav::parse_nav(&bytes).map_err(|source| ExtractError::Nav {
        map: map.to_string(),
        path: entry.path.clone(),
        source: Box::new(source),
    })?;
    Ok(Some(NavAreasDump::from_nav_mesh(&parsed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn lump(child_lumps: Vec<&str>) -> entities::EntityLump {
        entities::EntityLump {
            child_lumps: child_lumps.into_iter().map(str::to_string).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn exclude_child_lumps_removes_referenced_children() {
        let lumps = vec![
            ("a_c".to_string(), lump(vec!["b"])),
            ("b_c".to_string(), lump(vec![])),
        ];
        let out = exclude_child_lumps(lumps);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "a_c");
    }

    #[test]
    fn exclude_child_lumps_keeps_lumps_nobody_references() {
        let lumps = vec![
            ("a_c".to_string(), lump(vec![])),
            ("b_c".to_string(), lump(vec![])),
        ];
        let out = exclude_child_lumps(lumps);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn exclude_child_lumps_matches_names_case_insensitively_and_with_backslashes() {
        // m_childLumps stores an uncompiled, possibly-backslashed, possibly-mixed-case name;
        // the actual lump path is always lowercase forward-slashed `..._c`.
        let lumps = vec![
            ("a_c".to_string(), lump(vec![r"Worlds\Entities\B"])),
            ("worlds/entities/b_c".to_string(), lump(vec![])),
        ];
        let out = exclude_child_lumps(lumps);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "a_c");
    }

    #[test]
    fn child_lump_worklist_resolves_grandchildren_without_duplicate_loads() {
        // Diamond shape: top -> {b, c} -> d. `d` must be visited exactly once.
        let top = vec![("top".to_string(), lump(vec!["b", "c"]))];
        let load_count: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let out = child_lump_worklist(&top, |name| {
            load_count.borrow_mut().push(name.to_string());
            match name {
                "b" => Some(lump(vec!["d"])),
                "c" => Some(lump(vec!["d"])),
                "d" => Some(lump(vec![])),
                _ => None,
            }
        });
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "d"]);
        assert_eq!(load_count.borrow().iter().filter(|n| *n == "d").count(), 1);
    }

    #[test]
    fn child_lump_worklist_skips_unresolvable_names() {
        let top = vec![("top".to_string(), lump(vec!["missing"]))];
        let out = child_lump_worklist(&top, |_| None);
        assert!(out.is_empty());
    }
}
