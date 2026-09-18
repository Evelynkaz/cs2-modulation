//! CS2 navigation mesh (`.nav`) reader. Reference: `docs/FORMATS.md` section 9 and
//! `ValveResourceFormat/NavMesh/NavMesh*.cs` (VRF), which this mirrors field-for-field.
//!
//! `.nav` is not a resource container: it's a standalone file with its own magic number, read
//! sequentially with no block table. Layout, in order (VRF `NavMeshFile.cs:107-174 Read`):
//! magic, version, sub-version, an analyzed flag, an optional v36 KV3 block, a shared corner
//! table + polygons (v31+), an optional zero u32 (v32+), movable mesh ids (v35+), another
//! optional v36 KV3 block, areas, ladders, transformed bounds, generation parameters, a third
//! optional v36 KV3 block, and finally custom data (a KV3 block, only when sub-version > 0).

use crate::kv3;
use crate::util::{ReadError, Reader};

/// Named bits of [`NavArea::attribute_flags`], from `NavAttributeFlags.cs`. These names apply to
/// nav version 35 and newer; older versions store a different (unmapped) bit layout. Bits 19 and
/// above are game-specific.
pub mod flags {
    pub const JUMP: u64 = 0x2;
    pub const NO_JUMP: u64 = 0x8;
    pub const STOP: u64 = 0x10;
    pub const RUN: u64 = 0x20;
    pub const WALK: u64 = 0x40;
    pub const AVOID: u64 = 0x80;
    pub const TRANSIENT: u64 = 0x100;
    pub const DONT_HIDE: u64 = 0x200;
    pub const STAND: u64 = 0x400;
    pub const NO_HOSTAGES: u64 = 0x800;
    pub const STAIRS: u64 = 0x1000;
    pub const NO_MERGE: u64 = 0x2000;
    pub const OBSTACLE_TOP: u64 = 0x4000;
    pub const NON_Z_UP: u64 = 0x8000;
    pub const CROUCH_HEIGHT: u64 = 0x10000;
    pub const NON_Z_UP_TRANSITION: u64 = 0x20000;
    pub const CRAWL_HEIGHT: u64 = 0x40000;
}

/// Magic number for `.nav` files (`NavMeshFile.cs:18`).
pub const MAGIC: u32 = 0xFEED_FACE;
/// The [`NavArea::movable_mesh_id`] sentinel meaning "belongs to the static world"
/// (`NavMeshFile.cs:49`).
pub const NO_MOVABLE_MESH: u32 = 0xFFFF_FFFF;

/// An error parsing a `.nav` file.
#[derive(Debug, thiserror::Error)]
pub enum NavError {
    #[error("not a .nav file: bad magic {magic:#010x} (expected {MAGIC:#010x})")]
    BadMagic { magic: u32 },
    #[error("unsupported .nav version {version} (s2fmt supports 30..=36)")]
    UnsupportedVersion { version: u32 },
    #[error("truncated .nav data at {context}: {source}")]
    Truncated {
        context: &'static str,
        #[source]
        source: ReadError,
    },
    #[error("polygon index {index} out of range (have {count} polygons)")]
    BadPolygonIndex { index: u32, count: usize },
    #[error("negative hull count {count} in generation params")]
    BadHullCount { count: i32 },
    #[error("{count} trailing byte(s) after the last .nav section")]
    TrailingData { count: usize },
    #[error("embedded KV3 block at file offset {offset}: {source}")]
    Kv3 {
        offset: usize,
        #[source]
        source: kv3::Kv3Error,
    },
    #[error("embedded KV3 block at file offset {offset} has no determinable length")]
    Kv3Length { offset: usize },
}

fn trunc(context: &'static str, source: ReadError) -> NavError {
    NavError::Truncated { context, source }
}

