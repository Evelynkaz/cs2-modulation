//! Top-level map export: walks the world (scene objects + aggregates) and entities, resolves
//! materials/textures, and writes one glTF document -- `cs2mod export-glb`'s actual work (§2-§6).
//!
//! Ground truth: `IO/Gltf/GltfModelExporter.World.cs`, `GltfModelExporter.Mesh.cs`,
//! `GltfModelExporter.Material.cs`; facts cross-checked against `scratch/f3_survey/REPORT.md`.

use std::collections::HashMap;
use std::sync::Arc;

use s2fmt::entities::{self, Entity, EntityLump, entity_transform};
use serde_json::{Value, json};

use crate::buffer::Buffer;
use crate::entity as ent;
use crate::gltf::{self, GltfBuilder};
use crate::material::{self, AlphaMode, RawMaterial, ResolvedMaterial};
use crate::mesh::{DrawCall, Mesh};
use crate::model::{self, Model};
use crate::source::{Sources, compiled_path};
use crate::texture::{self, TextureBudget};
use crate::world::{self, AggregateRaw, SceneObjectRaw, WorldNodeRaw};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("failed to read world.vwrld_c: {0}")]
    World(#[from] crate::source::SourceError),
    #[error(
        "no entity lumps could be resolved (world listed none, and no *.vents_c in the map VPK)"
    )]
    NoEntityLumps,
    #[error("failed to write glb: {0}")]
    Glb(#[from] crate::gltf::GlbTooLarge),
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub max_texture: u32,
    pub jpeg_quality: u8,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            max_texture: 1024,
            jpeg_quality: 90,
        }
    }
}

pub struct ExportResult {
    pub glb: Vec<u8>,
    pub report: serde_json::Value,
}

/// Every dropped tools-material draw call, bucketed by `vmat` path (§6).
#[derive(Default)]
struct ToolsDrop {
    draw_calls: u32,
    triangles: u64,
}

#[derive(Default)]
struct ClassStats {
    total: u32,
    with_model: u32,
    drawn: u32,
    triangles: u64,
    reasons: HashMap<String, u32>,
}

#[derive(Default)]
struct Report {
    triangles: u64,
    per_material_triangles: HashMap<String, u64>,
    tools_dropped: HashMap<String, ToolsDrop>,
    missing_resources: Vec<String>,
    class_stats: HashMap<String, ClassStats>,
    material_extras: HashMap<u32, serde_json::Value>,
    /// `{classname, targetname, model, reason}` for every entity skipped by a §3 visibility rule
    /// (preview model, or `startdisabled`/`enabled`/`renderamt`/`rendermode`), in encounter order
    /// (§6's `dropped.hiddenEntities`).
    hidden_entities: Vec<serde_json::Value>,
    unlit_used: bool,
    scene_objects_total: u32,
    scene_objects_placed: u32,
    aggregates_total: u32,
    aggregates_fallback: u32,
    fragments_total: u32,
    fragments_placed: u32,
}

/// Cached-decode state shared across the whole export: models, materials, textures and the
/// glTF-mesh dedup cache keyed by (geometry source, draw call, overlay, material).
struct Ctx<'a> {
    sources: &'a Sources,
    budget: TextureBudget,
    builder: GltfBuilder,
    models: HashMap<String, Option<Arc<Model>>>,
    raw_materials: HashMap<String, Option<Arc<RawMaterial>>>,
    resolved_materials: HashMap<String, Arc<ResolvedMaterial>>,
    gltf_materials: HashMap<(String, [u8; 4]), u32>,
    textures: HashMap<(String, bool), Option<u32>>,
    meshes: HashMap<(String, usize, bool, u32), u32>,
    report: Report,
}

fn quantize_tint(t: [f32; 4]) -> [u8; 4] {
    t.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
}

impl<'a> Ctx<'a> {
    fn get_model(&mut self, path: &str) -> Option<Arc<Model>> {
        if let Some(cached) = self.models.get(path) {
            return cached.clone();
        }
        let result = match self.sources.resource(path) {
            Ok(resource) => match model::decode_model(&resource) {
                Ok(m) => Some(Arc::new(m)),
                Err(e) => {
                    self.report
                        .missing_resources
                        .push(format!("{path}: failed to decode model: {e}"));
                    None
                }
            },
            Err(_) => {
                self.report
                    .missing_resources
                    .push(format!("{path}: not found"));
                None
            }
        };
        self.models.insert(path.to_string(), result.clone());
        result
    }

    fn get_raw_material(&mut self, path: &str) -> Option<Arc<RawMaterial>> {
        if let Some(cached) = self.raw_materials.get(path) {
            return cached.clone();
        }
        let compiled = compiled_path(path);
        let result = match self.sources.resource(&compiled) {
            Ok(resource) => match material::decode_material(&resource, &compiled) {
                Ok(m) => Some(Arc::new(m)),
                Err(e) => {
                    self.report
                        .missing_resources
                        .push(format!("{compiled}: failed to decode material: {e}"));
                    None
                }
            },
            Err(_) => {
                self.report
                    .missing_resources
                    .push(format!("{compiled}: not found"));
                None
            }
        };
        self.raw_materials.insert(path.to_string(), result.clone());
        result
    }

    fn get_resolved_material(&mut self, path: &str) -> Option<Arc<ResolvedMaterial>> {
        if let Some(r) = self.resolved_materials.get(path) {
            return Some(r.clone());
        }
        let raw = self.get_raw_material(path)?;
        let resolved = Arc::new(material::resolve(&raw));
        self.resolved_materials
            .insert(path.to_string(), resolved.clone());
        Some(resolved)
    }

    fn get_texture(&mut self, path: &str, is_normal: bool) -> Option<u32> {
        let key = (path.to_string(), is_normal);
        if let Some(cached) = self.textures.get(&key) {
            return *cached;
        }
        let compiled = compiled_path(path);
        let result = match texture::load_and_encode(self.sources, &compiled, self.budget, is_normal)
        {
            Ok(t) => {
                let image = self.builder.add_image(&t.bytes, t.mime_type);
                Some(self.builder.add_texture(image))
            }
            Err(e) => {
                self.report
                    .missing_resources
                    .push(format!("{compiled}: failed to load texture: {e}"));
                None
            }
        };
        self.textures.insert(key, result);
        result
    }

