//! `POST /api/jobs/{extract|standspots|viewerdata}`, `GET /api/jobs[/{id}]`,
//! `DELETE /api/jobs/{id}`: background work that takes too long for a normal request/response -
//! extracting a map, computing its stand spots, or rendering its radar - run in `spawn_blocking`
//! and reported as an NDJSON stream any client can (re)attach to, mirroring `solve.rs`'s own
//! streaming mechanism (`RxStream`/`LineSender`/`ndjson_response`, reused here rather than
//! copied). One job per `(kind, map)` at a time (a repeat `POST` returns the same job); at most
//! one job per map and two jobs total run at once, no matter how many are queued.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use extract::mapdata::StandSpotsState;
use geom::math::V3;
use solver::standspots::Stance;

use crate::AppState;
use crate::registry::RegistryError;
use crate::routes::{UNKNOWN_MAP_ERROR, api_error, effective_game_dir};
use crate::solve::{LineSender, RxStream, ndjson_response};

/// `cs2mod standspots`' own default `--step` (`main.rs`), used since the job API takes no step
/// argument.
const STANDSPOTS_STEP: f32 = 16.0;
/// `cs2mod viewerdata`'s own default `--pixel-size`.
const RADAR_PIXEL_SIZE: f32 = 2.0;
/// `s6e_progress_jobs.md`: "не более двух [задач] на сервер".
const MAX_CONCURRENT_JOBS: usize = 2;
/// `GET /api/jobs`: "текущие и недавние задачи (последние 20)".
const MAX_RECENT_JOBS: usize = 20;
/// Caps how many finished jobs `JobsState` remembers at all (well beyond [`MAX_RECENT_JOBS`],
/// which only caps the listing), so a long-running server doesn't grow `jobs`/`order` without
/// bound.
const MAX_TRACKED_JOBS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    Extract,
    StandSpots,
    ViewerData,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Extract => "extract",
            JobKind::StandSpots => "standspots",
            JobKind::ViewerData => "viewerdata",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum JobStatus {
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

struct JobInner {
    /// Every NDJSON line sent so far (without the trailing `\n`), replayed to a `GET` that
    /// (re)attaches after some of them already went out.
    lines: Vec<String>,
    /// Streams currently attached and waiting for lines not yet in `lines`.
    waiters: Vec<LineSender>,
    status: JobStatus,
    finished: bool,
}

/// One background job: its identity, its cancellation flag (shared with the blocking task the
/// same way `solve.rs` shares one with a solve), and its NDJSON history.
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub map: String,
    pub cancel: Arc<AtomicBool>,
    inner: Mutex<JobInner>,
}

impl Job {
    fn new(id: String, kind: JobKind, map: String) -> Arc<Self> {
        Arc::new(Job {
            id,
            kind,
            map,
            cancel: Arc::new(AtomicBool::new(false)),
            inner: Mutex::new(JobInner {
                lines: Vec::new(),
                waiters: Vec::new(),
                status: JobStatus::Queued,
                finished: false,
            }),
        })
    }

    fn status(&self) -> JobStatus {
        self.inner.lock().unwrap().status
    }

    fn set_running(&self) {
        self.inner.lock().unwrap().status = JobStatus::Running;
    }

    /// Appends one line to this job's history and forwards it to every currently-attached `GET`
    /// stream; a waiter whose channel is full or gone is dropped (mirrors `solve.rs`'s own
    /// "the client left" handling - there is nobody left to notice a lost job-progress line).
    fn push_line(&self, value: &Value) {
        let line = value.to_string();
        let mut with_nl = line.clone();
        with_nl.push('\n');
        let bytes = Bytes::from(with_nl);
        let mut inner = self.inner.lock().unwrap();
        inner
            .waiters
            .retain(|w| w.try_send(Ok(bytes.clone())).is_ok());
        inner.lines.push(line);
    }

