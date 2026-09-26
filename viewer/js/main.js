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
  hasUsableRender,
} from "./api.js?v=1";
import { renderSetup } from "./setup.js?v=1";
import { startPrepare, reconnectJob, stageLabel } from "./jobs.js?v=1";
import { createMapView } from "./map2d.js?v=1";
import { runSolve, buildQuery, parseSetpos, selectionError } from "./solve.js?v=1";
import { createPanel, TYPE_LABELS } from "./panel.js?v=1";
import { createSceneView } from "./scene3d.js?v=1";

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
  // A link needs either a point `target` or a `targetArea` to be worth auto-running
  // (`s6g2_target_area.md`) - the two are mutually exclusive, so either is enough on its own.
  if (!map || (!targetStr && !params.get("targetArea"))) {
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
  const parts = [`map=${encodeURIComponent(body.map)}`];
  // `target` gets its own fixed-precision formatting; `targetArea` (area mode, `body.target`
  // absent - `s6g2_target_area.md`) falls through to the generic array formatting below, same as
  // `originArea` already does.
  if (body.target) {
    parts.push(`target=${body.target.map((v) => v.toFixed(1)).join(",")}`);
  }
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
  const threeContainer = el("div", { className: "three-container", hidden: true });
  const fpvOverlay = el("div", { className: "fpv-overlay", hidden: true });
  wrap.append(canvas, threeContainer, fpvOverlay);
  const caption = el("p", { className: "hint" });

  // F3b-1b: the 2D/3D toggle plus the 3D-only controls ("show collisions", fly/orbit).
  const toggle2dBtn = el("button", { type: "button", textContent: strings.view3d.toggle2d, className: "primary" });
  const toggle3dBtn = el("button", { type: "button", textContent: strings.view3d.toggle3d });
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
  const viewToolbar = el(
    "div",
    { className: "view-toolbar" },
    el("div", { className: "field-row", role: "group", "aria-label": "2D/3D" }, toggle2dBtn, toggle3dBtn, collisionsBtn, cameraModeBtn, lightingBtn),
    view3dStatus,
    el("div", { className: "field-row" }, prepare3dBtn, render3dCancelBtn),
    render3dProgressBox,
  );

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

  app.append(header, viewToolbar, caption, layout);

  const { data: radarData, error: radarError } = await fetchRadar(map);
  if (radarError !== undefined) {
    caption.className = "status status-error";
    caption.textContent = radarError ?? strings.errors.serverDown;
    return;
  }

  // ---- per-screen solve state ----
  const solveState = {
    targetMode: "point", // "point" | "area" (`s6g2_target_area.md`)
    target: null, // { x, y, z, label }
    targetArea: null, // { polygon: [[x,y],...] } - mutually exclusive with `target`
    origin: null, // { x, y, reach }
    originArea: null, // { polygon: [[x,y],...] } (`s6g_origin_area.md`) - mutually exclusive with `origin`
    params: {
      scope: "all", originReach: 300, tolerance: 80, minStability: 0.4, fineScan: false,
      types: [...ALL_TYPES], strengths: [...ALL_STRENGTHS], broken: [],
      areaZMin: null, areaZMax: null, targetAreaZMin: null, targetAreaZMax: null,
    },
    running: false,
    controller: null,
  };
  let pendingLevels = null; // { x, y, levels }
  let mapView = null;
  let sceneView = null; // lazily created on first switch to 3D, kept alive alongside mapView
  let viewMode = "2d"; // "2d" | "3d"
  let originStatusBox = null;
  let targetStatusBox = null;
  let runRefs = null;
  let scopeSelectRef = null;
  // The origin-area tool (`s6g_origin_area.md`): `areaMode` mirrors `mapView`'s own draw-mode
  // flag, `areaDraftCount` is the in-progress vertex count before the polygon is closed (once
  // closed, `solveState.originArea.polygon.length` is used instead).
  let areaMode = false;
  let areaDraftCount = 0;
  let areaToggleBtnRef = null;
  let areaDeleteBtnRef = null;
  // The target-area tool (`s6g2_target_area.md`), same pattern as the origin-area one above, key
  // `"target"` in `mapView`'s own two-area API.
  let targetAreaMode = false;
  let targetAreaDraftCount = 0;
  let targetAreaToggleBtnRef = null;
  let targetAreaDeleteBtnRef = null;
  // The stacked-floor level buttons (review G2 round 3, decision 4): the clusters
  // `prefillAreaZRange` found under the most recently closed area, one per key, or `null` before
  // any area has been closed or once one clears - `renderParamsBox`/`renderTargetAreaControls`
  // only show the button row while there is more than one cluster.
  let areaLevels = null;
  let targetAreaLevels = null;
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
  });

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
  }

  // RED-2: a click that only narrows down to a level choice must not leave the previous
  // target (and its cross on the map) in place - otherwise "run" solves for the old point.
  function clearTarget() {
    solveState.target = null;
    for (const v of views()) v.clearTarget();
  }

  // `wz`, when given (a 3D click - the ray already hit a real surface), skips the `/api/levels`
  // lookup entirely: there is no ambiguity to resolve, the hit point IS the target
  // (`s6f3b_viewer3d.md`: "точка попадания с высотой = цель").
  async function handleMapClick(wx, wy, wz) {
    // In "область" mode a plain map click is only ever meant to place a target-area vertex
    // (through the area tool's own handler, not this one) - ignore it here instead of quietly
    // setting a point target the user never asked for (`s6g2_target_area.md`).
    if (solveState.targetMode === "area") {
      return;
    }
    if (wz !== undefined) {
      caption.textContent = "";
      applyTarget({ x: wx, y: wy, z: wz, label: null });
      return;
    }
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

  // Vs. "точка ±R", shows "область: N вершин" while the origin-area tool has anything drawn
  // (`s6g_origin_area.md`: "строка статуса вместо «точка ±R» показывает «область: N вершин»").
  function updateOriginStatus() {
    if (!originStatusBox) {
      return;
    }
    if (solveState.originArea) {
      originStatusBox.textContent = strings.solveParams.areaStatus(solveState.originArea.polygon.length);
    } else if (areaDraftCount > 0) {
      originStatusBox.textContent = strings.solveParams.areaStatus(areaDraftCount);
    } else if (solveState.origin) {
      originStatusBox.textContent = `${strings.solveParams.scopePoint}: ${solveState.origin.x.toFixed(0)}, ${solveState.origin.y.toFixed(0)} (±${solveState.origin.reach})`;
    } else {
      originStatusBox.textContent = "";
    }
  }

  // Updates the "draw/edit/delete area" buttons' text and visibility from the current state -
  // called after anything that changes `areaMode` or `solveState.originArea`.
  function updateAreaButtons() {
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
    areaLevels = null;
    mapView.setArea("origin", null);
    updateSceneArea("origin");
    if (areaMode) {
      areaMode = false;
      mapView.setAreaMode("origin", false);
    }
  }

  function handleOriginClick(wx, wy) {
    solveState.origin = { x: wx, y: wy, reach: solveState.params.originReach };
    for (const v of views()) v.setOrigin(solveState.origin);
    // A right-click origin and the origin area are mutually exclusive - placing one resets the
    // other (`s6g_origin_area.md`).
    clearArea();
    updateAreaButtons();
    updateOriginStatus();
    syncScopeSelect();
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
    targetAreaLevels = null;
    mapView.setArea("target", null);
    updateSceneArea("target");
    if (targetAreaMode) {
      targetAreaMode = false;
      mapView.setAreaMode("target", false);
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
    const row = el("div", { className: "field-row" }, el("span", { className: "hint", textContent: strings.solveParams.areaLevelsLabel }));
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
        for (const v of views()) v.clearOrigin();
        syncScopeSelect();
      }
      if (solveState.originArea) {
        clearArea();
        updateAreaButtons();
      }
      updateOriginStatus();
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
        for (const v2 of views()) v2.setOrigin(solveState.origin);
        updateOriginStatus();
      }
    });
    paramsContent.append(
      el("div", { className: "field-row" }, el("label", { htmlFor: "origin-reach", textContent: strings.solveParams.originReachLabel }), originReachInput),
    );
    originStatusBox = el("p", { className: "hint" });
    paramsContent.append(originStatusBox);
    updateOriginStatus();

    // `s6g_origin_area.md`: draw/edit a polygon on the map restricting where a throw may
    // originate from, instead of the point+radius above.
    const areaToggleBtn = el("button", { type: "button" });
    const areaDeleteBtn = el("button", { type: "button", textContent: strings.solveParams.areaDeleteButton, hidden: true });
    areaToggleBtnRef = areaToggleBtn;
    areaDeleteBtnRef = areaDeleteBtn;
    areaToggleBtn.addEventListener("click", () => {
      areaMode = !areaMode;
      mapView.setAreaMode("origin", areaMode);
      if (areaMode && solveState.origin) {
        // Starting to draw/edit an area is exclusive with a point origin (`s6g_origin_area.md`).
        solveState.origin = null;
        for (const v of views()) v.clearOrigin();
        syncScopeSelect();
      }
      // map2d.js only ever drafts one area tool at a time - activating this one silently
      // deactivated the target-area tool there too, so its own local flag/button/status must
      // follow (review G2, risk 6).
      if (areaMode && targetAreaMode) {
        targetAreaMode = false;
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
    paramsContent.append(
      el("p", { textContent: strings.solveParams.areaLabel }),
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
    );
    if (areaLevelButtons) {
      paramsContent.append(areaLevelButtons);
    }
    paramsContent.append(el("p", { className: "hint", textContent: strings.solveParams.areaZHint }));
    updateAreaButtons();

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

  // `s6g2_target_area.md`: switches between a point target (as before) and an area target - the
  // two are mutually exclusive.
  function setTargetMode(mode) {
    if (solveState.targetMode === mode) {
      return;
    }
    solveState.targetMode = mode;
    if (mode === "area") {
      clearTarget();
      pendingLevels = null;
    } else {
      clearTargetAreaState();
      updateTargetAreaButtons();
    }
    renderTargetBox();
  }

  function renderTargetBox() {
    targetBox.replaceChildren();
    targetBox.append(el("h2", { textContent: strings.mapScreen.targetLabel }));

    const modeRow = el("div", { className: "field-row", role: "radiogroup", "aria-label": strings.mapScreen.targetModeLabel });
    for (const [value, label] of [["point", strings.mapScreen.targetModePoint], ["area", strings.mapScreen.targetModeArea]]) {
      const id = `target-mode-${value}`;
      const input = el("input", { type: "radio", name: "target-mode", id, value, checked: solveState.targetMode === value });
      input.addEventListener("change", () => setTargetMode(value));
      modeRow.append(el("span", { className: "radio-item" }, input, el("label", { htmlFor: id, textContent: label })));
    }
    targetBox.append(modeRow);

    if (solveState.targetMode === "area") {
      renderTargetAreaControls();
      return;
    }

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

  // The target-area tool's own controls (draw/edit/delete, z range, status) - same shape as the
  // origin area's block in `renderParamsBox`, just targeting `mapView`'s `"target"` area key.
  function renderTargetAreaControls() {
    const toggleBtn = el("button", { type: "button" });
    const deleteBtn = el("button", { type: "button", textContent: strings.solveParams.areaDeleteButton, hidden: true });
    targetAreaToggleBtnRef = toggleBtn;
    targetAreaDeleteBtnRef = deleteBtn;
    toggleBtn.addEventListener("click", () => {
      targetAreaMode = !targetAreaMode;
      mapView.setAreaMode("target", targetAreaMode);
      // Same reasoning as the origin area's own toggle above, mirrored (review G2, risk 6).
      if (targetAreaMode && areaMode) {
        areaMode = false;
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
    targetBox.append(
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
      targetBox.append(targetAreaLevelButtons);
    }
    targetBox.append(el("p", { className: "hint", textContent: strings.solveParams.areaZHint }));
    targetStatusBox = el("p", { className: "hint" });
    targetBox.append(targetStatusBox);
    updateTargetAreaButtons();
    updateTargetStatus();
  }

  function paintProgress(refs, lastPhase, startedAt, checkedTotal, verifiedTotal) {
    const elapsed = (Date.now() - startedAt) / 1000;
    const label = strings.solve.phases[lastPhase] ?? lastPhase;
    refs.status.className = "status";
    refs.status.textContent = `${label} - ${strings.solve.elapsed(elapsed)} - ${strings.solve.checkedCount(checkedTotal)}, ${strings.solve.verifiedCount(verifiedTotal)}`;
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
    if (!solveState.target && !solveState.targetArea) {
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
    lastLineups = [];
    lastSelectedId = null;
    for (const v of views()) {
      v.clearPoints();
      v.setLineups([]);
      v.setSelected(null);
    }
    panel.clear();
    runRefs.runBtn.disabled = true;
    runRefs.cancelBtn.hidden = false;
    runRefs.status.className = "status";
    runRefs.status.textContent = strings.solve.phases.queued;

    const originAreaForQuery = solveState.originArea
      ? { polygon: solveState.originArea.polygon, zMin: solveState.params.areaZMin, zMax: solveState.params.areaZMax }
      : null;
    const targetAreaForQuery = solveState.targetArea
      ? { polygon: solveState.targetArea.polygon, zMin: solveState.params.targetAreaZMin, zMax: solveState.params.targetAreaZMax }
      : null;
    const body =
      bodyOverride ??
      buildQuery(map, solveState.target, targetAreaForQuery, solveState.origin, originAreaForQuery, solveState.params);
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
    }
    if (body.origin) {
      solveState.origin = { x: body.origin[0], y: body.origin[1], reach: body.originReach ?? 300 };
      for (const v of views()) v.setOrigin(solveState.origin);
    } else if (body.originArea) {
      solveState.originArea = { polygon: body.originArea };
      mapView.setArea("origin", body.originArea);
      if (body.zMin != null) {
        solveState.params.areaZMin = body.zMin;
      }
      if (body.zMax != null) {
        solveState.params.areaZMax = body.zMax;
      }
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

  // ---- F3b-1b: the 2D/3D toggle, "show collisions", camera mode, and the first-person view ------

  function ensureSceneView() {
    if (sceneView) {
      return sceneView;
    }
    sceneView = createSceneView(threeContainer, map, mapSummary, state.theme);
    currentViews.push(sceneView);
    sceneView.onClick((wx, wy, wz) => handleMapClick(wx, wy, wz));
    sceneView.onLoadProgress((loaded, total) => {
      const percent = total > 0 ? Math.round((loaded / total) * 100) : null;
      view3dStatus.className = "hint";
      view3dStatus.textContent = strings.view3d.loading(percent);
    });
    sceneView.onLoadDone(() => {
      view3dStatus.textContent = strings.view3d.flyHint;
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
    if (!viewToolbar.isConnected) {
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

  function switchViewMode(mode) {
    if (viewMode === mode) {
      return;
    }
    if (mode === "3d" && !hasUsableRender(mapSummary)) {
      view3dStatus.className = "hint status-error";
      view3dStatus.textContent = mapSummary.hasRender ? strings.view3d.renderOutdated : strings.view3d.noRender;
      render3dBlocked = true;
      syncPrepare3dButton();
      return;
    }
    render3dBlocked = false;
    viewMode = mode;
    if (mode === "3d") {
      // Unhide *before* creating/resizing the scene view - `threeContainer.getBoundingClientRect()`
      // reads 0x0 while `hidden` (`display: none`), and nothing else is guaranteed to correct that
      // later (a `ResizeObserver` on a non-rendered box doesn't fire until it renders again).
      canvas.hidden = true;
      threeContainer.hidden = false;
      const view = ensureSceneView();
      view.resize();
      radarView = null; // theme toggle no-ops while 3D is shown, same as off the map screen
      toggle3dBtn.className = "primary";
      toggle2dBtn.className = "";
      collisionsBtn.hidden = false;
      cameraModeBtn.hidden = false;
      lightingBtn.hidden = !view.isLightingSupported();
      view3dStatus.className = "hint";
      view3dStatus.textContent = strings.view3d.flyHint;
    } else {
      canvas.hidden = false;
      threeContainer.hidden = true;
      radarView = mapView;
      toggle2dBtn.className = "primary";
      toggle3dBtn.className = "";
      collisionsBtn.hidden = true;
      cameraModeBtn.hidden = true;
      lightingBtn.hidden = true;
      view3dStatus.className = "hint";
      view3dStatus.textContent = "";
    }
    syncPrepare3dButton();
  }
  toggle2dBtn.addEventListener("click", () => switchViewMode("2d"));
  toggle3dBtn.addEventListener("click", () => switchViewMode("3d"));

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
    fpvOverlay.replaceChildren(
      el("div", { className: "fpv-crosshair", "aria-hidden": "true" }),
      el(
        "div",
        { className: "fpv-label" },
        el("div", { textContent: `${info.how} - ${info.click}` }),
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
    renderFpvOverlay(info);
    fpvOverlay.hidden = false;
    document.addEventListener("keydown", onFpvKeydown);
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
    mapView.onAreaChange("origin", ({ points, closed }) => {
      areaDraftCount = points.length;
      // `closed` stays true on every later drag of an already-closed polygon's vertex too, not
      // just the one event where it first closes - only that first transition should auto-exit
      // the tool, or a drag's very first `pointermove` would turn `areaMode` off mid-drag and the
      // rest of the drag would pan the map instead of moving the vertex.
      const justClosed = closed && !solveState.originArea;
      if (closed) {
        solveState.originArea = { polygon: points.map((p) => [p.x, p.y]) };
        updateSceneArea("origin");
        if (solveState.origin) {
          solveState.origin = null;
          for (const v of views()) v.clearOrigin();
          syncScopeSelect();
        }
        if (justClosed) {
          areaMode = false;
          mapView.setAreaMode("origin", false);
          prefillAreaZRange(solveState.originArea.polygon).then((prefill) => {
            if (!prefill) {
              // No nav under the new area (e.g. a roof): don't keep the previous area's range.
              areaLevels = null;
              solveState.params.areaZMin = null;
              solveState.params.areaZMax = null;
              renderParamsBox();
              updateSceneArea("origin");
              return;
            }
            areaLevels = prefill.clusters;
            [solveState.params.areaZMin, solveState.params.areaZMax] = prefill.range;
            renderParamsBox();
            updateSceneArea("origin");
          });
        }
      }
      updateAreaButtons();
      updateOriginStatus();
    });
    mapView.onAreaChange("target", ({ points, closed }) => {
      targetAreaDraftCount = points.length;
      // Same "only the first close transition exits the tool" reasoning as the origin area above.
      const justClosed = closed && !solveState.targetArea;
      if (closed) {
        solveState.targetArea = { polygon: points.map((p) => [p.x, p.y]) };
        updateSceneArea("target");
        if (solveState.target) {
          solveState.target = null;
          for (const v of views()) v.clearTarget();
        }
        if (justClosed) {
          targetAreaMode = false;
          mapView.setAreaMode("target", false);
          prefillAreaZRange(solveState.targetArea.polygon).then((prefill) => {
            if (!prefill) {
              // No nav under the new area (e.g. a roof): don't keep the previous area's range.
              targetAreaLevels = null;
              solveState.params.targetAreaZMin = null;
              solveState.params.targetAreaZMax = null;
              renderTargetBox();
              updateSceneArea("target");
              return;
            }
            targetAreaLevels = prefill.clusters;
            [solveState.params.targetAreaZMin, solveState.params.targetAreaZMax] = prefill.range;
            renderTargetBox();
            updateSceneArea("target");
          });
        }
      }
      updateTargetAreaButtons();
      updateTargetStatus();
    });
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
