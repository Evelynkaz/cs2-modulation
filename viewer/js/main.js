// Entry point: theme + screen routing. Screen 1 (first run) lives in setup.js; the "prepare a
// map" job flow lives in jobs.js; everything else (map list, radar screen) is small enough to
// live here.

import { state, applyTheme, resolveInitialTheme, storeTheme } from "./state.js?v=1";
import { strings } from "./strings.js?v=1";
import {
  fetchConfig,
  putConfig,
  fetchMaps,
  fetchJobs,
  fetchRadar,
  fetchLevels,
  radarPngUrl,
  deleteJob,
} from "./api.js?v=1";
import { renderSetup } from "./setup.js?v=1";
import { startPrepare, reconnectJob, stageLabel } from "./jobs.js?v=1";
import { createMapView } from "./map2d.js?v=1";
import { runSolve, buildQuery, parseSetpos, selectionError } from "./solve.js?v=1";
import { createPanel, TYPE_LABELS } from "./panel.js?v=1";

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
let radarView = null; // { recolor(): void } for the current map screen's canvas, if any.
// AMBER-7: the map view owns a ResizeObserver, a devicePixelRatio listener and a recolored
// canvas - each `showMapScreen` must destroy the previous one instead of leaking it.
let currentMapView = null;
function destroyCurrentMapView() {
  currentMapView?.destroy();
  currentMapView = null;
}

// Screen changes go through the dedicated `#status` live region, not `#app` itself - `#app`'s
// own DOM churn (a whole screen replaced at once) would otherwise be announced line by line.
function announce(text) {
  statusEl.textContent = text;
}

// ---- bootstrap ------------------------------------------------------------------------------

function wireThemeToggle() {
  const btn = document.getElementById("theme-toggle");
  const sync = () => {
    btn.textContent = state.theme === "dark" ? strings.theme.toggleToLight : strings.theme.toggleToDark;
  };
  sync();
  btn.addEventListener("click", () => {
    applyTheme(state.theme === "dark" ? "light" : "dark");
    storeTheme(state.theme);
    sync();
    radarView?.recolor(state.theme);
  });
}

function renderServerDown(root, retry) {
  root.replaceChildren(
    el("p", { className: "status status-error", textContent: strings.errors.serverDown }),
    el(
      "button",
      { type: "button", textContent: strings.errors.retryButton, onclick: retry },
    ),
  );
}

function renderLoadingMaps() {
  app.replaceChildren(
    el("h1", { textContent: strings.maps.heading }),
    el("p", { className: "hint", textContent: strings.maps.loading }),
  );
}

async function boot() {
  applyTheme(resolveInitialTheme());
  wireThemeToggle();
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
  if (!map || !targetStr) {
    return null;
  }
  const target = targetStr.split(",").map(Number);
  if (target.length < 3 || target.some((v) => !Number.isFinite(v))) {
    return null;
  }
  const query = { map, target };
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
    body.types = q.types.split(",");
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
  return body;
}