/// A navigation mesh, parsed from a `.nav` file (`NavMeshFile.cs`).
#[derive(Debug, Clone, PartialEq)]
pub struct NavMesh {
    pub version: u32,
    pub sub_version: u32,
    pub is_analyzed: bool,
    pub areas: Vec<NavArea>,
    pub ladders: Vec<NavLadder>,
    /// Only stored in version 35 and newer.
    pub movable_mesh_ids: Vec<String>,
    pub transformed_bounds: Vec<TransformedBounds>,
    pub generation_params: Option<GenerationParams>,
    pub custom_data: Option<kv3::Document>,
    /// The unnamed KV3 blocks stored in v36+ files (`NavMeshFile.cs` `KV3Unknown1/2/3`), in file
    /// order. Empty for files older than v36.
    pub unknown_kv3: Vec<kv3::Document>,
    /// Area id -> index into `areas`, for [`NavMesh::area`]. Not part of the public field list:
    /// VRF's `Areas` is a `Dictionary<uint, NavMeshArea>` (`NavMeshFile.cs:33`) built the same way
    /// (`AddArea`, `NavMeshFile.cs:290-302`), where a duplicate id simply overwrites the earlier
    /// entry; mirrored here by inserting in file order so the last duplicate wins.
    id_index: std::collections::HashMap<u32, usize>,
}

impl NavMesh {
    /// All areas with the given hull index, in file order (`NavMeshFile.cs` `GetHullAreas`).
    pub fn hull_areas(&self, hull: u8) -> impl Iterator<Item = &NavArea> {
        self.areas.iter().filter(move |a| a.hull_index == hull)
    }

    /// The area with the given id, if any (`NavMeshFile.cs` `GetArea`). If the file has more than
    /// one area with the same id, the last one in file order wins (matching VRF's dictionary).
    pub fn area(&self, id: u32) -> Option<&NavArea> {
        self.id_index.get(&id).map(|&i| &self.areas[i])
    }
}

/// A navigation mesh area (`NavMeshArea.cs`).
#[derive(Debug, Clone, PartialEq)]
pub struct NavArea {
    pub id: u32,
    /// Bits from [`flags`] (named bits apply to version 35+ only).
    pub attribute_flags: u64,
    pub hull_index: u8,
    pub corners: Vec<[f32; 3]>,
    /// `None` when this area belongs to the static world (file value [`NO_MOVABLE_MESH`]).
    pub movable_mesh_id: Option<u32>,
    /// One connection list per corner (i.e. per edge starting at that corner).
    pub connections: Vec<Vec<NavConnection>>,
    pub ladders_above: Vec<u32>,
    pub ladders_below: Vec<u32>,
    /// The float read right after the corners; VRF's comment says "almost always 0"
    /// (`NavMeshArea.cs:103`), meaning unknown.
    pub unknown_f32: f32,
}

/// A connection from one area's edge to another area (`NavMeshConnection.cs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavConnection {
    pub area_id: u32,
    pub edge_id: u32,
}

/// Cardinal direction of a ladder (`NavDirectionType.cs`). `.nav` stores this as a raw `u32`
/// with no validation, so an out-of-range value is kept rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    North,
    East,
    South,
    West,
    Other(u32),
}

impl NavDirection {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => NavDirection::North,
            1 => NavDirection::East,
            2 => NavDirection::South,
            3 => NavDirection::West,
            other => NavDirection::Other(other),
        }
    }
}

/// A ladder (`NavMeshLadder.cs`). The `*_area` fields are raw area ids as stored in the file
/// (resolve with [`NavMesh::area`]); `bottom_left_area`/`bottom_right_area` are `None` when the
/// file version predates them (< 35) rather than when the id fails to resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct NavLadder {
    pub id: u32,
    pub width: f32,
    pub length: f32,
    pub top: [f32; 3],
    pub bottom: [f32; 3],
    pub direction: NavDirection,
    pub top_forward_area: u32,
    pub top_left_area: u32,
    pub top_right_area: u32,
    pub top_behind_area: u32,
    pub bottom_area: u32,
    pub bottom_left_area: Option<u32>,
    pub bottom_right_area: Option<u32>,
}

/// A local-space bounding box together with a world-space transform
/// (`NavMeshTransformedBounds.cs`). `transform` is the raw row-major 3x4 matrix as stored (three
/// rotation rows, each followed by a translation component); what these are used for is unknown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformedBounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub transform: [[f32; 4]; 3],
}

