//! CS2 map extraction policy: merges world physics, solid brush entities,
//! static props and breakable glass into one collision mesh and manages the
//! per-game-version cache.

pub mod build;
pub mod cache;
pub mod game;
pub mod lookup;
pub mod mapdata;
mod mesh_build;
pub mod policy;
pub mod report;

pub use build::{EXTRACTOR_VERSION, ExtractOptions, Extraction, extract_map};
pub use game::GameInstall;
pub use lookup::{ModelLookup, ModelPhys, PhysSource};
pub use mapdata::{
    MapBundle, STANDSPOTS_VERSION, Spawns, StandSpotFile, StandSpotJson, StandSpotsState,
    load_bundle, load_nav_areas, load_spawns, load_stand_spots, save_stand_spots,
};
pub use report::{EntityRecord, ExtractMeta, ExtractReport, NavAreasDump};

use std::path::PathBuf;

/// An error extracting a map. Every variant carries enough context (map name plus the resource
/// path involved) to point at the offending file. Every `source` is boxed: the domain error
/// types this wraps (`VpkError`, `ResourceError`, ...) are themselves large (several path/string
/// fields), which would otherwise make every `Result<_, ExtractError>` needlessly big
/// (`clippy::result_large_err`).
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{map}: failed to open VPK {path}: {source}")]
    Vpk {
        map: String,
        path: PathBuf,
        #[source]
        source: Box<s2fmt::vpk::VpkError>,
    },
    #[error("{map}: failed to parse resource {path}: {source}")]
    Resource {
        map: String,
        path: String,
        #[source]
        source: Box<s2fmt::resource::ResourceError>,
    },
    #[error("{map}: failed to decode PHYS in {path}: {source}")]
    Phys {
        map: String,
        path: String,
        #[source]
        source: Box<s2fmt::phys::PhysError>,
    },
    #[error("{map}: failed to decode entity lump {path}: {source}")]
    Entity {
        map: String,
        path: String,
        #[source]
        source: Box<s2fmt::entities::EntityError>,
    },
    #[error("{map}: failed to parse nav file {path}: {source}")]
    Nav {
        map: String,
        path: String,
        #[source]
        source: Box<s2fmt::nav::NavError>,
    },
    #[error("{map}: KV3 error in {path}: {source}")]
    Kv3 {
        map: String,
        path: String,
        #[source]
        source: Box<s2fmt::kv3::Kv3Error>,
    },
    #[error("{map}: mesh error building {context}: {source}")]
    Mesh {
        map: String,
        context: String,
        #[source]
        source: Box<geom::mesh::MeshError>,
    },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Other(String),
}
