//! The app's own persisted config: game directory, cache directory, port, last map, theme.
//! Stored at `%APPDATA%\cs2-modulation\config.json` (`$XDG_CONFIG_HOME/cs2-modulation/config.json`
//! elsewhere), read/written by an explicit path parameter everywhere so tests never touch the
//! real per-user config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use extract::game::GameInstall;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub game_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub port: u16,
    pub last_map: Option<String>,
    pub theme: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            game_dir: None,
            cache_dir: None,
            port: 8137,
            last_map: None,
            theme: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to create {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// `%APPDATA%\cs2-modulation\config.json` on Windows, `$XDG_CONFIG_HOME/cs2-modulation/
/// config.json` (falling back to `~/.config/...`) elsewhere.
pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("cs2-modulation").join("config.json")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir);
        base.join("cs2-modulation").join("config.json")
    }
}

/// Loads the config at `path`; missing or unparsable -> the default config, never an error (a
/// first run, or a corrupt file, both just mean "unconfigured").
pub fn load_at(path: &Path) -> AppConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes `cfg` to `path` atomically: a temp file (named with this process's pid, as
/// `extract::cache::save_extraction` does) written and fsync'd via `rename`, then renamed into
/// place, so a crash mid-write never leaves a truncated config for the next run to load.
pub fn save_at(path: &Path, cfg: &AppConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp_name = format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".to_string()),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    let text = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&tmp_path, text).map_err(|source| ConfigError::Write {
        path: tmp_path.clone(),
        source,
    })?;
    std::fs::rename(&tmp_path, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp_path);
        ConfigError::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// A validated CS2 game directory.
#[derive(Debug, Clone)]
pub struct GameDirInfo {
    /// The resolved `...\game\csgo` directory (may differ from what was passed in, see
    /// `adjusted`).
    pub csgo_dir: PathBuf,
    pub build: String,
    /// True when `p` itself had no `maps/` (an install root, not the `csgo` directory) and
    /// `csgo_dir` was derived by appending `game/csgo`.
    pub adjusted: bool,
}

/// Validates that `p` (or `p/csgo` or `p/game/csgo`, if `p` itself doesn't look like a `csgo`
/// dir) has `pak01_dir.vpk`, at least one `maps/*.vpk`, and `steam.inf`, returning its build id.
/// Every failure is a human-readable string, suitable to hand straight back to the viewer.
pub fn validate_game_dir(p: &Path) -> Result<GameDirInfo, String> {
    let candidates = [
        (p.to_path_buf(), false),
        (p.join("csgo"), true),
        (p.join("game").join("csgo"), true),
    ];
    let found = candidates
        .into_iter()
        .find(|(candidate, _)| candidate.join("maps").is_dir());
    let Some((csgo_dir, adjusted)) = found else {
        return Err(format!(
            "{} does not look like a CS2 game directory: no maps/ subdirectory (checked it \
             directly, as {}, and as {})",
            p.display(),
            p.join("csgo").display(),
            p.join("game").join("csgo").display()
        ));
    };
    if !csgo_dir.join("pak01_dir.vpk").is_file() {
        return Err(format!(
            "{} does not look like a CS2 game directory: no pak01_dir.vpk",
            csgo_dir.display()
        ));
    }
    let has_map_vpk = std::fs::read_dir(csgo_dir.join("maps"))
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|ext| ext == "vpk"))
        })
        .unwrap_or(false);
    if !has_map_vpk {
        return Err(format!(
            "{}: maps/ has no .vpk files",
            csgo_dir.join("maps").display()
        ));
    }
    if !csgo_dir.join("steam.inf").is_file() {
        return Err(format!("{} is missing steam.inf", csgo_dir.display()));
    }
    let install = GameInstall::new(&csgo_dir).map_err(|e| e.to_string())?;
    let build = install.build_id().map_err(|e| e.to_string())?;
    Ok(GameDirInfo {
        csgo_dir,
        build,
        adjusted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_is_default() {
        let path = std::env::temp_dir().join(format!(
            "cs2mod-config-test-missing-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let cfg = load_at(&path);
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn load_corrupt_file_is_default() {
        let path = std::env::temp_dir().join(format!(
            "cs2mod-config-test-corrupt-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"not json").unwrap();
        let cfg = load_at(&path);
        assert_eq!(cfg, AppConfig::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "cs2mod-config-test-roundtrip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let cfg = AppConfig {
            port: 9000,
            theme: Some("dark".to_string()),
            ..AppConfig::default()
        };
        save_at(&path, &cfg).unwrap();
        let loaded = load_at(&path);
        assert_eq!(loaded, cfg);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_game_dir_rejects_missing_maps() {
        let dir =
            std::env::temp_dir().join(format!("cs2mod-config-test-nogame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(validate_game_dir(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_game_dir_adjusts_install_root() {
        let root =
            std::env::temp_dir().join(format!("cs2mod-config-test-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let csgo = root.join("game").join("csgo");
        std::fs::create_dir_all(csgo.join("maps")).unwrap();
        std::fs::write(csgo.join("pak01_dir.vpk"), b"x").unwrap();
        std::fs::write(csgo.join("maps").join("de_test.vpk"), b"x").unwrap();
        std::fs::write(csgo.join("steam.inf"), b"ClientVersion=2000908\n").unwrap();
        let info = validate_game_dir(&root).unwrap();
        assert!(info.adjusted);
        assert_eq!(info.csgo_dir, csgo);
        assert_eq!(info.build, "2000908");
        std::fs::remove_dir_all(&root).ok();
    }
}