    /// Builds (or reuses) the glTF material for `vmat_path` tinted by `tint_rgba`; `None` if the
    /// material failed to load or is a tools material (caller checks the latter separately so it
    /// can also skip the draw call and bucket it in the report).
    fn get_gltf_material(&mut self, vmat_path: &str, tint_rgba: [f32; 4]) -> Option<u32> {
        let key = (vmat_path.to_string(), quantize_tint(tint_rgba));
        if let Some(&idx) = self.gltf_materials.get(&key) {
            return Some(idx);
        }
        let resolved = self.get_resolved_material(vmat_path)?;
        let (base_color_factor, extras_tint) = material::base_color_factor(&resolved, tint_rgba);

        let mut mat = serde_json::Map::new();
        mat.insert("name".into(), json!(vmat_path));

        let mut pbr = serde_json::Map::new();
        pbr.insert("baseColorFactor".into(), json!(base_color_factor));
        pbr.insert("metallicFactor".into(), json!(0.0));
        pbr.insert("roughnessFactor".into(), json!(1.0));

        if resolved.constant_black {
            pbr.insert(
                "baseColorFactor".into(),
                json!([0.0, 0.0, 0.0, base_color_factor[3]]),
            );
        } else if let Some(tex_path) = &resolved.base_color_texture
            && let Some(tex_index) = self.get_texture(tex_path, false)
        {
            pbr.insert("baseColorTexture".into(), json!({ "index": tex_index }));
        }
        mat.insert("pbrMetallicRoughness".into(), Value::Object(pbr));

        if let Some(normal_path) = &resolved.normal_texture
            && let Some(tex_index) = self.get_texture(normal_path, true)
        {
            mat.insert("normalTexture".into(), json!({ "index": tex_index }));
        }

        mat.insert(
            "alphaMode".into(),
            json!(match resolved.alpha_mode {
                AlphaMode::Opaque => "OPAQUE",
                AlphaMode::Mask => "MASK",
                AlphaMode::Blend => "BLEND",
            }),
        );
        if let Some(cutoff) = resolved.alpha_cutoff {
            mat.insert("alphaCutoff".into(), json!(cutoff));
        }
        if resolved.double_sided {
            mat.insert("doubleSided".into(), json!(true));
        }
        if resolved.unlit {
            mat.insert("extensions".into(), json!({ "KHR_materials_unlit": {} }));
            self.report.unlit_used = true;
        }

        let mut extras = serde_json::Map::new();
        if resolved.mod2x {
            extras.insert("blendMode".into(), json!("mod2x"));
        }
        if let Some(tint) = extras_tint {
            extras.insert("tint".into(), json!(tint));
            if let Some(mask_path) = &resolved.tint_mask_texture
                && let Some(idx) = self.get_texture(mask_path, false)
            {
                extras.insert("tintMask".into(), json!(idx));
            }
        }
        if let Some(layers) = &resolved.layers {
            let mut layer_json = serde_json::Map::new();
            if let Some(p) = &layers.layer2_color
                && let Some(idx) = self.get_texture(p, false)
            {
                layer_json.insert("layer2ColorTexture".into(), json!(idx));
            }
            if let Some(p) = &layers.layer2_normal
                && let Some(idx) = self.get_texture(p, true)
            {
                layer_json.insert("layer2NormalTexture".into(), json!(idx));
            }
            if let Some(p) = &layers.blend_modulation
                && let Some(idx) = self.get_texture(p, false)
            {
                layer_json.insert("blendModulationTexture".into(), json!(idx));
            }
            layer_json.insert("formula".into(), json!(material::LAYER_BLEND_FORMULA));
            extras.insert("layers".into(), Value::Object(layer_json));
        }
        if !extras.is_empty() {
            mat.insert("extras".into(), Value::Object(extras.clone()));
        }

        let index = self.builder.add_material(Value::Object(mat));
        if !extras.is_empty() {
            self.report
                .material_extras
                .insert(index, Value::Object(extras));
        }
        self.gltf_materials.insert(key, index);
        Some(index)
    }
}

/// A field's decoded full-buffer array, for whichever of POSITION/NORMAL/TEXCOORD/`_BLEND` a
/// draw call carries -- found by searching every vertex buffer the draw call binds, since which
/// physical stream carries which semantic varies (`REPORT.md` §7: overlay UV sometimes sits in
/// the position stream).
fn find_field<'b>(
    mesh: &'b Mesh,
    dc: &DrawCall,
    semantic: &str,
    index: i32,
) -> Option<(&'b Buffer, &'b crate::buffer::InputLayoutField)> {
    for &vb_idx in &dc.vertex_buffers {
        if let Some(buf) = mesh.vertex_buffers.get(vb_idx)
            && let Some(f) = buf.field(semantic, index)
        {
            return Some((buf, f));
        }
    }
    None
}

struct BuiltGeometry {
    position: u32,
    normal: u32,
    uv0: Option<u32>,
    uv1: Option<u32>,
    blend: Option<u32>,
    indices: u32,
    triangles: u64,
}

/// Decodes one draw call's geometry into fresh glTF accessors (positions/normals/UV/`_BLEND`
/// plus a compacted local index buffer), applying the overlay normal offset if requested (§4/§6).
#[allow(clippy::too_many_arguments)]
fn build_geometry(
    builder: &mut GltfBuilder,
    mesh: &Mesh,
    dc: &DrawCall,
    overlay: bool,
    needs_uv1: bool,
    lightmap_uv_scale: [f32; 2],
    needs_blend: bool,
) -> Result<BuiltGeometry, crate::error::MeshError> {
    let abs_indices = dc.resolve_indices(mesh)?;

    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut order: Vec<u32> = Vec::new();
    let mut local_indices = Vec::with_capacity(abs_indices.len());
    for &ai in &abs_indices {
        let local = *remap.entry(ai).or_insert_with(|| {
            order.push(ai);
            (order.len() - 1) as u32
        });
        local_indices.push(local);
    }

    let (pos_buf, pos_field) =
        find_field(mesh, dc, "POSITION", 0).ok_or_else(|| crate::error::MeshError::Missing {
            path: "POSITION".to_string(),
        })?;
    let positions_full = crate::attributes::decode_positions(pos_buf, pos_field)?;

    let normals_full = find_field(mesh, dc, "NORMAL", 0)
        .map(|(b, f)| crate::attributes::decode_normals(b, f))
        .transpose()?;
    let uv0_full = find_field(mesh, dc, "TEXCOORD", 0)
        .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
        .transpose()?;
    let uv1_full = if needs_uv1 {
        find_field(mesh, dc, "TEXCOORD", 1)
            .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
            .transpose()?
    } else {
        None
    };
    let blend_full = if needs_blend {
        find_field(mesh, dc, "TEXCOORD", 4)
            .map(|(b, f)| crate::attributes::decode_texcoord(b, f))
            .transpose()?
    } else {
        None
    };

    let mut positions = Vec::with_capacity(order.len());
    let mut normals = Vec::with_capacity(order.len());
    let mut uv0 = Vec::with_capacity(order.len());
    let mut uv1 = Vec::with_capacity(order.len());
    let mut blend = Vec::with_capacity(order.len());

    for &ai in &order {
        let i = ai as usize;
        let mut p = *positions_full.get(i).unwrap_or(&[0.0, 0.0, 0.0]);
        let n = normals_full
            .as_ref()
            .and_then(|arr| arr.get(i))
            .map(|n| n.normal)
            .unwrap_or([0.0, 0.0, 1.0]);
        if overlay {
            p = [
                p[0] + n[0] * material::OVERLAY_NORMAL_OFFSET,
                p[1] + n[1] * material::OVERLAY_NORMAL_OFFSET,
                p[2] + n[2] * material::OVERLAY_NORMAL_OFFSET,
            ];
        }
        positions.push(p);
        normals.push(n);
        uv0.push(
            uv0_full
                .as_ref()
                .map(|a| [a.get(i)[0], a.get(i)[1]])
                .unwrap_or([0.0, 0.0]),
        );
        if needs_uv1 {
            let raw = uv1_full
                .as_ref()
                .map(|a| [a.get(i)[0], a.get(i)[1]])
                .unwrap_or([0.0, 0.0]);
            uv1.push([raw[0] * lightmap_uv_scale[0], raw[1] * lightmap_uv_scale[1]]);
        }
        if needs_blend {
            let w = blend_full.as_ref().map(|a| a.get(i)[0]).unwrap_or(0.0);
            blend.push(w.clamp(0.0, 1.0));
        }
    }

    let triangles = (local_indices.len() / 3) as u64;
    let position = builder.add_positions(&positions);
    let normal = builder.add_vec3(&normals);
    let uv0_idx = Some(builder.add_vec2(&uv0));
    let uv1_idx = if needs_uv1 {
        Some(builder.add_vec2(&uv1))
    } else {
        None
    };
    let blend_idx = if needs_blend {
        Some(builder.add_blend_f32(&blend))
    } else {
        None
    };
    let indices = builder.add_indices(&local_indices);

    Ok(BuiltGeometry {
        position,
        normal,
        uv0: uv0_idx,
        uv1: uv1_idx,
        blend: blend_idx,
        indices,
        triangles,
    })
}

