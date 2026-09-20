// Entry point: theme + screen routing. Screen 1 (first run) lives in setup.js; the "prepare a
// map" job flow lives in jobs.js; everything else (map list, radar screen) is small enough to
// live here.

import { state, applyTheme, resolveInitialTheme, storeTheme } from "./state.js?v=1";
import { strings } from "./strings.js?v=1";
import { fetchConfig, putConfig, fetchMaps, fetchJobs, fetchRadar, radarPngUrl, deleteJob } from "./api.js?v=1";
import { renderSetup } from "./setup.js?v=1";
import { startPrepare, reconnectJob, stageLabel } from "./jobs.js?v=1";

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
    radarView?.recolor();
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
  reattachJobs();
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

// ---- screen 3: map preparation (radar only for now) --------------------------------------------

async function showMapScreen(map) {
  state.screen = "map";
  state.currentMap = map;
  radarView = null;
  app.replaceChildren();
  announce(map);
  const header = el(
    "div",
    { className: "map-screen-header" },
    el("h1", { textContent: map }),
    el("button", { type: "button", textContent: strings.mapScreen.backToList, onclick: showMapsScreen }),
  );
  const caption = el("p", { className: "hint", textContent: strings.mapScreen.targetComingSoon });
  const wrap = el("div", { className: "radar-wrap" });
  const canvas = el("canvas", {
    id: "radar-canvas",
    role: "img",
    "aria-label": `Радар карты ${map}`,
  });
  wrap.append(canvas);
  app.append(header, caption, wrap);

  const { data, error } = await fetchRadar(map);
  if (error !== undefined) {
    caption.textContent = error ?? strings.errors.serverDown;
    return;
  }

  const img = new Image();
  img.onload = () => {
    radarView = setupRadarCanvas(canvas, img);
  };
  img.onerror = () => {
    caption.textContent = strings.errors.genericPrefix;
  };
  img.src = radarPngUrl(map) + `?v=${encodeURIComponent(data.build ?? "0")}`;
}

// R = class (0 floor, 128 low cover, 255 wall), G = floor height tint, A = nav coverage. The
// boundary outline is written as pure red ([255,0,0,255]) - the same bytes a wall pixel with a
// zero height-tint would already have, so no special case is needed: it just paints as "wall".
const PALETTES = {
  light: { floorLo: [214, 217, 222], floorHi: [236, 238, 241], cover: [176, 182, 192], wall: [46, 50, 58] },
  dark: { floorLo: [36, 40, 48], floorHi: [58, 64, 76], cover: [86, 94, 108], wall: [214, 219, 227] },
};

function lerp(a, b, t) {
  return [
    Math.round(a[0] + (b[0] - a[0]) * t),
    Math.round(a[1] + (b[1] - a[1]) * t),
    Math.round(a[2] + (b[2] - a[2]) * t),
  ];
}

function recolor(img, theme) {
  const off = document.createElement("canvas");
  off.width = img.naturalWidth;
  off.height = img.naturalHeight;
  const octx = off.getContext("2d");
  octx.drawImage(img, 0, 0);
  const imageData = octx.getImageData(0, 0, off.width, off.height);
  const d = imageData.data;
  const palette = PALETTES[theme] ?? PALETTES.light;
  for (let i = 0; i < d.length; i += 4) {
    const r = d[i];
    const g = d[i + 1];
    const a = d[i + 3];
    if (a === 0) {
      d[i + 3] = 0;
      continue;
    }
    let color;
    if (r >= 192) {
      color = palette.wall;
    } else if (r >= 64) {
      color = palette.cover;
    } else {
      color = lerp(palette.floorLo, palette.floorHi, g / 255);
    }
    d[i] = color[0];
    d[i + 1] = color[1];
    d[i + 2] = color[2];
    d[i + 3] = 255;
  }
  octx.putImageData(imageData, 0, 0);
  return off;
}