    /// The terminal line: records `status` for `GET /api/jobs`, then closes every attached
    /// stream (dropping their senders, which turns the receiving side's next poll into `None`).
    fn finish(&self, status: JobStatus, value: &Value) {
        self.push_line(value);
        let mut inner = self.inner.lock().unwrap();
        inner.status = status;
        inner.finished = true;
        inner.waiters.clear();
    }

    /// A fresh NDJSON body: every line sent so far, then - if the job hasn't finished - every
    /// line still to come. Registering the new waiter under the same lock as the replay keeps a
    /// line pushed mid-replay from being both replayed and duplicated onto the live channel.
    fn stream(self: &Arc<Self>) -> RxStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1024);
        let mut inner = self.inner.lock().unwrap();
        for line in &inner.lines {
            let mut s = line.clone();
            s.push('\n');
            let _ = tx.try_send(Ok(Bytes::from(s)));
        }
        if !inner.finished {
            inner.waiters.push(tx);
        }
        RxStream(rx)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobListEntry {
    id: String,
    kind: &'static str,
    map: String,
    status: JobStatus,
}

/// Registry of every job this server run has started, plus the concurrency limits `s6e_progress_
/// jobs.md` asks for: at most one job per `(kind, map)` at a time (a repeat `POST` joins it
/// instead of starting a second one), at most one job per map (`map_locks`), and at most
/// [`MAX_CONCURRENT_JOBS`] running at once (`semaphore`).
pub struct JobsState {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    order: Mutex<VecDeque<String>>,
    dedupe: Mutex<HashMap<(JobKind, String), String>>,
    map_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    semaphore: tokio::sync::Semaphore,
    next_id: AtomicU64,
}

impl Default for JobsState {
    fn default() -> Self {
        Self::new()
    }
}