/// Gets or builds the one-primitive glTF mesh for `(mesh_key, draw call index into `mesh`'s own
/// flattened `m_sceneObjects[].m_drawCalls[]` list, overlay, material)`. Multiple placements
/// sharing this key become separate nodes instancing the same glTF mesh (§6's "instances are
/// nodes with matrices") -- a deliberate simplification vs. the reference's "one glTF mesh per
/// source vmesh, N primitives" grouping: geometrically and in triangle count these are identical,
/// only the JSON grouping granularity differs.
#[allow(clippy::too_many_arguments)]
fn get_or_build_mesh(
    ctx: &mut Ctx,
    mesh_key: &str,
    mesh: &Mesh,
    flat_index: usize,
    dc: &DrawCall,
    overlay: bool,
    lightmap_uv_scale: [f32; 2],
    tint_rgba: [f32; 4],
) -> Option<(u32, u64)> {
    let Some(material_path) = &dc.material_path else {
        ctx.report.missing_resources.push(format!(
            "{mesh_key}#{flat_index}: draw call has no material path"
        ));
        return None;
    };
    let tris = (dc.index_count.max(0) / 3) as u64;
    let raw = ctx.get_raw_material(material_path)?;
    if raw.is_tools_material() {
        let entry = ctx
            .report
            .tools_dropped
            .entry(material_path.clone())
            .or_default();
        entry.draw_calls += 1;
        entry.triangles += tris;
        return None;
    }

    let material_index = ctx.get_gltf_material(material_path, tint_rgba)?;
    // Every instance (cache hit or not) contributes `tris` to the report's "instanced" total,
    // matching VRF's own `triangles_instanced` -- an instance sharing a cached mesh still draws
    // its own copy of the triangles at render time (§6's "so counts the reference").
    ctx.report.triangles += tris;
    *ctx.report
        .per_material_triangles
        .entry(material_path.clone())
        .or_insert(0) += tris;

    let key = (mesh_key.to_string(), flat_index, overlay, material_index);
    if let Some(&mesh_idx) = ctx.meshes.get(&key) {
        return Some((mesh_idx, tris));
    }

    let resolved = ctx.get_resolved_material(material_path)?;
    let needs_uv1 = false; // TEXCOORD_1/lightmap UV is F3a-4's concern (§6); wired up but unused for now.
    let needs_blend = resolved.layers.is_some();
    let geometry = match build_geometry(
        &mut ctx.builder,
        mesh,
        dc,
        overlay,
        needs_uv1,
        lightmap_uv_scale,
        needs_blend,
    ) {
        Ok(g) => g,
        Err(e) => {
            ctx.report
                .missing_resources
                .push(format!("{mesh_key}#{flat_index}: {e}"));
            return None;
        }
    };

    let mut attributes = serde_json::Map::new();
    attributes.insert("POSITION".into(), json!(geometry.position));
    attributes.insert("NORMAL".into(), json!(geometry.normal));
    if let Some(uv0) = geometry.uv0 {
        attributes.insert("TEXCOORD_0".into(), json!(uv0));
    }
    if let Some(uv1) = geometry.uv1 {
        attributes.insert("TEXCOORD_1".into(), json!(uv1));
    }
    if let Some(blend) = geometry.blend {
        attributes.insert("_BLEND".into(), json!(blend));
    }
    let primitive = json!({ "attributes": Value::Object(attributes), "indices": geometry.indices, "material": material_index });
    let mesh_json = json!({ "primitives": [primitive] });
    let mesh_idx = ctx.builder.add_mesh(mesh_json);
    ctx.meshes.insert(key, mesh_idx);
    debug_assert_eq!(
        geometry.triangles, tris,
        "resolved index count must match the draw call's own"
    );

    Some((mesh_idx, tris))
}

fn flatten_draw_calls(mesh: &Mesh) -> Vec<&DrawCall> {
    mesh.scene_objects
        .iter()
        .flat_map(|so| so.draw_calls.iter())
        .collect()
}

/// `overlay_order` is `Some(m_nOverlayRenderOrder)` only for a placement whose *object* is itself
/// flagged overlay (`OBJECT_TYPE_OVERLAY`), never merely because a material sets `F_OVERLAY` (§4);
/// aggregates and entities always pass `None`. Written as `extras.overlayOrder` on the node only
/// when `Some`.
fn add_instance(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    name: Option<&str>,
    mesh_idx: u32,
    transform: &[[f32; 4]; 3],
    overlay_order: Option<i64>,
) {
    let mut node = serde_json::Map::new();
    if let Some(n) = name {
        node.insert("name".into(), json!(n));
    }
    node.insert("mesh".into(), json!(mesh_idx));
    if *transform != world::IDENTITY_TRANSFORM {
        node.insert("matrix".into(), json!(gltf::node_matrix(transform)));
    }
    if let Some(order) = overlay_order {
        node.insert("extras".into(), json!({ "overlayOrder": order }));
    }
    let idx = ctx.builder.add_node(Value::Object(node));
    children.push(idx);
}

