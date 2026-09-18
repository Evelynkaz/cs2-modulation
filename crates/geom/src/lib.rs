//! Collision geometry: triangle meshes with per-triangle collision
//! attributes, attribute filters, a versioned `.cgeo` file format, and OBJ
//! export.

pub mod bvh;
pub mod cgeo;
pub mod collider;
pub mod filter;
pub mod grid;
pub mod math;
pub mod mesh;
pub mod obj;
pub mod tri;
pub mod voxel;