impl JobsState {
    pub fn new() -> Self {
        JobsState {
            jobs: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
            dedupe: Mutex::new(HashMap::new()),
            map_locks: Mutex::new(HashMap::new()),
            semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS),
            next_id: AtomicU64::new(1),
        }
    }

    /// The in-flight job for `(kind, map)` if there is one, else a freshly registered `Queued`
    /// one; the `bool` says which. `(kind, map)`'s dedupe entry is cleared once that job finishes
    /// (`clear_dedupe`), so a later `POST` starts a new job rather than replaying the old one. A
    /// job whose `cancel` flag is already set is treated as absent - it's on its way out, and a
    /// `POST` arriving in that window must get its own job rather than wait out the cancellation.
    fn get_or_create(&self, kind: JobKind, map: &str) -> (Arc<Job>, bool) {
        let key = (kind, map.to_string());
        let mut dedupe = self.dedupe.lock().unwrap();
        if let Some(id) = dedupe.get(&key)
            && let Some(job) = self.jobs.lock().unwrap().get(id)
            && !job.cancel.load(Ordering::Relaxed)
        {
            return (job.clone(), false);
        }
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("job-{n:08x}");
        let job = Job::new(id.clone(), kind, map.to_string());
        dedupe.insert(key, id.clone());
        drop(dedupe);
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());
        let mut order = self.order.lock().unwrap();
        order.push_back(id);
        while order.len() > MAX_TRACKED_JOBS {
            if let Some(old_id) = order.pop_front() {
                self.jobs.lock().unwrap().remove(&old_id);
            }
        }
        drop(order);
        (job, true)
    }

    fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    fn list(&self) -> Vec<JobListEntry> {
        let order = self.order.lock().unwrap();
        let jobs = self.jobs.lock().unwrap();
        order
            .iter()
            .rev()
            .filter_map(|id| jobs.get(id))
            .take(MAX_RECENT_JOBS)
            .map(|j| JobListEntry {
                id: j.id.clone(),
                kind: j.kind.as_str(),
                map: j.map.clone(),
                status: j.status(),
            })
            .collect()
    }

    fn clear_dedupe(&self, kind: JobKind, map: &str) {
        self.dedupe.lock().unwrap().remove(&(kind, map.to_string()));
    }

    /// The lock that keeps at most one job running for `map` at a time, no matter its kind.
    fn map_lock(&self, map: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.map_locks
            .lock()
            .unwrap()
            .entry(map.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Drops `map`'s lock entry once nothing else references it (`run_job`, after its own clone
    /// of the lock has already gone out of scope) - otherwise `map_locks` grows one entry per
    /// distinct map string ever posted, forever.
    fn prune_map_lock(&self, map: &str) {
        let mut locks = self.map_locks.lock().unwrap();
        if locks.get(map).is_some_and(|l| Arc::strong_count(l) == 1) {
            locks.remove(map);
        }
    }
}

enum JobOutcome {
    Done(Value),
    Error(String),
    Cancelled,
}

/// `state.registry.get`, off the async executor: it can sha256 the map's `world.cgeo` and parse
/// an 11 MB mesh file while holding a `std::sync::Mutex`, which would otherwise block a tokio
/// worker thread for the duration.
async fn get_entry_blocking(
    state: &Arc<AppState>,
    map: &str,
) -> Result<Arc<crate::registry::MapEntry>, JobOutcome> {
    let state = state.clone();
    let map = map.to_string();
    match tokio::task::spawn_blocking(move || {
        let game_dir = effective_game_dir(&state);
        state.registry.get(&map, game_dir.as_deref())
    })
    .await
    {
        Ok(Ok(entry)) => Ok(entry),
        Ok(Err(RegistryError::UnknownMap)) => Err(JobOutcome::Error(UNKNOWN_MAP_ERROR.to_string())),
        Ok(Err(e)) => Err(JobOutcome::Error(e.to_string())),
        Err(_) => Err(JobOutcome::Error(
            "map lookup task panicked - check server log".to_string(),
        )),
    }
}

/// Waits for this job's map lock and a global permit, runs the work, then reports the outcome
/// and releases `(kind, map)`'s dedupe entry.
async fn run_job(state: Arc<AppState>, job: Arc<Job>) {
    job.push_line(&json!({ "stage": "queued", "done": 0, "total": 1 }));
    {
        let lock = state.jobs.map_lock(&job.map);
        let _map_guard = lock.lock().await;
        let Ok(_permit) = state.jobs.semaphore.acquire().await else {
            return;
        };
        job.set_running();
        job.push_line(&json!({ "stage": job.kind.as_str(), "done": 0, "total": 1 }));

        let outcome = match job.kind {
            JobKind::Extract => run_extract(&state, &job).await,
            JobKind::StandSpots => run_standspots(&state, &job).await,
            JobKind::ViewerData => run_viewerdata(&state, &job).await,
        };

        match outcome {
            JobOutcome::Done(result) => job.finish(JobStatus::Done, &json!({ "result": result })),
            JobOutcome::Error(msg) => job.finish(JobStatus::Error, &json!({ "error": msg })),
            JobOutcome::Cancelled => {
                job.finish(JobStatus::Cancelled, &json!({ "status": "cancelled" }))
            }
        }
        // `lock` (this job's clone of the map lock) drops here, before `prune_map_lock` checks
        // whether it's the last one standing.
    }
    state.jobs.clear_dedupe(job.kind, &job.map);
    state.jobs.prune_map_lock(&job.map);
}

/// `extract::extract_map` + `extract::cache::save_extraction`, exactly as `cmd_extract.rs::
/// extract` calls them, minus the printing. `cancel` is only checked between these coarse steps
/// (there is no finer-grained hook into `extract_map` to check it against) - a cancel arriving
/// mid-extract takes effect once the current step finishes, not immediately.
async fn run_extract(state: &Arc<AppState>, job: &Arc<Job>) -> JobOutcome {
    let Some(game_dir) = effective_game_dir(state) else {
        return JobOutcome::Error(
            "no game directory configured - open the setup page (or PUT /api/config) and set one first"
                .to_string(),
        );
    };
    let cache_root = state.registry.cache_root().to_path_buf();
    let map = job.map.clone();
    let cancel = job.cancel.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<(std::path::PathBuf, bool), String> {
        let install = extract::GameInstall::new(&game_dir).map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        if let Some(dir) =
            extract::cache::find_cached(&cache_root, &install, &map).map_err(|e| e.to_string())?
        {
            return Ok((dir, true));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let extraction = extract::extract_map(&install, &map, &extract::ExtractOptions::default())
            .map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let dir = extract::cache::save_extraction(&cache_root, &extraction, false)
            .map_err(|e| e.to_string())?;
        Ok((dir, false))
    })
    .await;

    match res {
        Ok(Ok((dir, reused))) => {
            state.registry.invalidate(&job.map);
            let build = extract::cache::load_manifest(&dir)
                .ok()
                .map(|m| m.meta.game_build);
            JobOutcome::Done(json!({ "map": job.map, "build": build, "reused": reused }))
        }
        Ok(Err(msg)) if msg == "cancelled" => JobOutcome::Cancelled,
        Ok(Err(msg)) => JobOutcome::Error(msg),
        Err(_) => JobOutcome::Error("extract task panicked - check server log".to_string()),
    }
}

/// `solver::standspots::compute` + `extract::mapdata::save_stand_spots`, exactly as
/// `cmd_solver.rs::standspots` calls them. Unlike `run_extract`, `compute`'s progress callback
/// runs once per scanned row, so `cancel` is checked there too and the scan stops as soon as it's
/// set, rather than only before/after the whole call.
async fn run_standspots(state: &Arc<AppState>, job: &Arc<Job>) -> JobOutcome {
    let entry = match get_entry_blocking(state, &job.map).await {
        Ok(e) => e,
        Err(outcome) => return outcome,
    };
    if entry.bundle.nav_areas.is_empty() {
        return JobOutcome::Error("map has no nav data (see /api/maps)".to_string());
    }
    if let StandSpotsState::Loaded(cached) = &entry.bundle.stand_spots
        && cached.step == STANDSPOTS_STEP
    {
        return JobOutcome::Done(
            json!({ "map": job.map, "count": cached.spots.len(), "step": cached.step, "reused": true }),
        );
    }

    let job_for_progress = job.clone();
    let cancel = job.cancel.clone();
    let entry_for_blocking = entry.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<extract::StandSpotFile, String> {
        let mesh = &entry_for_blocking.bundle.mesh;
        let Some((min, max)) = mesh.bounds() else {
            return Err(format!(
                "{}'s mesh has no triangles",
                entry_for_blocking.map
            ));
        };
        let (min, max) = (V3::from_array(min), V3::from_array(max));
        let mask = geom::filter::player_mask(mesh);
        let collider =
            geom::grid::UniformGrid::build(mesh, &mask, None, 128.0).map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let mut last_percent = -1i32;
        let spots = solver::standspots::compute(
            &collider,
            &entry_for_blocking.bundle.nav_areas,
            min,
            max,
            STANDSPOTS_STEP,
            Some(&mut |done, total| {
                let percent = if total > 0 { done * 100 / total } else { 100 };
                if percent != last_percent && percent % 10 == 0 {
                    last_percent = percent;
                    job_for_progress
                        .push_line(&json!({ "stage": "standspots", "done": done, "total": total }));
                }
                !cancel.load(Ordering::Relaxed)
            }),
        );
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        Ok(extract::StandSpotFile {
            version: extract::STANDSPOTS_VERSION,
            map: entry_for_blocking.map.clone(),
            step: STANDSPOTS_STEP,
            spots: spots
                .iter()
                .map(|s| extract::StandSpotJson {
                    feet: [
                        (s.feet.x * 100.0).round_ties_even() / 100.0,
                        (s.feet.y * 100.0).round_ties_even() / 100.0,
                        (s.feet.z * 100.0).round_ties_even() / 100.0,
                    ],
                    stance: match s.stance {
                        Stance::Standing => "Standing",
                        Stance::Crouching => "Crouching",
                        Stance::None => "None",
                    }
                    .to_string(),
                    nav: s.nav_covered,
                })
                .collect(),
        })
    })
    .await;

    match res {
        Ok(Ok(payload)) => {
            if let Err(e) = extract::save_stand_spots(&entry.dir, &payload) {
                return JobOutcome::Error(e.to_string());
            }
            state.registry.reload_stand_spots(&job.map);
            JobOutcome::Done(
                json!({ "map": job.map, "count": payload.spots.len(), "step": payload.step }),
            )
        }
        Ok(Err(msg)) if msg == "cancelled" => JobOutcome::Cancelled,
        Ok(Err(msg)) => JobOutcome::Error(msg),
        Err(_) => JobOutcome::Error("standspots task panicked - check server log".to_string()),
    }
}