/// Every mesh a regular (non-aggregate) placement's model carries, at its lowest (most detailed)
/// LOD level: embedded meshes first, then referenced ones, matching `LoadModelMeshes`
/// (`GltfModelExporter.cs:576-598`).
fn model_meshes_at_lowest_lod(
    ctx: &mut Ctx,
    model_path: &str,
    model: &Model,
) -> Vec<(String, Arc<Mesh>)> {
    let level = model.lod.lowest_level;
    let mut out = Vec::new();
    for em in &model.embedded_meshes {
        if !model.lod.is_mesh_in_level(em.mesh_index, level) {
            continue;
        }
        out.push((
            format!("{model_path}#embedded{}", em.mesh_index),
            Arc::new(em.mesh.clone()),
        ));
    }
    for rm in &model.ref_meshes {
        if !model.lod.is_mesh_in_level(rm.mesh_index, level) {
            continue;
        }
        let ref_path = compiled_path(&rm.mesh_path);
        match ctx.sources.resource(&ref_path) {
            Ok(resource) => match crate::mesh::decode_mesh_resource(&resource, &ref_path) {
                Ok(m) => out.push((
                    format!("{model_path}#ref{}:{ref_path}", rm.mesh_index),
                    Arc::new(m),
                )),
                Err(e) => ctx
                    .report
                    .missing_resources
                    .push(format!("{ref_path}: failed to decode mesh: {e}")),
            },
            Err(_) => ctx
                .report
                .missing_resources
                .push(format!("{ref_path}: not found")),
        }
    }
    out
}

fn combine_tint(outer: [f32; 4], dc: &DrawCall) -> [f32; 4] {
    let rgb = dc.tint_color.unwrap_or([1.0, 1.0, 1.0]);
    let a = dc.alpha.unwrap_or(1.0);
    [
        outer[0] * rgb[0],
        outer[1] * rgb[1],
        outer[2] * rgb[2],
        outer[3] * a,
    ]
}

fn place_scene_object(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    so: &SceneObjectRaw,
    lightmap_uv_scale: [f32; 2],
) {
    // Absent/default `m_nOverlayRenderOrder` (0) means "no explicit order", not "order 0" -- on
    // Mirage most overlay objects have no order set at all, only 4 do (§4).
    let overlay_order = (so.overlay_render_order != 0).then_some(so.overlay_render_order);
    if let Some(renderable_model) = &so.renderable_model {
        let path = compiled_path(renderable_model);
        let Some(model) = ctx.get_model(&path) else {
            return;
        };
        // Overlay world scene objects still go through `place_model`'s non-overlay path when the
        // object flag alone drives it; folded in below by re-resolving materials per draw call
        // instead (kept simple: overlay offset also applies when the *material* says overlay,
        // handled inside `get_or_build_mesh`'s geometry key via the object-level flag OR'd with
        // the material's own -- see the call below).
        place_model_with_overlay(
            ctx,
            children,
            &path,
            &model,
            None,
            &so.transform,
            so.tint,
            None,
            lightmap_uv_scale,
            so.is_overlay,
            overlay_order,
        );
    } else if let Some(renderable) = &so.renderable {
        let path = compiled_path(renderable);
        let Ok(resource) = ctx.sources.resource(&path) else {
            ctx.report
                .missing_resources
                .push(format!("{path}: not found"));
            return;
        };
        let mesh = match crate::mesh::decode_mesh_resource(&resource, &path) {
            Ok(m) => m,
            Err(e) => {
                ctx.report
                    .missing_resources
                    .push(format!("{path}: failed to decode mesh: {e}"));
                return;
            }
        };
        place_mesh_draw_calls(
            ctx,
            children,
            &path,
            &mesh,
            None,
            &so.transform,
            so.tint,
            None,
            lightmap_uv_scale,
            so.is_overlay,
            overlay_order,
        );
    }
}

/// Places every triangle-list draw call in one already-resolved mesh: overlay is `object_overlay
/// || material.material_says_overlay` (§4), with an optional per-draw-call skin remap. Shared by
/// [`place_model_with_overlay`] (looping over a model's meshes) and `place_scene_object`'s
/// `m_renderable` path (already just one mesh), so both `m_renderableModel` and `m_renderable`
/// scene objects place their draw calls the same way (§2/§11).
#[allow(clippy::too_many_arguments)]
fn place_mesh_draw_calls(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    mesh_key: &str,
    mesh: &Mesh,
    name: Option<&str>,
    transform: &[[f32; 4]; 3],
    tint_rgba: [f32; 4],
    skin_map: Option<&HashMap<String, String>>,
    lightmap_uv_scale: [f32; 2],
    object_overlay: bool,
    overlay_order: Option<i64>,
) -> u64 {
    let mut triangles = 0u64;
    let flat = flatten_draw_calls(mesh);
    for (i, dc) in flat.iter().enumerate() {
        if !dc.is_triangle_list {
            continue;
        }
        let remapped;
        let dc_ref: &DrawCall = if let (Some(map), Some(orig)) = (skin_map, &dc.material_path) {
            if let Some(new_path) = map.get(orig) {
                let mut cloned = (*dc).clone();
                cloned.material_path = Some(new_path.clone());
                remapped = cloned;
                &remapped
            } else {
                dc
            }
        } else {
            dc
        };
        let Some(material_path) = &dc_ref.material_path else {
            continue;
        };
        let material_overlay = ctx
            .get_resolved_material(material_path)
            .map(|r| r.material_says_overlay)
            .unwrap_or(false);
        let overlay = object_overlay || material_overlay;
        let final_tint = combine_tint(tint_rgba, dc_ref);
        let Some((mesh_idx, tris)) = get_or_build_mesh(
            ctx,
            mesh_key,
            mesh,
            i,
            dc_ref,
            overlay,
            lightmap_uv_scale,
            final_tint,
        ) else {
            continue;
        };
        triangles += tris;
        // `m_nOverlayRenderOrder` is a scene-object property: only write it when the *object*
        // itself is the overlay (not merely a regular placement whose material happens to set
        // `F_OVERLAY`, which still gets the vertex offset above but has no such order of its own).
        add_instance(
            ctx,
            children,
            name,
            mesh_idx,
            transform,
            if object_overlay { overlay_order } else { None },
        );
    }
    triangles
}