function syncHash(body) {
  const parts = [`map=${encodeURIComponent(body.map)}`, `target=${body.target.map((v) => v.toFixed(1)).join(",")}`];
  for (const [k, v] of Object.entries(body)) {
    if (k === "map" || k === "target") {
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
  if (!data.configured) {
    state.screen = "setup";
    renderSetup(
      app,
      (cfg) => {
        state.config = cfg;
        showMapsScreen();
      },
      data,
    );
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

function pill(label, present) {
  const marker = present ? strings.maps.pillOk : strings.maps.pillMissing;
  return el("span", {
    className: present ? "pill pill-ok" : "pill pill-missing",
    textContent: `${marker} ${label}`,
  });
}

function renderMapsScreen() {
  app.replaceChildren();
  announce(strings.maps.heading);
  const settingsBtn = el("button", {
    type: "button",
    textContent: strings.maps.backToSetup,
    onclick: () => {
      state.screen = "setup";
      renderSetup(
        app,
        (cfg) => {
          state.config = cfg;
          showMapsScreen();
        },
        state.config,
      );
    },
  });
  app.append(
    el(
      "div",
      { className: "map-screen-header" },
      el("h1", { textContent: strings.maps.heading }),
      settingsBtn,
    ),
  );

  if (state.maps.length === 0) {
    app.append(renderExtractNewForm());
    return;
  }

  const table = el(
    "table",
    { className: "maps" },
    el(
      "thead",
      null,
      el(
        "tr",
        null,
        el("th", { textContent: strings.maps.columnMap }),
        el("th", { textContent: strings.maps.columnBuild }),
        el("th", { textContent: "" }),
        el("th", { textContent: "" }),
      ),
    ),
  );
  const tbody = el("tbody");
  for (const m of state.maps) {
    tbody.append(renderMapRow(m));
  }
  table.append(tbody);
  app.append(table);
  app.append(renderExtractNewForm(true));
}

function renderProgressBox() {
  const label = el("div");
  const barSpan = el("span");
  const bar = el("div", { className: "progress-bar" }, barSpan);
  const box = el("div", { className: "progress", hidden: true }, label, bar);
  return { box, label, barSpan };
}

// Which of `extract`/`standspots`/`viewerdata` a map still needs, in run order. "Подготовить"
// only runs these - not the ones it already has - so a machine missing only the radar (say, the
// CS2 install is gone) still gets the radar instead of failing at `extract`.
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
  return kinds;
}

// ---- running jobs: `state.activeJobs` holds one record per map with a job in flight, so a
// table re-render (switching maps, saving settings, a finished sibling job) never orphans it;
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
  const elapsed = ((Date.now() - record.startedAt) / 1000).toFixed(1);
  const msg = record.lastMsg;
  const progressText = msg?.total ? `${msg.done ?? 0}/${msg.total}` : strings.maps.stageQueued;
  record.sink.label.textContent = `${stageLabel(record.currentKind)} - ${progressText} (${elapsed} c)`;
}

// Binds `record` to a freshly rendered row's progress DOM and repaints it from the record's
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

function renderMapRow(m) {
  const allReady = m.hasLineups && m.hasStandSpots && m.hasRadar;
  const statusTd = el(
    "td",
    null,
    pill(strings.maps.geometry, m.hasLineups),
    pill(strings.maps.standSpots, m.hasStandSpots),
    pill(strings.maps.radar, m.hasRadar),
  );
  if (m.stale) {
    const msg = state.config?.gameDir ? strings.maps.staleWrongBuild : strings.maps.staleNoGameDir;
    statusTd.append(el("div", { className: "stale", textContent: msg }));
  }

  const actions = el("div", { className: "map-actions" });
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
    const openBtn = el("button", {
      type: "button",
      className: "primary",
      textContent: strings.maps.openButton,
    });
    openBtn.addEventListener("click", () => selectMap(m.map));
    actions.append(openBtn);
  } else {
    const prepareBtn = el("button", { type: "button", textContent: strings.maps.prepareButton });
    prepareBtn.addEventListener("click", () => {
      startJobFlow(
        missingKinds(m),
        m.map,
        { progressBox, label, barSpan, disableButtons: [prepareBtn], cancelBtn },
        () => showMapsScreen(),
      );
    });
    actions.append(prepareBtn);
  }

  if (m.stale && !record) {
    const reextractBtn = el("button", { type: "button", textContent: strings.maps.reextractButton });
    reextractBtn.addEventListener("click", () => {
      startJobFlow(
        ["extract"],
        m.map,
        { progressBox, label, barSpan, disableButtons: [reextractBtn], cancelBtn },
        () => showMapsScreen(),
      );
    });
    actions.append(reextractBtn);
  }
  actions.append(cancelBtn);

  return el(
    "tr",
    null,
    el("td", { textContent: m.map }),
    el("td", { textContent: m.build }),
    statusTd,
    el("td", null, actions, progressBox),
  );
}

function renderExtractNewForm(compact = false) {
  const input = el("input", {
    id: "extract-map-name",
    type: "text",
    placeholder: strings.maps.extractNamePlaceholder,
  });
  const label = el("label", { htmlFor: "extract-map-name", textContent: strings.maps.extractNewLabel });
  const btn = el("button", { type: "button", textContent: strings.maps.extractButton });
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
    children.unshift(el("p", { className: "hint", textContent: strings.maps.noMaps }));
  }
  return el("div", { className: "field" }, ...children);
}

