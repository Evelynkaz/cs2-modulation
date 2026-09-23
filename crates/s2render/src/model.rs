//! A model (`vmdl_c`): embedded meshes (both `embedded_meshes` KV dialects -- old
//! `data_block`/`vbib_block`, new `m_nMeshIndex`/`m_nDataBlock` with the VBIB inline in the KV),
//! external mesh references, LOD structure and material groups (`Model.cs:217-347`,
//! `ModelLodInfo.cs`).

use s2fmt::kv3::Value;
use s2fmt::resource::{Block, FourCC, Resource, ResourceError};

use crate::buffer;
use crate::error::MeshError;
use crate::mesh::{self, Mesh};

/// This model's level-of-detail structure (`ModelLodInfo.cs:13-87`): which meshes belong to
/// which LOD level, and the per-level switch values.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LodInfo {
    mesh_lod_masks: Vec<i64>,
    /// Index N is the screen-size metric at which LOD level N becomes active.
    pub switch_distances: Vec<f32>,
    /// Bitwise OR of every mesh's LOD mask.
    pub combined_mask: i64,
    /// The lowest LOD level that actually contains meshes (0 unless LOD0 is left empty); a lower
    /// level is higher detail (`ModelLodInfo.cs:28-32`).
    pub lowest_level: i64,
    /// Sorted distinct LOD levels present across all meshes.
    pub available_levels: Vec<i64>,
}

impl LodInfo {
    fn from_kv3(root: &Value) -> LodInfo {
        let mesh_lod_masks: Vec<i64> = root
            .get("m_refLODGroupMasks")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let switch_distances: Vec<f32> = root
            .get("m_lodGroupSwitchDistances")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_f32).collect())
            .unwrap_or_default();

        let combined_mask = mesh_lod_masks.iter().fold(0i64, |acc, &m| acc | m);
        let lowest_level = if combined_mask == 0 {
            0
        } else {
            i64::from(combined_mask.trailing_zeros())
        };
        let available_levels = (0..64)
            .filter(|level| combined_mask & (1i64 << level) != 0)
            .collect();

        LodInfo {
            mesh_lod_masks,
            switch_distances,
            combined_mask,
            lowest_level,
            available_levels,
        }
    }

    /// The LOD mask of a mesh (bit N set => present in level N), zero when the mesh has no entry
    /// (`ModelLodInfo.cs:93-94`).
    pub fn mesh_mask(&self, mesh_index: usize) -> i64 {
        self.mesh_lod_masks.get(mesh_index).copied().unwrap_or(0)
    }

    /// Whether the mesh at `mesh_index` is present in LOD `level`; a mesh with no mask entry is
    /// always present (`ModelLodInfo.cs:100-101`).
    pub fn is_mesh_in_level(&self, mesh_index: usize, level: i64) -> bool {
        mesh_index >= self.mesh_lod_masks.len()
            || (self.mesh_lod_masks[mesh_index] & (1i64 << level)) != 0
    }
}

/// An external mesh reference (`m_refMeshes`; `Model.cs:217-239`). Empty slots (filled by an
/// embedded mesh instead) are left out.
#[derive(Debug, Clone, PartialEq)]
pub struct RefMesh {
    pub mesh_index: usize,
    /// `.vmesh` path, without the trailing `_c` (caller resolves and loads it via VPK).
    pub mesh_path: String,
    pub lod_mask: i64,
}

/// A mesh embedded directly in the model's own resource (`Model.cs:266-347`).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedMesh {
    pub name: String,
    pub mesh_index: usize,
    pub lod_mask: i64,
    pub mesh: Mesh,
}

/// One `m_materialGroups[]` entry: a skin (`Model.cs:564-566`).
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialGroup {
    pub name: String,
    pub materials: Vec<String>,
}

/// A fully decoded model.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Model {
    pub embedded_meshes: Vec<EmbeddedMesh>,
    pub ref_meshes: Vec<RefMesh>,
    pub lod: LodInfo,
    pub material_groups: Vec<MaterialGroup>,
}

fn wrap_resource_error(path: &str, source: ResourceError) -> MeshError {
    MeshError::Resource {
        path: path.to_string(),
        source: Box::new(source),
    }
}

/// Resolves a CTRL/embedded-mesh block index through the size-0-filtered block list
/// (`docs/FORMATS.md` section 2.2 "(!)"; the same scheme `s2fmt::resource::Resource::embedded_phys`
/// uses for `phys_data_block`).
fn resolve_block<'a>(
    resource: &'a Resource,
    index: i64,
    path: &str,
) -> Result<&'a Block, MeshError> {
    usize::try_from(index)
        .ok()
        .and_then(|i| resource.block_by_filtered_index(i))
        .ok_or_else(|| MeshError::BlockNotFound {
            path: path.to_string(),
            index,
        })
}

fn scene_objects_of(
    resource: &Resource,
    block: &Block,
    path: &str,
) -> Result<Vec<mesh::SceneObject>, MeshError> {
    let doc = resource
        .kv3(block)
        .map_err(|source| wrap_resource_error(path, source))?;
    mesh::decode_scene_objects(&doc.root, path)
}