/// Like [`place_model`], but resolves the per-draw-call overlay flag as `object_overlay ||
/// material.material_says_overlay` (§4).
#[allow(clippy::too_many_arguments)]
fn place_model_with_overlay(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    model_path: &str,
    model: &Model,
    name: Option<&str>,
    transform: &[[f32; 4]; 3],
    tint_rgba: [f32; 4],
    skin_map: Option<&HashMap<String, String>>,
    lightmap_uv_scale: [f32; 2],
    object_overlay: bool,
    overlay_order: Option<i64>,
) -> u64 {
    let mut triangles = 0u64;
    for (mesh_key, mesh) in model_meshes_at_lowest_lod(ctx, model_path, model) {
        triangles += place_mesh_draw_calls(
            ctx,
            children,
            &mesh_key,
            &mesh,
            name,
            transform,
            tint_rgba,
            skin_map,
            lightmap_uv_scale,
            object_overlay,
            overlay_order,
        );
    }
    triangles
}

fn place_aggregate(
    ctx: &mut Ctx,
    children: &mut Vec<u32>,
    agg: &AggregateRaw,
    lightmap_uv_scale: [f32; 2],
) {
    let Some(renderable_model) = &agg.renderable_model else {
        return;
    };
    let path = compiled_path(renderable_model);
    let Some(model) = ctx.get_model(&path) else {
        return;
    };

    let has_fragment_data = agg.fragments.iter().any(|f| f.draw_call_index >= 0);
    if agg.fragments.is_empty() || !has_fragment_data {
        // No usable m_nDrawCallIndex data: fall back to a single identity-placed instance
        // (`AggregateCreateFragments` returning `false` -> `LoadModel` fallback).
        ctx.report.aggregates_fallback += 1;
        place_model_with_overlay(
            ctx,
            children,
            &path,
            &model,
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            lightmap_uv_scale,
            false,
            None,
        );
        return;
    }

    let mesh = if let Some(em) = model.embedded_meshes.first() {
        em.mesh.clone()
    } else if let Some(rm) = model.ref_meshes.first() {
        let ref_path = compiled_path(&rm.mesh_path);
        match ctx.sources.resource(&ref_path).and_then(|r| {
            crate::mesh::decode_mesh_resource(&r, &ref_path).map_err(|e| {
                crate::source::SourceError::Parse {
                    path: ref_path.clone(),
                    source: s2fmt::resource::ResourceError::Invalid {
                        detail: e.to_string(),
                    },
                }
            })
        }) {
            Ok(m) => m,
            Err(_) => {
                ctx.report
                    .missing_resources
                    .push(format!("{ref_path}: not found or failed to decode"));
                return;
            }
        }
    } else {
        ctx.report
            .missing_resources
            .push(format!("{path}: aggregate model has no mesh"));
        return;
    };

    let placements = match world::build_fragment_placements(agg) {
        Ok(p) => p,
        Err(e) => {
            ctx.report.missing_resources.push(format!("{path}: {e}"));
            return;
        }
    };
    let flat = flatten_draw_calls(&mesh);
    for placement in &placements {
        let Some(&dc) = flat.get(placement.draw_call_index) else {
            ctx.report.missing_resources.push(format!(
                "{path}: fragment references draw call {} out of {}",
                placement.draw_call_index,
                flat.len()
            ));
            continue;
        };
        if !dc.is_triangle_list {
            continue;
        }
        let Some(material_path) = &dc.material_path else {
            continue;
        };
        let material_overlay = ctx
            .get_resolved_material(material_path)
            .map(|r| r.material_says_overlay)
            .unwrap_or(false);
        let tint = [placement.tint[0], placement.tint[1], placement.tint[2], 1.0];
        let final_tint = combine_tint(tint, dc);
        let Some((mesh_idx, _tris)) = get_or_build_mesh(
            ctx,
            &path,
            &mesh,
            placement.draw_call_index,
            dc,
            material_overlay,
            lightmap_uv_scale,
            final_tint,
        ) else {
            continue;
        };
        ctx.report.fragments_placed += 1;
        add_instance(ctx, children, None, mesh_idx, &placement.transform, None);
    }
}

fn bump_class<'r>(report: &'r mut Report, classname: &str, has_model: bool) -> &'r mut ClassStats {
    let stats = report.class_stats.entry(classname.to_string()).or_default();
    stats.total += 1;
    if has_model {
        stats.with_model += 1;
    }
    stats
}

struct EntityLight {
    direction: [f32; 3],
    color: [f32; 3],
    brightness: f32,
    /// `brightnessscale` (default 1, §6): multiplies into `colorLinear` alongside `brightness`.
    brightness_scale: f32,
    skycolor: [f32; 3],
    skyintensity: f32,
}

fn entity_color01(e: &Entity, key: &str) -> Option<[f32; 3]> {
    let v = e.get_vec3(key)?;
    Some([v[0] / 255.0, v[1] / 255.0, v[2] / 255.0])
}

