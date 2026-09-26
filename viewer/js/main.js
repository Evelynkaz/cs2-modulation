// Entry point: theme + screen routing. Screen 1 (first run) lives in setup.js; the "prepare a
// map" job flow lives in jobs.js; everything else (map list, radar screen) is small enough to
// live here.

import { state, applyTheme, resolveInitialTheme, storeTheme } from "./state.js?v=1";
import { strings } from "./strings.js?v=5";
import {
  fetchConfig,
  putConfig,
  fetchMaps,
  fetchJobs,
  fetchRadar,
  fetchLevels,
  fetchTrajectory,
  radarPngUrl,
  mapArtUrl,
  fetchOverview,
  deleteJob,
  hasUsableRender,
} from "./api.js?v=1";
import { renderSetup } from "./setup.js?v=2";
import { startPrepare, reconnectJob, stageLabel } from "./jobs.js?v=1";
import { createMapView, renderThumbnail } from "./map2d.js?v=4";
import { runSolve, buildQuery, parseSetpos, selectionError } from "./solve.js?v=3";
import { createPanel, TYPE_LABELS, CLICK_LABELS } from "./panel.js?v=5";
import { createSceneView } from "./scene3d.js?v=1";
import { segmented, chip, collapsible } from "./ui.js?v=2";
import { icon, THROW_TYPE_ICON } from "./icons.js?v=1";

function el(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (key === "role" || key.startsWith("aria-")) {
      node.setAttribute(key, value);
    } else {
      node[key] = value;
    }
  }
  for (const child of children) {
    if (child != null) {
      node.append(child);
    }
  }
  return node;
}

const app = document.getElementById("app");
const statusEl = document.getElementById("status");
const globalTopbar = document.getElementById("topbar");
const globalThemeBtn = document.getElementById("theme-toggle");
const globalBetaBadge = document.getElementById("beta-badge");
const globalReportBtn = document.getElementById("report-issue-btn");
let radarView = null; // { recolor(): void } for the current map screen's 2D canvas, if any.
// AMBER-7: the map view owns a ResizeObserver, a devicePixelRatio listener and a recolored
// canvas - each `showMapScreen` must destroy the previous one instead of leaking it. F3b-1b: a
// map screen may now hold up to two views at once (2D + a lazily-created 3D scene, kept alive
// together so the 2D/3D toggle is instant instead of reloading `render.glb` every switch) - both
// go in `currentViews` and are destroyed together.
let currentViews = [];
function destroyCurrentMapView() {
  for (const v of currentViews) {
    v.destroy();
  }
  currentViews = [];
}

// Screen changes go through the dedicated `#status` live region, not `#app` itself - `#app`'s
// own DOM churn (a whole screen replaced at once) would otherwise be announced line by line.
function announce(text) {
  statusEl.textContent = text;
}

// Non-map screens (setup, map list) scroll normally inside a centred `.page`; the map screen owns
// its own full-viewport layout and must never scroll at the `#app` level (S6m: "no page scroll").
function setAppScroll(scroll) {
  app.className = scroll ? "app-scroll" : "";
}

// The map screen builds its own combined header row (back + map name + 2D/3D + theme) - the
// generic topbar is hidden while it's open and restored by every other screen.
function setGlobalTopbarVisible(visible) {
  globalTopbar.hidden = !visible;
}

// ---- bootstrap ------------------------------------------------------------------------------

function syncThemeButton(btn) {
  btn.innerHTML = icon(state.theme === "dark" ? "sun" : "moon", 18);
  const label = state.theme === "dark" ? strings.theme.toggleToLight : strings.theme.toggleToDark;
  btn.setAttribute("aria-label", label);
  btn.title = label;
}

// Flips the theme and keeps every currently-mounted theme button (the global one, plus the map
// screen's own copy, when that's the one open) in sync. `extraSync`: the caller's own button, if
// it isn't `globalThemeBtn`.
function toggleThemeAnd(extraSync) {
  applyTheme(state.theme === "dark" ? "light" : "dark");
  storeTheme(state.theme);
  syncThemeButton(globalThemeBtn);
  extraSync?.();
  radarView?.recolor(state.theme);
}

function wireThemeToggle() {
  syncThemeButton(globalThemeBtn);
  globalThemeBtn.addEventListener("click", () => toggleThemeAnd());
}

// «ОБТ 0.6.0-beta.1»: shown next to the app title once `/api/config` answers with a `version`
// - hidden (not "ОБТ undefined") for an older server that predates the field.
function syncBetaBadge(badgeEl) {
  const version = state.config?.version;
  if (version) {
    badgeEl.textContent = strings.beta.badge(version);
    badgeEl.hidden = false;
  } else {
    badgeEl.hidden = true;
  }
}

const ISSUE_TRACKER_URL = "https://github.com/Evelynkaz/cs2-modulation/issues/new";

// Prefills the GitHub bug report form's own fields (by id: `version`, `map`, `lineup`) via query
// params. `lineup`, when given, is a lineup card's own `l` (its `consoleExact`/`console` plus
// `type` go into the `lineup` field together, since the form has no separate field for the type).
function reportIssueUrl(lineup) {
  const params = new URLSearchParams({ template: "bug.yml" });
  const version = state.config?.version;
  if (version) {
    params.set("version", version);
  }
  const map = state.currentMap;
  if (map) {
    params.set("map", map);
  }
  params.set("title", map ? `Раскидка не сработала — ${map}` : "Раскидка не сработала");
  if (lineup) {
    const exact = lineup.consoleExact ?? lineup.console;
    if (exact) {
      params.set("lineup", `${exact} (тип: ${TYPE_LABELS[lineup.type] ?? lineup.type})`);
    }
  }
  return `${ISSUE_TRACKER_URL}?${params.toString()}`;
}

// Fills in the icon, label, title and click handler shared by the global topbar's button and the
// map screen's own copy - same reasoning as `syncThemeButton`/`toggleThemeAnd` above.
function wireReportIssueButton(btn) {
  btn.innerHTML = icon("warning", 16);
  btn.append(el("span", { className: "report-issue-label", textContent: strings.beta.reportButton }));
  btn.title = strings.beta.reportButton;
  btn.setAttribute("aria-label", strings.beta.reportButton);
  btn.addEventListener("click", () => window.open(reportIssueUrl(), "_blank", "noopener"));
}

function renderServerDown(root, retry) {
  setAppScroll(true);
  root.replaceChildren(
    el(
      "div",
      { className: "page" },
      el("p", { className: "status status-error", textContent: strings.errors.serverDown }),
      el("button", { type: "button", className: "primary", textContent: strings.errors.retryButton, onclick: retry }),
    ),
  );
}

function renderLoadingMaps() {
  setAppScroll(true);
  app.replaceChildren(
    el("div", { className: "page" }, el("h1", { textContent: strings.maps.heading }), el("p", { className: "hint", textContent: strings.maps.loading })),
  );
}

function openSetup(config) {
  setAppScroll(true);
  setGlobalTopbarVisible(true);
  state.screen = "setup";
  renderSetup(
    app,
    (cfg) => {
      state.config = cfg;
      showMapsScreen();
    },
    config,
  );
}

async function boot() {
  applyTheme(resolveInitialTheme());
  wireThemeToggle();
  wireReportIssueButton(globalReportBtn);
  await routeFromConfig();
  // `s6f2_solve_ui.md`: a link with `#map=...&target=...` opens straight to that map and starts
  // the same solve, instead of stopping at the map list.
  if (state.screen === "maps") {
    const hashQuery = readHash();
    const ready = hashQuery && state.maps.find((m) => m.map === hashQuery.map && m.hasLineups && m.hasStandSpots && m.hasRadar);
    if (ready) {
      state.screen = "map";
      putConfig({ lastMap: hashQuery.map });
      showMapScreen(hashQuery.map, { autoBody: bodyFromHash(hashQuery) });
    }
  }
  reattachJobs();
}

// ---- address-bar state (`#map=...&target=x,y,z&tolerance=80&...`) -------------------------------