/// Parses one `embedded_meshes[]` entry, in either KV dialect (`Model.cs:266-347`; change item 4
/// in `s6f3a1_mesh.md`).
fn parse_embedded_mesh(
    resource: &Resource,
    entry: &Value,
    lod: &LodInfo,
    path: &str,
) -> Result<EmbeddedMesh, MeshError> {
    if let Some(vbib_block_val) = entry.get("vbib_block") {
        // Old dialect: buffers live in a separate binary VBIB block.
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mesh_index = entry
            .get("mesh_index")
            .and_then(Value::as_i64)
            .ok_or_else(|| MeshError::Missing {
                path: format!("{path}.mesh_index"),
            })?;
        let data_block_index =
            entry
                .get("data_block")
                .and_then(Value::as_i64)
                .ok_or_else(|| MeshError::Missing {
                    path: format!("{path}.data_block"),
                })?;
        let vbib_block_index = vbib_block_val
            .as_i64()
            .ok_or_else(|| MeshError::WrongType {
                path: format!("{path}.vbib_block"),
                expected: "integer",
            })?;

        let data_block = resolve_block(resource, data_block_index, &format!("{path}.data_block"))?;
        let scene_objects = scene_objects_of(resource, data_block, path)?;

        let vbib_block = resolve_block(resource, vbib_block_index, &format!("{path}.vbib_block"))?;
        let (vertex_buffers, index_buffers) =
            buffer::parse_vbib_block(resource.block_bytes(vbib_block))?;

        let mesh_index = usize::try_from(mesh_index).unwrap_or(0);
        Ok(EmbeddedMesh {
            name,
            mesh_index,
            lod_mask: lod.mesh_mask(mesh_index),
            mesh: Mesh {
                vertex_buffers,
                index_buffers,
                scene_objects,
            },
        })
    } else {
        // New (MVTX/MIDX) dialect: the entry itself carries m_vertexBuffers/m_indexBuffers.
        let name = entry
            .get("m_Name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mesh_index = entry
            .get("m_nMeshIndex")
            .and_then(Value::as_i64)
            .ok_or_else(|| MeshError::Missing {
                path: format!("{path}.m_nMeshIndex"),
            })?;
        let data_block_index = entry
            .get("m_nDataBlock")
            .and_then(Value::as_i64)
            .ok_or_else(|| MeshError::Missing {
                path: format!("{path}.m_nDataBlock"),
            })?;

        let data_block =
            resolve_block(resource, data_block_index, &format!("{path}.m_nDataBlock"))?;
        let scene_objects = scene_objects_of(resource, data_block, path)?;

        let (vertex_buffers, index_buffers) = buffer::parse_kv3_buffers(resource, entry, path)?;

        let mesh_index = usize::try_from(mesh_index).unwrap_or(0);
        Ok(EmbeddedMesh {
            name,
            mesh_index,
            lod_mask: lod.mesh_mask(mesh_index),
            mesh: Mesh {
                vertex_buffers,
                index_buffers,
                scene_objects,
            },
        })
    }
}

/// Decodes a `vmdl_c` resource's `DATA` root plus (if present) its `CTRL` block's
/// `embedded_meshes` (`Model.cs:217-347`).
pub fn decode_model(resource: &Resource) -> Result<Model, MeshError> {
    let path = "DATA";
    let doc = resource
        .data_kv3()
        .map_err(|source| wrap_resource_error(path, source))?;
    let root = &doc.root;

    let lod = LodInfo::from_kv3(root);

    let ref_meshes = root
        .get("m_refMeshes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, v)| {
                    let mesh_path = v.as_str()?;
                    if mesh_path.is_empty() {
                        return None;
                    }
                    Some(RefMesh {
                        mesh_index: i,
                        mesh_path: mesh_path.to_string(),
                        lod_mask: lod.mesh_mask(i),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let material_groups = root
        .get("m_materialGroups")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|g| MaterialGroup {
                    name: g
                        .get("m_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    materials: g
                        .get("m_materials")
                        .and_then(Value::as_array)
                        .map(|m| {
                            m.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    let embedded_meshes = match resource.block(FourCC::CTRL) {
        Some(ctrl_block) => {
            let ctrl_doc = resource
                .kv3(ctrl_block)
                .map_err(|source| wrap_resource_error("CTRL", source))?;
            match ctrl_doc
                .root
                .get("embedded_meshes")
                .and_then(Value::as_array)
            {
                Some(arr) => arr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        parse_embedded_mesh(
                            resource,
                            e,
                            &lod,
                            &format!("CTRL.embedded_meshes[{i}]"),
                        )
                    })
                    .collect::<Result<_, _>>()?,
                None => Vec::new(),
            }
        }
        None => Vec::new(),
    };

    Ok(Model {
        embedded_meshes,
        ref_meshes,
        lod,
        material_groups,
    })
}
