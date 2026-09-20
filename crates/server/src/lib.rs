//! HTTP API (axum) and static hosting for the web viewer, with long-running
//! solve jobs.

pub mod config;
pub mod mesh_payload;
pub mod registry;
pub mod routes;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use config::AppConfig;
use registry::MapRegistry;

/// What `serve` needs to start: the CLI's `--port`/`--open`/`--game`/`--cache` flags. `game`/
/// `cache`/`port`, if given, override whatever the persisted config already has for this run
/// only (but are not written back to it - only `PUT /api/config` persists).
pub struct ServeConfig {
    pub port: Option<u16>,
    pub open: bool,
    pub game: Option<PathBuf>,
    pub cache: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("port {port} is already in use; try --port <N> for a different one")]
    PortInUse { port: u16 },
    #[error("failed to bind 127.0.0.1:{port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("server error: {0}")]
    Io(#[from] std::io::Error),
}

/// Shared state for every route: the persisted config (mutable, guarded by a mutex since `PUT
/// /api/config` writes it), the map registry, where `viewer/`'s static files live, and this
/// run's CLI overrides - which affect what's served but, unlike `config`, are never written back
/// by `PUT /api/config`.
pub struct AppState {
    pub config_path: PathBuf,
    pub config: Mutex<AppConfig>,
    pub registry: MapRegistry,
    pub viewer_dir: PathBuf,
    pub game_override: Option<PathBuf>,
    pub cache_override: Option<PathBuf>,
    pub port_override: Option<u16>,
}

impl AppState {
    pub fn new(
        config_path: PathBuf,
        config: AppConfig,
        cache_root: PathBuf,
        viewer_dir: PathBuf,
    ) -> Self {
        AppState {
            config_path,
            config: Mutex::new(config),
            registry: MapRegistry::new(cache_root),
            viewer_dir,
            game_override: None,
            cache_override: None,
            port_override: None,
        }
    }

    /// Attaches this run's CLI overrides (`--game`/`--cache`/`--port`), which response
    /// formation reads but `PUT /api/config` never persists.
    pub fn with_overrides(
        mut self,
        game_override: Option<PathBuf>,
        cache_override: Option<PathBuf>,
        port_override: Option<u16>,
    ) -> Self {
        self.game_override = game_override;
        self.cache_override = cache_override;
        self.port_override = port_override;
        self
    }
}

/// Walks up from `dir` to the nearest ancestor containing `.git` (mirrors
/// `crates/cli/src/game_path.rs::find_git_root`, duplicated here since that helper is private to
/// the CLI crate).
fn find_git_root(mut dir: &Path) -> Option<PathBuf> {
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// `<repo-or-cwd>/cache`, matching the CLI's own default.
fn default_cache_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = find_git_root(&cwd).unwrap_or_else(|| cwd.clone());
    root.join("cache")
}

/// `viewer/` next to the running executable if it exists there, else `<repo-or-cwd>/viewer`
/// (running from the source tree during development).
fn find_viewer_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("viewer");
        if candidate.is_dir() {
            return candidate;
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = find_git_root(&cwd).unwrap_or_else(|| cwd.clone());
    root.join("viewer")
}

fn open_browser(port: u16) {
    let url = format!("http://127.0.0.1:{port}/");
    #[cfg(windows)]
    {
        // `cmd /c start "" <url>` (`ServeCommand`-equivalent has no reference precedent - our
        // own addition, see s6c_server.md's CLI section).
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        println!("--open is only implemented on Windows; open {url} manually");
    }
}

/// Runs the local HTTP server until Ctrl-C. Listens on loopback only.
pub async fn serve(cfg: ServeConfig) -> Result<(), ServerError> {
    let config_path = config::config_path();
    let app_config = config::load_at(&config_path);

    let cache_root = cfg
        .cache
        .clone()
        .or_else(|| app_config.cache_dir.clone())
        .unwrap_or_else(default_cache_dir);
    let viewer_dir = find_viewer_dir();
    let port = cfg.port.unwrap_or(app_config.port);
    let state = Arc::new(
        AppState::new(config_path, app_config, cache_root, viewer_dir).with_overrides(
            cfg.game.clone(),
            cfg.cache.clone(),
            cfg.port,
        ),
    );

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AddrInUse {
                ServerError::PortInUse { port }
            } else {
                ServerError::Bind { port, source }
            }
        })?;

    println!("serving http://127.0.0.1:{port}/ (ctrl-c to stop)");
    if cfg.open {
        open_browser(port);
    }

    let app = routes::router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