function readHash() {
  const raw = location.hash.replace(/^#/, "");
  if (!raw) {
    return null;
  }
  const params = new URLSearchParams(raw);
  const map = params.get("map");
  const targetStr = params.get("target");
  // A link needs a point `target`, a `targetArea`, or a `sightline` to be worth auto-running
  // (`s6g2_target_area.md`, `s6r_sightline_target.md`) - the three are mutually exclusive, so any
  // one of them is enough on its own.
  if (!map || (!targetStr && !params.get("targetArea") && !params.get("sightline"))) {
    return null;
  }
  const query = { map };
  if (targetStr) {
    const target = targetStr.split(",").map(Number);
    if (target.length < 3 || target.some((v) => !Number.isFinite(v))) {
      return null;
    }
    query.target = target;
  }
  for (const [k, v] of params.entries()) {
    if (k === "map" || k === "target") {
      continue;
    }
    query[k] = v;
  }
  return query;
}

// AMBER-10: a non-numeric hash value must be dropped, not turned into a `NaN` that serialises to
// `null`, gets rejected by the server, and then gets written straight back into the hash.
function finiteNumber(str) {
  const v = Number(str);
  return Number.isFinite(v) ? v : undefined;
}

function bodyFromHash(q) {
  const body = { map: q.map, target: q.target };
  // `s6g2_target_area.md`: same flat-list idiom as `originArea` below, for `targetArea` - only
  // meaningful when there is no `target` (the two are mutually exclusive).
  if (!q.target && q.targetArea) {
    const nums = q.targetArea.split(",").map(Number);
    if (nums.length >= 6 && nums.length % 2 === 0 && nums.every(Number.isFinite)) {
      const polygon = [];
      for (let i = 0; i < nums.length; i += 2) {
        polygon.push([nums[i], nums[i + 1]]);
      }
      body.targetArea = polygon;
    }
  }
  if (q.targetZMin) {
    const v = finiteNumber(q.targetZMin);
    if (v !== undefined) {
      body.targetZMin = v;
    }
  }
  if (q.targetZMax) {
    const v = finiteNumber(q.targetZMax);
    if (v !== undefined) {
      body.targetZMax = v;
    }
  }
  // `s6r_sightline_target.md`: a flat "fx,fy,fz,tx,ty,tz" list (the two eye points) - only
  // meaningful when there is neither a `target` nor a `targetArea` (all three are mutually
  // exclusive).
  if (!q.target && !q.targetArea && q.sightline) {
    const nums = q.sightline.split(",").map(Number);
    if (nums.length === 6 && nums.every(Number.isFinite)) {
      body.sightline = { from: nums.slice(0, 3), to: nums.slice(3, 6) };
    }
  }
  if (q.origin) {
    const o = q.origin.split(",").map(Number);
    if (o.length === 2 && o.every(Number.isFinite)) {
      body.origin = o;
    }
  }
  if (q.originReach) {
    const v = finiteNumber(q.originReach);
    if (v !== undefined) {
      body.originReach = v;
    }
  }
  // `s6g_origin_area.md`: a flat "x0,y0,x1,y1,..." list (an even count, at least 3 points) -
  // `syncHash` writes it that way since `Array.prototype.join` flattens the polygon's nested
  // `[x,y]` pairs on its own.
  if (q.originArea) {
    const nums = q.originArea.split(",").map(Number);
    if (nums.length >= 6 && nums.length % 2 === 0 && nums.every(Number.isFinite)) {
      const polygon = [];
      for (let i = 0; i < nums.length; i += 2) {
        polygon.push([nums[i], nums[i + 1]]);
      }
      body.originArea = polygon;
    }
  }
  if (q.zMin) {
    const v = finiteNumber(q.zMin);
    if (v !== undefined) {
      body.zMin = v;
    }
  }
  if (q.zMax) {
    const v = finiteNumber(q.zMax);
    if (v !== undefined) {
      body.zMax = v;
    }
  }
  if (q.scope) {
    body.scope = q.scope;
  }
  if (q.tolerance) {
    const v = finiteNumber(q.tolerance);
    if (v !== undefined) {
      body.tolerance = v;
    }
  }
  if (q.minStability) {
    const v = finiteNumber(q.minStability);
    if (v !== undefined) {
      body.minStability = v;
    }
  }
  if (q.fineScan === "true") {
    body.fineScan = true;
  }
  if (q.types) {
    // S6m: "RunJumpThrow" no longer exists in the product - a link bookmarked before this change
    // must not resurrect it (`s6m_consumer_redesign.md`: "a stale persisted selection... must be
    // cleaned on load").
    const types = q.types.split(",").filter((t) => t !== "RunJumpThrow");
    if (types.length > 0) {
      body.types = types;
    }
  }
  if (q.strengths) {
    const strengths = q.strengths.split(",").map(finiteNumber).filter((v) => v !== undefined);
    if (strengths.length > 0) {
      body.strengths = strengths;
    }
  }
  if (q.broken) {
    body.broken = q.broken.split(",");
  }
  if (q.originPin) {
    body.originPin = q.originPin;
  }
  return body;
}

function syncHash(body) {
  const parts = [`map=${encodeURIComponent(body.map)}`];
  // `target` gets its own fixed-precision formatting; `targetArea` (area mode, `body.target`
  // absent - `s6g2_target_area.md`) falls through to the generic array formatting below, same as
  // `originArea` already does.
  if (body.target) {
    parts.push(`target=${body.target.map((v) => v.toFixed(1)).join(",")}`);
  }
  // `s6r_sightline_target.md`: same flat-list idiom as `targetArea`/`originArea`, for `sightline`
  // (an object, not an array - the generic loop below only knows how to flatten arrays).
  if (body.sightline) {
    const flat = [...body.sightline.from, ...body.sightline.to];
    parts.push(`sightline=${flat.map((v) => v.toFixed(1)).join(",")}`);
  }
  for (const [k, v] of Object.entries(body)) {
    if (k === "map" || k === "target" || k === "sightline") {
      continue;
    }
    const text = Array.isArray(v) ? v.join(",") : String(v);
    parts.push(`${k}=${encodeURIComponent(text)}`);
  }
  history.replaceState(null, "", "#" + parts.join("&"));
}

async function routeFromConfig() {
  renderLoadingMaps();
  const { data, error } = await fetchConfig();
  if (error !== undefined) {
    renderServerDown(app, routeFromConfig);
    return;
  }
  state.config = data;
  syncBetaBadge(globalBetaBadge);
  if (!data.configured) {
    openSetup(data);
    return;
  }
  // The maps list is already in `data.maps` - no need for a second `/api/maps` round trip.
  state.maps = data.maps ?? [];
  state.screen = "maps";
  renderMapsScreen();
}

boot();

// ---- screen 2: map list -----------------------------------------------------------------------

async function showMapsScreen() {
  state.screen = "maps";
  destroyCurrentMapView();
  radarView = null;
  renderLoadingMaps();
  const { data, error } = await fetchMaps();
  if (error !== undefined) {
    renderServerDown(app, showMapsScreen);
    return;
  }
  state.maps = data;
  renderMapsScreen();
}

// "de_mirage" -> "Mirage" - a friendlier card title; the raw map name stays visible underneath
// (`.map-card-sub`) so two maps that prettify the same way are never actually ambiguous.
function prettifyMapName(map) {
  const base = map.replace(/^(de|cs|aim|arena|dz|ar)_/i, "");
  return base.length > 0 ? base[0].toUpperCase() + base.slice(1) : map;
}

// The game mode a map's own name prefix implies (round-2 design critique 7) - `null` for a prefix
// with no defined mode (e.g. "aim_"/"arena_"), which just sorts after the three known ones and
// shows no mode tag.
const MODE_ORDER = { de: 0, cs: 1, ar: 2 };
function mapPrefix(map) {
  const m = /^(de|cs|ar)_/i.exec(map);
  return m ? m[1].toLowerCase() : null;
}
function modeLabel(prefix) {
  return { de: strings.maps.modeDe, cs: strings.maps.modeCs, ar: strings.maps.modeAr }[prefix] ?? null;
}

// de_ maps first, then cs_, then ar_ (and anything else after), alphabetical by pretty name
// within each group.
function sortedMaps(maps) {
  return [...maps].sort((a, b) => {
    const oa = MODE_ORDER[mapPrefix(a.map)] ?? 99;
    const ob = MODE_ORDER[mapPrefix(b.map)] ?? 99;
    if (oa !== ob) {
      return oa - ob;
    }
    return prettifyMapName(a.map).localeCompare(prettifyMapName(b.map), "ru");
  });
}

function mapStatusInfo(m) {
  if (m.stale) {
    return { text: strings.maps.statusStale, cls: "badge-hard" };
  }
  if (!(m.hasLineups && m.hasStandSpots && m.hasRadar)) {
    return { text: strings.maps.statusNeedsPrepare, cls: "badge-medium" };
  }
  if (hasUsableRender(m)) {
    return { text: strings.maps.status3dReady, cls: "badge-easy" };
  }
  return { text: strings.maps.statusReady, cls: "badge-easy" };
}

function renderMapsScreen() {
  setAppScroll(true);
  setGlobalTopbarVisible(true);
  app.replaceChildren();
  announce(strings.maps.heading);
  const settingsBtn = el("button", { type: "button", className: "btn-ghost" });
  settingsBtn.innerHTML = `${icon("gear", 16)} ${strings.maps.backToSetup}`;
  settingsBtn.addEventListener("click", () => openSetup(state.config));

  const page = el(
    "div",
    { className: "page" },
    el("div", { className: "page-header" }, el("h1", { textContent: strings.maps.heading }), settingsBtn),
  );

  if (state.maps.length === 0) {
    page.append(renderExtractNewForm());
  } else {
    const grid = el("div", { className: "map-grid" });
    for (const m of sortedMaps(state.maps)) {
      grid.append(renderMapCard(m));
    }
    page.append(grid, renderExtractNewForm(true));
  }
  app.append(page);
}

function renderProgressBox() {
  const label = el("div");
  const barSpan = el("span");
  const bar = el("div", { className: "progress-bar" }, barSpan);
  const box = el("div", { className: "progress", hidden: true }, label, bar);
  return { box, label, barSpan };
}

// Which of `extract`/`standspots`/`viewerdata`/`render` a map still needs, in run order.
// "Подготовить" only runs these - not the ones it already has - so a machine missing only the
// radar (say, the CS2 install is gone) still gets the radar instead of failing at `extract`.
// `render` (`s6i_render_job_areas3d.md`: "одной кнопкой получает всё... И 3D-файлы карты") joins
// the chain here only for a map that isn't otherwise ready yet; a map already ready for 2D but
// still missing 3D keeps its "Открыть" button here - it gets a dedicated "Подготовить 3D" button
// on the map screen instead (`showMapScreen`'s 3D toggle).
function missingKinds(m) {
  const kinds = [];
  if (!m.hasLineups) {
    kinds.push("extract");
  }
  if (!m.hasStandSpots) {
    kinds.push("standspots");
  }
  if (!m.hasRadar) {
    kinds.push("viewerdata");
  }
  if (!hasUsableRender(m)) {
    kinds.push("render");
  }
  return kinds;
}

// ---- running jobs: `state.activeJobs` holds one record per map with a job in flight, so a
// map-list re-render (switching maps, saving settings, a finished sibling job) never orphans it;
// `attachDom`/`renderReconnectInto` (re)bind it to whatever DOM the current render produced.

function startElapsedTimer(record) {
  stopElapsedTimer(record);
  record.timer = setInterval(() => paintLabel(record), 1000);
}

function stopElapsedTimer(record) {
  if (record.timer) {
    clearInterval(record.timer);
    record.timer = null;
  }
}

function finishRecord(record) {
  stopElapsedTimer(record);
  state.activeJobs.delete(record.map);
}

// Ticks the elapsed-time label even between progress lines (`extract` emits none), so a
// multi-minute stage doesn't look hung at "0/1".
function paintLabel(record) {
  if (!record.sink || !record.currentKind) {
    return;
  }
  // Russian decimal comma (round-2 design critique 9).
  const elapsed = ((Date.now() - record.startedAt) / 1000).toFixed(1).replace(".", ",");
  const msg = record.lastMsg;
  // A render job's own phase lines (`{stage:"render",phase:"entities"|"lighting"|"skybox"}`) carry
  // no `total` - previously fell through to "в очереди" ("queued") mid-job, even though the job was
  // running (review_f3a8 fix item 3).
  const progressText = msg?.total
    ? `${msg.done ?? 0}/${msg.total}`
    : msg?.phase
      ? (strings.maps.renderPhases[msg.phase] ?? msg.phase)
      : strings.maps.stageQueued;
  record.sink.label.textContent = `${stageLabel(record.currentKind)} - ${progressText} (${elapsed} c)`;
}

// Binds `record` to a freshly rendered card's progress DOM and repaints it from the record's
// current state, so a re-render of a still-running job shows exactly where it is.
function attachDom(record, dom) {
  record.sink = dom;
  dom.progressBox.hidden = false;
  dom.disableButtons.forEach((b) => (b.disabled = true));
  dom.cancelBtn.hidden = false;
  dom.cancelBtn.onclick = () => record.controller?.cancel();
  paintLabel(record);
  const msg = record.lastMsg;
  dom.barSpan.style.width =
    msg?.total ? `${Math.min(100, Math.round((msg.done / msg.total) * 100))}%` : "0%";
}

function runChain(record, kinds, onSettled) {
  record.broken = false;
  record.remainingKinds = kinds;
  startElapsedTimer(record);

  const controller = startPrepare(
    record.map,
    {
      onStage: (kind) => {
        record.currentKind = kind;
        record.lastMsg = null;
        paintLabel(record);
        if (record.sink) {
          record.sink.barSpan.style.width = "0%";
        }
      },
      onLine: (msg) => {
        // The terminal line (`result`/`error`/`status`) carries no `total` - skip it, or it
        // rewrites a just-finished stage's label back to "в очереди".
        if (msg.result || msg.error || msg.status) {
          return;
        }
        record.lastMsg = msg;
        paintLabel(record);
        if (typeof msg.done === "number" && msg.total && record.sink) {
          record.sink.barSpan.style.width = `${Math.min(100, Math.round((msg.done / msg.total) * 100))}%`;
        }
      },
    },
    kinds,
  );
  record.controller = controller;

  controller.promise.then((outcome) => {
    stopElapsedTimer(record);
    if (record.sink) {
      record.sink.disableButtons.forEach((b) => (b.disabled = false));
      record.sink.cancelBtn.hidden = true;
    }
    if (outcome.done) {
      finishRecord(record);
      if (record.sink) {
        record.sink.label.textContent = strings.maps.stageDone;
      }
      onSettled(outcome);
      return;
    }
    if (outcome.cancelled) {
      finishRecord(record);
      if (record.sink) {
        record.sink.progressBox.hidden = true;
      }
      return;
    }
    if (outcome.broken) {
      record.broken = true;
      record.jobId = outcome.jobId;
      const idx = kinds.indexOf(record.currentKind);
      record.remainingKinds = kinds.slice(idx < 0 ? 0 : idx);
      if (record.sink) {
        renderReconnectInto(record, onSettled);
      }
      return;
    }
    // A real error: refresh the map list too, since "extract succeeded, standspots failed" still
    // needs its readiness pills updated.
    finishRecord(record);
    if (record.sink) {
      record.sink.label.textContent = `${strings.errors.genericPrefix}: ${outcome.error ?? strings.errors.serverDown}`;
    }
    onSettled(outcome);
  });
}

function startJobFlow(kinds, map, dom, onSettled) {
  const record = {
    map,
    startedAt: Date.now(),
    controller: null,
    currentKind: null,
    lastMsg: null,
    broken: false,
    jobId: null,
    remainingKinds: kinds,
    sink: null,
    timer: null,
  };
  state.activeJobs.set(map, record);
  attachDom(record, dom);
  runChain(record, kinds, onSettled);
}

// Renders the "stream dropped, reconnect?" button into `record.sink.progressBox`, appending it
// rather than replacing the box's children - the progress handler keeps writing into `label`/
// `barSpan` for as long as `record.sink` points at them.
function renderReconnectInto(record, onSettled) {
  const dom = record.sink;
  if (!dom) {
    return;
  }
  dom.label.textContent = strings.errors.streamBroken;
  const btn = el("button", { type: "button", textContent: strings.errors.reconnectButton });
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    const outcome = await reconnectJob(record.jobId, {
      onLine: (msg) => {
        if (msg.result || msg.error || msg.status) {
          return;
        }
        if (typeof msg.done === "number" && msg.total && record.sink) {
          record.sink.barSpan.style.width = `${Math.min(100, Math.round((msg.done / msg.total) * 100))}%`;
        }
      },
    });
    if (outcome.done) {
      btn.remove();
      // Carry on with whatever the broken stage's chain still owed - a dropped `extract` stream
      // must not skip `standspots`/`viewerdata` once it's confirmed done.
      const remaining = record.remainingKinds.slice(1);
      if (remaining.length > 0) {
        if (record.sink) {
          attachDom(record, record.sink);
        }
        runChain(record, remaining, onSettled);
      } else {
        finishRecord(record);
        if (record.sink) {
          record.sink.label.textContent = strings.maps.stageDone;
        }
        onSettled(outcome);
      }
      return;
    }
    btn.disabled = false;
    if (outcome.broken) {
      record.jobId = outcome.jobId;
      if (record.sink) {
        record.sink.label.textContent = strings.errors.streamBroken;
      }
      return;
    }
    finishRecord(record);
    if (record.sink) {
      record.sink.label.textContent = outcome.error ?? strings.errors.streamBroken;
    }
    onSettled(outcome);
  });
  dom.progressBox.append(btn);
}

// `GET /api/jobs` on boot: reattaches to anything still queued or running so a reload (or a
// first paint after opening the app) doesn't lose track of it.
async function reattachJobs() {
  const { data, error } = await fetchJobs();
  if (error !== undefined || !Array.isArray(data)) {
    return;
  }
  let any = false;
  for (const j of data) {
    if (j.status !== "queued" && j.status !== "running") {
      continue;
    }
    if (state.activeJobs.has(j.map)) {
      continue;
    }
    const record = {
      map: j.map,
      startedAt: Date.now(),
      controller: { cancel: () => deleteJob(j.id) },
      currentKind: j.kind,
      lastMsg: null,
      broken: false,
      jobId: j.id,
      remainingKinds: [j.kind],
      sink: null,
      timer: null,
    };
    state.activeJobs.set(j.map, record);
    startElapsedTimer(record);
    reconnectJob(j.id, {
      onLine: (msg) => {
        if (msg.result || msg.error || msg.status) {
          return;
        }
        record.lastMsg = msg;
        paintLabel(record);
        if (typeof msg.done === "number" && msg.total && record.sink) {
          record.sink.barSpan.style.width = `${Math.min(100, Math.round((msg.done / msg.total) * 100))}%`;
        }
      },
    }).then((outcome) => {
      stopElapsedTimer(record);
      if (record.sink) {
        record.sink.disableButtons.forEach((b) => (b.disabled = false));
        record.sink.cancelBtn.hidden = true;
      }
      if (outcome.done) {
        finishRecord(record);
        if (record.sink) {
          record.sink.label.textContent = strings.maps.stageDone;
        }
        // Refetch, not a plain re-render (round-2 review finding 3): `state.maps` is whatever was
        // loaded before this job finished - a completed job changes readiness/status, so the
        // list needs `/api/maps` again, not just its own stale entries redrawn.
        if (state.screen === "maps") {
          showMapsScreen();
        }
        return;
      }
      if (outcome.cancelled) {
        finishRecord(record);
        if (record.sink) {
          record.sink.progressBox.hidden = true;
        }
        return;
      }
      if (outcome.broken) {
        record.broken = true;
        record.jobId = outcome.jobId;
        if (record.sink) {
          renderReconnectInto(record, () => showMapsScreen());
        }
        return;
      }
      finishRecord(record);
      if (record.sink) {
        record.sink.label.textContent = `${strings.errors.genericPrefix}: ${outcome.error ?? strings.errors.serverDown}`;
      }
      if (state.screen === "maps") {
        showMapsScreen();
      }
    });
    any = true;
  }
  if (any && state.screen === "maps") {
    renderMapsScreen();
  }
}

// The map card's own thumbnail - prefers the game's own loading-screen screenshot
// (`mapArtUrl`/`s6p_map_art.md`, a parallel task; 404s cleanly if that endpoint or file isn't
// there yet), falls back to our recolored radar (`viewer-map.png` is a class/height data texture,
// not a human-viewable photo, so it's recolored the same way the map screen's own canvas is via
// `map2d.js`'s `renderThumbnail`), and only ever leaves the plain placeholder icon if both fail -
// never an empty box (round-2 design critique 7).
function renderThumbSlot(m) {
  const placeholder = el("div", { className: "map-card-thumb-empty", innerHTML: icon("layers", 28) });

  function useRecolor() {
    if (!m.hasRadar) {
      return;
    }
    const raw = new Image();
    raw.onload = () => {
      try {
        placeholder.replaceWith(el("img", { className: "map-card-thumb", alt: "", src: renderThumbnail(raw, state.theme) }));
      } catch {
        // Leave the placeholder - a broken thumbnail must not break the card.
      }
    };
    raw.src = `${radarPngUrl(m.map)}?v=${encodeURIComponent(m.build ?? "0")}`;
  }

  const art = new Image();
  art.onload = () => placeholder.replaceWith(el("img", { className: "map-card-thumb", alt: "", src: art.src }));
  art.onerror = useRecolor;
  art.src = mapArtUrl(m.map, "screenshot");
  return placeholder;
}