/// Per-hull navigation agent generation parameters (`NavMeshGenerationHullParams.cs`). Fields
/// gated on `NavGenVersion` keep their C# default (`false`/`0`/`true` for `enabled`) when the
/// generation version is too old to store them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenerationHullParams {
    pub enabled: bool,
    pub radius: f32,
    pub height: f32,
    pub short_height_enabled: bool,
    pub short_height: f32,
    pub agent_crawl_enabled: bool,
    pub agent_crawl_height: f32,
    pub max_climb: f32,
    pub max_slope: i32,
    pub max_jump_down_dist: f32,
    pub max_jump_horiz_dist_base: f32,
    pub max_jump_up_dist: f32,
    pub border_erosion: i32,
}

impl Default for GenerationHullParams {
    fn default() -> Self {
        GenerationHullParams {
            enabled: true,
            radius: 0.0,
            height: 0.0,
            short_height_enabled: false,
            short_height: 0.0,
            agent_crawl_enabled: false,
            agent_crawl_height: 0.0,
            max_climb: 0.0,
            max_slope: 0,
            max_jump_down_dist: 0.0,
            max_jump_horiz_dist_base: 0.0,
            max_jump_up_dist: 0.0,
            border_erosion: 0,
        }
    }
}

/// Navigation mesh generation parameters (`NavMeshGenerationParams.cs`). Fields gated on
/// `nav_gen_version` keep a zero/empty default when the generation version is too old to store
/// them.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationParams {
    pub nav_gen_version: i32,
    pub use_project_defaults: bool,
    pub tile_size: f32,
    pub cell_size: f32,
    pub cell_height: f32,
    pub min_region_size: i32,
    pub merged_region_size: i32,
    pub mesh_sample_distance: f32,
    pub max_sample_error: f32,
    pub max_edge_length: i32,
    pub max_edge_error: f32,
    pub verts_per_poly: i32,
    /// `nav_gen_version >= 7` only.
    pub small_area_on_edge_removal: f32,
    /// `nav_gen_version >= 12` only.
    pub hull_preset_name: Option<String>,
    /// `nav_gen_version >= 12` only.
    pub hull_definitions_file: Option<String>,
    pub hulls: Vec<GenerationHullParams>,
    /// `nav_gen_version >= 12` only.
    pub gravity_follows_rotation: bool,
}

/// One decoded polygon from the shared corner table (`NavMeshPolygon.cs`); areas of version 31+
/// reference these by index instead of storing their own corners inline.
struct Polygon {
    corners: Vec<[f32; 3]>,
    movable_mesh_id: u32,
}

