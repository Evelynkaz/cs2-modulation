//! HTTP API (axum) and static hosting for the web viewer, with long-running
//! solve jobs.

pub mod art;
pub mod config;
pub mod jobs;
mod kv1;
pub mod mesh_payload;
pub mod physics;
pub mod registry;
pub mod routes;
pub mod solve;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use config::AppConfig;
use registry::MapRegistry;
use sim::ThrowConstants;

/// At most this many solves run at once (`s6d_solve_api.md`: `tokio::sync::Semaphore`, 2).
const MAX_CONCURRENT_SOLVES: usize = 2;

/// What `serve` needs to start: the CLI's `--port`/`--open`/`--game`/`--cache`/`--public` flags.
/// `game`/`cache`/`port`, if given, override whatever the persisted config already has for this
/// run only (but are not written back to it - only `PUT /api/config` persists).
pub struct ServeConfig {
    pub port: Option<u16>,
    pub open: bool,
    pub game: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    /// `s6t_public_mode.md`: runs a public demo server, tunnelled by FRP to a real hostname -
    /// disables `PUT /api/config` and every job endpoint, strips local paths from responses, and
    /// caps solve concurrency/time. Never opens a browser regardless of `open`.
    pub public: bool,
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
    #[error("{0}")]
    BadConstants(String),
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
    /// `s6t_public_mode.md`: true for `cs2mod serve --public`. Gates `PUT /api/config` and every
    /// job endpoint to 403, strips local paths out of `GET /api/config`, filters `GET /api/maps`
    /// to fully-prepared maps, and switches `POST /api/lineup` onto the public solve
    /// semaphore/queue/rayon pool/time limit below instead of the regular ones. Also skips the
    /// local-mode `Host` header check (`routes::host_check`) - a public server is deliberately
    /// reachable by a real hostname.
    pub public: bool,
    /// The port this run's listener is actually bound to (set once, in `serve()`, from the same
    /// `port` local that binds the socket) - used by `routes::host_check`'s DNS-rebinding guard,
    /// independent of `config`'s own (mutable, but not re-bound live) `port` field. `0` for the
    /// test harnesses, which never bind a real socket.
    pub bound_port: u16,
    /// Bounds how many solves run at once (`solve::run_lineup_stream`); acquired for the
    /// duration of one `solve_for_target` call.
    pub solve_semaphore: tokio::sync::Semaphore,
    /// How many `POST /api/lineup` requests are currently waiting for a semaphore permit
    /// (not counting ones already solving); `> 16` of these is a 429.
    pub solve_queue: AtomicUsize,
    /// `s6t_public_mode.md`: the public demo's own single-permit solve semaphore, used instead of
    /// `solve_semaphore` when `public` - one demo solve at a time, on a dedicated (capped) rayon
    /// pool, so a visitor's search can't peg every core on the host PC.
    pub public_solve_semaphore: tokio::sync::Semaphore,
    /// `public_solve_semaphore`'s own queue counter, capped by `public_queue_cap` instead of the
    /// regular `MAX_QUEUED_SOLVES`.
    pub public_solve_queue: AtomicUsize,
    /// `public_solve_queue`'s cap (`s6t_public_mode.md`: "очередь до 4"); a field, not a bare
    /// constant, so a test can shrink it to trigger the 429 without needing several real
    /// concurrent solves (same idiom as `max_stream_points` below).
    pub public_queue_cap: usize,
    /// Follow-up security review item 2: bounds concurrent *physics* computations
    /// (`physics.rs::get_trajectory`/`get_lineup_one`/`get_slack`) in public mode - with a non-empty
    /// `broken`, each rebuilds a whole-map grenade collider from scratch (~0.5s CPU, ~195MB on a
    /// big map) instead of reusing the map's cached one, and was otherwise uncapped. Sized by
    /// `solve::public_solve_threads()`, same as the public solve pool, so this and a concurrent
    /// public solve together still leave the host roughly half its cores. Also bounds `/api/overview`
    /// and `/api/mapart` computation in public mode (`art.rs::spawn_art_compute`, follow-up review
    /// round 3, item 3). `Arc`-wrapped (round 3, item 1) so `acquire_owned` can hand out a permit
    /// the blocking closure itself owns, not the cancellable request future.
    pub public_physics_semaphore: Arc<tokio::sync::Semaphore>,
    /// Follow-up security review item 3: a public solve's wall-clock force-cancel deadline
    /// (`solve::PUBLIC_SOLVE_TIME_LIMIT`'s default); overridable so a test can exercise it without
    /// a real 120s wait.
    pub public_solve_time_limit: Duration,
    /// Follow-up security review item 3: how long one NDJSON line write may block on a full,
    /// undrained channel before being treated as "the client's gone" (`solve::SEND_TIMEOUT`'s
    /// default) - without this, a stalled-but-connected client (e.g. behind a reverse proxy with
    /// `proxy_buffering off`) could hold a solve's permit forever. Overridable so a test can
    /// exercise it without a real 10s wait.
    pub send_timeout: Duration,
    /// Throw constants, resolved once (`solve::load_constants`) rather than re-read from disk on
    /// every request; defaults to `ThrowConstants::default()` until `with_constants` sets it
    /// (only `serve()` validates and does that - direct `AppState::new` callers, like the test
    /// harnesses, get the built-in defaults, matching the old per-request fallback when
    /// `data/throw-constants.json` is absent).
    pub constants: ThrowConstants,
    /// Background `extract`/`standspots`/`viewerdata` jobs (`jobs.rs`).
    pub jobs: jobs::JobsState,
    /// Per-solve cap on `checked`/`verified` progress points sent to a `POST /api/lineup`
    /// stream before it switches to `{"phase":"progress-truncated"}` (`solve::MAX_STREAM_POINTS`'
    /// default); overridable so tests can exercise truncation without a real 200k-point sweep.
    pub max_stream_points: usize,
    /// Per-line cap on progress points batched into one `checked`/`verified` line
    /// (`solve::MAX_POINTS_PER_LINE`'s default); overridable for the same reason.
    pub max_points_per_line: usize,
    /// Follow-up security review item 5: `GET /api/overview`'s own parsed-JSON cache, keyed by map
    /// name and reused only while the stored pak01 identity hash still matches the live one
    /// (`art::hashed_vpk`'s own memoized identity) - reopening pak01 and reparsing its overview
    /// `.txt` cost 80-100ms per request otherwise.
    pub(crate) overview_cache:
        Mutex<std::collections::HashMap<String, (String, serde_json::Value)>>,
    /// Follow-up security review round 3, item 2: `art::cached_pak01`'s shared handle, reused
    /// while the stored pak01_dir.vpk path and identity hash still match the live ones - an unknown
    /// map/section name that passes `is_safe_ident` now costs one in-memory index lookup instead of
    /// reopening the whole VPK (~110ms) per request. Keyed by path too, not identity alone: the game
    /// folder can move to another install whose pak01_dir.vpk bytes hash identically, and the cached
    /// `Vpk` handle opens its pak01_NNN archives lazily from its own directory, so serving it for a
    /// different path would read archives from the wrong install.
    pub(crate) pak01_cache: Mutex<Option<(PathBuf, String, Arc<s2fmt::vpk::Vpk>)>>,
    /// Round 3, item 2's own test hook: how many times `art::cached_pak01` actually opened pak01
    /// (not served from `pak01_cache`) - a test confirms this stays at 1 across repeated requests
    /// for an unknown map.
    pub pak01_open_count: AtomicUsize,
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
            public: false,
            bound_port: 0,
            solve_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_SOLVES),
            solve_queue: AtomicUsize::new(0),
            public_solve_semaphore: tokio::sync::Semaphore::new(1),
            public_solve_queue: AtomicUsize::new(0),
            public_queue_cap: solve::MAX_PUBLIC_QUEUED_SOLVES,
            public_physics_semaphore: Arc::new(tokio::sync::Semaphore::new(
                solve::public_solve_threads(),
            )),
            public_solve_time_limit: solve::PUBLIC_SOLVE_TIME_LIMIT,
            send_timeout: solve::SEND_TIMEOUT,
            constants: ThrowConstants::default(),
            jobs: jobs::JobsState::new(),
            max_stream_points: solve::MAX_STREAM_POINTS,
            max_points_per_line: solve::MAX_POINTS_PER_LINE,
            overview_cache: Mutex::new(std::collections::HashMap::new()),
            pak01_cache: Mutex::new(None),
            pak01_open_count: AtomicUsize::new(0),
        }
    }

    /// `<cache root>/solves`, where `POST /api/lineup` caches solved queries
    /// (`s6d_solve_api.md`: `<cache>/solves/<20 hex>.json`).
    pub fn solve_cache_dir(&self) -> PathBuf {
        self.registry.cache_root().join("solves")
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

    /// Sets the throw constants this run actually resolved at startup (`serve()`, after
    /// `solve::load_constants` succeeded); left at `ThrowConstants::default()` otherwise.
    pub fn with_constants(mut self, constants: ThrowConstants) -> Self {
        self.constants = constants;
        self
    }

    /// `s6t_public_mode.md`: sets this run's public/local mode (`serve()`, from `--public`).
    pub fn with_public(mut self, public: bool) -> Self {
        self.public = public;
        self
    }

    /// Records the port the listener actually bound to (`serve()`, before `axum::serve` starts),
    /// for `routes::host_check`'s DNS-rebinding guard.
    pub fn with_bound_port(mut self, port: u16) -> Self {
        self.bound_port = port;
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

/// The repository root the viewer/cache defaults hang off: the nearest `.git` ancestor of the
/// current directory, else of the running executable's own directory (a shortcut or a launch
/// from another folder), else the current directory itself.
fn default_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(root) = find_git_root(&cwd) {
        return root;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Some(root) = find_git_root(dir)
    {
        return root;
    }
    cwd
}

/// `<repo-or-exe-dir-or-cwd>/cache` (see [`default_root`]), matching the CLI's own default.
fn default_cache_dir() -> PathBuf {
    default_root().join("cache")
}

/// `viewer/` next to the running executable if it exists there, else
/// `<repo-or-exe-dir-or-cwd>/viewer` (see [`default_root`]; the latter covers running from the
/// source tree during development).
fn find_viewer_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("viewer");
        if candidate.is_dir() {
            return candidate;
        }
    }
    default_root().join("viewer")
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
    let constants = solve::load_constants().map_err(ServerError::BadConstants)?;
    let state = Arc::new(
        AppState::new(config_path, app_config, cache_root, viewer_dir)
            .with_overrides(cfg.game.clone(), cfg.cache.clone(), cfg.port)
            .with_constants(constants)
            .with_public(cfg.public)
            .with_bound_port(port),
    );

    solve::prune_cache(&state.solve_cache_dir());
    // Follow-up security review item 4: `solve::prune_cache` otherwise only ever runs once, at
    // startup - a long-running server (either mode) never revisits `<cache>/solves` again, so a
    // stale/oversized cache just keeps growing for the rest of the process's life. Every hour
    // after that first prune (the immediate first tick is skipped, since it just ran above).
    let prune_cache_dir = state.solve_cache_dir();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            let cache_dir = prune_cache_dir.clone();
            let _ = tokio::task::spawn_blocking(move || solve::prune_cache(&cache_dir)).await;
        }
    });

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
    if cfg.public {
        println!("public mode: settings and map-prep jobs are disabled for visitors");
    } else if cfg.open {
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