function renderMapCard(m) {
  const allReady = m.hasLineups && m.hasStandSpots && m.hasRadar;
  const status = mapStatusInfo(m);
  const thumb = renderThumbSlot(m);

  const actions = el("div", { className: "map-card-actions" });
  const { box: progressBox, label, barSpan } = renderProgressBox();
  const cancelBtn = el("button", { type: "button", textContent: strings.maps.cancelButton, hidden: true });
  const record = state.activeJobs.get(m.map);

  if (record) {
    const dom = { progressBox, label, barSpan, disableButtons: [], cancelBtn };
    if (record.broken) {
      progressBox.hidden = false;
      cancelBtn.hidden = true;
      record.sink = dom;
      renderReconnectInto(record, () => showMapsScreen());
    } else {
      attachDom(record, dom);
    }
  } else if (allReady) {
    const openBtn = el("button", { type: "button", className: "primary", textContent: strings.maps.openButton });
    openBtn.addEventListener("click", () => selectMap(m.map));
    actions.append(openBtn);
  } else {
    const prepareBtn = el("button", { type: "button", className: "primary", textContent: strings.maps.prepareButton });
    prepareBtn.addEventListener("click", () => {
      startJobFlow(missingKinds(m), m.map, { progressBox, label, barSpan, disableButtons: [prepareBtn], cancelBtn }, () => showMapsScreen());
    });
    actions.append(prepareBtn);
  }
  if (m.stale && !record) {
    const reextractBtn = el("button", { type: "button", textContent: strings.maps.reextractButton });
    reextractBtn.addEventListener("click", () => {
      startJobFlow(["extract"], m.map, { progressBox, label, barSpan, disableButtons: [reextractBtn], cancelBtn }, () => showMapsScreen());
    });
    actions.append(reextractBtn);
  }
  actions.append(cancelBtn);

  const mode = modeLabel(mapPrefix(m.map));
  const statusRow = el("div", { className: "map-card-status" }, el("span", { className: `badge ${status.cls}`, textContent: status.text }));
  if (mode) {
    statusRow.append(el("span", { className: "badge badge-neutral", textContent: mode }));
  }
  const body = el(
    "div",
    { className: "map-card-body" },
    el("span", { className: "map-card-title", textContent: prettifyMapName(m.map) }),
    el("span", { className: "map-card-sub", textContent: m.map }),
    statusRow,
  );
  // The stale reason (round-2 review finding 11 - dropped in the S6m redesign): whether the cache
  // has simply never been checked against an installed game (no game dir configured yet) or was
  // built from a different game build.
  if (m.stale) {
    const reason = state.config?.gameDir ? strings.maps.staleWrongBuild : strings.maps.staleNoGameDir;
    body.append(el("p", { className: "hint status-error", textContent: reason }));
  }
  body.append(actions, progressBox);
  return el("div", { className: "map-card" }, thumb, body);
}

function renderExtractNewForm(compact = false) {
  const input = el("input", {
    id: "extract-map-name",
    type: "text",
    placeholder: strings.maps.extractNamePlaceholder,
  });
  const label = el("label", { htmlFor: "extract-map-name", textContent: strings.maps.extractNewLabel });
  const btn = el("button", { type: "button", className: "primary", textContent: strings.maps.extractButton });
  const cancelBtn = el("button", { type: "button", textContent: strings.maps.cancelButton, hidden: true });
  const { box: progressBox, label: progressLabel, barSpan } = renderProgressBox();

  btn.addEventListener("click", () => {
    const name = input.value.trim();
    if (!name) {
      return;
    }
    startJobFlow(
      ["extract"],
      name,
      { progressBox, label: progressLabel, barSpan, disableButtons: [btn, input], cancelBtn },
      () => showMapsScreen(),
    );
  });

  const children = [
    label,
    el("div", { className: "field-row" }, input, btn, cancelBtn),
    progressBox,
  ];
  if (!compact) {
    // Only the empty-list case reaches here uncompacted (round-2 review finding 11 - dropped in
    // the S6m redesign) - explains why there's nothing above this form yet.
    children.unshift(el("p", { className: "hint", textContent: strings.maps.noMaps }));
  }
  return el("div", { className: compact ? "field" : "field card" }, ...children);
}

function selectMap(map) {
  showMapScreen(map);
  putConfig({ lastMap: map });
}

// ---- screen 3: map + target + solve -------------------------------------------------------------

const ALL_TYPES = ["Stand", "Crouch", "JumpThrow", "CrouchJumpThrow"];
const ALL_STRENGTHS = [1, 0.5, 0];