function setupRadarCanvas(canvas, img) {
  const ctx = canvas.getContext("2d");
  let source = recolor(img, state.theme);
  let userScale = 1;
  const offset = { x: 0, y: 0 };

  function fitScale() {
    const rect = canvas.parentElement.getBoundingClientRect();
    return Math.min(rect.width / source.width, rect.height / source.height);
  }

  function draw() {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.parentElement.getBoundingClientRect();
    canvas.width = Math.max(1, Math.round(rect.width * dpr));
    canvas.height = Math.max(1, Math.round(rect.height * dpr));
    canvas.style.width = `${rect.width}px`;
    canvas.style.height = `${rect.height}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, rect.width, rect.height);
    const scale = fitScale() * userScale;
    // The default view downscales the source (e.g. 2056x1752 into ~920x630); nearest-neighbour
    // there aliases thin walls away. Only go nearest when magnifying past 1:1.
    ctx.imageSmoothingEnabled = scale < 1;
    if (scale < 1) {
      ctx.imageSmoothingQuality = "high";
    }
    const dw = source.width * scale;
    const dh = source.height * scale;
    const cx = rect.width / 2 + offset.x;
    const cy = rect.height / 2 + offset.y;
    ctx.drawImage(source, cx - dw / 2, cy - dh / 2, dw, dh);
  }

  function zoomAt(clientX, clientY, factor) {
    const rect = canvas.getBoundingClientRect();
    const mx = clientX - rect.left;
    const my = clientY - rect.top;
    const oldScale = fitScale() * userScale;
    const cx = rect.width / 2 + offset.x;
    const cy = rect.height / 2 + offset.y;
    const imgX = (mx - cx) / oldScale;
    const imgY = (my - cy) / oldScale;
    userScale = Math.min(8, Math.max(0.5, userScale * factor));
    const newScale = fitScale() * userScale;
    offset.x = mx - imgX * newScale - rect.width / 2;
    offset.y = my - imgY * newScale - rect.height / 2;
    draw();
  }

  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      zoomAt(e.clientX, e.clientY, e.deltaY < 0 ? 1.1 : 1 / 1.1);
    },
    { passive: false },
  );

  // Pointer tracking doubles as drag-to-pan (one pointer) and pinch-to-zoom (two pointers);
  // `touch-action: none` in app.css hands both gestures to us instead of the browser.
  const pointers = new Map();
  let dragging = false;
  let last = null;
  let pinchStartDist = null;
  let pinchStartScale = null;

  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 1) {
      dragging = true;
      last = { x: e.clientX, y: e.clientY };
    } else if (pointers.size === 2) {
      dragging = false;
      const [a, b] = [...pointers.values()];
      pinchStartDist = Math.hypot(a.x - b.x, a.y - b.y);
      pinchStartScale = userScale;
    }
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) {
      return;
    }
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      if (pinchStartDist) {
        userScale = Math.min(8, Math.max(0.5, pinchStartScale * (dist / pinchStartDist)));
        draw();
      }
      return;
    }
    if (!dragging) {
      return;
    }
    offset.x += e.clientX - last.x;
    offset.y += e.clientY - last.y;
    last = { x: e.clientX, y: e.clientY };
    draw();
  });
  const releasePointer = (e) => {
    pointers.delete(e.pointerId);
    if (pointers.size < 2) {
      pinchStartDist = null;
    }
    if (pointers.size === 1) {
      const [p] = pointers.values();
      dragging = true;
      last = { x: p.x, y: p.y };
    } else {
      dragging = false;
    }
  };
  canvas.addEventListener("pointerup", releasePointer);
  canvas.addEventListener("pointercancel", releasePointer);

  canvas.tabIndex = 0;
  canvas.addEventListener("keydown", (e) => {
    const step = 40;
    switch (e.key) {
      case "ArrowLeft":
        offset.x += step;
        break;
      case "ArrowRight":
        offset.x -= step;
        break;
      case "ArrowUp":
        offset.y += step;
        break;
      case "ArrowDown":
        offset.y -= step;
        break;
      case "+":
      case "=":
        userScale = Math.min(8, userScale * 1.1);
        break;
      case "-":
        userScale = Math.max(0.5, userScale / 1.1);
        break;
      default:
        return;
    }
    e.preventDefault();
    draw();
  });

  const resizeObserver = new ResizeObserver(draw);
  resizeObserver.observe(canvas.parentElement);

  // `resize`/ResizeObserver only fire on a CSS-box change; moving the window to a monitor with a
  // different scale factor leaves the same box but a different `devicePixelRatio`, which needs
  // its own watch (the standard `matchMedia(resolution)` idiom - it fires once, so re-arm it).
  function watchDpr() {
    const mql = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    mql.addEventListener(
      "change",
      () => {
        draw();
        watchDpr();
      },
      { once: true },
    );
  }
  watchDpr();

  draw();

  return {
    recolor() {
      source = recolor(img, state.theme);
      draw();
    },
  };
}