/// Reads `bytes` as `f32` (little-endian). [`Reader::f32`] is test-only, so `.nav` parsing
/// (which needs `f32` outside tests, unlike the rest of the crate so far) reads the bytes itself.
fn read_f32(r: &mut Reader, context: &'static str) -> Result<f32, NavError> {
    let bytes = r.bytes(4).map_err(|e| trunc(context, e))?;
    Ok(f32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_vec3(r: &mut Reader, context: &'static str) -> Result<[f32; 3], NavError> {
    Ok([
        read_f32(r, context)?,
        read_f32(r, context)?,
        read_f32(r, context)?,
    ])
}

/// Advances `r` to the next multiple of 8, relative to the start of the file
/// (`NavMeshFile.cs:179`).
fn align8(r: &mut Reader) -> Result<(), NavError> {
    let pos = r.pos();
    let aligned = (pos + 7) & !7usize;
    if aligned > pos {
        r.bytes(aligned - pos)
            .map_err(|e| trunc("kv3 alignment", e))?;
    }
    Ok(())
}

/// Reads one embedded binary KV3 document, 8-byte aligned (`NavMeshFile.cs:176-188 ReadKV3`).
/// `bytes` is the whole file, so `r`'s position can be used directly as a file offset to slice
/// from -- avoiding needing to know the document's length before parsing it. Parses with
/// [`kv3::parse_binary_prefix`] (a single pass, unlike calling [`kv3::binary_document_len`] and
/// [`kv3::parse_binary`] separately, which would parse the document twice); v0/legacy documents
/// aren't given a trustworthy length by `parse_binary_prefix` (see its doc comment), so those are
/// rejected here as [`NavError::Kv3Length`] rather than risk desyncing the rest of the file.
fn read_kv3(r: &mut Reader, bytes: &[u8]) -> Result<kv3::Document, NavError> {
    align8(r)?;
    let start = r.pos();
    let rest = &bytes[start..];
    let (doc, len) = kv3::parse_binary_prefix(rest).map_err(|source| NavError::Kv3 {
        offset: start,
        source,
    })?;
    if matches!(doc.version, kv3::Kv3Version::Binary(0)) {
        return Err(NavError::Kv3Length { offset: start });
    }
    r.set_pos(start + len);
    Ok(doc)
}

/// Caps a file-supplied element count to a sane `Vec::with_capacity` argument: an untrusted
/// count read from `bytes` must not be used to request a huge allocation, but is safe up to the
/// number of bytes actually remaining (every element takes at least one byte).
fn cap(count: u32, r: &Reader) -> usize {
    (count as usize).min(r.remaining())
}

fn read_polygon_versioned(
    r: &mut Reader,
    corners: &[[f32; 3]],
    has_movable_mesh_id: bool,
) -> Result<Polygon, NavError> {
    let corner_count = r.u8().map_err(|e| trunc("polygon corner count", e))?;
    let mut polygon_corners = Vec::with_capacity(corner_count as usize);
    for _ in 0..corner_count {
        let index = r.u32().map_err(|e| trunc("polygon corner index", e))?;
        let corner = *corners
            .get(index as usize)
            .ok_or(NavError::BadPolygonIndex {
                index,
                count: corners.len(),
            })?;
        polygon_corners.push(corner);
    }

    let movable_mesh_id = if has_movable_mesh_id {
        r.u32().map_err(|e| trunc("polygon movable mesh id", e))?
    } else {
        NO_MOVABLE_MESH
    };

    Ok(Polygon {
        corners: polygon_corners,
        movable_mesh_id,
    })
}

fn read_connections(r: &mut Reader) -> Result<Vec<NavConnection>, NavError> {
    let count = r.u32().map_err(|e| trunc("connection count", e))?;
    let mut connections = Vec::with_capacity(cap(count, r));
    for _ in 0..count {
        let area_id = r.u32().map_err(|e| trunc("connection area id", e))?;
        let edge_id = r.u32().map_err(|e| trunc("connection edge id", e))?;
        connections.push(NavConnection { area_id, edge_id });
    }
    Ok(connections)
}

fn read_area(
    r: &mut Reader,
    version: u32,
    polygons: Option<&[Polygon]>,
) -> Result<NavArea, NavError> {
    let id = r.u32().map_err(|e| trunc("area id", e))?;
    let attribute_flags = r.u64().map_err(|e| trunc("area attribute flags", e))?;
    let hull_index = r.u8().map_err(|e| trunc("area hull index", e))?;

    let (corners, movable_mesh_id) = if version >= 31 {
        let polygons = polygons.expect("polygons are read for every version >= 31");
        let index = r.u32().map_err(|e| trunc("area polygon index", e))?;
        let polygon = polygons
            .get(index as usize)
            .ok_or(NavError::BadPolygonIndex {
                index,
                count: polygons.len(),
            })?;
        (polygon.corners.clone(), polygon.movable_mesh_id)
    } else {
        let corner_count = r.u32().map_err(|e| trunc("area corner count", e))?;
        let mut corners = Vec::with_capacity(cap(corner_count, r));
        for _ in 0..corner_count {
            corners.push(read_vec3(r, "area corner")?);
        }
        (corners, NO_MOVABLE_MESH)
    };

    let unknown_f32 = read_f32(r, "area unknown float")?;

    let mut connections = Vec::with_capacity(corners.len());
    for _ in 0..corners.len() {
        connections.push(read_connections(r)?);
    }

    // Probably legacy hiding spot / spot encounter data counts; always 0 in practice
    // (`NavMeshArea.cs:111-114`). Not enforced here: `.nav`'s own release build only
    // `Debug.Assert`s this, which is compiled out, so nothing is known to depend on it.
    let _legacy_hiding_spot_count = r.u8().map_err(|e| trunc("area unknown byte", e))?;
    let _legacy_spot_encounter_count = r.u32().map_err(|e| trunc("area unknown u32", e))?;

    let ladder_above_count = r.u32().map_err(|e| trunc("ladders above count", e))?;
    let mut ladders_above = Vec::with_capacity(cap(ladder_above_count, r));
    for _ in 0..ladder_above_count {
        ladders_above.push(r.u32().map_err(|e| trunc("ladder above id", e))?);
    }

    let ladder_below_count = r.u32().map_err(|e| trunc("ladders below count", e))?;
    let mut ladders_below = Vec::with_capacity(cap(ladder_below_count, r));
    for _ in 0..ladder_below_count {
        ladders_below.push(r.u32().map_err(|e| trunc("ladder below id", e))?);
    }

    Ok(NavArea {
        id,
        attribute_flags,
        hull_index,
        corners,
        movable_mesh_id: (movable_mesh_id != NO_MOVABLE_MESH).then_some(movable_mesh_id),
        connections,
        ladders_above,
        ladders_below,
        unknown_f32,
    })
}

fn read_areas(
    r: &mut Reader,
    version: u32,
    polygons: Option<&[Polygon]>,
) -> Result<Vec<NavArea>, NavError> {
    let count = r.u32().map_err(|e| trunc("area count", e))?;
    let mut areas = Vec::with_capacity(cap(count, r));
    for _ in 0..count {
        areas.push(read_area(r, version, polygons)?);
    }
    Ok(areas)
}

fn read_ladder(r: &mut Reader, version: u32) -> Result<NavLadder, NavError> {
    let id = r.u32().map_err(|e| trunc("ladder id", e))?;
    let width = read_f32(r, "ladder width")?;
    let top = read_vec3(r, "ladder top")?;
    let bottom = read_vec3(r, "ladder bottom")?;
    let length = read_f32(r, "ladder length")?;
    let direction = NavDirection::from_u32(r.u32().map_err(|e| trunc("ladder direction", e))?);

    let top_forward_area = r.u32().map_err(|e| trunc("ladder top forward area", e))?;
    let top_left_area = r.u32().map_err(|e| trunc("ladder top left area", e))?;
    let top_right_area = r.u32().map_err(|e| trunc("ladder top right area", e))?;
    let top_behind_area = r.u32().map_err(|e| trunc("ladder top behind area", e))?;
    let bottom_area = r.u32().map_err(|e| trunc("ladder bottom area", e))?;

    let (bottom_left_area, bottom_right_area) = if version >= 35 {
        (
            Some(r.u32().map_err(|e| trunc("ladder bottom left area", e))?),
            Some(r.u32().map_err(|e| trunc("ladder bottom right area", e))?),
        )
    } else {
        (None, None)
    };

    Ok(NavLadder {
        id,
        width,
        length,
        top,
        bottom,
        direction,
        top_forward_area,
        top_left_area,
        top_right_area,
        top_behind_area,
        bottom_area,
        bottom_left_area,
        bottom_right_area,
    })
}

fn read_ladders(r: &mut Reader, version: u32) -> Result<Vec<NavLadder>, NavError> {
    let count = r.u32().map_err(|e| trunc("ladder count", e))?;
    let mut ladders = Vec::with_capacity(cap(count, r));
    for _ in 0..count {
        ladders.push(read_ladder(r, version)?);
    }
    Ok(ladders)
}

fn read_transformed_bounds_one(r: &mut Reader) -> Result<TransformedBounds, NavError> {
    let min = read_vec3(r, "transformed bounds min")?;
    let max = read_vec3(r, "transformed bounds max")?;
    let mut transform = [[0.0f32; 4]; 3];
    for row in &mut transform {
        for cell in row.iter_mut() {
            *cell = read_f32(r, "transformed bounds transform")?;
        }
    }
    Ok(TransformedBounds {
        min,
        max,
        transform,
    })
}

fn read_transformed_bounds(r: &mut Reader) -> Result<Vec<TransformedBounds>, NavError> {
    let count = r.u32().map_err(|e| trunc("transformed bounds count", e))?;
    let mut bounds = Vec::with_capacity(cap(count, r));
    for _ in 0..count {
        bounds.push(read_transformed_bounds_one(r)?);
    }
    Ok(bounds)
}

fn read_hull_params(
    r: &mut Reader,
    nav_gen_version: i32,
) -> Result<GenerationHullParams, NavError> {
    let mut hull = GenerationHullParams::default();

    if nav_gen_version >= 9 {
        hull.enabled = r.u8().map_err(|e| trunc("hull enabled", e))? > 0;
    }

    hull.radius = read_f32(r, "hull radius")?;
    hull.height = read_f32(r, "hull height")?;

    if nav_gen_version >= 9 {
        hull.short_height_enabled = r.u8().map_err(|e| trunc("hull short height enabled", e))? > 0;
        hull.short_height = read_f32(r, "hull short height")?;
    }

    if nav_gen_version >= 13 {
        hull.agent_crawl_enabled = r.u8().map_err(|e| trunc("hull agent crawl enabled", e))? > 0;
        hull.agent_crawl_height = read_f32(r, "hull agent crawl height")?;
    }

    hull.max_climb = read_f32(r, "hull max climb")?;
    hull.max_slope = r.i32().map_err(|e| trunc("hull max slope", e))?;
    hull.max_jump_down_dist = read_f32(r, "hull max jump down dist")?;
    hull.max_jump_horiz_dist_base = read_f32(r, "hull max jump horiz dist base")?;
    hull.max_jump_up_dist = read_f32(r, "hull max jump up dist")?;

    if nav_gen_version >= 11 {
        hull.border_erosion = r.i32().map_err(|e| trunc("hull border erosion", e))?;
    }

    Ok(hull)
}

fn read_generation_params(r: &mut Reader) -> Result<GenerationParams, NavError> {
    let nav_gen_version = r.i32().map_err(|e| trunc("nav gen version", e))?;
    let use_project_defaults = r.u32().map_err(|e| trunc("use project defaults", e))? != 0;

    let tile_size = read_f32(r, "tile size")?;
    let cell_size = read_f32(r, "cell size")?;
    let cell_height = read_f32(r, "cell height")?;

    let min_region_size = r.i32().map_err(|e| trunc("min region size", e))?;
    let merged_region_size = r.i32().map_err(|e| trunc("merged region size", e))?;

    let mesh_sample_distance = read_f32(r, "mesh sample distance")?;
    let max_sample_error = read_f32(r, "max sample error")?;

    let max_edge_length = r.i32().map_err(|e| trunc("max edge length", e))?;
    let max_edge_error = read_f32(r, "max edge error")?;
    let verts_per_poly = r.i32().map_err(|e| trunc("verts per poly", e))?;

    let small_area_on_edge_removal = if nav_gen_version >= 7 {
        read_f32(r, "small area on edge removal")?
    } else {
        0.0
    };

    let (hull_preset_name, hull_definitions_file) = if nav_gen_version >= 12 {
        (
            Some(
                r.cstr()
                    .map_err(|e| trunc("hull preset name", e))?
                    .to_string(),
            ),
            Some(
                r.cstr()
                    .map_err(|e| trunc("hull definitions file", e))?
                    .to_string(),
            ),
        )
    } else {
        (None, None)
    };

    let hull_count = r.i32().map_err(|e| trunc("hull count", e))?;
    let hull_count_usize =
        usize::try_from(hull_count).map_err(|_| NavError::BadHullCount { count: hull_count })?;
    let mut hulls = Vec::with_capacity(hull_count_usize.min(r.remaining()));
    for _ in 0..hull_count_usize {
        hulls.push(read_hull_params(r, nav_gen_version)?);
    }

    // Version <= 11 always stores 3 hulls even if fewer are used; the extras are discarded
    // (`NavMeshGenerationParams.cs:150-158`).
    if nav_gen_version <= 11 {
        for _ in hull_count_usize..3 {
            read_hull_params(r, nav_gen_version)?;
        }
    }

    let gravity_follows_rotation = if nav_gen_version >= 12 {
        r.u8().map_err(|e| trunc("gravity follows rotation", e))? != 0
    } else {
        false
    };

    Ok(GenerationParams {
        nav_gen_version,
        use_project_defaults,
        tile_size,
        cell_size,
        cell_height,
        min_region_size,
        merged_region_size,
        mesh_sample_distance,
        max_sample_error,
        max_edge_length,
        max_edge_error,
        verts_per_poly,
        small_area_on_edge_removal,
        hull_preset_name,
        hull_definitions_file,
        hulls,
        gravity_follows_rotation,
    })
}

fn read_movable_mesh_ids(r: &mut Reader) -> Result<Vec<String>, NavError> {
    let count = r.u32().map_err(|e| trunc("movable mesh count", e))?;
    let mut ids = Vec::with_capacity(cap(count, r));
    for _ in 0..count {
        ids.push(
            r.cstr()
                .map_err(|e| trunc("movable mesh id", e))?
                .to_string(),
        );
        // The 48 bytes following the id are most likely a baked reference transform (a 3x4
        // matrix), but no known file has any entries here so the layout is unverified
        // (`NavMeshFile.cs:208-210`).
        r.bytes(48).map_err(|e| trunc("movable mesh reserved", e))?;
    }
    Ok(ids)
}

/// Parses a `.nav` file (versions 30..=36, `NavMeshFile.cs:107-174`).
pub fn parse_nav(bytes: &[u8]) -> Result<NavMesh, NavError> {
    let mut r = Reader::new(bytes);

    let magic = r.u32().map_err(|e| trunc("magic", e))?;
    if magic != MAGIC {
        return Err(NavError::BadMagic { magic });
    }

    let version = r.u32().map_err(|e| trunc("version", e))?;
    if !(30..=36).contains(&version) {
        return Err(NavError::UnsupportedVersion { version });
    }

    let sub_version = r.u32().map_err(|e| trunc("sub version", e))?;

    let unk1 = r.u32().map_err(|e| trunc("analyzed flag", e))?;
    let is_analyzed = unk1 & 1 != 0;

    let mut unknown_kv3 = Vec::new();
    if version >= 36 {
        unknown_kv3.push(read_kv3(&mut r, bytes)?);
    }

    let polygons = if version >= 31 {
        Some(read_polygons_versioned(&mut r, version)?)
    } else {
        None
    };

    if version >= 32 {
        // Always 0 in practice (`NavMeshFile.cs:141-145`); not enforced (see the area-read
        // comment above for why).
        let _unk2 = r.u32().map_err(|e| trunc("post-polygon reserved u32", e))?;
    }

    let movable_mesh_ids = if version >= 35 {
        read_movable_mesh_ids(&mut r)?
    } else {
        Vec::new()
    };

    if version >= 36 {
        unknown_kv3.push(read_kv3(&mut r, bytes)?);
    }

    let areas = read_areas(&mut r, version, polygons.as_deref())?;
    let ladders = read_ladders(&mut r, version)?;
    let transformed_bounds = read_transformed_bounds(&mut r)?;
    let generation_params = Some(read_generation_params(&mut r)?);

    if version >= 36 {
        unknown_kv3.push(read_kv3(&mut r, bytes)?);
    }

    let custom_data = if sub_version > 0 {
        Some(read_kv3(&mut r, bytes)?)
    } else {
        None
    };

    if r.remaining() != 0 {
        return Err(NavError::TrailingData {
            count: r.remaining(),
        });
    }

    let mut id_index = std::collections::HashMap::with_capacity(areas.len());
    for (i, area) in areas.iter().enumerate() {
        id_index.insert(area.id, i);
    }

    Ok(NavMesh {
        version,
        sub_version,
        is_analyzed,
        areas,
        ladders,
        movable_mesh_ids,
        transformed_bounds,
        generation_params,
        custom_data,
        unknown_kv3,
        id_index,
    })
}

/// Reads the shared corner table and polygon list (version 31+, `NavMeshFile.cs:248-264`); only
/// version 35+ polygons carry a movable mesh id (`NavMeshFile.cs:277-281`).
fn read_polygons_versioned(r: &mut Reader, version: u32) -> Result<Vec<Polygon>, NavError> {
    let corner_count = r.u32().map_err(|e| trunc("polygon corner count", e))?;
    let mut corners = Vec::with_capacity(cap(corner_count, r));
    for _ in 0..corner_count {
        corners.push(read_vec3(r, "polygon corner")?);
    }

    let polygon_count = r.u32().map_err(|e| trunc("polygon count", e))?;
    let mut polygons = Vec::with_capacity(cap(polygon_count, r));
    for _ in 0..polygon_count {
        polygons.push(read_polygon_versioned(r, &corners, version >= 35)?);
    }
    Ok(polygons)
}

#[cfg(test)]
mod tests;
