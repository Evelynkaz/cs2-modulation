//! Resource lookup across the map VPK, `csgo/pak01_dir.vpk` and `core/pak01_dir.vpk`, in that
//! order (§1: "Поиск ресурсов по порядку"). `core` carries a handful of instrument materials
//! (`toolsblocklight`/`toolssolidblocklight`, `REPORT.md` §5 item 6) that aren't in `csgo`'s own
//! pak01.

use std::path::{Path, PathBuf};

use s2fmt::resource::{Resource, ResourceError};
use s2fmt::vpk::{Vpk, VpkError};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("failed to open VPK {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: VpkError,
    },
    #[error("{path} not found in any source VPK")]
    NotFound { path: String },
    #[error("failed to read {path} from VPK: {source}")]
    Read {
        path: String,
        #[source]
        source: VpkError,
    },
    #[error("failed to parse {path} as a resource: {source}")]
    Parse {
        path: String,
        #[source]
        source: ResourceError,
    },
}

/// The map VPK, `csgo/pak01_dir.vpk`, and (if present) `core/pak01_dir.vpk`, searched in that
/// order for every resource lookup.
pub struct Sources {
    vpks: Vec<Vpk>,
}

impl Sources {
    /// Opens the map VPK and `csgo_dir/pak01_dir.vpk`. `csgo_dir`'s sibling `core/pak01_dir.vpk`
    /// is opened too if it exists; a CS2 install without one (unusual, but not fatal to the rest
    /// of the export) is not an error.
    pub fn open(map_vpk_path: &Path, csgo_dir: &Path) -> Result<Sources, SourceError> {
        let mut vpks = Vec::with_capacity(3);
        vpks.push(Vpk::open(map_vpk_path).map_err(|source| SourceError::Open {
            path: map_vpk_path.to_path_buf(),
            source,
        })?);

        let csgo_pak01 = csgo_dir.join("pak01_dir.vpk");
        vpks.push(Vpk::open(&csgo_pak01).map_err(|source| SourceError::Open {
            path: csgo_pak01,
            source,
        })?);

        if let Some(game_dir) = csgo_dir.parent() {
            let core_pak01 = game_dir.join("core").join("pak01_dir.vpk");
            if core_pak01.is_file() {
                vpks.push(Vpk::open(&core_pak01).map_err(|source| SourceError::Open {
                    path: core_pak01,
                    source,
                })?);
            }
        }

        Ok(Sources { vpks })
    }

    /// Reads raw bytes for `path` (already `_c`-suffixed), or `None` if it isn't in any source
    /// (a missing resource is reported by the caller, not an error here -- §1).
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        for vpk in &self.vpks {
            if let Some(entry) = vpk.find(path) {
                return vpk.read(entry).ok();
            }
        }
        None
    }

    /// [`Sources::read`], parsed as a [`Resource`].
    pub fn resource(&self, path: &str) -> Result<Resource, SourceError> {
        for vpk in &self.vpks {
            if let Some(entry) = vpk.find(path) {
                let bytes = vpk.read(entry).map_err(|source| SourceError::Read {
                    path: path.to_string(),
                    source,
                })?;
                return Resource::parse(bytes).map_err(|source| SourceError::Parse {
                    path: path.to_string(),
                    source,
                });
            }
        }
        Err(SourceError::NotFound {
            path: path.to_string(),
        })
    }

    /// Finds the first entry (across every source, map VPK first) whose path ends with `suffix`
    /// (e.g. `"world.vwrld_c"`, which sits under `maps/<map>/` rather than at the tree root --
    /// `extract::build`'s own `find_entry_ending_with`), and returns its path and parsed resource.
    pub fn resource_ending_with(&self, suffix: &str) -> Result<(String, Resource), SourceError> {
        for vpk in &self.vpks {
            if let Some(entry) = vpk
                .entries()
                .find(|e| e.path.to_ascii_lowercase().ends_with(suffix))
            {
                let path = entry.path.clone();
                let bytes = vpk.read(entry).map_err(|source| SourceError::Read {
                    path: path.clone(),
                    source,
                })?;
                let resource = Resource::parse(bytes).map_err(|source| SourceError::Parse {
                    path: path.clone(),
                    source,
                })?;
                return Ok((path, resource));
            }
        }
        Err(SourceError::NotFound {
            path: format!("*{suffix}"),
        })
    }

    /// Every entry path in the map VPK (the first source) with the given extension (no leading
    /// dot), lowercased for comparison -- used for the entity-lump fallback (§3's own reference,
    /// `extract::build::resolve_entity_lumps`'s "every `*.vents_c` in the map VPK").
    pub fn entries_with_extension(&self, ext: &str) -> Vec<String> {
        let Some(map_vpk) = self.vpks.first() else {
            return Vec::new();
        };
        map_vpk
            .entries()
            .filter(|e| e.extension().eq_ignore_ascii_case(ext))
            .map(|e| e.path.clone())
            .collect()
    }
}

impl SourceError {
    pub fn from_resource_err(source: ResourceError) -> SourceError {
        SourceError::Parse {
            path: String::new(),
            source,
        }
    }
}

/// Appends a trailing `_c` if not already present (world/entity paths sometimes carry it,
/// sometimes don't, depending on which KV3 field they came from).
pub fn compiled_path(path: &str) -> String {
    let stripped = path.strip_suffix("_c").unwrap_or(path);
    format!("{stripped}_c")
}
