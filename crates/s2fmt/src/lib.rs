//! Source 2 file formats: VPK archives, the `*_c` resource container, KV3
//! (binary and text), physics aggregate data, entity lumps, world nodes and
//! CS2 `.nav` meshes. Pure parsing with no knowledge of CS2 gameplay.

pub mod compress;
pub mod entities;
pub mod hash;
pub mod kv3;
pub mod nav;
pub mod phys;
pub mod resource;
mod util;
pub mod vpk;
pub mod worldnode;