fn place_entities(
    ctx: &mut Ctx,
    lump: &EntityLump,
    children: &mut Vec<u32>,
    sun: &mut Option<EntityLight>,
    lightmap_uv_scale: [f32; 2],
) {
    for entity in &lump.entities {
        let classname = entity.classname();
        if classname.is_empty() {
            continue;
        }
        let model = entity.get_str("model").unwrap_or("");
        let has_model = !model.is_empty();

        if classname == "light_environment" && sun.is_none() {
            let angles = entity.angles();
            // `forward = R * (1,0,0)^T`, i.e. the first *column* (`s2fmt::entities::
            // angles_to_matrix`'s own doc comment) -- verified against `REPORT.md`'s own
            // Mirage number: angles [60,318,0] -> (0.3716,-0.3346,-0.8660).
            let m = entities::angles_to_matrix(angles);
            let direction = [m[0][0], m[1][0], m[2][0]];
            *sun = Some(EntityLight {
                direction,
                color: entity_color01(entity, "color").unwrap_or([1.0, 1.0, 1.0]),
                brightness: entity
                    .get("brightness")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        s2fmt::entities::EntityValue::Int(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
                brightness_scale: entity
                    .get("brightnessscale")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        s2fmt::entities::EntityValue::Int(i) => Some(*i as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
                skycolor: entity_color01(entity, "skycolor").unwrap_or([1.0, 1.0, 1.0]),
                skyintensity: entity
                    .get("skyintensity")
                    .and_then(|v| match v {
                        s2fmt::entities::EntityValue::Float(f) => Some(*f as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0),
            });
        }

        let stats = bump_class(&mut ctx.report, classname, has_model);
        if !has_model {
            continue;
        }
        if classname == "csgo_player_previewmodel" {
            *stats
                .reasons
                .entry("preview model".to_string())
                .or_insert(0) += 1;
            ctx.report.hidden_entities.push(json!({
                "classname": classname,
                "targetname": entity.targetname(),
                "model": model,
                "reason": "preview model",
            }));
            continue;
        }
        if !ent::should_render(entity) {
            *stats
                .reasons
                .entry("hidden (startdisabled/enabled/renderamt/rendermode)".to_string())
                .or_insert(0) += 1;
            ctx.report.hidden_entities.push(json!({
                "classname": classname,
                "targetname": entity.targetname(),
                "model": model,
                "reason": "hidden (startdisabled/enabled/renderamt/rendermode)",
            }));
            continue;
        }

        let path = compiled_path(model);
        let Some(loaded_model) = ctx.get_model(&path) else {
            let stats = ctx
                .report
                .class_stats
                .entry(classname.to_string())
                .or_default();
            *stats
                .reasons
                .entry("model not found".to_string())
                .or_insert(0) += 1;
            continue;
        };

        let transform = entity_transform(entity.origin(), entity.angles(), entity.scales());
        let tint = ent::entity_tint(entity);
        let skin = ent::skin_name(entity);
        let skin_map = skin.and_then(|s| ent::skin_remap(&loaded_model, s));
        let name = entity.targetname().or(Some(classname));

        let triangles = place_model_with_overlay(
            ctx,
            children,
            &path,
            &loaded_model,
            name,
            &transform,
            tint,
            skin_map.as_ref(),
            lightmap_uv_scale,
            false,
            None,
        );

        let stats = ctx
            .report
            .class_stats
            .entry(classname.to_string())
            .or_default();
        if triangles > 0 {
            stats.drawn += 1;
            stats.triangles += triangles;
        } else {
            *stats
                .reasons
                .entry("no render mesh (physics-only model)".to_string())
                .or_insert(0) += 1;
        }
    }
}

/// `m_entityLumps`/`m_childLumps` entries are `.vents` resource paths (`world.rs`'s
/// `world_node_path` applies the same lower-case/forward-slash transform for `.vwnod`); this is
/// that transform, `_c`-suffixed, shared by the listed lumps and the child-lump walk below.
fn entity_lump_path(name: &str) -> String {
    format!("{}_c", name.replace('\\', "/").to_ascii_lowercase())
}

fn load_entity_lump(sources: &Sources, path: &str) -> Option<EntityLump> {
    let resource = sources.resource(path).ok()?;
    let doc = resource
        .data_kv3()
        .map_err(|source| crate::source::SourceError::Parse {
            path: path.to_string(),
            source,
        })
        .ok()?;
    entities::decode_entity_lump(&doc.root).ok()
}

fn resolve_entity_lumps(sources: &Sources, listed: &[String]) -> Vec<EntityLump> {
    let mut out = Vec::new();
    let mut ok = !listed.is_empty();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in listed {
        let path = entity_lump_path(name);
        visited.insert(path.clone());
        match load_entity_lump(sources, &path) {
            Some(lump) => out.push(lump),
            None => {
                ok = false;
                break;
            }
        }
    }
    if ok && !out.is_empty() {
        // `m_childLumps` (§3 "включая дочерние"): walked by a growing index so a child lump's
        // own children are followed too (de_vertigo's own children have none, but nothing here
        // assumes that), in deterministic encounter order; `visited` (keyed the same way as the
        // path lookup) skips a name already loaded instead of looping on a cycle.
        let mut i = 0;
        while i < out.len() {
            let children = out[i].child_lumps.clone();
            for child in &children {
                let path = entity_lump_path(child);
                if !visited.insert(path.clone()) {
                    continue;
                }
                if let Some(lump) = load_entity_lump(sources, &path) {
                    out.push(lump);
                }
            }
            i += 1;
        }
        return out;
    }
    out.clear();
    for path in sources.entries_with_extension("vents_c") {
        if let Some(lump) = load_entity_lump(sources, &path) {
            out.push(lump);
        }
    }
    out
}

/// Exports `map` (already resolved to `sources`) to a `.glb` byte buffer and a `render.json`
/// report value.
pub fn export_map(sources: &Sources, options: &ExportOptions) -> Result<ExportResult, ExportError> {
    let (_world_path, world_resource) = sources.resource_ending_with("world.vwrld_c")?;
    let world_doc = world::decode_world(
        &world_resource
            .data_kv3()
            .map_err(crate::source::SourceError::from_resource_err)?
            .root,
    );

    let mut ctx = Ctx {
        sources,
        budget: TextureBudget {
            max_side: options.max_texture,
            jpeg_quality: options.jpeg_quality,
        },
        builder: GltfBuilder::new(),
        models: HashMap::new(),
        raw_materials: HashMap::new(),
        resolved_materials: HashMap::new(),
        gltf_materials: HashMap::new(),
        textures: HashMap::new(),
        meshes: HashMap::new(),
        report: Report::default(),
    };

    let mut world_children: Vec<u32> = Vec::new();
    for prefix in &world_doc.world_node_prefixes {
        let node_path = world::world_node_path(prefix);
        let node_resource = match sources.resource(&node_path) {
            Ok(r) => r,
            Err(_) => {
                ctx.report
                    .missing_resources
                    .push(format!("{node_path}: not found"));
                continue;
            }
        };
        let root = match node_resource.data_kv3() {
            Ok(d) => d.root,
            Err(e) => {
                ctx.report
                    .missing_resources
                    .push(format!("{node_path}: {e}"));
                continue;
            }
        };
        let node: WorldNodeRaw = match world::decode_world_node(&root) {
            Ok(n) => n,
            Err(_) => continue,
        };
        for so in &node.scene_objects {
            ctx.report.scene_objects_total += 1;
            let before = ctx.report.triangles;
            place_scene_object(
                &mut ctx,
                &mut world_children,
                so,
                world_doc.lightmap_uv_scale,
            );
            if ctx.report.triangles > before {
                ctx.report.scene_objects_placed += 1;
            }
        }
        for agg in &node.aggregates {
            ctx.report.aggregates_total += 1;
            ctx.report.fragments_total += agg.fragments.len() as u32;
            place_aggregate(
                &mut ctx,
                &mut world_children,
                agg,
                world_doc.lightmap_uv_scale,
            );
        }
    }

    let lumps = resolve_entity_lumps(sources, &world_doc.entity_lumps);
    if lumps.is_empty() {
        return Err(ExportError::NoEntityLumps);
    }
    let mut entity_children: Vec<u32> = Vec::new();
    let mut sun: Option<EntityLight> = None;
    for lump in &lumps {
        place_entities(
            &mut ctx,
            lump,
            &mut entity_children,
            &mut sun,
            world_doc.lightmap_uv_scale,
        );
    }

    let world_group = ctx
        .builder
        .add_node(json!({ "name": "world", "children": world_children }));
    let entities_group = ctx
        .builder
        .add_node(json!({ "name": "entities", "children": entity_children }));

    let extensions_used = if ctx.report.unlit_used {
        vec!["KHR_materials_unlit".to_string()]
    } else {
        Vec::new()
    };

    let node_count = ctx.builder.node_count();
    let mesh_count = ctx.builder.mesh_count();
    let material_count = ctx.builder.material_count();
    let texture_count = ctx.builder.texture_count();
    let geometry_bytes = ctx.builder.geometry_bytes();
    let texture_bytes = ctx.builder.texture_bytes();

    let sun_json = sun.as_ref().map(|s| {
        let to_sun = [-s.direction[0], -s.direction[1], -s.direction[2]];
        let linear = material::srgb_to_linear(s.color);
        let color_linear = [
            linear[0] * s.brightness * s.brightness_scale,
            linear[1] * s.brightness * s.brightness_scale,
            linear[2] * s.brightness * s.brightness_scale,
        ];
        json!({
            "direction": s.direction,
            "directionMeaning": "unit vector the sunlight travels along (from the sun toward the scene), game axes, Z up",
            "toSun": to_sun,
            "color": s.color,
            "colorSpace": "srgb",
            "colorLinear": color_linear,
            "brightness": s.brightness,
            "brightnessScale": s.brightness_scale,
            "skycolor": s.skycolor,
            "skyintensity": s.skyintensity,
        })
    });

    let mut per_material: Vec<(String, u64)> =
        ctx.report.per_material_triangles.into_iter().collect();
    // Triangle count descending, material path ascending as a tiebreaker: `per_material_triangles`
    // is a `HashMap`, so ties must not fall back to its (per-process-random) iteration order --
    // `export-glb`'s determinism check (§ проверки item 6) needs the same `render.json` bytes on
    // every run.
    per_material.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut tools_dropped: Vec<serde_json::Value> = ctx
        .report
        .tools_dropped
        .iter()
        .map(|(path, d)| json!({ "material": path, "drawCalls": d.draw_calls, "triangles": d.triangles }))
        .collect();
    tools_dropped.sort_by(|a, b| a["material"].as_str().cmp(&b["material"].as_str()));

    let mut entity_table: Vec<serde_json::Value> = ctx
        .report
        .class_stats
        .iter()
        .map(|(classname, s)| {
            json!({
                "classname": classname,
                "total": s.total,
                "withModel": s.with_model,
                "drawn": s.drawn,
                "triangles": s.triangles,
                "reasons": s.reasons,
            })
        })
        .collect();
    entity_table.sort_by(|a, b| a["classname"].as_str().cmp(&b["classname"].as_str()));

    let report = json!({
        "formatVersion": 1,
        "textureBudget": { "maxSide": options.max_texture, "jpegQuality": options.jpeg_quality },
        "sun": sun_json,
        "counts": {
            "nodes": node_count,
            "meshes": mesh_count,
            "triangles": ctx.report.triangles,
            "materials": material_count,
            "textures": texture_count,
            "geometryBytes": geometry_bytes,
            "textureBytes": texture_bytes,
            "sceneObjectsTotal": ctx.report.scene_objects_total,
            "sceneObjectsPlaced": ctx.report.scene_objects_placed,
            "aggregatesTotal": ctx.report.aggregates_total,
            "aggregatesFallback": ctx.report.aggregates_fallback,
            "fragmentsTotal": ctx.report.fragments_total,
            "fragmentsPlaced": ctx.report.fragments_placed,
        },
        "perMaterialTriangles": per_material.into_iter().map(|(k,v)| json!({"material": k, "triangles": v})).collect::<Vec<_>>(),
        "dropped": {
            "toolsMaterials": tools_dropped,
            "missingResources": ctx.report.missing_resources,
            "hiddenEntities": ctx.report.hidden_entities,
        },
        "entities": entity_table,
        "materialExtras": {
            "tintColorSpace": "linear (extras.tint is sRGB->linear converted, same as baseColorFactor; extras.layers/extras.tintMask hold glTF texture indices, not colors)",
            "byMaterial": ctx.report.material_extras.iter().map(|(k,v)| (k.to_string(), v.clone())).collect::<serde_json::Map<_,_>>(),
        },
    });

    let glb = ctx.builder.finish(
        vec![world_group, entities_group],
        extensions_used,
        json!({}),
    )?;

    Ok(ExportResult { glb, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Compression, InputLayoutField};
    use crate::format::DxgiFormat;
    use crate::mesh::{DrawCallFlags, SceneObject};

    struct TempDir(std::path::PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "s2render-export-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    /// A resource container with no blocks (`docs/FORMATS.md` §2.1-2.2): `Resource::parse`
    /// succeeds but `data_kv3` (and so `decode_mesh_resource`) fails -- exercises the
    /// `m_renderable` path's mesh-decode-failure reporting below (§11).
    fn empty_resource_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 16];
        b[0..4].copy_from_slice(&16u32.to_le_bytes()); // file size
        b[4..6].copy_from_slice(&12u16.to_le_bytes()); // header version
        b[8..12].copy_from_slice(&8u32.to_le_bytes()); // block_offset -> zero blocks follow
        b
    }

    /// A trivial version-1 VPK tree (`docs/FORMATS.md` §1.2) with at most one inline-stored
    /// entry -- just enough for `Sources::open`/`Sources::resource` below; no numbered archives,
    /// preload, or CRC checking needed (`Vpk::read` never verifies the CRC).
    fn write_test_vpk(path: &std::path::Path, entry: Option<(&str, &[u8])>) {
        let mut tree = Vec::new();
        if let Some((entry_path, data)) = entry {
            let (dir, file) = entry_path.rsplit_once('/').unwrap_or((" ", entry_path));
            let (name, ext) = file.rsplit_once('.').unwrap_or((file, " "));
            tree.extend_from_slice(ext.as_bytes());
            tree.push(0);
            tree.extend_from_slice(dir.as_bytes());
            tree.push(0);
            tree.extend_from_slice(name.as_bytes());
            tree.push(0);
            tree.extend_from_slice(&0u32.to_le_bytes()); // crc32 (unchecked by Vpk::read)
            tree.extend_from_slice(&0u16.to_le_bytes()); // preload_len
            tree.extend_from_slice(&0x7FFFu16.to_le_bytes()); // archive index: stored inline
            tree.extend_from_slice(&0u32.to_le_bytes()); // offset into dir data
            tree.extend_from_slice(&(data.len() as u32).to_le_bytes());
            tree.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entry terminator
            tree.push(0); // end of file-name loop
            tree.push(0); // end of dir loop
        }
        tree.push(0); // end of extension loop

        let mut out = Vec::new();
        out.extend_from_slice(&0x55AA_1234u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
        out.extend_from_slice(&tree);
        if let Some((_, data)) = entry {
            out.extend_from_slice(data);
        }
        std::fs::write(path, &out).expect("write synthetic vpk");
    }

    fn open_empty_sources(dir: &std::path::Path, entry: Option<(&str, &[u8])>) -> Sources {
        write_test_vpk(&dir.join("map.vpk"), entry);
        write_test_vpk(&dir.join("pak01_dir.vpk"), None);
        Sources::open(&dir.join("map.vpk"), dir).expect("open synthetic sources")
    }

    fn empty_ctx(sources: &Sources) -> Ctx<'_> {
        Ctx {
            sources,
            budget: TextureBudget::default(),
            builder: GltfBuilder::new(),
            models: HashMap::new(),
            raw_materials: HashMap::new(),
            resolved_materials: HashMap::new(),
            gltf_materials: HashMap::new(),
            textures: HashMap::new(),
            meshes: HashMap::new(),
            report: Report::default(),
        }
    }

    fn base_material(shader: &str) -> RawMaterial {
        RawMaterial {
            shader: shader.to_string(),
            ..Default::default()
        }
    }

    /// Pre-populates both material caches so `get_raw_material`/`get_resolved_material` never
    /// touch `ctx.sources`, keeping these tests fully offline past `Sources::open` itself.
    fn cache_material(ctx: &mut Ctx, path: &str, mat: RawMaterial) {
        let resolved = material::resolve(&mat);
        ctx.raw_materials
            .insert(path.to_string(), Some(Arc::new(mat)));
        ctx.resolved_materials
            .insert(path.to_string(), Arc::new(resolved));
    }

    /// A one-triangle mesh with a single draw call referencing `material_path` -- just a
    /// `POSITION` stream, matching what `build_geometry` needs when the material carries no
    /// normal/UV/blend requirement (true of every material `base_material` builds).
    fn triangle_mesh(material_path: &str) -> Mesh {
        let positions: Vec<u8> = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .iter()
            .flat_map(|p| p.iter().flat_map(|f| f.to_le_bytes()))
            .collect();
        let vertex_buffer = Buffer {
            element_count: 3,
            element_size: 12,
            fields: vec![InputLayoutField {
                semantic_name: "POSITION".to_string(),
                semantic_index: 0,
                format: DxgiFormat::R32G32B32Float,
                offset: 0,
            }],
            data: positions,
            compression: Compression::default(),
        };
        let index_buffer = Buffer {
            element_count: 3,
            element_size: 2,
            fields: vec![],
            data: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            compression: Compression::default(),
        };
        Mesh {
            vertex_buffers: vec![vertex_buffer],
            index_buffers: vec![index_buffer],
            scene_objects: vec![SceneObject {
                draw_calls: vec![DrawCall {
                    material_path: Some(material_path.to_string()),
                    is_triangle_list: true,
                    base_vertex: 0,
                    start_index: 0,
                    index_count: 3,
                    vertex_count: 3,
                    index_buffer: 0,
                    vertex_buffers: vec![0],
                    tint_color: None,
                    alpha: None,
                    flags: DrawCallFlags::None,
                }],
            }],
        }
    }

    fn glb_json(bytes: &[u8]) -> serde_json::Value {
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[20..20 + json_len]).unwrap()
    }

    /// §2/§11's synthetic coverage for `m_renderable`: no real map exercises this scene-object
    /// path (`world.rs`'s own doc comment on `SceneObjectRaw::renderable`), so this is the only
    /// test that a resource found but failing to decode as a mesh is reported, not swallowed.
    #[test]
    fn renderable_scene_object_reports_mesh_decode_failures() {
        let dir = temp_dir("renderable-decode-fail");
        let sources = open_empty_sources(
            &dir,
            Some(("models/broken.vmesh_c", &empty_resource_bytes())),
        );
        let mut ctx = empty_ctx(&sources);
        let mut children = Vec::new();
        let so = SceneObjectRaw {
            renderable_model: None,
            renderable: Some("models/broken.vmesh".to_string()),
            transform: world::IDENTITY_TRANSFORM,
            tint: [1.0, 1.0, 1.0, 1.0],
            is_overlay: false,
            overlay_render_order: 0,
            layer_index: None,
        };

        place_scene_object(&mut ctx, &mut children, &so, [1.0, 1.0]);

        assert!(children.is_empty());
        assert!(
            ctx.report
                .missing_resources
                .iter()
                .any(|m| m.contains("failed to decode mesh")),
            "{:?}",
            ctx.report.missing_resources
        );
    }

    /// §1/§11: the shared per-draw-call loop resolves the geometry-offset `overlay` flag as
    /// `object_overlay || material.material_says_overlay`, but `overlay_order` only ever threads
    /// into `extras.overlayOrder` for an object-level overlay (`m_nOverlayRenderOrder` is a
    /// scene-object property, not a material one) -- on Mirage most overlay-material draws come
    /// from ordinary (non-overlay-object) placements and must not get a stray `overlayOrder`.
    #[test]
    fn place_mesh_draw_calls_overlay_order_requires_the_object_flag_not_just_the_material() {
        let dir = temp_dir("overlay-or");
        let sources = open_empty_sources(&dir, None);
        let mut ctx = empty_ctx(&sources);
        cache_material(
            &mut ctx,
            "materials/plain.vmat",
            base_material("csgo_vertexlitgeneric"),
        );
        cache_material(
            &mut ctx,
            "materials/overlay.vmat",
            base_material("csgo_static_overlay"),
        );

        let mut children = Vec::new();
        // Neither the object nor the material says overlay: no overlayOrder even though one is
        // supplied.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "a",
            &triangle_mesh("materials/plain.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            false,
            Some(7),
        );
        // Material alone says overlay (csgo_static_overlay's shader): gets the vertex offset, but
        // not `overlayOrder` -- that field belongs to an overlay *object*, not a material.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "b",
            &triangle_mesh("materials/overlay.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            false,
            Some(7),
        );
        // Object flag on: gets both the offset and overlayOrder, even on a non-overlay material.
        place_mesh_draw_calls(
            &mut ctx,
            &mut children,
            "c",
            &triangle_mesh("materials/plain.vmat"),
            None,
            &world::IDENTITY_TRANSFORM,
            [1.0, 1.0, 1.0, 1.0],
            None,
            [1.0, 1.0],
            true,
            Some(9),
        );

        assert_eq!(children.len(), 3);
        let glb = ctx
            .builder
            .finish(children.clone(), Vec::new(), json!({}))
            .expect("small doc");
        let doc = glb_json(&glb);
        let nodes = doc["nodes"].as_array().unwrap();
        assert!(nodes[0].get("extras").is_none(), "{:?}", nodes[0]);
        assert!(nodes[1].get("extras").is_none(), "{:?}", nodes[1]);
        assert_eq!(nodes[2]["extras"]["overlayOrder"], 9);
    }
}