/// `entities.json` missing -> empty list (same "no error" treatment as `cmd_viewerdata.rs::
/// load_entities`, duplicated here since it's a CLI-crate private helper).
fn load_entities(dir: &Path) -> Result<Vec<extract::EntityRecord>, String> {
    let path = dir.join("entities.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// `radar::render` + `radar::write_png`, plus the same atomic-write dance `cmd_viewerdata.rs`
/// uses for `viewer-map.png`/`.json`. Skips the render entirely if both files already exist
/// (matching the CLI's own default, unforced, behavior - the job API exposes no `force`).
async fn run_viewerdata(state: &Arc<AppState>, job: &Arc<Job>) -> JobOutcome {
    let entry = match get_entry_blocking(state, &job.map).await {
        Ok(e) => e,
        Err(outcome) => return outcome,
    };
    if entry.dir.join("viewer-map.png").is_file() && entry.dir.join("viewer-map.json").is_file() {
        return JobOutcome::Done(json!({ "map": job.map, "reused": true }));
    }

    let cancel = job.cancel.clone();
    let entry_for_blocking = entry.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<radar::ViewerMap, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let manifest =
            extract::cache::load_manifest(&entry_for_blocking.dir).map_err(|e| e.to_string())?;
        let entities = load_entities(&entry_for_blocking.dir)?;
        let opts = radar::RadarOptions {
            pixel_size: RADAR_PIXEL_SIZE,
            region: None,
        };
        let image = radar::render(
            &entry_for_blocking.bundle.mesh,
            &entry_for_blocking.bundle.nav_areas,
            &opts,
        )
        .map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let callouts = radar::callouts(&entities, image.region);
        let viewer_map = radar::ViewerMap {
            map: manifest.meta.map.clone(),
            build: manifest.meta.game_build.clone(),
            region: image.region,
            image: "viewer-map.png".to_string(),
            pixel_size: RADAR_PIXEL_SIZE,
            callouts,
        };

        let dir = &entry_for_blocking.dir;
        let png_tmp = dir.join(format!("viewer-map.png.tmp-{}", std::process::id()));
        let json_tmp = dir.join(format!("viewer-map.json.tmp-{}", std::process::id()));
        let png_path = dir.join("viewer-map.png");
        let json_path = dir.join("viewer-map.json");
        if let Err(e) = radar::write_png(&image, &png_tmp) {
            let _ = std::fs::remove_file(&png_tmp);
            return Err(format!("failed to write viewer-map.png: {e}"));
        }
        if let Err(e) = std::fs::rename(&png_tmp, &png_path) {
            let _ = std::fs::remove_file(&png_tmp);
            return Err(format!("failed to rename into {}: {e}", png_path.display()));
        }
        let json_text = serde_json::to_string(&viewer_map).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::write(&json_tmp, json_text) {
            let _ = std::fs::remove_file(&json_tmp);
            return Err(format!("failed to write {}: {e}", json_tmp.display()));
        }
        if let Err(e) = std::fs::rename(&json_tmp, &json_path) {
            let _ = std::fs::remove_file(&json_tmp);
            return Err(format!(
                "failed to rename into {}: {e}",
                json_path.display()
            ));
        }
        Ok(viewer_map)
    })
    .await;

    match res {
        // No registry invalidation needed: `viewer-map.png`/`.json` aren't part of any cached
        // `MapEntry` field (`MapEntry::has_radar` stats the file directly), so nothing there goes
        // stale.
        Ok(Ok(viewer_map)) => {
            JobOutcome::Done(json!({ "map": job.map, "callouts": viewer_map.callouts.len() }))
        }
        Ok(Err(msg)) if msg == "cancelled" => JobOutcome::Cancelled,
        Ok(Err(msg)) => JobOutcome::Error(msg),
        Err(_) => JobOutcome::Error("viewerdata task panicked - check server log".to_string()),
    }
}