// `opts.autoBody`: a full `/api/lineup` request body reconstructed from the address bar
// (`bodyFromHash`) - when given, the target/origin/params it carries are applied and the solve
// starts immediately once the radar has loaded.
async function showMapScreen(map, opts = {}) {
  state.screen = "map";
  state.currentMap = map;
  destroyCurrentMapView();
  radarView = null;
  setAppScroll(false);
  setGlobalTopbarVisible(false);
  app.replaceChildren();
  announce(map);

  const mapSummary = state.maps.find((m) => m.map === map) ?? {};

  // ---- combined header row: back / map name / 2D-3D / theme ----
  const backBtn = el("button", { type: "button", className: "btn-ghost", textContent: strings.mapScreen.backToList });
  backBtn.addEventListener("click", showMapsScreen);
  const mapTitleEl = el("h1", { className: "map-title", textContent: prettifyMapName(map), title: map });
  const mapBetaBadge = el("span", { className: "pill badge-beta", hidden: true });
  syncBetaBadge(mapBetaBadge);
  const mapReportBtn = el("button", { type: "button", className: "btn-ghost report-issue-btn" });
  wireReportIssueButton(mapReportBtn);
  const viewModeSeg = segmented({
    name: "view-mode",
    ariaLabel: "2D/3D",
    value: "2d",
    options: [
      { value: "2d", label: strings.view3d.toggle2d },
      { value: "3d", label: strings.view3d.toggle3d },
    ],
    onChange: (v) => switchViewMode(v),
  });
  const localThemeBtn = el("button", { type: "button", className: "icon-btn btn-ghost" });
  syncThemeButton(localThemeBtn);
  localThemeBtn.addEventListener("click", () => toggleThemeAnd(() => syncThemeButton(localThemeBtn)));
  const mapHeader = el(
    "header",
    { className: "topbar map-topbar" },
    el("div", { className: "topbar-left" }, backBtn, mapTitleEl, mapBetaBadge),
    viewModeSeg,
    el("div", { className: "topbar-right" }, mapReportBtn, localThemeBtn),
  );

  // ---- centre stage: radar / 3D, with the 3D toolbar overlaid top-left ----
  const canvas = el("canvas", { id: "radar-canvas", role: "img", "aria-label": `Радар карты ${map}` });
  const threeContainer = el("div", { className: "three-container", hidden: true });
  const fpvOverlay = el("div", { className: "fpv-overlay", hidden: true });
  const caption = el("p", { className: "stage-caption" });

  const collisionsBtn = el("button", { type: "button", textContent: strings.view3d.collisionsOn, hidden: true });
  const cameraModeBtn = el("button", { type: "button", textContent: strings.view3d.modeOrbit, hidden: true });
  // F3b-2: "game"/"simple" lighting toggle (`s6f3b2_lighting_shader.md` §7) - hidden until a 3D view
  // exists and says this map actually has the data for it (`sceneView.isLightingSupported()`).
  const lightingBtn = el("button", { type: "button", textContent: strings.view3d.lightingSimple, hidden: true });
  const view3dStatus = el("span", { className: "hint" });
  // `s6i_render_job_areas3d.md` change item 2: a map already ready for 2D but still missing a
  // usable 3D export gets this button in place of the plain "нет 3D-данных" message, instead of
  // sending the user back to the map list (whose own "Подготовить" only appears while *something*
  // 2D is still missing too - see `missingKinds`). Hidden until `switchViewMode` finds it needed.
  const prepare3dBtn = el("button", { type: "button", className: "primary", textContent: strings.view3d.prepareButton, hidden: true });
  const {
    box: render3dProgressBox,
    label: render3dProgressLabel,
    barSpan: render3dBarSpan,
  } = renderProgressBox();
  const render3dCancelBtn = el("button", { type: "button", textContent: strings.maps.cancelButton, hidden: true });
  // `s6p_map_art.md` (round-2 design critique 8): the official radar as the 2D base layer - both
  // start hidden and only ever appear once `fetchOverview` actually succeeds for this map.
  const schemeBtn = el("button", { type: "button", textContent: strings.mapScreen.schemeToggle, hidden: true });
  const sectionRow = el("div", { className: "view-toolbar-row", role: "group", "aria-label": strings.mapScreen.schemeToggle, hidden: true });
  const viewToolbar = el(
    "div",
    { className: "view-toolbar" },
    el("div", { className: "view-toolbar-row", role: "group", "aria-label": "2D" }, schemeBtn),
    sectionRow,
    el("div", { className: "view-toolbar-row", role: "group", "aria-label": "3D" }, collisionsBtn, cameraModeBtn, lightingBtn),
    view3dStatus,
    el("div", { className: "view-toolbar-row" }, prepare3dBtn, render3dCancelBtn),
    render3dProgressBox,
  );
  const stage = el("div", { className: "map-stage" }, canvas, threeContainer, fpvOverlay, viewToolbar, caption);

  // ---- left column: the 3 steps + run button + "Дополнительно" ----
  const targetModeSeg = segmented({
    name: "target-mode",
    ariaLabel: strings.mapScreen.targetModeLabel,
    value: "point",
    options: [
      { value: "point", label: strings.mapScreen.targetModePoint },
      { value: "area", label: strings.mapScreen.targetModeArea },
      { value: "sightline", label: strings.mapScreen.targetModeSightline },
    ],
    onChange: (v) => setTargetMode(v),
  });
  const targetStepBody = el("div", {});
  const targetCard = el(
    "div",
    { className: "card" },
    el("div", { className: "card-heading" }, el("span", { className: "step-number", textContent: "1" }), el("span", { textContent: strings.mapScreen.stepTarget })),
    targetModeSeg,
    targetStepBody,
  );

  const originStepBody = el("div", {});
  const originCard = el(
    "div",
    { className: "card" },
    el("div", { className: "card-heading" }, el("span", { className: "step-number", textContent: "2" }), el("span", { textContent: strings.mapScreen.stepOrigin })),
    originStepBody,
  );

  const throwStepBody = el("div", {});
  const throwCard = el(
    "div",
    { className: "card" },
    el("div", { className: "card-heading" }, el("span", { className: "step-number", textContent: "3" }), el("span", { textContent: strings.mapScreen.stepThrow })),
    throwStepBody,
  );

  const runCardBody = el("div", {});
  const runCard = el("div", { className: "card" }, runCardBody);

  const advancedContent = el("div", {});
  const advancedBox = collapsible({
    summary: strings.solveParams.heading,
    content: advancedContent,
    storageKey: "cs2-modulation-advanced-open",
    defaultOpen: false,
  });

  const stepsCol = el("div", { className: "map-steps" }, targetCard, originCard, throwCard, runCard, advancedBox);
  const resultsCol = el("div", { className: "map-results" });

  const tabsSeg = segmented({
    name: "map-tabs",
    value: "steps",
    options: [
      { value: "steps", label: "Настройка" },
      { value: "results", label: strings.panel.heading },
    ],
    onChange: (v) => {
      stepsCol.classList.toggle("tab-hidden", v !== "steps");
      resultsCol.classList.toggle("tab-hidden", v !== "results");
    },
  });
  resultsCol.classList.add("tab-hidden");
  const tabsWrap = el("div", { className: "map-tabs" }, tabsSeg);

  const body = el("div", { className: "map-body" }, tabsWrap, stepsCol, stage, resultsCol);
  const screen = el("div", { className: "map-screen" }, mapHeader, body);
  app.append(screen);

  const { data: radarData, error: radarError } = await fetchRadar(map);
  if (radarError !== undefined) {
    caption.className = "stage-caption status-error";
    caption.textContent = radarError ?? strings.errors.serverDown;
    return;
  }

  // ---- per-screen solve state ----
  const solveState = {
    targetMode: "point", // "point" | "area" (`s6g2_target_area.md`) | "sightline" (`s6r_sightline_target.md`)
    target: null, // { x, y, z, label }
    targetArea: null, // { polygon: [[x,y],...] } - mutually exclusive with `target`
    // `s6r_sightline_target.md`: `{ from: {x,y,z,label}, to: {x,y,z,label} }` (eye points, already
    // +64 above the clicked floor) - mutually exclusive with `target`/`targetArea`.
    sightline: null,
    origin: null, // { x, y, reach }
    originArea: null, // { polygon: [[x,y],...] } (`s6g_origin_area.md`) - mutually exclusive with `origin`
    params: {
      scope: "all", originReach: 300, tolerance: 80, minStability: 0.4, fineScan: false,
      types: [...ALL_TYPES], strengths: [...ALL_STRENGTHS], broken: [], originPin: null,
      areaZMin: null, areaZMax: null, targetAreaZMin: null, targetAreaZMax: null,
    },
    running: false,
    controller: null,
  };
  let pendingLevels = null; // { x, y, levels }
  let mapView = null;
  let sceneView = null; // lazily created on first switch to 3D, kept alive alongside mapView
  let viewMode = "2d"; // "2d" | "3d"
  let originStatusBox = null; // point-status `.step-row` text span
  let originAreaStatusBox = null; // area vertex-count hint
  let originPointBlock = null;
  let originAreaBlock = null;
  let targetStatusBox = null;
  let runRefs = null;
  let scopeSegRef = null;
  // The origin-scope segmented's own UI intent ("all" | "spawns" | "point" | "area"), distinct
  // from `solveState.params.scope` (server-relevant, only ever "all"/"spawns") - picking "точка"/
  // "область" before anything is actually placed has nothing else to remember that choice by, and
  // reading `solveState.params.scope` back for it just snapped the segmented back to "Карта".
  let originScopeUi = "all";
  // The origin-area tool (`s6g_origin_area.md`): `areaMode` mirrors `mapView`'s own draw-mode
  // flag, `areaDraftCount` is the in-progress vertex count before the polygon is closed (once
  // closed, `solveState.originArea.polygon.length` is used instead).
  let areaMode = false;
  let areaDraftCount = 0;
  // Review fix item 3: whether there is actually an unfinished draft right now (`!closed && count >
  // 0`) - `areaDraftCount` alone can't tell a genuine in-progress draft apart from the count of a
  // polygon that just closed (same field, `handleAreaChange` sets it from `points.length`
  // regardless of `closed`), which made the "draft discarded" hint fire on a clean 2D<->3D switch.
  let areaDraftOpen = false;
  let areaToggleBtnRef = null;
  let areaDeleteBtnRef = null;
  // The target-area tool (`s6g2_target_area.md`), same pattern as the origin-area one above, key
  // `"target"` in `mapView`'s own two-area API.
  let targetAreaMode = false;
  let targetAreaDraftCount = 0;
  let targetAreaDraftOpen = false;
  let targetAreaToggleBtnRef = null;
  let targetAreaDeleteBtnRef = null;
  // The stacked-floor level buttons (review G2 round 3, decision 4): the clusters
  // `prefillAreaZRange` found under the most recently closed area, one per key, or `null` before
  // any area has been closed or once one clears - `renderOriginStep`/`renderTargetAreaControls`
  // only show the button row while there is more than one cluster.
  let areaLevels = null;
  let targetAreaLevels = null;
  // `s6r_sightline_target.md`: the first eye point, while waiting for the second click -
  // `{ x, y, z, label }`, `z` already lifted by +64. `null` once both are placed
  // (`solveState.sightline`) or before the first click.
  let sightlineDraft = null;
  // The last result/selection, replayed into a 3D view created after they already happened
  // (`ensureSceneView`) - `panel.js` owns the definitive copies, these just let a freshly built
  // view catch up without re-running the solve.
  let lastLineups = [];
  let lastSelectedId = null;

  // Every view currently alive for this screen - state changes (target/origin/lineups/selected)
  // broadcast to all of them, so whichever is visible after a toggle is already correct
  // (`s6f3b_viewer3d.md`: "состояние общее").
  function views() {
    return [mapView, sceneView].filter(Boolean);
  }

  // `s6i_render_job_areas3d.md` change item 3: keeps the 3D view's own area prisms in sync with
  // the 2D tool's polygon and height range - called after anything that changes
  // `solveState.originArea`/`targetArea` or their z fields. A no-op before a 3D view exists;
  // `ensureSceneView` replays the current state for both keys once it's created.
  function updateSceneArea(key) {
    if (!sceneView) {
      return;
    }
    const area = key === "origin" ? solveState.originArea : solveState.targetArea;
    if (!area) {
      sceneView.clearArea(key);
      return;
    }
    const zMin = key === "origin" ? solveState.params.areaZMin : solveState.params.targetAreaZMin;
    const zMax = key === "origin" ? solveState.params.areaZMax : solveState.params.targetAreaZMax;
    sceneView.setArea(key, area.polygon, zMin, zMax);
  }

  // The origin-scope segmented's true value from state, not from whatever was last clicked -
  // a right-click origin or a closed/drafting area sets it on their own (mirrors the old
  // dynamically-appearing "точка" `<option>`).
  function currentScopeValue() {
    if (solveState.origin) {
      return "point";
    }
    if (solveState.originArea || areaMode) {
      return "area";
    }
    return originScopeUi;
  }

  const panel = createPanel(resultsCol, {
    onSelect: (id) => {
      lastSelectedId = id;
      for (const v of views()) v.setSelected(id);
    },
    onHoverEnter: (id) => {
      for (const v of views()) v.setHover(id);
    },
    onHoverLeave: () => {
      for (const v of views()) v.setHover(null);
    },
    onFirstPerson: (l) => handleFirstPerson(l),
    onShow3d: () => switchViewMode("3d"),
    requestPreview: (l, kind) => requestPreview(l, kind),
    reportIssueUrl: (l) => reportIssueUrl(l),
  });
  // `panel`'s own preview Blob URLs otherwise outlive this screen (`destroyCurrentMapView` never
  // touched it) - `clear()` already revokes them, so a `destroy` that just calls it is enough
  // (round-2 review finding 8).
  currentViews.push({ destroy: () => panel.clear() });

  function applyTarget(t) {
    solveState.target = t;
    pendingLevels = null;
    // A point target and a target area are mutually exclusive (`s6g2_target_area.md`) - picking
    // a point clears any drawn area.
    if (solveState.targetArea || targetAreaMode) {
      clearTargetAreaState();
      updateTargetAreaButtons();
    }
    for (const v of views()) v.setTarget(t);
    renderTargetBox();
    updateTargetStatus();
    updateOfficialSectionForTarget();
  }

  // RED-2: a click that only narrows down to a level choice must not leave the previous
  // target (and its cross on the map) in place - otherwise "run" solves for the old point.
  function clearTarget() {
    solveState.target = null;
    for (const v of views()) v.clearTarget();
  }

  // `s6r_sightline_target.md`: how far above a clicked floor an eye sits, for both ends of the
  // sightline - "clicked floor z + 64" (the spec's own number, not `solve.js`'s standing-eye
  // constant, which converts a pasted `setpos` the other way).
  const SIGHTLINE_EYE_LIFT = 64;

  function clearSightlineState() {
    solveState.sightline = null;
    sightlineDraft = null;
    for (const v of views()) v.clearSightline();
  }

  // `s6r_sightline_target.md`: one endpoint (already eye-lifted) of the two-click sightline. The
  // first call becomes "откуда смотрят" (`sightlineDraft`); the second becomes "куда смотрят",
  // completing `solveState.sightline` and clearing the draft.
  function applySightlinePoint(eye) {
    // Review fix: a click here always names a fresh point, so any stacked-level chooser left over
    // from resolving it is done its job - leaving it up let a second pick in it silently become
    // "куда" at the same x,y (a 40u vertical lane).
    pendingLevels = null;
    if (!sightlineDraft) {
      // Review fix: a third click (after a sightline was already completed) starts a brand new
      // draft - drop the finished sightline first, or its status/lane stayed on screen while only
      // a lone marker was actually drawn, and "Найти раскидки" solved the stale pair.
      solveState.sightline = null;
      for (const v of views()) v.clearSightline();
      sightlineDraft = eye;
      for (const v of views()) v.setSightline({ from: sightlineDraft, to: null });
    } else {
      solveState.sightline = { from: sightlineDraft, to: eye };
      sightlineDraft = null;
      for (const v of views()) v.setSightline(solveState.sightline);
    }
    renderTargetBox();
  }

  // Same two paths as `handleMapClick` below (3D hit point vs. a 2D click resolved through
  // `/api/levels`), routed into `applySightlinePoint` instead of `applyTarget` - see that
  // function's own comment for why `wz` skips the levels lookup for the height itself.
  async function handleSightlineClick(wx, wy, wz, normalZ) {
    if (wz !== undefined) {
      caption.textContent = "";
      // Review fix: +64 only lifts a floor/roof click (an up-facing, near-horizontal surface -
      // `normalZ` close to 1) to a standing eye height - a click on a wall already names a spot at
      // that exact height (a ledge, a windowsill), so the clicked point itself is the eye there.
      const eyeZ = normalZ >= 0.7 ? wz + SIGHTLINE_EYE_LIFT : wz;
      const eye = { x: wx, y: wy, z: eyeZ, label: null };
      applySightlinePoint(eye);
      fetchLevels(map, wx, wy).then(({ data }) => {
        if (sightlineDraft !== eye && solveState.sightline?.from !== eye && solveState.sightline?.to !== eye) {
          return; // superseded while this was in flight
        }
        let label = null;
        let bestDist = Infinity;
        for (const lvl of data?.levels ?? []) {
          const dist = Math.abs(lvl.z - wz);
          if (dist < bestDist) {
            bestDist = dist;
            label = lvl.name ?? null;
          }
        }
        eye.label = bestDist <= 64 ? label : null;
        renderTargetBox();
      });
      return;
    }
    const { data, error } = await fetchLevels(map, wx, wy);
    if (error !== undefined) {
      caption.className = "stage-caption status-error";
      caption.textContent = error ?? strings.errors.serverDown;
      return;
    }
    const levels = data.levels ?? [];
    if (levels.length === 0) {
      caption.className = "stage-caption status-error";
      caption.textContent = strings.mapScreen.noFloorHere;
      return;
    }
    caption.className = "stage-caption";
    caption.textContent = "";
    if (levels.length === 1) {
      applySightlinePoint({ x: wx, y: wy, z: levels[0].z + SIGHTLINE_EYE_LIFT, label: levels[0].name ?? null });
    } else {
      pendingLevels = { x: wx, y: wy, levels, forSightline: true };
      renderTargetBox();
    }
  }

  // `wz`, when given (a 3D click - the ray already hit a real surface), skips using `/api/levels`
  // to resolve an ambiguous z - the hit point IS the target's height, no choice to make
  // (`s6f3b_viewer3d.md`: "точка попадания с высотой = цель"). It's still fetched for its nav place
  // name (coordinator follow-up to `s6k_draw_in_3d.md`): a 3D ray can pass clean through an opening
  // (e.g. a window) and land on a floor the user never meant to see - the closest level's own name,
  // when the click landed within 64u of it, makes that obvious immediately.
  // `normalZ`, when given (a 3D click - `scene3d.js`'s own world-space hit-normal Z), is only
  // read by the sightline mode below; every other mode here (and every 2D click, which never has
  // one) simply ignores the extra argument.
  async function handleMapClick(wx, wy, wz, normalZ) {
    // In "область" mode a plain map click is only ever meant to place a target-area vertex
    // (through the area tool's own handler, not this one) - ignore it here instead of quietly
    // setting a point target the user never asked for (`s6g2_target_area.md`).
    if (solveState.targetMode === "area") {
      return;
    }
    if (solveState.targetMode === "sightline") {
      return handleSightlineClick(wx, wy, wz, normalZ);
    }
    if (wz !== undefined) {
      caption.textContent = "";
      // Review fix item 4: apply the target immediately (no ambiguity to resolve, the hit point IS
      // it) and fill the label in once `/api/levels` answers, instead of awaiting it first - a
      // second, faster click while the first's request was still in flight used to race, letting a
      // slower reply re-apply a now-stale target over the newer one, and every 3D pick paid the
      // round trip's latency for a label that's a nice-to-have, not the target itself.
      const t = { x: wx, y: wy, z: wz, label: null };
      applyTarget(t);
      fetchLevels(map, wx, wy).then(({ data }) => {
        if (solveState.target !== t) {
          return; // superseded by a later click, or the mode changed, while this was in flight
        }
        let label = null;
        let bestDist = Infinity;
        for (const lvl of data?.levels ?? []) {
          const dist = Math.abs(lvl.z - wz);
          if (dist < bestDist) {
            bestDist = dist;
            label = lvl.name ?? null;
          }
        }
        t.label = bestDist <= 64 ? label : null;
        renderTargetBox();
      });
      return;
    }
    const { data, error } = await fetchLevels(map, wx, wy);
    if (error !== undefined) {
      caption.className = "stage-caption status-error";
      caption.textContent = error ?? strings.errors.serverDown;
      return;
    }
    const levels = data.levels ?? [];
    if (levels.length === 0) {
      // AMBER-11: no nav mesh here - inventing z=0 would put the target in mid-air (and, on a
      // map whose geometry sits far from the origin, feed it straight into the server crash).
      clearTarget();
      pendingLevels = null;
      caption.className = "stage-caption status-error";
      caption.textContent = strings.mapScreen.noFloorHere;
      renderTargetBox();
      return;
    }
    caption.className = "stage-caption";
    caption.textContent = "";
    if (levels.length === 1) {
      applyTarget({ x: wx, y: wy, z: levels[0].z, label: levels[0].name ?? null });
    } else {
      clearTarget();
      pendingLevels = { x: wx, y: wy, levels };
      renderTargetBox();
    }
  }

  // Refreshes the point-origin `.step-row` (coords + radius, with a clear button) and the area
  // tool's vertex-count hint, then makes sure the right one of the two blocks is visible and the
  // scope segmented reflects reality (mirrors the old dynamically-appearing "точка" `<option>`).
  function updateOriginStatus() {
    scopeSegRef?.setValue(currentScopeValue());
    if (originPointBlock) {
      originPointBlock.hidden = currentScopeValue() !== "point";
    }
    if (originAreaBlock) {
      originAreaBlock.hidden = currentScopeValue() !== "area";
    }
    if (originStatusBox) {
      originStatusBox.hidden = !solveState.origin;
      if (solveState.origin) {
        originStatusBox.querySelector(".step-row-text").textContent =
          `${solveState.origin.x.toFixed(0)}, ${solveState.origin.y.toFixed(0)} · R=${solveState.origin.reach}`;
      }
    }
    if (originAreaStatusBox) {
      if (solveState.originArea) {
        originAreaStatusBox.textContent = strings.solveParams.areaStatus(solveState.originArea.polygon.length);
      } else if (areaDraftCount > 0) {
        originAreaStatusBox.textContent = strings.solveParams.areaStatus(areaDraftCount);
      } else {
        originAreaStatusBox.textContent = "";
      }
    }
  }

  // Updates the "draw/edit/delete area" buttons' text and visibility from the current state -
  // called after anything that changes `areaMode` or `solveState.originArea`.
  function updateAreaButtons() {
    syncView3dHint();
    if (!areaToggleBtnRef) {
      return;
    }
    areaToggleBtnRef.textContent = areaMode
      ? strings.solveParams.areaStopButton
      : solveState.originArea
        ? strings.solveParams.areaEditButton
        : strings.solveParams.areaDrawButton;
    areaToggleBtnRef.className = areaMode ? "primary" : "";
    if (areaDeleteBtnRef) {
      areaDeleteBtnRef.hidden = !solveState.originArea;
    }
  }

  // Discards the origin area (drafted or committed) and leaves the tool off - shared by the
  // "delete area" button, picking a point origin, and changing scope away from an area.
  function clearArea() {
    solveState.originArea = null;
    areaDraftCount = 0;
    areaDraftOpen = false;
    areaLevels = null;
    mapView.setArea("origin", null);
    updateSceneArea("origin");
    if (areaMode) {
      areaMode = false;
      for (const v of views()) v.setAreaMode("origin", false);
    }
  }

  // `wz`: only given by the 3D view's Shift+LMB pick (`s6k_draw_in_3d.md` item 4) - the exact floor
  // that click hit, passed through so the marker's own downward raycast picks the same one back out
  // of a stack instead of always the topmost; `buildQuery` never reads it (still only `x`/`y`/`reach`).
  function handleOriginClick(wx, wy, wz) {
    solveState.origin = { x: wx, y: wy, reach: solveState.params.originReach };
    if (wz !== undefined) {
      solveState.origin.z = wz;
    }
    for (const v of views()) v.setOrigin(solveState.origin);
    // A right-click origin and the origin area are mutually exclusive - placing one resets the
    // other (`s6g_origin_area.md`).
    clearArea();
    updateAreaButtons();
    updateOriginStatus();
  }

  // Vs. "точка", shows "область: N вершин" while the target-area tool has anything drawn
  // (`s6g2_target_area.md`: same status-line convention as the origin area).
  function updateTargetStatus() {
    if (!targetStatusBox) {
      return;
    }
    if (solveState.targetArea) {
      targetStatusBox.textContent = strings.solveParams.areaStatus(solveState.targetArea.polygon.length);
    } else if (targetAreaDraftCount > 0) {
      targetStatusBox.textContent = strings.solveParams.areaStatus(targetAreaDraftCount);
    } else {
      targetStatusBox.textContent = "";
    }
  }

  function updateTargetAreaButtons() {
    syncView3dHint();
    if (!targetAreaToggleBtnRef) {
      return;
    }
    targetAreaToggleBtnRef.textContent = targetAreaMode
      ? strings.solveParams.areaStopButton
      : solveState.targetArea
        ? strings.solveParams.areaEditButton
        : strings.solveParams.areaDrawButton;
    targetAreaToggleBtnRef.className = targetAreaMode ? "primary" : "";
    if (targetAreaDeleteBtnRef) {
      targetAreaDeleteBtnRef.hidden = !solveState.targetArea;
    }
  }

  // Discards the target area (drafted or committed) and leaves the tool off - shared by the
  // "delete area" button and picking a point target.
  function clearTargetAreaState() {
    solveState.targetArea = null;
    targetAreaDraftCount = 0;
    targetAreaDraftOpen = false;
    targetAreaLevels = null;
    mapView.setArea("target", null);
    updateSceneArea("target");
    if (targetAreaMode) {
      targetAreaMode = false;
      for (const v of views()) v.setAreaMode("target", false);
    }
  }

  // `s6k_draw_in_3d.md` item 3: the payload both area tools (2D's `map2d.js`, 3D's `scene3d.js`)
  // emit on every vertex change, `closed:true` once - shared so a polygon drawn in either view
  // behaves exactly the same way. `view` is whichever view actually emitted this change - the one
  // `setAreaMode(key, false)` needs to turn its own tool off on once the polygon closes.
  function handleAreaChange(key, view, { points, closed, zs }) {
    const isOrigin = key === "origin";
    // Review fix item 3: an unfinished draft right now, as opposed to `points.length` merely
    // holding the just-closed polygon's own vertex count - the two look identical in
    // `areaDraftCount` alone, which made a clean 2D<->3D switch (nothing to lose) claim a draft was
    // discarded.
    const draftOpen = !closed && points.length > 0;
    if (isOrigin) {
      areaDraftCount = points.length;
      areaDraftOpen = draftOpen;
    } else {
      targetAreaDraftCount = points.length;
      targetAreaDraftOpen = draftOpen;
    }
    const already = isOrigin ? solveState.originArea : solveState.targetArea;
    // Review fix item 1: `!already` alone missed a polygon closed in 3D when an area already
    // existed (re-arm via "Редактировать область", or a tool carried over from 2D that starts a
    // fresh 3D draft) - `closed` from `view !== mapView` (i.e. scene3d) is always a genuinely new
    // polygon regardless of `already`, since scene3d only ever emits `closed:true` once per polygon
    // (`closeDraft` is its one and only emitter, and re-arming an already-closed key in 3D always
    // starts over from an empty draft - `setAreaMode`'s own reset, `s6k_draw_in_3d.md` item 1).
    const justClosed = closed && (!already || view !== mapView);
    if (closed) {
      const area = { polygon: points.map((p) => [p.x, p.y]) };
      if (isOrigin) {
        solveState.originArea = area;
      } else {
        solveState.targetArea = area;
      }
      updateSceneArea(key);
      // A polygon closed in 3D must also appear on the 2D radar (and vice versa the 3D prism
      // already follows `updateSceneArea` above) - idempotent when `view` already IS `mapView`.
      mapView.setArea(key, area.polygon);
      if (isOrigin && solveState.origin) {
        solveState.origin = null;
        for (const v of views()) v.clearOrigin();
      }
      if (!isOrigin && solveState.target) {
        solveState.target = null;
        for (const v of views()) v.clearTarget();
      }
      if (justClosed) {
        if (isOrigin) {
          areaMode = false;
        } else {
          targetAreaMode = false;
        }
        view.setAreaMode(key, false);
        // Item 2: the clicked heights (3D only) set the z range directly, instead of waiting on
        // the nav-based `prefillAreaZRange` below (2D drawing never has `zs` at all).
        if (zs && zs.length > 0) {
          const zMin = Math.round(Math.min(...zs) - 24);
          const zMax = Math.round(Math.max(...zs) + 48);
          if (isOrigin) {
            solveState.params.areaZMin = zMin;
            solveState.params.areaZMax = zMax;
          } else {
            solveState.params.targetAreaZMin = zMin;
            solveState.params.targetAreaZMax = zMax;
          }
          updateSceneArea(key);
        }
        prefillAreaZRange(area.polygon).then((prefill) => {
          if (isOrigin) {
            areaLevels = prefill ? prefill.clusters : null;
          } else {
            targetAreaLevels = prefill ? prefill.clusters : null;
          }
          if (!zs || zs.length === 0) {
            if (!prefill) {
              // No nav under the new area (e.g. a roof): don't keep the previous area's range.
              if (isOrigin) {
                solveState.params.areaZMin = null;
                solveState.params.areaZMax = null;
              } else {
                solveState.params.targetAreaZMin = null;
                solveState.params.targetAreaZMax = null;
              }
            } else if (isOrigin) {
              [solveState.params.areaZMin, solveState.params.areaZMax] = prefill.range;
            } else {
              [solveState.params.targetAreaZMin, solveState.params.targetAreaZMax] = prefill.range;
            }
            updateSceneArea(key);
          }
          if (isOrigin) {
            renderOriginStep();
          } else {
            renderTargetBox();
          }
        });
      }
    }
    if (isOrigin) {
      updateAreaButtons();
      updateOriginStatus();
    } else {
      updateTargetAreaButtons();
      updateTargetStatus();
    }
  }

  // Groups sorted-ascending `zs` into clusters, starting a new one whenever the gap to the
  // previous value exceeds 64 (review G2 round 3, item 3 - the exact split a sloped single floor
  // needs to still read as one cluster, while two real stacked floors read as two). Each cluster
  // reports its own `[zMin, zMax]` plus a representative `z` (its own mode, so an exact,
  // unanimous floor height like a nav mesh's own literal z stays exactly that number).
  function clusterLevels(zs) {
    const sorted = [...zs].sort((a, b) => a - b);
    const groups = [];
    for (const z of sorted) {
      const last = groups[groups.length - 1];
      if (last && z - last[last.length - 1] <= 64) {
        last.push(z);
      } else {
        groups.push([z]);
      }
    }
    return groups.map((group) => {
      const counts = new Map();
      for (const z of group) {
        counts.set(z, (counts.get(z) ?? 0) + 1);
      }
      let mode = group[0];
      let modeCount = -1;
      for (const [z, count] of counts) {
        if (count > modeCount) {
          modeCount = count;
          mode = z;
        }
      }
      return { zMin: Math.min(...group), zMax: Math.max(...group), z: mode };
    });
  }

  // A freshly closed area has no height range yet, so as drawn it would cover every level under
  // it (e.g. every floor stacked under the same roof) - prefilling from the floor under the area
  // makes "this floor" the default instead, while the fields stay editable or clearable
  // afterward (`s6g2_target_area.md` decision 9b). Levels come from `/api/levels` at the
  // polygon's own vertices, its centroid, and its edge midpoints (review G2 round 4 - a stacked
  // floor under the middle of a drawn area, away from every vertex, still needs a sample point
  // nearby to be found at all).
  //
  // Whether to cluster at all depends on whether any single sampled *point* itself ever reports
  // more than one level there (review G2 round 4, correcting round 3's own item 3): a sloped
  // single floor has every point reporting exactly one level each, just at different heights as
  // the slope rises - clustering those by a flat z gap still cuts the floor in half. Only when at
  // least one point reports two-or-more levels *at that same point* is a real stack of floors
  // actually there; only then are the stacked points' own levels clustered (sort, split on >64
  // gaps), the top cluster becomes the default, and the level-picker buttons show. A single-level
  // point then joins whichever cluster its own z falls inside, or the nearest one.
  async function prefillAreaZRange(polygon) {
    const cx = polygon.reduce((sum, p) => sum + p[0], 0) / polygon.length;
    const cy = polygon.reduce((sum, p) => sum + p[1], 0) / polygon.length;
    const edgeMidpoints = polygon.map((p, i) => {
      const q = polygon[(i + 1) % polygon.length];
      return [(p[0] + q[0]) / 2, (p[1] + q[1]) / 2];
    });
    const points = [...polygon, [cx, cy], ...edgeMidpoints];
    const results = await Promise.all(points.map(([x, y]) => fetchLevels(map, x, y)));
    const perPointZ = results.map(({ data }) => {
      const seen = new Set();
      const zs = [];
      for (const lvl of data?.levels ?? []) {
        const z = Math.round(lvl.z);
        if (seen.has(z)) {
          continue;
        }
        seen.add(z);
        zs.push(z);
      }
      return zs;
    });
    const allZ = perPointZ.flat();
    if (allZ.length === 0) {
      return null;
    }
    if (!perPointZ.some((zs) => zs.length >= 2)) {
      // One level per point everywhere sampled - a single (possibly sloped) floor.
      return { range: [Math.min(...allZ) - 32, Math.max(...allZ) + 96], clusters: null };
    }
    const stackedZ = perPointZ.filter((zs) => zs.length >= 2).flat();
    const clusters = clusterLevels(stackedZ).sort((a, b) => b.z - a.z);
    for (const zs of perPointZ) {
      if (zs.length >= 2) {
        continue; // already part of the clustering above
      }
      for (const z of zs) {
        let best = null;
        let bestDist = Infinity;
        for (const c of clusters) {
          const dist = z < c.zMin ? c.zMin - z : z > c.zMax ? z - c.zMax : 0;
          if (dist < bestDist) {
            bestDist = dist;
            best = c;
          }
        }
        best.zMin = Math.min(best.zMin, z);
        best.zMax = Math.max(best.zMax, z);
      }
    }
    // Default to the floor under the most sampled points, not simply the top one: a second level
    // under a single corner must not pull the default away from the floor under the rest of the
    // area. Ties keep the top-first order, so a fully stacked spot still defaults to the upper
    // floor the radar shows.
    let chosen = clusters[0];
    let chosenCount = -1;
    for (const c of clusters) {
      const count = perPointZ.filter((zs) => zs.some((z) => z >= c.zMin && z <= c.zMax)).length;
      if (count > chosenCount) {
        chosenCount = count;
        chosen = c;
      }
    }
    return {
      range: [chosen.zMin - 32, chosen.zMax + 96],
      clusters: clusters.length > 1 ? clusters : null,
    };
  }

  // Review G2 round 3, decision 4: a row of small buttons next to the height fields, one per
  // stacked floor `prefillAreaZRange` found under the area - `null` (nothing rendered) while
  // there is only one (or none yet). `clusters` is already sorted top-first.
  function renderAreaLevelButtons(clusters, apply) {
    if (!clusters || clusters.length < 2) {
      return null;
    }
    const row = el("div", { className: "chip-row" }, el("span", { className: "hint", textContent: strings.solveParams.areaLevelsLabel }));
    clusters.forEach((c, i) => {
      let label;
      if (clusters.length === 2) {
        label = i === 0 ? strings.solveParams.areaLevelTop(c.z) : strings.solveParams.areaLevelBottom(c.z);
      } else {
        label = strings.solveParams.areaLevelNth(i + 1, c.z);
      }
      const btn = el("button", { type: "button", textContent: label });
      btn.addEventListener("click", () => apply(c.zMin - 32, c.zMax + 96));
      row.append(btn);
    });
    return row;
  }

  function toggleInArray(arr, value, checked) {
    const i = arr.indexOf(value);
    if (checked && i < 0) {
      arr.push(value);
    } else if (!checked && i >= 0) {
      arr.splice(i, 1);
    }
  }

  // ---- step 2: "Откуда бросать" -----------------------------------------------------------------

  function onOriginScopeChange(v) {
    originScopeUi = v;
    if (v === "all" || v === "spawns") {
      solveState.params.scope = v;
      if (solveState.origin) {
        solveState.origin = null;
        for (const vv of views()) vv.clearOrigin();
      }
      if (solveState.originArea || areaMode) {
        clearArea();
        updateAreaButtons();
      }
    } else if (v === "point") {
      solveState.params.scope = "all";
      if (solveState.originArea || areaMode) {
        clearArea();
        updateAreaButtons();
      }
    } else if (v === "area") {
      solveState.params.scope = "all";
      if (solveState.origin) {
        solveState.origin = null;
        for (const vv of views()) vv.clearOrigin();
      }
    }
    updateOriginStatus();
  }

  function renderOriginStep() {
    originStepBody.replaceChildren();

    scopeSegRef = segmented({
      name: "origin-scope",
      ariaLabel: strings.solveParams.scopeLabel,
      value: currentScopeValue(),
      options: [
        { value: "all", label: strings.solveParams.scopeAll },
        { value: "spawns", label: strings.solveParams.scopeSpawns },
        { value: "point", label: strings.solveParams.scopePointSeg },
        { value: "area", label: strings.solveParams.scopeAreaSeg },
      ],
      onChange: onOriginScopeChange,
    });
    originStepBody.append(scopeSegRef);

    // ---- "точка": hint + radius slider + the placed point, with a clear button ----
    const reachRange = el("input", { id: "origin-reach", type: "range", min: 16, max: 2000, step: 8, value: solveState.params.originReach });
    const reachVal = el("span", { className: "hint", textContent: `${solveState.params.originReach} ед.` });
    reachRange.addEventListener("input", () => {
      const v = parseFloat(reachRange.value);
      solveState.params.originReach = v;
      reachVal.textContent = `${v} ед.`;
      if (solveState.origin) {
        solveState.origin.reach = v;
        for (const vv of views()) vv.setOrigin(solveState.origin);
        updateOriginStatus();
      }
    });
    const clearOriginBtn = el("button", { type: "button", className: "icon-btn btn-ghost", innerHTML: icon("close", 14), "aria-label": strings.mapScreen.originRowClear });
    clearOriginBtn.addEventListener("click", () => {
      solveState.origin = null;
      for (const vv of views()) vv.clearOrigin();
      updateOriginStatus();
    });
    originStatusBox = el("div", { className: "step-row", hidden: true }, el("span", { className: "step-row-text" }), clearOriginBtn);
    originPointBlock = el(
      "div",
      {},
      el("p", { className: "hint", textContent: strings.mapScreen.originHintPoint }),
      el("div", { className: "field-row" }, el("label", { htmlFor: "origin-reach", textContent: strings.solveParams.originReachLabel }), reachRange, reachVal),
      originStatusBox,
    );

    // ---- "область": draw/edit/delete + z range + floor chips ----
    const areaToggleBtn = el("button", { type: "button" });
    const areaDeleteBtn = el("button", { type: "button", textContent: strings.solveParams.areaDeleteButton, hidden: true });
    areaToggleBtnRef = areaToggleBtn;
    areaDeleteBtnRef = areaDeleteBtn;
    areaToggleBtn.addEventListener("click", () => {
      areaMode = !areaMode;
      // `s6k_draw_in_3d.md` item 3/5: arm only whichever view is currently visible (2D -> mapView,
      // 3D -> sceneView) - a hidden view's own tool must never stay silently armed.
      const active = viewMode === "3d" ? sceneView : mapView;
      for (const v of views()) v.setAreaMode("origin", areaMode && v === active);
      if (areaMode && solveState.origin) {
        // Starting to draw/edit an area is exclusive with a point origin (`s6g_origin_area.md`).
        solveState.origin = null;
        for (const v of views()) v.clearOrigin();
      }
      // Only one area tool drafts at a time (either view) - activating this one silently
      // deactivated the target-area tool too, so its own local flag/button/status must follow
      // (review G2, risk 6).
      if (areaMode && targetAreaMode) {
        targetAreaMode = false;
        for (const v of views()) v.setAreaMode("target", false);
        updateTargetAreaButtons();
        updateTargetStatus();
      }
      updateAreaButtons();
      updateOriginStatus();
    });
    areaDeleteBtn.addEventListener("click", () => {
      clearArea();
      updateAreaButtons();
      updateOriginStatus();
    });
    const areaZMinInput = el("input", { id: "area-zmin", type: "number", placeholder: "-", value: solveState.params.areaZMin ?? "" });
    areaZMinInput.addEventListener("input", () => {
      const v = parseFloat(areaZMinInput.value);
      solveState.params.areaZMin = Number.isFinite(v) ? v : null;
      updateSceneArea("origin");
    });
    const areaZMaxInput = el("input", { id: "area-zmax", type: "number", placeholder: "-", value: solveState.params.areaZMax ?? "" });
    areaZMaxInput.addEventListener("input", () => {
      const v = parseFloat(areaZMaxInput.value);
      solveState.params.areaZMax = Number.isFinite(v) ? v : null;
      updateSceneArea("origin");
    });
    const areaLevelButtons = renderAreaLevelButtons(areaLevels, (zMin, zMax) => {
      areaZMinInput.value = zMin;
      areaZMaxInput.value = zMax;
      solveState.params.areaZMin = zMin;
      solveState.params.areaZMax = zMax;
      updateSceneArea("origin");
    });
    originAreaStatusBox = el("p", { className: "hint" });
    originAreaBlock = el(
      "div",
      {},
      el("p", { className: "hint", textContent: strings.mapScreen.targetHintArea }),
      el("p", { className: "hint", textContent: strings.solveParams.areaHint }),
      el("div", { className: "field-row" }, areaToggleBtn, areaDeleteBtn),
      el(
        "div",
        { className: "field-row" },
        el("label", { htmlFor: "area-zmin", textContent: strings.solveParams.areaZMinLabel }),
        areaZMinInput,
        el("label", { htmlFor: "area-zmax", textContent: strings.solveParams.areaZMaxLabel }),
        areaZMaxInput,
      ),
      areaLevelButtons,
      el("p", { className: "hint", textContent: strings.solveParams.areaZHint }),
      originAreaStatusBox,
    );

    originStepBody.append(originPointBlock, originAreaBlock);

    // ---- "Упор": any / wall-or-corner / corner-only ----
    const pinSeg = segmented({
      name: "origin-pin",
      ariaLabel: strings.solveParams.originPinLabel,
      value: solveState.params.originPin ?? "",
      options: [
        { value: "", label: strings.solveParams.originPinAny },
        { value: "wall", label: strings.solveParams.originPinWallOrCorner },
        { value: "corner", label: strings.solveParams.originPinCornerOnly },
      ],
      onChange: (v) => {
        solveState.params.originPin = v || null;
      },
    });
    originStepBody.append(
      el("p", { className: "hint", textContent: strings.solveParams.originPinLabel }),
      pinSeg,
    );

    updateAreaButtons();
    updateOriginStatus();
  }

  // ---- step 1: "Куда бросить" --------------------------------------------------------------------

  // `s6g2_target_area.md`/`s6r_sightline_target.md`: switches between a point target (as before),
  // an area target, and a sightline - all three are mutually exclusive.
  function setTargetMode(mode) {
    if (solveState.targetMode === mode) {
      return;
    }
    solveState.targetMode = mode;
    pendingLevels = null;
    if (mode !== "point") {
      clearTarget();
    }
    if (mode !== "area") {
      clearTargetAreaState();
      updateTargetAreaButtons();
    }
    if (mode !== "sightline") {
      clearSightlineState();
    }
    renderTargetBox();
  }

  function renderTargetBox() {
    // Keeps the "Точка | Область | Перекрыть обзор" segmented in sync with `solveState.targetMode`
    // - without this it never updated after the initial render (round-2 review finding 9), e.g. a
    // deep link that opens straight into area mode still showed "Точка" selected.
    targetModeSeg.setValue(solveState.targetMode);
    targetStepBody.replaceChildren();

    if (solveState.targetMode === "area") {
      renderTargetAreaControls();
      return;
    }
    if (solveState.targetMode === "sightline") {
      renderSightlineControls();
      return;
    }

    if (solveState.target) {
      const t = solveState.target;
      const rowText = `${t.label ?? "Точка"} · ${strings.mapScreen.targetRowZ(Math.round(t.z))}`;
      const clearBtn = el("button", { type: "button", className: "icon-btn btn-ghost", innerHTML: icon("close", 14), "aria-label": strings.mapScreen.targetRowClear });
      clearBtn.addEventListener("click", () => {
        clearTarget();
        renderTargetBox();
      });
      targetStepBody.append(el("div", { className: "step-row" }, el("span", { className: "step-row-text", title: `${t.x.toFixed(0)}, ${t.y.toFixed(0)}, ${t.z.toFixed(0)}`, textContent: rowText }), clearBtn));
    } else {
      targetStepBody.append(el("p", { className: "hint", textContent: strings.mapScreen.targetHintPoint }));
    }

    if (pendingLevels && !pendingLevels.forSightline) {
      const chooser = el("div", { className: "level-chooser" });
      chooser.append(el("p", { className: "hint", textContent: strings.mapScreen.levelsHeading }));
      for (const lvl of pendingLevels.levels) {
        const btn = el("button", {
          type: "button",
          textContent: `${lvl.name ?? strings.mapScreen.levelUnnamed} (z=${lvl.z.toFixed(0)})`,
        });
        btn.addEventListener("click", () => {
          applyTarget({ x: pendingLevels.x, y: pendingLevels.y, z: lvl.z, label: lvl.name });
        });
        chooser.append(btn);
      }
      targetStepBody.append(chooser);
    }

    // A rarely-needed affordance (paste a console `setpos` line) - tucked behind a small toggle so
    // it doesn't compete with the map-click flow that covers the common case.
    const manualToggleBtn = el("button", { type: "button", className: "btn-ghost", textContent: strings.mapScreen.manualToggle });
    const manualInput = el("input", { id: "manual-setpos", type: "text", placeholder: strings.mapScreen.manualPlaceholder });
    const manualBtn = el("button", { type: "button", textContent: strings.mapScreen.manualButton });
    const manualStatus = el("p", { className: "status" });
    const manualRow = el("div", { hidden: true }, el("div", { className: "field-row" }, manualInput, manualBtn), manualStatus);
    manualToggleBtn.addEventListener("click", () => {
      manualRow.hidden = !manualRow.hidden;
    });
    manualBtn.addEventListener("click", () => {
      const parsed = parseSetpos(manualInput.value);
      if (!parsed) {
        manualStatus.className = "status status-error";
        manualStatus.textContent = strings.mapScreen.manualBad;
        return;
      }
      manualStatus.className = "status";
      manualStatus.textContent = "";
      applyTarget({ x: parsed.x, y: parsed.y, z: parsed.z, label: null });
    });
    targetStepBody.append(manualToggleBtn, manualRow);
  }

  // `s6r_sightline_target.md`: "перекрыть обзор" mode's own controls - the hint, the running
  // status ("Обзор: из <место/z> в <место/z>" once both eyes are placed, or just the first one
  // while waiting for the second click), a clear button, and (mirroring the point mode) a
  // stacked-floor chooser for either click.
  function renderSightlineControls() {
    targetStepBody.append(el("p", { className: "hint", textContent: strings.mapScreen.targetHintSightline }));

    const placeZ = (p) => (p.label ? `${p.label}/${Math.round(p.z)}` : strings.mapScreen.targetRowZ(Math.round(p.z)));
    if (solveState.sightline) {
      const { from, to } = solveState.sightline;
      const rowText = strings.mapScreen.sightlineStatus(placeZ(from), placeZ(to));
      const clearBtn = el("button", { type: "button", className: "icon-btn btn-ghost", innerHTML: icon("close", 14), "aria-label": strings.mapScreen.targetRowClear });
      clearBtn.addEventListener("click", () => {
        clearSightlineState();
        renderTargetBox();
      });
      targetStepBody.append(el("div", { className: "step-row" }, el("span", { className: "step-row-text", textContent: rowText }), clearBtn));
    } else if (sightlineDraft) {
      targetStepBody.append(el("p", { className: "hint", textContent: `${strings.mapScreen.sightlineFromLabel}: ${placeZ(sightlineDraft)}` }));
    }

    if (pendingLevels && pendingLevels.forSightline) {
      const chooser = el("div", { className: "level-chooser" });
      chooser.append(el("p", { className: "hint", textContent: strings.mapScreen.levelsHeading }));
      for (const lvl of pendingLevels.levels) {
        const btn = el("button", {
          type: "button",
          textContent: `${lvl.name ?? strings.mapScreen.levelUnnamed} (z=${lvl.z.toFixed(0)})`,
        });
        btn.addEventListener("click", () => {
          applySightlinePoint({ x: pendingLevels.x, y: pendingLevels.y, z: lvl.z + SIGHTLINE_EYE_LIFT, label: lvl.name });
        });
        chooser.append(btn);
      }
      targetStepBody.append(chooser);
    }
  }

  // The target-area tool's own controls (draw/edit/delete, z range, status) - same shape as the
  // origin area's block in `renderOriginStep`, just targeting `mapView`'s `"target"` area key.
  function renderTargetAreaControls() {
    const toggleBtn = el("button", { type: "button" });
    const deleteBtn = el("button", { type: "button", textContent: strings.solveParams.areaDeleteButton, hidden: true });
    targetAreaToggleBtnRef = toggleBtn;
    targetAreaDeleteBtnRef = deleteBtn;
    toggleBtn.addEventListener("click", () => {
      targetAreaMode = !targetAreaMode;
      const active = viewMode === "3d" ? sceneView : mapView;
      for (const v of views()) v.setAreaMode("target", targetAreaMode && v === active);
      // Same reasoning as the origin area's own toggle above, mirrored (review G2, risk 6).
      if (targetAreaMode && areaMode) {
        areaMode = false;
        for (const v of views()) v.setAreaMode("origin", false);
        updateAreaButtons();
        updateOriginStatus();
      }
      updateTargetAreaButtons();
      updateTargetStatus();
    });
    deleteBtn.addEventListener("click", () => {
      clearTargetAreaState();
      updateTargetAreaButtons();
      updateTargetStatus();
    });
    const zMinInput = el("input", { id: "target-area-zmin", type: "number", placeholder: "-", value: solveState.params.targetAreaZMin ?? "" });
    zMinInput.addEventListener("input", () => {
      const v = parseFloat(zMinInput.value);
      solveState.params.targetAreaZMin = Number.isFinite(v) ? v : null;
      updateSceneArea("target");
    });
    const zMaxInput = el("input", { id: "target-area-zmax", type: "number", placeholder: "-", value: solveState.params.targetAreaZMax ?? "" });
    zMaxInput.addEventListener("input", () => {
      const v = parseFloat(zMaxInput.value);
      solveState.params.targetAreaZMax = Number.isFinite(v) ? v : null;
      updateSceneArea("target");
    });
    const targetAreaLevelButtons = renderAreaLevelButtons(targetAreaLevels, (zMin, zMax) => {
      zMinInput.value = zMin;
      zMaxInput.value = zMax;
      solveState.params.targetAreaZMin = zMin;
      solveState.params.targetAreaZMax = zMax;
      updateSceneArea("target");
    });
    targetStepBody.append(
      el("p", { className: "hint", textContent: strings.mapScreen.targetHintArea }),
      el("p", { className: "hint", textContent: strings.solveParams.targetAreaHint }),
      el("div", { className: "field-row" }, toggleBtn, deleteBtn),
      el(
        "div",
        { className: "field-row" },
        el("label", { htmlFor: "target-area-zmin", textContent: strings.solveParams.areaZMinLabel }),
        zMinInput,
        el("label", { htmlFor: "target-area-zmax", textContent: strings.solveParams.areaZMaxLabel }),
        zMaxInput,
      ),
    );
    if (targetAreaLevelButtons) {
      targetStepBody.append(targetAreaLevelButtons);
    }
    targetStatusBox = el("p", { className: "hint" });
    targetStepBody.append(el("p", { className: "hint", textContent: strings.solveParams.areaZHint }), targetStatusBox);
    updateTargetAreaButtons();
    updateTargetStatus();
  }

  // ---- step 3: "Граната и бросок" ----------------------------------------------------------------

  function renderThrowStep() {
    throwStepBody.replaceChildren();

    // A single active "Смок" chip + a muted line for what's coming - not four big disabled chips
    // for grenades that don't exist yet (round-2 design critique 4).
    const smokeId = "grenade-smoke";
    const grenadeRow = el(
      "div",
      { className: "chip-row", role: "radiogroup", "aria-label": strings.solveParams.grenadeLabel },
      el("input", { type: "radio", name: "grenade", id: smokeId, value: "smoke", checked: true, className: "chip-input" }),
      el("label", { htmlFor: smokeId, className: "chip", textContent: strings.solveParams.grenadeSmoke }),
    );
    throwStepBody.append(
      el("p", { className: "hint", textContent: strings.solveParams.grenadeLabel }),
      grenadeRow,
      el("p", { className: "hint", textContent: strings.solveParams.grenadeOthersSoon }),
    );

    const typesRow = el("div", { className: "chip-row" });
    for (const t of ALL_TYPES) {
      typesRow.append(
        chip({
          id: `type-${t}`,
          label: TYPE_LABELS[t],
          iconName: THROW_TYPE_ICON[t],
          checked: solveState.params.types.includes(t),
          onChange: (checked) => toggleInArray(solveState.params.types, t, checked),
        }),
      );
    }
    throwStepBody.append(el("p", { className: "hint", textContent: strings.solveParams.typesLabel }), typesRow);

    const strengthsRow = el("div", { className: "chip-row" });
    const strengthDefs = [
      [1, strings.solveParams.strength1],
      [0.5, strings.solveParams.strengthHalf],
      [0, strings.solveParams.strength0],
    ];
    for (const [val, label] of strengthDefs) {
      strengthsRow.append(
        chip({
          id: `strength-${val}`,
          label,
          checked: solveState.params.strengths.includes(val),
          onChange: (checked) => toggleInArray(solveState.params.strengths, val, checked),
        }),
      );
    }
    throwStepBody.append(el("p", { className: "hint", textContent: strings.solveParams.strengthsLabel }), strengthsRow);
  }

  // ---- "Дополнительно": допуск, мин. стабильность, подробный поиск, разрушения -------------------

  function renderAdvanced() {
    advancedContent.replaceChildren();

    const tolInput = el("input", { id: "tolerance-input", type: "number", min: 1, max: 512, value: solveState.params.tolerance });
    tolInput.addEventListener("input", () => {
      const v = parseFloat(tolInput.value);
      if (Number.isFinite(v)) {
        solveState.params.tolerance = v;
      }
    });
    advancedContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "tolerance-input", textContent: strings.solveParams.toleranceLabel }), tolInput),
    );

    const stabInput = el("input", { id: "stability-input", type: "number", min: 0.05, max: 1, step: 0.05, value: solveState.params.minStability });
    stabInput.addEventListener("input", () => {
      const v = parseFloat(stabInput.value);
      if (Number.isFinite(v)) {
        solveState.params.minStability = v;
      }
    });
    advancedContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "stability-input", textContent: strings.solveParams.minStabilityLabel }), stabInput),
    );

    advancedContent.append(
      chip({
        id: "fine-scan",
        label: strings.solveParams.fineScanLabel,
        checked: solveState.params.fineScan,
        onChange: (checked) => {
          solveState.params.fineScan = checked;
        },
      }),
    );

    if (mapSummary.hasGlass || mapSummary.hasDoors) {
      const brokenRow = el("div", { className: "chip-row" });
      if (mapSummary.hasGlass) {
        brokenRow.append(
          chip({
            id: "broken-glass",
            label: strings.solveParams.brokenGlass,
            checked: solveState.params.broken.includes("glass"),
            onChange: (checked) => toggleInArray(solveState.params.broken, "glass", checked),
          }),
        );
      }
      if (mapSummary.hasDoors) {
        brokenRow.append(
          chip({
            id: "broken-doors",
            label: strings.solveParams.brokenDoors,
            checked: solveState.params.broken.includes("doors"),
            onChange: (checked) => toggleInArray(solveState.params.broken, "doors", checked),
          }),
        );
      }
      advancedContent.append(el("p", { className: "hint", textContent: strings.solveParams.brokenLabel }), brokenRow);
    }
  }

  // ---- run button / progress ---------------------------------------------------------------------

  function paintProgress(refs, lastPhase, startedAt, checkedTotal, verifiedTotal) {
    const elapsed = (Date.now() - startedAt) / 1000;
    const label = strings.solve.phases[lastPhase] ?? lastPhase;
    refs.progressText.textContent = `${label} - ${strings.solve.elapsed(elapsed)} - ${strings.solve.checkedCount(checkedTotal)}, ${strings.solve.verifiedCount(verifiedTotal)}`;
  }

  function setRunning(running) {
    runRefs.runBtn.hidden = running;
    runRefs.progressWrap.hidden = !running;
  }

  function applyResult(data, cameFromCache, broken) {
    const lineups = data.lineups ?? [];
    // AMBER-5: the progress cloud has done its job once a result is in - leaving it drawn just
    // buries the result overlays under however many search points were streamed.
    // F3b-1b: the full lineup objects go to the views (not just id/feet/rest) - the 3D view also
    // needs yaw/pitch/type/strength/runDeg to draw the aim line and drive the first-person view;
    // the 2D view only ever reads id/feet/rest and ignores the rest.
    // The `broken` groups the solve actually ran with (a snapshot from when the solve started, not
    // whatever the params panel holds now) - the 3D trajectory must match the same world state.
    for (const l of lineups) {
      l.broken = broken;
    }
    lastLineups = lineups;
    lastSelectedId = null;
    for (const v of views()) {
      v.clearPoints();
      v.setLineups(lineups);
    }
    // BLUE-19: use the server's own settled target (it can differ from the clicked point by a
    // few units), not `solveState.target`.
    const settledTarget = data.target ? { x: data.target[0], y: data.target[1], z: data.target[2] } : solveState.target;
    // The results column shows its own empty state (with the server's own reason, translated -
    // round-3 review finding 7: the server's `emptyReason` is English prose, never shown raw)
    // when there is nothing to list (round-2 review finding 10) - `panel.setResult` needs it too.
    const emptyReasonRu = strings.solve.translateEmptyReason(data.emptyReason);
    panel.setResult(lineups, settledTarget, emptyReasonRu);
    const cachedNote = cameFromCache ? `${strings.solve.cachedResult} ` : "";
    if (lineups.length === 0) {
      runRefs.idleStatus.className = "status";
      runRefs.idleStatus.textContent = `${cachedNote}${emptyReasonRu} ${strings.solve.emptyHint}`.trim();
    } else {
      runRefs.idleStatus.className = "status status-ok";
      runRefs.idleStatus.textContent = `${cachedNote}${strings.solve.verifiedCount(lineups.length)}`.trim();
      // Below 1280px, jump to the results tab only once there's actually something to show there
      // (round-2 review finding 10) - zero results is better read next to the run button/status.
      tabsSeg.setValue("results");
      stepsCol.classList.add("tab-hidden");
      resultsCol.classList.remove("tab-hidden");
      resultsCol.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }

  function startSolve(bodyOverride) {
    if (!solveState.target && !solveState.targetArea && !solveState.sightline) {
      runRefs.idleStatus.className = "status status-error";
      runRefs.idleStatus.textContent = strings.solve.needTarget;
      return;
    }
    // AMBER-9: an empty types/strengths selection is indistinguishable, once serialised, from
    // "use every default" - the server would then silently solve with all of them.
    const selErr = selectionError(solveState.params);
    if (selErr) {
      runRefs.idleStatus.className = "status status-error";
      runRefs.idleStatus.textContent = selErr === "types" ? strings.solve.needTypes : strings.solve.needStrengths;
      return;
    }
    if (solveState.running) {
      return;
    }
    solveState.running = true;
    lastLineups = [];
    lastSelectedId = null;
    for (const v of views()) {
      v.clearPoints();
      v.setLineups([]);
      v.setSelected(null);
    }
    panel.clear();
    setRunning(true);
    runRefs.idleStatus.textContent = "";
    runRefs.progressText.textContent = strings.solve.phases.queued;

    const originAreaForQuery = solveState.originArea
      ? { polygon: solveState.originArea.polygon, zMin: solveState.params.areaZMin, zMax: solveState.params.areaZMax }
      : null;
    const targetAreaForQuery = solveState.targetArea
      ? { polygon: solveState.targetArea.polygon, zMin: solveState.params.targetAreaZMin, zMax: solveState.params.targetAreaZMax }
      : null;
    const body =
      bodyOverride ??
      buildQuery(map, solveState.target, targetAreaForQuery, solveState.sightline, solveState.origin, originAreaForQuery, solveState.params);
    syncHash(body);
    // Snapshot now, not read back from `solveState.params.broken` in `onResult` - the panel stays
    // interactive while the solve runs, so those checkboxes could have changed by the time it ends.
    const brokenAtStart = [...(body.broken ?? [])];

    const startedAt = Date.now();
    let lastPhase = "queued";
    let checkedTotal = 0;
    let verifiedTotal = 0;
    const timer = setInterval(() => paintProgress(runRefs, lastPhase, startedAt, checkedTotal, verifiedTotal), 1000);

    function finish() {
      clearInterval(timer);
      solveState.running = false;
      solveState.controller = null;
      setRunning(false);
    }

    solveState.controller = runSolve(body, {
      onLine: (msg) => {
        if (msg.phase) {
          lastPhase = msg.phase;
          paintProgress(runRefs, lastPhase, startedAt, checkedTotal, verifiedTotal);
          return;
        }
        if (msg.checked) {
          checkedTotal += msg.checked.length;
          const pts = msg.checked.map((p) => ({ x: p[0], y: p[1] }));
          for (const v of views()) v.addCheckedPoints(pts);
        } else if (msg.verified) {
          verifiedTotal += msg.verified.length;
          // AMBER-4: `ok` (index 3) tells a verified-and-failed candidate apart from a real find
          // - painting both in the bright "found" colour would misrepresent the search.
          const pts = msg.verified.map((p) => ({ x: p[0], y: p[1], ok: !!p[3] }));
          for (const v of views()) v.addVerifiedPoints(pts);
        }
      },
      onResult: (data, streamed) => {
        finish();
        // AMBER-8: "came from cache" is whether the stream reader ever saw a non-terminal line,
        // not whether any points happened to land on the map - a first solve that streams
        // nothing (a target inside solid geometry) is not a cache hit.
        applyResult(data, !streamed, brokenAtStart);
      },
      onError: (message) => {
        finish();
        runRefs.idleStatus.className = "status status-error";
        runRefs.idleStatus.textContent = message ?? strings.errors.serverDown;
      },
      onCancelled: () => {
        finish();
        runRefs.idleStatus.className = "status";
        runRefs.idleStatus.textContent = "";
      },
    });
  }

  function renderRunBox() {
    runCardBody.replaceChildren();
    const runBtn = el("button", { type: "button", className: "primary btn-block", textContent: strings.solve.runButton });
    const cancelBtn = el("button", { type: "button", className: "btn-block", textContent: strings.solve.cancelButton });
    const progressText = el("p", { className: "status", role: "status" });
    const progressBarSpan = el("span");
    const progressBar = el("div", { className: "progress-bar indeterminate" }, progressBarSpan);
    const progressWrap = el("div", { hidden: true }, progressBar, progressText, cancelBtn);
    const idleStatus = el("p", { className: "status", role: "status" });
    runBtn.addEventListener("click", () => startSolve());
    cancelBtn.addEventListener("click", () => solveState.controller?.cancel());
    runCardBody.append(runBtn, progressWrap, idleStatus);
    runRefs = { runBtn, cancelBtn, progressWrap, progressText, idleStatus };
  }

  function applyAutoBody(body) {
    if (body.target) {
      applyTarget({ x: body.target[0], y: body.target[1], z: body.target[2], label: null });
    } else if (body.targetArea) {
      solveState.targetMode = "area";
      solveState.targetArea = { polygon: body.targetArea };
      mapView.setArea("target", body.targetArea);
      if (body.targetZMin != null) {
        solveState.params.targetAreaZMin = body.targetZMin;
      }
      if (body.targetZMax != null) {
        solveState.params.targetAreaZMax = body.targetZMax;
      }
    } else if (body.sightline) {
      solveState.targetMode = "sightline";
      solveState.sightline = {
        from: { x: body.sightline.from[0], y: body.sightline.from[1], z: body.sightline.from[2], label: null },
        to: { x: body.sightline.to[0], y: body.sightline.to[1], z: body.sightline.to[2], label: null },
      };
      for (const v of views()) v.setSightline(solveState.sightline);
    }
    if (body.origin) {
      solveState.origin = { x: body.origin[0], y: body.origin[1], reach: body.originReach ?? 300 };
      for (const v of views()) v.setOrigin(solveState.origin);
      originScopeUi = "point";
    } else if (body.originArea) {
      solveState.originArea = { polygon: body.originArea };
      mapView.setArea("origin", body.originArea);
      if (body.zMin != null) {
        solveState.params.areaZMin = body.zMin;
      }
      if (body.zMax != null) {
        solveState.params.areaZMax = body.zMax;
      }
      originScopeUi = "area";
    }
    if (body.scope) {
      solveState.params.scope = body.scope;
      originScopeUi = body.scope;
    }
    if (body.tolerance != null) {
      solveState.params.tolerance = body.tolerance;
    }
    if (body.minStability != null) {
      solveState.params.minStability = body.minStability;
    }
    if (body.fineScan) {
      solveState.params.fineScan = true;
    }
    if (body.types) {
      solveState.params.types = body.types;
    }
    if (body.strengths) {
      solveState.params.strengths = body.strengths;
    }
    if (body.broken) {
      solveState.params.broken = body.broken;
    }
    if (body.originPin) {
      solveState.params.originPin = body.originPin;
    }
  }

  // ---- F3b-1b: the 2D/3D toggle, "show collisions", camera mode, and the first-person view ------

  function ensureSceneView() {
    if (sceneView) {
      return sceneView;
    }
    sceneView = createSceneView(threeContainer, map, mapSummary, state.theme);
    currentViews.push(sceneView);
    sceneView.onClick((wx, wy, wz, normalZ) => handleMapClick(wx, wy, wz, normalZ));
    sceneView.onRightClick((wx, wy, wz) => handleOriginClick(wx, wy, wz));
    sceneView.onAreaChange("origin", (payload) => handleAreaChange("origin", sceneView, payload));
    sceneView.onAreaChange("target", (payload) => handleAreaChange("target", sceneView, payload));
    sceneView.onLoadProgress((loaded, total) => {
      const percent = total > 0 ? Math.round((loaded / total) * 100) : null;
      view3dStatus.className = "hint";
      view3dStatus.textContent = strings.view3d.loading(percent);
    });
    sceneView.onLoadDone(() => {
      view3dStatus.textContent = strings.view3d.flyHint;
      syncView3dHint();
      // Any gallery still showing "unavailable" gets a fresh try now that the model is actually
      // loaded (round-2 review finding 6).
      panel.retryPreviews();
    });
    sceneView.onLoadError((msg) => {
      view3dStatus.className = "hint status-error";
      view3dStatus.textContent = `${strings.view3d.loadError} ${msg ?? ""}`.trim();
    });
    sceneView.onLightingReady((supported) => {
      lightingBtn.hidden = !supported;
      if (supported) {
        lightingBtn.textContent = sceneView.getLightingMode() === "game" ? strings.view3d.lightingGame : strings.view3d.lightingSimple;
      }
    });
    // Catch up on state this view missed by not existing yet.
    if (solveState.target) {
      sceneView.setTarget(solveState.target);
    }
    if (solveState.sightline || sightlineDraft) {
      sceneView.setSightline(solveState.sightline ?? { from: sightlineDraft, to: null });
    }
    if (solveState.origin) {
      sceneView.setOrigin(solveState.origin);
    }
    updateSceneArea("origin");
    updateSceneArea("target");
    sceneView.setLineups(lastLineups);
    if (lastSelectedId) {
      sceneView.setSelected(lastSelectedId);
    }
    return sceneView;
  }

  // Review fix item 2: `view3dStatus` is written from several places (load progress/done/error,
  // the fly hint, the area-draw hint) - only ever touches it while it's currently showing one of
  // the two *steady-state* hints (leaves a load-progress/error message alone), and always picks
  // between them from the tool state right now, so calling it after any of those writers, or after
  // `areaMode`/`targetAreaMode` changes, can't leave a stale or wrong hint showing.
  function syncView3dHint() {
    if (viewMode !== "3d") {
      return;
    }
    const current = view3dStatus.textContent;
    if (current !== strings.view3d.flyHint && current !== strings.view3d.areaDrawHint) {
      return;
    }
    view3dStatus.className = "hint";
    view3dStatus.textContent = areaMode || targetAreaMode ? strings.view3d.areaDrawHint : strings.view3d.flyHint;
  }

  // `s6i_render_job_areas3d.md` change item 2: whether `switchViewMode("3d")` is currently
  // refusing because this map has no usable render yet - distinct from `viewMode` itself, which
  // never actually becomes "3d" on that path. Reset the moment the user leaves for 2D or a fresh
  // render becomes usable, so a background "Подготовить 3D" job finishing after the user already
  // moved on doesn't yank them back into 3D.
  let render3dBlocked = false;

  // Shows "Подготовить 3D" exactly while it would do something useful - blocked on this map's own
  // missing/outdated render, and no `render` job (started from here or from the map list) already
  // in flight for it.
  function syncPrepare3dButton() {
    prepare3dBtn.hidden = !render3dBlocked || hasUsableRender(mapSummary) || state.activeJobs.has(map);
  }

  prepare3dBtn.addEventListener("click", () => {
    startJobFlow(
      ["render"],
      map,
      {
        progressBox: render3dProgressBox,
        label: render3dProgressLabel,
        barSpan: render3dBarSpan,
        disableButtons: [prepare3dBtn],
        cancelBtn: render3dCancelBtn,
      },
      onRenderPrepared,
    );
    // A cancelled job settles without calling onRenderPrepared - bring the button back then.
    state.activeJobs.get(map)?.controller.promise.then((outcome) => {
      if (outcome.cancelled) {
        syncPrepare3dButton();
      }
    });
    syncPrepare3dButton();
  });

  // Refetches `/api/maps` and, if this screen is still open, updates `mapSummary` in place (same
  // object `ensureSceneView`/`switchViewMode` already read from) so a freshly built render.glb
  // becomes usable without a page reload (`s6i_render_job_areas3d.md`: "кнопка 3D становится
  // доступной без перезагрузки страницы"). Only actually enters 3D on its own when the user is
  // still sitting on the blocked 3D attempt that started this job - not if they've since switched
  // back to 2D.
  async function onRenderPrepared() {
    const { data } = await fetchMaps();
    if (Array.isArray(data)) {
      state.maps = data;
      const fresh = data.find((x) => x.map === map);
      if (fresh) {
        Object.assign(mapSummary, fresh);
      }
    }
    if (!screen.isConnected) {
      // This map screen was left while the job ran - never touch its views (a hidden scene on a
      // detached container, and radarView reset for whatever screen is open now). Re-render the
      // list if the user is on it, the same way the list's own flows settle.
      if (state.screen === "maps") {
        renderMapsScreen();
      }
      return;
    }
    syncPrepare3dButton();
    if (render3dBlocked && hasUsableRender(mapSummary)) {
      switchViewMode("3d");
    }
  }

  // ---- `s6p_map_art.md` official radar overlay (round-2 design critique 8) -----------------------
  // A parallel task adds `GET /api/overview`/`GET /api/mapart` to the server - both are fetched
  // defensively (a 404/failed `fetchOverview` just leaves `officialSections` empty, so every
  // control here stays hidden and `mapView`'s rendering is exactly what it always was).
  let officialOverview = null;
  let officialSections = [];
  let activeSectionName = null;
  let schemeOn = false;
  const sectionImageCache = new Map(); // section.radar -> loaded Image

  function sectionForZ(z) {
    if (officialSections.length === 0) {
      return null;
    }
    if (z != null) {
      const hit = officialSections.find((s) => z >= s.altitudeMin && z <= s.altitudeMax);
      if (hit) {
        return hit;
      }
    }
    return officialSections[0];
  }

  function loadSectionImage(section) {
    return new Promise((resolve) => {
      const cached = sectionImageCache.get(section.radar);
      if (cached) {
        resolve(cached);
        return;
      }
      const img2 = new Image();
      img2.onload = () => {
        sectionImageCache.set(section.radar, img2);
        resolve(img2);
      };
      img2.onerror = () => resolve(null);
      img2.src = mapArtUrl(map, "radar", section.radar);
    });
  }

  function renderSectionSwitcher() {
    sectionRow.replaceChildren();
    if (officialSections.length < 2) {
      sectionRow.hidden = true;
      return;
    }
    sectionRow.hidden = viewMode !== "2d";
    for (const s of officialSections) {
      const label = { default: strings.mapScreen.sectionDefault, lower: strings.mapScreen.sectionLower, upper: strings.mapScreen.sectionUpper }[s.name]
        ?? (s.name.charAt(0).toUpperCase() + s.name.slice(1));
      const btn = el("button", { type: "button", className: s.name === activeSectionName ? "primary" : "", textContent: label });
      btn.addEventListener("click", () => applyOfficialSection(s));
      sectionRow.append(btn);
    }
  }

  async function applyOfficialSection(section) {
    if (section.name === activeSectionName) {
      return;
    }
    activeSectionName = section.name;
    renderSectionSwitcher();
    const image = await loadSectionImage(section);
    // A second call (e.g. the target moved to another floor again) may have already changed
    // `activeSectionName` while this one's image was still loading - applying this now-stale
    // image would flash the wrong section back in behind the newer one (round-3 review finding 6).
    if (section.name !== activeSectionName) {
      return;
    }
    if (!mapView) {
      return; // screen left while the image was loading
    }
    if (!image) {
      mapView.setOfficialLayer(null);
      return;
    }
    mapView.setOfficialLayer({ image, posX: officialOverview.posX, posY: officialOverview.posY, scale: officialOverview.scale });
    mapView.setShowScheme(schemeOn);
    syncSchemeControls();
  }

  // Auto-picks the section from the target's own z (round-2 design critique 8) - a live hover-z
  // pick would need `map2d.js` to report pointer moves in world space, which it doesn't expose
  // today; re-picking on every target change covers the common case (aiming a search at a level)
  // without that extra plumbing.
  function updateOfficialSectionForTarget() {
    if (officialSections.length === 0) {
      return;
    }
    const z = solveState.target?.z ?? null;
    const section = sectionForZ(z);
    if (section) {
      applyOfficialSection(section);
    }
  }

  function syncSchemeControls() {
    const has = !!mapView?.hasOfficialLayer();
    schemeBtn.hidden = viewMode !== "2d" || !has;
    schemeBtn.className = schemeOn ? "primary" : "";
    sectionRow.hidden = viewMode !== "2d" || officialSections.length < 2;
  }

  schemeBtn.addEventListener("click", () => {
    schemeOn = !schemeOn;
    mapView?.setShowScheme(schemeOn);
    syncSchemeControls();
  });

  async function initOfficialArt() {
    const { data, error } = await fetchOverview(map);
    if (error !== undefined || !data || !Array.isArray(data.sections) || data.sections.length === 0) {
      return; // No official art for this map (or the endpoint doesn't exist yet) - unchanged rendering.
    }
    officialOverview = data;
    officialSections = data.sections;
    await updateOfficialSectionForTarget();
  }

  function switchViewMode(mode) {
    if (viewMode === mode) {
      return;
    }
    if (mode === "3d" && !hasUsableRender(mapSummary)) {
      view3dStatus.className = "hint status-error";
      view3dStatus.textContent = mapSummary.hasRender ? strings.view3d.renderOutdated : strings.view3d.noRender;
      render3dBlocked = true;
      syncPrepare3dButton();
      viewModeSeg.setValue("2d");
      return;
    }
    render3dBlocked = false;
    // `s6k_draw_in_3d.md` item 3: a half-drawn draft doesn't survive a 2D<->3D switch - the armed
    // tool itself moves to whichever view becomes visible, instead of staying silently armed on the
    // one being hidden. Captured before `viewMode` changes below (and before the disarm-on-the-old-
    // view call below resets the draft count to 0).
    const armedKey = areaMode ? "origin" : targetAreaMode ? "target" : null;
    const hadDraft = armedKey === "origin" ? areaDraftOpen : armedKey === "target" ? targetAreaDraftOpen : false;
    viewMode = mode;
    viewModeSeg.setValue(mode);
    if (mode === "3d") {
      // Unhide *before* creating/resizing the scene view - `threeContainer.getBoundingClientRect()`
      // reads 0x0 while `hidden` (`display: none`), and nothing else is guaranteed to correct that
      // later (a `ResizeObserver` on a non-rendered box doesn't fire until it renders again).
      canvas.hidden = true;
      threeContainer.hidden = false;
      const view = ensureSceneView();
      view.resize();
      // Covers the case `onLoadDone` won't fire again for - a scene that was already loaded from
      // an earlier visit to 3D (round-2 review finding 6).
      panel.retryPreviews();
      radarView = null; // theme toggle no-ops while 3D is shown, same as off the map screen
      collisionsBtn.hidden = false;
      cameraModeBtn.hidden = false;
      lightingBtn.hidden = !view.isLightingSupported();
      schemeBtn.hidden = true;
      sectionRow.hidden = true;
      view3dStatus.className = "hint";
      view3dStatus.textContent = strings.view3d.flyHint;
    } else {
      canvas.hidden = false;
      threeContainer.hidden = true;
      radarView = mapView;
      // Theme toggles while in 3D are a no-op for the 2D canvas (`radarView` is `null` then) - it
      // can be stale by the time 2D is shown again, so recolor unconditionally here (cheap, and a
      // no-op recompute when nothing actually changed) rather than only on the toggle itself
      // (round-2 polish pass).
      mapView?.recolor(state.theme);
      collisionsBtn.hidden = true;
      cameraModeBtn.hidden = true;
      lightingBtn.hidden = true;
      syncSchemeControls();
      view3dStatus.className = "hint";
      view3dStatus.textContent = "";
    }
    if (armedKey) {
      const active = mode === "3d" ? sceneView : mapView;
      for (const v of views()) v.setAreaMode(armedKey, v === active);
      syncView3dHint();
      if (hadDraft) {
        caption.className = "stage-caption";
        caption.textContent = strings.view3d.areaDraftDiscarded;
      }
    }
    syncPrepare3dButton();
  }

  lightingBtn.addEventListener("click", () => {
    if (!sceneView) {
      return;
    }
    const next = sceneView.getLightingMode() === "game" ? "simple" : "game";
    sceneView.setLightingMode(next);
    lightingBtn.textContent = next === "game" ? strings.view3d.lightingGame : strings.view3d.lightingSimple;
  });

  let collisionsOn = false;
  collisionsBtn.addEventListener("click", () => {
    collisionsOn = !collisionsOn;
    sceneView?.setCollisionOverlay(collisionsOn);
    collisionsBtn.textContent = collisionsOn ? strings.view3d.collisionsOff : strings.view3d.collisionsOn;
  });
  cameraModeBtn.addEventListener("click", () => {
    if (!sceneView) {
      return;
    }
    const next = sceneView.getCameraMode() === "orbit" ? "fly" : "orbit";
    sceneView.setCameraMode(next);
    cameraModeBtn.textContent = next === "orbit" ? strings.view3d.modeFly : strings.view3d.modeOrbit;
  });

  function onFpvKeydown(e) {
    if (e.key === "Escape") {
      sceneView?.exitFirstPerson();
    }
  }

  function copyFpvConsole(text) {
    if (navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(text).catch(() => {});
    }
  }

  function renderFpvOverlay(info) {
    // Built from `info.type`/`info.click` (structured fields), never the server's own English
    // `info.how` (round-2 review finding 5).
    const label = `${TYPE_LABELS[info.type] ?? strings.panel.typeUnknown} · ${CLICK_LABELS[info.click] ?? strings.panel.clickUnknown}`;
    fpvOverlay.replaceChildren(
      el("div", { className: "fpv-crosshair", "aria-hidden": "true" }),
      el(
        "div",
        { className: "fpv-label" },
        el("div", { textContent: label }),
        el("p", { className: "hint", textContent: strings.fpv.exitHint }),
      ),
      el(
        "div",
        { className: "fpv-controls" },
        el("button", { type: "button", textContent: strings.fpv.copyButton, onclick: () => copyFpvConsole(info.console) }),
        el("button", { type: "button", textContent: strings.fpv.exitButton, onclick: () => sceneView?.exitFirstPerson() }),
      ),
    );
  }

  function handleFirstPerson(l) {
    if (viewMode !== "3d") {
      switchViewMode("3d");
      if (viewMode !== "3d") {
        return; // `switchViewMode` refused (no render.glb for this map) - `view3dStatus` said why.
      }
    }
    const view = ensureSceneView();
    const info = view.enterFirstPerson(l, () => {
      fpvOverlay.hidden = true;
      document.removeEventListener("keydown", onFpvKeydown);
    });
    // `s6q_robust_aim.md`: the first-person copy uses the exact console string too, falling back
    // to `console` for an older cached result that predates the field.
    renderFpvOverlay({ ...info, console: l.consoleExact ?? l.console });
    fpvOverlay.hidden = false;
    document.addEventListener("keydown", onFpvKeydown);
  }

  // Previews (S6n's `sceneView.capturePreview`, called by `panel.js`'s lineup card gallery) -
  // feature-detected since the method may not exist yet, and treated the same way whether it's
  // absent or resolves `null` (panel.js shows a placeholder + "Показать прицел в 3D" either way).
  async function requestPreview(l, kind) {
    if (typeof sceneView?.capturePreview !== "function") {
      return null;
    }
    // `ensureLoaded()` (S6n) waits for render.glb + game lighting + the collision mesh, so the
    // very first capture after switching to 3D doesn't race `capturePreview`'s own "not loaded
    // yet -> null" return - a `null` this early would otherwise get read as "capturePreview isn't
    // really available" and never retried until `onLoadDone`/a 3D switch fires again.
    const ready = await sceneView.ensureLoaded();
    if (!ready) {
      return null;
    }
    // A lineup's own JSON never carries `trajectory` - only "land" needs the flight arc (the aim/
    // stand previews are a static pose), so it's fetched here, same call `scene3d.js`'s own
    // selected-lineup arc already makes (round-2 review finding 7).
    let trajectory;
    if (kind === "land") {
      const { data: traj } = await fetchTrajectory(map, {
        x: l.feet[0], y: l.feet[1], z: l.feet[2], type: l.type, pitch: l.pitch, yaw: l.yaw,
        strength: l.strength, runDeg: l.runDeg, broken: l.broken,
      });
      trajectory = traj?.points;
    }
    try {
      const blob = await sceneView.capturePreview({
        kind, feet: l.feet, pitch: l.pitch, yaw: l.yaw, type: l.type, rest: l.rest, trajectory,
        width: 480, height: 270,
      });
      return blob ?? null;
    } catch {
      return null;
    }
  }

  const img = new Image();
  img.onload = () => {
    mapView = createMapView(canvas, radarData, img, state.theme);
    currentViews.push(mapView);
    if (viewMode === "2d") {
      radarView = mapView;
    }
    mapView.onClick((wx, wy) => handleMapClick(wx, wy));
    mapView.onRightClick((wx, wy) => handleOriginClick(wx, wy));
    mapView.onAreaChange("origin", (payload) => handleAreaChange("origin", mapView, payload));
    mapView.onAreaChange("target", (payload) => handleAreaChange("target", mapView, payload));
    if (opts.autoBody) {
      applyAutoBody(opts.autoBody);
    }
    initOfficialArt();
    renderOriginStep();
    renderTargetBox();
    renderThrowStep();
    renderRunBox();
    renderAdvanced();
    if (opts.autoBody) {
      // Not `startSolve(opts.autoBody)`: that skipped `buildQuery` entirely, which is also where
      // `aimPrecision` gets added and `types` gets normalised - a deep link's own solve would run
      // without either, including the server's "RunJumpThrow" back into the results
      // (round-2 review finding 2). `applyAutoBody` above already applied every field into
      // `solveState`, so a fresh `buildQuery()` reconstructs the same request correctly.
      startSolve();
    }
  };
  img.onerror = () => {
    caption.className = "stage-caption status-error";
    caption.textContent = strings.errors.genericPrefix;
  };
  img.src = radarPngUrl(map) + `?v=${encodeURIComponent(radarData.build ?? "0")}`;
}
