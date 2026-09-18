//! CS2 install layout: where a map's VPK, the shared content archive, and the game build id
//! live on disk (`cs2-smoke-solver/MapExtractor.cs:143-166` for the shared-VPK search order).

use std::fs;
use std::path::PathBuf;

use crate::ExtractError;

/// A CS2 game install (`.../game/csgo`).
#[derive(Debug, Clone)]
pub struct GameInstall {
    pub csgo_dir: PathBuf,
}

impl GameInstall {
    /// Validates that `csgo_dir` looks like a CS2 `csgo` directory: it must contain `maps/` and
    /// `pak01_dir.vpk`.
    pub fn new(csgo_dir: impl Into<PathBuf>) -> Result<Self, ExtractError> {
        let csgo_dir = csgo_dir.into();
        if !csgo_dir.join("maps").is_dir() {
            return Err(ExtractError::Other(format!(
                "{} does not look like a CS2 game directory: no maps/ subdirectory",
                csgo_dir.display()
            )));
        }
        if !csgo_dir.join("pak01_dir.vpk").is_file() {
            return Err(ExtractError::Other(format!(
                "{} does not look like a CS2 game directory: no pak01_dir.vpk",
                csgo_dir.display()
            )));
        }
        Ok(GameInstall { csgo_dir })
    }

    /// `<csgo>/maps/<map>.vpk`.
    pub fn map_vpk(&self, map: &str) -> PathBuf {
        self.csgo_dir.join("maps").join(format!("{map}.vpk"))
    }

    /// The `ClientVersion` line of `steam.inf` (e.g. `"2000908"`).
    pub fn build_id(&self) -> Result<String, ExtractError> {
        let path = self.csgo_dir.join("steam.inf");
        let text = fs::read_to_string(&path).map_err(|source| ExtractError::Io {
            path: path.clone(),
            source,
        })?;
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("ClientVersion=") {
                return Ok(rest.trim().to_string());
            }
        }
        Err(ExtractError::Other(format!(
            "{}: no ClientVersion line",
            path.display()
        )))
    }

    /// `pak01_dir.vpk`, then `<game>/csgo_community_addons/<map>/<map>_dir.vpk` if it exists
    /// (`MapExtractor.cs:143-166`).
    pub fn shared_vpks(&self, map: &str) -> Vec<PathBuf> {
        let mut out = vec![self.csgo_dir.join("pak01_dir.vpk")];
        if let Some(game_dir) = self.csgo_dir.parent() {
            let community = game_dir
                .join("csgo_community_addons")
                .join(map)
                .join(format!("{map}_dir.vpk"));
            if community.is_file() {
                out.push(community);
            }
        }
        out
    }

    /// `maps/*.vpk` stems, excluding non-map archives (`*_vanity`, `graphics_settings`,
    /// `lobby_mapveto`, `warehouse_vanity`, `workshop_preview_*`).
    pub fn list_maps(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.csgo_dir.join("maps")) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("vpk") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.ends_with("_vanity")
                || stem == "graphics_settings"
                || stem == "lobby_mapveto"
                || stem == "warehouse_vanity"
                || stem.starts_with("workshop_preview_")
            {
                continue;
            }
            out.push(stem.to_string());
        }
        out.sort();
        out
    }
}
