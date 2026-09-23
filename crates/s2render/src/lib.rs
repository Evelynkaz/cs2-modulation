//! CS2 render meshes and models: vertex/index buffers (`VBIB`/`MBUF`, and the KV3
//! `MVTX`/`MIDX` form), zstd + meshoptimizer decompression, typed attribute decoding
//! (positions, normals, UV, color), draw calls, and models (embedded/external meshes, LOD).
//! Pure parsing with no knowledge of CS2 gameplay -- mirrors `s2fmt`'s scope, one layer up.
//!
//! Ground truth: `VRF/Resource/Blocks/VBIB.cs`, `Resource/ResourceTypes/{Mesh,Model}.cs`,
//! `Resource/ResourceTypes/ModelData/ModelLodInfo.cs`, `IO/Gltf/GltfModelExporter.Mesh.cs`.

pub mod attributes;
pub mod buffer;
pub mod error;
pub mod format;
pub mod mesh;
pub mod model;
mod reader;

pub use attributes::{FloatAttribute, Normal};
pub use buffer::{Buffer, Compression, InputLayoutField};
pub use error::MeshError;
pub use format::DxgiFormat;
pub use mesh::{
    DrawCall, DrawCallFlags, Mesh, SceneObject, decode_mesh_resource, decode_scene_objects,
};
pub use model::{EmbeddedMesh, LodInfo, MaterialGroup, Model, RefMesh, decode_model};