// ---- routes ---------------------------------------------------------------------------------

#[derive(Deserialize)]
struct JobRequest {
    map: String,
}

/// `^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$` - a plain CS2 map name, never a path: no separators, no
/// `..`, no drive letters, nothing that `GameInstall::map_vpk`/the cache layout would resolve
/// outside the game install or `cache_root`.
fn is_plain_map_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Parses `{"map": "..."}`, trimmed and lower-cased (Windows paths are case-insensitive, so
/// `de_dust2` and `DE_DUST2` must land on the same job/lock/cache directory - see `dedupe`/
/// `map_locks`) and validated against [`is_plain_map_name`] so it can never be joined into a
/// filesystem path unchecked.
fn parse_job_body(body: &Bytes) -> Result<String, Box<Response>> {
    let req: JobRequest = serde_json::from_slice(body).map_err(|_| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "body must be {\"map\": \"...\"}",
        ))
    })?;
    let trimmed = req.map.trim();
    if trimmed.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "map must not be empty",
        )));
    }
    if !is_plain_map_name(trimmed) {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "map name must be a plain CS2 map name",
        )));
    }
    Ok(trimmed.to_ascii_lowercase())
}

async fn start_job(
    state: Arc<AppState>,
    kind: JobKind,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type_ok = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/json"));
    if !content_type_ok {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        );
    }
    let map = match parse_job_body(&body) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    if kind == JobKind::Extract && effective_game_dir(&state).is_none() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "no game directory configured - open the setup page (or PUT /api/config) and set one first",
        );
    }
    let (job, is_new) = state.jobs.get_or_create(kind, &map);
    if is_new {
        tokio::spawn(run_job(state.clone(), job.clone()));
    }
    (StatusCode::ACCEPTED, Json(json!({ "job": job.id }))).into_response()
}

pub async fn post_extract(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    start_job(state, JobKind::Extract, headers, body).await
}

pub async fn post_standspots(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    start_job(state, JobKind::StandSpots, headers, body).await
}

pub async fn post_viewerdata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    start_job(state, JobKind::ViewerData, headers, body).await
}

pub async fn get_job(State(state): State<Arc<AppState>>, AxPath(id): AxPath<String>) -> Response {
    let Some(job) = state.jobs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "unknown job");
    };
    ndjson_response(Body::from_stream(job.stream()))
}

pub async fn delete_job(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Response {
    let Some(job) = state.jobs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "unknown job");
    };
    job.cancel.store(true, Ordering::Relaxed);
    // Fixed, not `job.status()`: the job may still report `running` for a while after this (it's
    // cooperative - see `run_standspots`'s progress callback), but it is, from this call's point
    // of view, already on its way out.
    Json(json!({ "job": job.id, "status": "cancelling" })).into_response()
}

pub async fn get_jobs(State(state): State<Arc<AppState>>) -> Response {
    Json(state.jobs.list()).into_response()
}