function selectMap(map) {
  showMapScreen(map);
  putConfig({ lastMap: map });
}

// ---- screen 3: map + target + solve -------------------------------------------------------------

const ALL_TYPES = ["Stand", "Crouch", "JumpThrow", "CrouchJumpThrow", "RunJumpThrow"];
const ALL_STRENGTHS = [1, 0.5, 0];

// `opts.autoBody`: a full `/api/lineup` request body reconstructed from the address bar
// (`bodyFromHash`) - when given, the target/origin/params it carries are applied and the solve
// starts immediately once the radar has loaded.
async function showMapScreen(map, opts = {}) {
  state.screen = "map";
  state.currentMap = map;
  destroyCurrentMapView();
  radarView = null;
  app.replaceChildren();
  announce(map);

  const mapSummary = state.maps.find((m) => m.map === map) ?? {};

  const header = el(
    "div",
    { className: "map-screen-header" },
    el("h1", { textContent: map }),
    el("button", { type: "button", textContent: strings.mapScreen.backToList, onclick: showMapsScreen }),
  );
  const wrap = el("div", { className: "radar-wrap" });
  const canvas = el("canvas", { id: "radar-canvas", role: "img", "aria-label": `Радар карты ${map}` });
  wrap.append(canvas);
  const caption = el("p", { className: "hint" });

  // RED-1: params is the tallest box by far - it goes last, collapsed, so target/run/results
  // (what a two-minute solve actually needs seen) are the ones sitting in the visible band.
  const paramsContent = el("div", { className: "params-content" });
  const paramsBox = el(
    "details",
    { className: "field params-box" },
    el("summary", { textContent: strings.solveParams.heading }),
    paramsContent,
  );
  const targetBox = el("div", { className: "field target-box" });
  const runBox = el("div", { className: "field run-box" });
  const panelBox = el("div", { className: "panel-box" });
  const sidebar = el("div", { className: "map-sidebar" }, targetBox, runBox, panelBox, paramsBox);
  const layout = el("div", { className: "solve-layout" }, wrap, sidebar);

  app.append(header, caption, layout);

  const { data: radarData, error: radarError } = await fetchRadar(map);
  if (radarError !== undefined) {
    caption.className = "status status-error";
    caption.textContent = radarError ?? strings.errors.serverDown;
    return;
  }

  // ---- per-screen solve state ----
  const solveState = {
    target: null, // { x, y, z, label }
    origin: null, // { x, y, reach }
    params: { scope: "all", originReach: 300, tolerance: 80, minStability: 0.4, fineScan: false, types: [...ALL_TYPES], strengths: [...ALL_STRENGTHS], broken: [] },
    running: false,
    controller: null,
  };
  let pendingLevels = null; // { x, y, levels }
  let mapView = null;
  let originStatusBox = null;
  let runRefs = null;
  let scopeSelectRef = null;

  // AMBER-12: a right-click origin must be reflected in the "where to throw from" select, not
  // just on the map - otherwise the control keeps reading "по всей карте" while the request
  // actually carries an origin, and there is no single action that clears it.
  function syncScopeSelect() {
    if (!scopeSelectRef) {
      return;
    }
    const pointOption = scopeSelectRef.querySelector('option[value="point"]');
    if (solveState.origin && !pointOption) {
      scopeSelectRef.append(el("option", { value: "point", textContent: strings.solveParams.scopePointOption }));
    } else if (!solveState.origin && pointOption) {
      pointOption.remove();
    }
    scopeSelectRef.value = solveState.origin ? "point" : solveState.params.scope;
  }

  const panel = createPanel(panelBox, {
    onSelect: (id) => mapView?.setSelected(id),
    onHoverEnter: (id) => mapView?.setHover(id),
    onHoverLeave: () => mapView?.setHover(null),
  });

  function applyTarget(t) {
    solveState.target = t;
    pendingLevels = null;
    mapView?.setTarget(t);
    renderTargetBox();
  }

  // RED-2: a click that only narrows down to a level choice must not leave the previous
  // target (and its cross on the map) in place - otherwise "run" solves for the old point.
  function clearTarget() {
    solveState.target = null;
    mapView?.clearTarget();
  }

  async function handleMapClick(wx, wy) {
    const { data, error } = await fetchLevels(map, wx, wy);
    if (error !== undefined) {
      caption.className = "status status-error";
      caption.textContent = error ?? strings.errors.serverDown;
      return;
    }
    const levels = data.levels ?? [];
    if (levels.length === 0) {
      // AMBER-11: no nav mesh here - inventing z=0 would put the target in mid-air (and, on a
      // map whose geometry sits far from the origin, feed it straight into the server crash).
      clearTarget();
      pendingLevels = null;
      caption.className = "status status-error";
      caption.textContent = strings.mapScreen.noFloorHere;
      renderTargetBox();
      return;
    }
    caption.textContent = "";
    if (levels.length === 1) {
      applyTarget({ x: wx, y: wy, z: levels[0].z, label: levels[0].name ?? null });
    } else {
      clearTarget();
      pendingLevels = { x: wx, y: wy, levels };
      renderTargetBox();
    }
  }

  function updateOriginStatus() {
    if (!originStatusBox) {
      return;
    }
    originStatusBox.textContent = solveState.origin
      ? `${strings.solveParams.scopePoint}: ${solveState.origin.x.toFixed(0)}, ${solveState.origin.y.toFixed(0)} (±${solveState.origin.reach})`
      : "";
  }

  function handleOriginClick(wx, wy) {
    solveState.origin = { x: wx, y: wy, reach: solveState.params.originReach };
    mapView?.setOrigin(solveState.origin);
    updateOriginStatus();
    syncScopeSelect();
  }

  function toggleInArray(arr, value, checked) {
    const i = arr.indexOf(value);
    if (checked && i < 0) {
      arr.push(value);
    } else if (!checked && i >= 0) {
      arr.splice(i, 1);
    }
  }

  function renderParamsBox() {
    paramsContent.replaceChildren();

    const grenadeOptions = [
      ["smoke", strings.solveParams.grenadeSmoke, true],
      ["flash", strings.solveParams.grenadeFlash, false],
      ["he", strings.solveParams.grenadeHe, false],
      ["molotov", strings.solveParams.grenadeMolotov, false],
      ["decoy", strings.solveParams.grenadeDecoy, false],
    ];
    const grenadeRow = el("div", { className: "field-row", role: "radiogroup", "aria-label": strings.solveParams.grenadeLabel });
    for (const [value, label, enabled] of grenadeOptions) {
      const id = `grenade-${value}`;
      const input = el("input", { type: "radio", name: "grenade", id, value, checked: value === "smoke", disabled: !enabled });
      const text = enabled ? label : `${label} (${strings.solveParams.grenadeComingSoon})`;
      grenadeRow.append(el("span", { className: "radio-item" }, input, el("label", { htmlFor: id, textContent: text })));
    }
    paramsContent.append(el("p", { className: "hint", textContent: strings.solveParams.grenadeLabel }), grenadeRow);

    const scopeSelect = el("select", { id: "scope-select" });
    scopeSelect.append(
      el("option", { value: "all", textContent: strings.solveParams.scopeAll }),
      el("option", { value: "spawns", textContent: strings.solveParams.scopeSpawns }),
    );
    scopeSelectRef = scopeSelect;
    syncScopeSelect();
    scopeSelect.addEventListener("change", () => {
      // The "point" option only ever exists while `solveState.origin` is set (`syncScopeSelect`
      // adds/removes it) - picking any other option is how the placed origin gets removed.
      solveState.params.scope = scopeSelect.value;
      if (solveState.origin) {
        solveState.origin = null;
        mapView?.clearOrigin();
        updateOriginStatus();
        syncScopeSelect();
      }
    });
    paramsContent.append(
      el("label", { htmlFor: "scope-select", textContent: strings.solveParams.scopeLabel }),
      scopeSelect,
      el("p", { className: "hint", textContent: strings.solveParams.scopePoint }),
    );

    const originReachInput = el("input", { id: "origin-reach", type: "number", min: 16, max: 4000, value: solveState.params.originReach });
    originReachInput.addEventListener("input", () => {
      const v = parseFloat(originReachInput.value);
      if (!Number.isFinite(v)) {
        return;
      }
      solveState.params.originReach = v;
      if (solveState.origin) {
        solveState.origin.reach = v;
        mapView?.setOrigin(solveState.origin);
        updateOriginStatus();
      }
    });
    paramsContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "origin-reach", textContent: strings.solveParams.originReachLabel }), originReachInput),
    );
    originStatusBox = el("p", { className: "hint" });
    paramsContent.append(originStatusBox);
    updateOriginStatus();

    const tolInput = el("input", { id: "tolerance-input", type: "number", min: 1, max: 512, value: solveState.params.tolerance });
    tolInput.addEventListener("input", () => {
      const v = parseFloat(tolInput.value);
      if (Number.isFinite(v)) {
        solveState.params.tolerance = v;
      }
    });
    paramsContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "tolerance-input", textContent: strings.solveParams.toleranceLabel }), tolInput),
    );

    const stabInput = el("input", { id: "stability-input", type: "number", min: 0.05, max: 1, step: 0.05, value: solveState.params.minStability });
    stabInput.addEventListener("input", () => {
      const v = parseFloat(stabInput.value);
      if (Number.isFinite(v)) {
        solveState.params.minStability = v;
      }
    });
    paramsContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "stability-input", textContent: strings.solveParams.minStabilityLabel }), stabInput),
    );

    const fineCb = el("input", { type: "checkbox", id: "fine-scan", checked: solveState.params.fineScan });
    fineCb.addEventListener("change", () => {
      solveState.params.fineScan = fineCb.checked;
    });
    paramsContent.append(el("label", { htmlFor: "fine-scan" }, fineCb, ` ${strings.solveParams.fineScanLabel}`));

    paramsContent.append(el("p", { className: "hint", textContent: strings.solveParams.typesLabel }));
    const typesRow = el("div", { className: "field-row" });
    for (const t of ALL_TYPES) {
      const id = `type-${t}`;
      const cb = el("input", { type: "checkbox", id, checked: solveState.params.types.includes(t) });
      cb.addEventListener("change", () => toggleInArray(solveState.params.types, t, cb.checked));
      typesRow.append(el("span", { className: "checkbox-item" }, cb, el("label", { htmlFor: id, textContent: TYPE_LABELS[t] })));
    }
    paramsContent.append(typesRow);

    paramsContent.append(el("p", { className: "hint", textContent: strings.solveParams.strengthsLabel }));
    const strengthsRow = el("div", { className: "field-row" });
    const strengthDefs = [
      [1, strings.solveParams.strength1],
      [0.5, strings.solveParams.strengthHalf],
      [0, strings.solveParams.strength0],
    ];
    for (const [val, label] of strengthDefs) {
      const id = `strength-${val}`;
      const cb = el("input", { type: "checkbox", id, checked: solveState.params.strengths.includes(val) });
      cb.addEventListener("change", () => toggleInArray(solveState.params.strengths, val, cb.checked));
      strengthsRow.append(el("span", { className: "checkbox-item" }, cb, el("label", { htmlFor: id, textContent: label })));
    }
    paramsContent.append(strengthsRow);

    if (mapSummary.hasGlass || mapSummary.hasDoors) {
      paramsContent.append(el("p", { className: "hint", textContent: strings.solveParams.brokenLabel }));
      const brokenRow = el("div", { className: "field-row" });
      if (mapSummary.hasGlass) {
        const cb = el("input", { type: "checkbox", id: "broken-glass", checked: solveState.params.broken.includes("glass") });
        cb.addEventListener("change", () => toggleInArray(solveState.params.broken, "glass", cb.checked));
        brokenRow.append(el("span", { className: "checkbox-item" }, cb, el("label", { htmlFor: "broken-glass", textContent: strings.solveParams.brokenGlass })));
      }
      if (mapSummary.hasDoors) {
        const cb = el("input", { type: "checkbox", id: "broken-doors", checked: solveState.params.broken.includes("doors") });
        cb.addEventListener("change", () => toggleInArray(solveState.params.broken, "doors", cb.checked));
        brokenRow.append(el("span", { className: "checkbox-item" }, cb, el("label", { htmlFor: "broken-doors", textContent: strings.solveParams.brokenDoors })));
      }
      paramsContent.append(brokenRow);
    }
  }

  function renderTargetBox() {
    targetBox.replaceChildren();
    targetBox.append(el("h2", { textContent: strings.mapScreen.targetLabel }));
    if (solveState.target) {
      const t = solveState.target;
      const label = t.label ? ` (${t.label})` : "";
      targetBox.append(el("p", { textContent: `${t.x.toFixed(0)}, ${t.y.toFixed(0)}, ${t.z.toFixed(0)}${label}` }));
    } else {
      targetBox.append(el("p", { className: "hint", textContent: strings.mapScreen.targetNone }));
    }
    targetBox.append(el("p", { className: "hint", textContent: strings.mapScreen.targetHint }));

    const manualInput = el("input", { id: "manual-setpos", type: "text", placeholder: strings.mapScreen.manualPlaceholder });
    const manualBtn = el("button", { type: "button", textContent: strings.mapScreen.manualButton });
    const manualStatus = el("p", { className: "status" });
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
    targetBox.append(
      el("label", { htmlFor: "manual-setpos", textContent: strings.mapScreen.manualLabel }),
      el("div", { className: "field-row" }, manualInput, manualBtn),
      manualStatus,
    );

    if (pendingLevels) {
      const chooser = el("div", { className: "level-chooser" });
      chooser.append(el("p", { textContent: strings.mapScreen.levelsHeading }));
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
      targetBox.append(chooser);
    }
  }

  function paintProgress(refs, lastPhase, startedAt, checkedTotal, verifiedTotal) {
    const elapsed = (Date.now() - startedAt) / 1000;
    const label = strings.solve.phases[lastPhase] ?? lastPhase;
    refs.status.className = "status";
    refs.status.textContent = `${label} - ${strings.solve.elapsed(elapsed)} - ${strings.solve.checkedCount(checkedTotal)}, ${strings.solve.verifiedCount(verifiedTotal)}`;
  }

  function applyResult(data, cameFromCache) {
    const lineups = data.lineups ?? [];
    // AMBER-5: the progress cloud has done its job once a result is in - leaving it drawn just
    // buries the result overlays under however many search points were streamed.
    mapView?.clearPoints();
    mapView?.setLineups(lineups.map((l) => ({ id: l.id, feet: l.feet, rest: l.rest })));
    // BLUE-19: use the server's own settled target (it can differ from the clicked point by a
    // few units), not `solveState.target`.
    const settledTarget = data.target ? { x: data.target[0], y: data.target[1], z: data.target[2] } : solveState.target;
    panel.setResult(lineups, settledTarget);
    const cachedNote = cameFromCache ? `${strings.solve.cachedResult} ` : "";
    if (lineups.length === 0) {
      runRefs.status.className = "status";
      runRefs.status.textContent = `${cachedNote}${data.emptyReason ?? ""} ${strings.solve.emptyHint}`.trim();
    } else {
      runRefs.status.className = "status status-ok";
      runRefs.status.textContent = `${cachedNote}${strings.panel.count(lineups.length)}`.trim();
    }
    // RED-1: scroll the results into view - after a long solve the sidebar may still be
    // showing the run box (or an empty results box) from before the page had anything to show.
    panelBox.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }

  function startSolve(bodyOverride) {
    if (!solveState.target) {
      runRefs.status.className = "status status-error";
      runRefs.status.textContent = strings.solve.needTarget;
      return;
    }
    // AMBER-9: an empty types/strengths selection is indistinguishable, once serialised, from
    // "use every default" - the server would then silently solve with all of them.
    const selErr = selectionError(solveState.params);
    if (selErr) {
      runRefs.status.className = "status status-error";
      runRefs.status.textContent = selErr === "types" ? strings.solve.needTypes : strings.solve.needStrengths;
      return;
    }
    if (solveState.running) {
      return;
    }
    solveState.running = true;
    mapView?.clearPoints();
    mapView?.setLineups([]);
    mapView?.setSelected(null);
    panel.clear();
    runRefs.runBtn.disabled = true;
    runRefs.cancelBtn.hidden = false;
    runRefs.status.className = "status";
    runRefs.status.textContent = strings.solve.phases.queued;

    const body = bodyOverride ?? buildQuery(map, solveState.target, solveState.origin, solveState.params);
    syncHash(body);

    const startedAt = Date.now();
    let lastPhase = "queued";
    let checkedTotal = 0;
    let verifiedTotal = 0;
    const timer = setInterval(() => paintProgress(runRefs, lastPhase, startedAt, checkedTotal, verifiedTotal), 1000);

    function finish() {
      clearInterval(timer);
      solveState.running = false;
      solveState.controller = null;
      runRefs.runBtn.disabled = false;
      runRefs.cancelBtn.hidden = true;
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
          mapView?.addCheckedPoints(msg.checked.map((p) => ({ x: p[0], y: p[1] })));
        } else if (msg.verified) {
          verifiedTotal += msg.verified.length;
          // AMBER-4: `ok` (index 3) tells a verified-and-failed candidate apart from a real find
          // - painting both in the bright "found" colour would misrepresent the search.
          mapView?.addVerifiedPoints(msg.verified.map((p) => ({ x: p[0], y: p[1], ok: !!p[3] })));
        }
      },
      onResult: (data, streamed) => {
        finish();
        // AMBER-8: "came from cache" is whether the stream reader ever saw a non-terminal line,
        // not whether any points happened to land on the map - a first solve that streams
        // nothing (a target inside solid geometry) is not a cache hit.
        applyResult(data, !streamed);
      },
      onError: (message) => {
        finish();
        runRefs.status.className = "status status-error";
        runRefs.status.textContent = message ?? strings.errors.serverDown;
      },
      onCancelled: () => {
        finish();
        runRefs.status.className = "status";
        runRefs.status.textContent = "";
      },
    });
  }

  function renderRunBox() {
    runBox.replaceChildren();
    const runBtn = el("button", { type: "button", className: "primary", textContent: strings.solve.runButton });
    const cancelBtn = el("button", { type: "button", textContent: strings.solve.cancelButton, hidden: true });
    const status = el("p", { className: "status", role: "status" });
    runBtn.addEventListener("click", () => startSolve());
    cancelBtn.addEventListener("click", () => solveState.controller?.cancel());
    runBox.append(el("div", { className: "field-row" }, runBtn, cancelBtn), status);
    runRefs = { runBtn, cancelBtn, status };
  }

  function applyAutoBody(body) {
    applyTarget({ x: body.target[0], y: body.target[1], z: body.target[2], label: null });
    if (body.origin) {
      solveState.origin = { x: body.origin[0], y: body.origin[1], reach: body.originReach ?? 300 };
      mapView?.setOrigin(solveState.origin);
    }
    if (body.scope) {
      solveState.params.scope = body.scope;
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
  }

  const img = new Image();
  img.onload = () => {
    mapView = createMapView(canvas, radarData, img, state.theme);
    radarView = mapView;
    currentMapView = mapView;
    mapView.onClick((wx, wy) => handleMapClick(wx, wy));
    mapView.onRightClick((wx, wy) => handleOriginClick(wx, wy));
    if (opts.autoBody) {
      applyAutoBody(opts.autoBody);
    }
    renderParamsBox();
    renderTargetBox();
    renderRunBox();
    if (opts.autoBody) {
      startSolve(opts.autoBody);
    }
  };
  img.onerror = () => {
    caption.className = "status status-error";
    caption.textContent = strings.errors.genericPrefix;
  };
  img.src = radarPngUrl(map) + `?v=${encodeURIComponent(radarData.build ?? "0")}`;
}
