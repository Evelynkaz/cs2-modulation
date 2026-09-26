// The lineup list: sort/filters, one card per lineup with plain-Russian instructions, a preview
// gallery (S6n's `sceneView.capturePreview`), hover/select wiring back to the map, and the copy-
// `console`-string action.

import { strings } from "./strings.js?v=4";
import { segmented, chip, showToast, emptyState } from "./ui.js?v=2";
import { icon } from "./icons.js?v=1";

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

export const TYPE_LABELS = {
  Stand: strings.solveParams.typeStand,
  Crouch: strings.solveParams.typeCrouch,
  JumpThrow: strings.solveParams.typeJumpThrow,
  CrouchJumpThrow: strings.solveParams.typeCrouchJumpThrow,
};

// Exported so `main.js`'s first-person overlay label can build itself from the same structured
// fields instead of the server's own English `info.how`/`info.click` (round-2 review finding 5).
export const CLICK_LABELS = {
  left: strings.panel.clickLeft,
  "left+right": strings.panel.clickBoth,
  right: strings.panel.clickRight,
};

const DIFFICULTY_LABEL = { easy: strings.panel.difficultyEasy, medium: strings.panel.difficultyMedium, hard: strings.panel.difficultyHard };
const DIFFICULTY_BADGE = { easy: "badge-easy", medium: "badge-medium", hard: "badge-hard" };

// Difficulty rule (`s6m_consumer_redesign.md`): uses `stabilityWide` when the server provides it
// (another task is adding the field), else falls back to `stability`. "Лёгкая" (green) = wide >=
// 0.8 and bounces <= 6; "Средняя" (amber) = wide >= 0.4 or stability >= 0.8; else "Сложная" (red).
function difficultyOf(l) {
  const wide = l.stabilityWide ?? l.stability;
  if (wide >= 0.8 && l.Bounces <= 6) {
    return "easy";
  }
  if (wide >= 0.4 || l.stability >= 0.8) {
    return "medium";
  }
  return "hard";
}

function isCrouchType(t) {
  return t === "Crouch" || t === "CrouchJumpThrow";
}

function isJumpType(t) {
  return t === "JumpThrow" || t === "CrouchJumpThrow";
}

// Numbered, plain-Russian steps built from structured fields (`type`, `pin`, `click`) - never
// from the server's own English `how` text.
function buildInstructions(l) {
  const steps = [];
  if (l.pin === "corner") {
    steps.push(strings.panel.instructionStandCorner);
  } else if (l.pin === "wall") {
    steps.push(strings.panel.instructionStandWall);
  } else {
    steps.push(strings.panel.instructionStandOpen);
  }
  if (isCrouchType(l.type)) {
    steps.push(strings.panel.instructionCrouch);
  }
  steps.push(strings.panel.instructionAim);
  const clickWord = CLICK_LABELS[l.click] ?? l.click;
  if (isJumpType(l.type)) {
    steps.push(l.click === "left+right" ? strings.panel.instructionThrowJumpBoth : strings.panel.instructionThrowJump(clickWord));
  } else {
    steps.push(l.click === "left+right" ? strings.panel.instructionThrowBoth : strings.panel.instructionThrowClick(clickWord));
  }
  return steps;
}

const AIM_TEXT = { sky: strings.panel.aimSky, edge: strings.panel.aimEdge, reticle: strings.panel.aimReticle, flat: strings.panel.aimFlat };

function aimRefText(aimRef) {
  return AIM_TEXT[aimRef?.tier] ?? strings.panel.aimFlat;
}

function warningsOf(l) {
  const list = [];
  if (l.glass > 0) {
    list.push(strings.panel.warnGlass(l.glass));
  }
  if (l.stateDependent) {
    list.push(strings.panel.warnStateDependent);
  }
  return list;
}

// Russian decimal comma, one decimal place (round-2 design critique 9 - "7,0 с", not "7.00 с").
function ru1(n) {
  return n.toFixed(1).replace(".", ",");
}

function distance3(a, b) {
  const dx = a[0] - b[0];
  const dy = a[1] - b[1];
  const dz = a[2] - b[2];
  return Math.sqrt(dx * dx + dy * dy + dz * dz);
}

// Selects `text` in `input` and tries `document.execCommand("copy")` as the fallback path when
// `navigator.clipboard` is unavailable - the input stays visible either way so a failed
// `execCommand` still leaves the text selected for a manual Ctrl+C.
function fallbackCopy(input, text) {
  input.value = text;
  input.hidden = false;
  input.select();
  try {
    return document.execCommand("copy");
  } catch {
    return false;
  }
}

// `container`: where the panel renders. `handlers`: `onSelect(id|null)`, `onHoverEnter(id)`,
// `onHoverLeave()`, `onFirstPerson(l)`, `onShow3d()` (switches the map to 3D - used by the
// preview placeholder's button), `requestPreview(l, kind)` (-> `Promise<Blob|null>`, feature-
// detects `sceneView.capturePreview` itself - see `main.js`).
export function createPanel(container, handlers) {
  let lineups = [];
  let target = null;
  let selectedId = null;
  // "welcome" (never searched yet) | "searching" (a solve is running) | "settled" (a result, empty
  // or not, has come back at least once) - which empty-column state `render()` shows while
  // `lineups.length === 0` (round-4 integration item 2: the column was just blank before the
  // first solve, with nothing to say while one is running).
  let phase = "welcome";
  const filters = { easyOnly: false, hiddenOnly: false, type: "all", position: "all" };
  let sortMode = "best"; // "best" | "easy" | "fast"
  let filtersOpen = false;
  // The filters popover is `<body>`-appended (see `renderHeader`), so `container.replaceChildren()`
  // never cleans it up on its own - tracked here so every `render()` can remove the previous one
  // before deciding whether to show a fresh one.
  let popoverEl = null;
  // Removes the CURRENT popover's own outside-click/Escape listeners - every `openFiltersPopover`
  // call must replace this (after invoking whatever was here before), or the previous listener
  // pair leaks: it keeps firing on every future click anywhere in the document, each one closing
  // whatever popover happens to be open at the time (round-3 review finding 2).
  let popoverCleanup = null;
  // "popover" | "button" | null - where focus should land once the *next* `render()` finishes;
  // set right before calling `render()`, consumed (and reset to `null`) at the end of it
  // (round-3 review finding 4).
  let focusAfterRender = null;
  // Which cards are pinned open via the "Подробнее" chevron - persists across a filter/sort
  // re-render so toggling a filter doesn't quietly collapse a card the user opened on purpose.
  const expandedIds = new Set();
  // Blob URLs from `capturePreview`, keyed by `${id}:${kind}` - kept across re-renders (a filter
  // change rebuilds the DOM but must not re-fetch the 3D preview), revoked on the next result.
  // Only ever holds successfully-resolved previews - a `null` result is never cached here (round-2
  // review finding 6), so a later retry (once 3D has actually loaded) can succeed.
  const previewCache = new Map();

  function revokeAllPreviews() {
    for (const entry of previewCache.values()) {
      if (entry.url) {
        URL.revokeObjectURL(entry.url);
      }
    }
    previewCache.clear();
  }

  function previewKey(id, kind) {
    return `${id}:${kind}`;
  }

  // Resolves a single preview, through the cache. Returns `null` (never throws, never caches the
  // failure) when unavailable - the caller decides what to show and whether to retry later.
  async function fetchPreview(l, kind) {
    const key = previewKey(l.id, kind);
    const cached = previewCache.get(key);
    if (cached?.status === "ready") {
      return cached;
    }
    const blob = await Promise.resolve(handlers.requestPreview?.(l, kind));
    if (!blob) {
      return null;
    }
    const entry = { status: "ready", url: URL.createObjectURL(blob) };
    previewCache.set(key, entry);
    return entry;
  }

  function passesFilters(l) {
    if (filters.easyOnly && difficultyOf(l) !== "easy") {
      return false;
    }
    if (filters.hiddenOnly && l.exposed) {
      return false;
    }
    if (filters.type !== "all" && l.type !== filters.type) {
      return false;
    }
    if (filters.position === "corner" && l.pin !== "corner") {
      return false;
    }
    if (filters.position === "wall" && l.pin !== "corner" && l.pin !== "wall") {
      return false;
    }
    return true;
  }

  function sortList(list) {
    if (sortMode === "fast") {
      return [...list].sort((a, b) => a.flightTime - b.flightTime);
    }
    if (sortMode === "easy") {
      const rank = { easy: 0, medium: 1, hard: 2 };
      return [...list].sort((a, b) => rank[difficultyOf(a)] - rank[difficultyOf(b)]);
    }
    return list; // "best" - the server's own ranked order
  }

  function typeOptions() {
    const seen = new Set(lineups.map((l) => l.type));
    return [...seen];
  }

  function openLightbox(url) {
    const closeBtn = el("button", { type: "button", className: "lightbox-close icon-btn", innerHTML: icon("close", 18), "aria-label": strings.gallery.lightboxClose });
    const overlay = el("div", { className: "lightbox" }, el("img", { src: url, alt: "" }), closeBtn);
    function close() {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
    }
    function onKey(e) {
      if (e.key === "Escape") {
        close();
      }
    }
    closeBtn.addEventListener("click", close);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) {
        close();
      }
    });
    document.addEventListener("keydown", onKey);
    document.body.append(overlay);
  }

  function buildPreviewTile(entry, label, big) {
    const tile = el("div", { className: big ? "preview-tile preview-aim" : "preview-tile" });
    tile.append(el("img", { src: entry.url, alt: label }), el("span", { className: "preview-tile-label", textContent: label }));
    tile.addEventListener("click", () => openLightbox(entry.url));
    return tile;
  }

  function buildUnsupportedBlock() {
    const btn = el("button", { type: "button", textContent: strings.gallery.showIn3d });
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      handlers.onShow3d?.();
    });
    return el(
      "div",
      { className: "preview-unsupported" },
      el("span", { innerHTML: icon("camera", 22) }),
      el("span", { textContent: strings.gallery.unsupportedBlock }),
      btn,
    );
  }

  // Builds the gallery into `galleryWrap` (once per card instance) - a single placeholder block
  // when previews aren't available right now (2D, or `capturePreview` doesn't exist yet), never
  // three empty tiles (round-2 design critique 6). Also fills in the collapsed card's own small
  // "aim" thumbnail, via `onAimReady`, once that one resolves.
  async function ensureGallery(l, galleryWrap, onAimReady) {
    if (galleryWrap.dataset.state === "loading" || galleryWrap.dataset.state === "ready") {
      return;
    }
    galleryWrap.dataset.state = "loading";
    galleryWrap.className = "preview-gallery";
    galleryWrap.replaceChildren(el("div", { className: "preview-placeholder", textContent: strings.gallery.loading }));

    const aim = await fetchPreview(l, "aim");
    if (!aim) {
      galleryWrap.dataset.state = "";
      galleryWrap.className = "preview-gallery preview-gallery-single";
      galleryWrap.replaceChildren(buildUnsupportedBlock());
      return;
    }
    onAimReady?.(aim);
    const [stand, land] = await Promise.all([fetchPreview(l, "stand"), fetchPreview(l, "land")]);
    galleryWrap.dataset.state = "ready";
    galleryWrap.className = "preview-gallery";
    galleryWrap.replaceChildren(buildPreviewTile(aim, strings.gallery.aimTitle, true));
    galleryWrap.append(
      stand ? buildPreviewTile(stand, strings.gallery.standTitle, false) : buildUnsupportedBlock(),
      land ? buildPreviewTile(land, strings.gallery.landTitle, false) : buildUnsupportedBlock(),
    );
  }

  // Drops any gallery left showing the "unavailable" placeholder and re-runs it - called once 3D
  // actually finishes loading, or right after switching to it (round-2 review finding 6). Only
  // for a card that's actually expanded/selected right now: `galleryWrap` itself is never
  // `hidden` (its parent `.lineup-expanded` is, via `syncCardExpansion`) - checking that instead
  // is what keeps this from rebuilding all N cards' galleries (up to N capture + trajectory
  // requests) when at most one or two are ever visible (round-3 review finding 5).
  function retryPreviews() {
    for (const row of container.querySelectorAll(".lineup-card")) {
      const galleryWrap = row.querySelector(".preview-gallery");
      if (!galleryWrap || galleryWrap.dataset.state !== "" || row.querySelector(".lineup-expanded")?.hidden) {
        continue;
      }
      const l = lineups.find((x) => x.id === row.dataset.id);
      if (!l) {
        continue;
      }
      const thumbImg = row.querySelector(".lineup-thumb");
      ensureGallery(l, galleryWrap, (aim) => {
        if (thumbImg) {
          thumbImg.src = aim.url;
          thumbImg.hidden = false;
        }
      });
    }
  }

  // Shows/builds the expanded block (instructions + gallery + details) exactly while `l`'s card
  // is expanded or selected (`s6m_consumer_redesign.md`: "when a card is expanded or selected").
  function syncCardExpansion(id, row) {
    const l = lineups.find((x) => x.id === id);
    const expandedBlock = row?.querySelector(".lineup-expanded");
    const chevronBtn = row?.querySelector(".lineup-expand-btn");
    if (!l || !expandedBlock) {
      return;
    }
    const visible = selectedId === id || expandedIds.has(id);
    expandedBlock.hidden = !visible;
    if (chevronBtn) {
      chevronBtn.classList.toggle("open", visible);
      chevronBtn.setAttribute("aria-expanded", String(visible));
      chevronBtn.setAttribute("aria-label", visible ? strings.panel.lessButton : strings.panel.moreButton);
    }
    if (visible) {
      const galleryWrap = expandedBlock.querySelector(".preview-gallery");
      const thumbImg = row.querySelector(".lineup-thumb");
      ensureGallery(l, galleryWrap, (aim) => {
        if (thumbImg) {
          thumbImg.src = aim.url;
          thumbImg.hidden = false;
        }
      });
    }
  }

  function renderRow(l) {
    const diff = difficultyOf(l);
    const card = el("li", { className: "lineup-card" + (l.id === selectedId ? " selected" : "") });
    card.dataset.id = l.id;

    const selectArea = el("div", {
      className: "lineup-select",
      tabIndex: 0,
      role: "button",
      "aria-pressed": l.id === selectedId ? "true" : "false",
    });
    selectArea.addEventListener("click", () => toggleSelected(l.id, card, selectArea));
    // Only Enter/Space that started on `selectArea` itself toggle it - a key that bubbled up
    // from the copy/fpv/expand buttons must not also trigger row selection.
    selectArea.addEventListener("keydown", (e) => {
      if (e.target !== selectArea) {
        return;
      }
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        toggleSelected(l.id, card, selectArea);
      }
    });
    selectArea.addEventListener("pointerenter", () => handlers.onHoverEnter?.(l.id));
    selectArea.addEventListener("pointerleave", () => handlers.onHoverLeave?.());

    const header = el(
      "div",
      { className: "lineup-header" },
      el("span", { className: `badge ${DIFFICULTY_BADGE[diff]}`, textContent: DIFFICULTY_LABEL[diff] }),
      el("span", { className: "lineup-name", textContent: `${TYPE_LABELS[l.type] ?? strings.panel.typeUnknown} · ${CLICK_LABELS[l.click] ?? strings.panel.clickUnknown}` }),
    );
    if (l.pin === "corner") {
      header.append(el("span", { className: "pill", textContent: strings.panel.pinCorner }));
    } else if (l.pin === "wall") {
      header.append(el("span", { className: "pill", textContent: strings.panel.pinWall }));
    }
    if (l.insideTargetArea === true) {
      header.append(el("span", { className: "pill", textContent: strings.panel.insideArea }));
    }

    const stats = el(
      "div",
      { className: "lineup-stats" },
      el("span", { innerHTML: icon("clock", 14) }, `${ru1(l.flightTime)} с`),
      el("span", { innerHTML: icon("bounce", 14) }, `${l.Bounces}`),
      el("span", { innerHTML: icon("gauge", 14) }, `${Math.round(l.stability * 100)}%`),
    );
    // A point-target distance stat only - an area target has no single "distance to" (its own
    // "в области" pill above already says what matters) (round-2 review finding 14).
    if (l.insideTargetArea === undefined && target) {
      const dist = Math.round(distance3(l.rest, [target.x, target.y, target.z]));
      stats.append(el("span", { innerHTML: icon("target", 14) }, strings.panel.toTarget(dist)));
    }

    const thumbImg = el("img", { className: "lineup-thumb", alt: "", hidden: true });
    const cachedAim = previewCache.get(previewKey(l.id, "aim"));
    if (cachedAim) {
      thumbImg.src = cachedAim.url;
      thumbImg.hidden = false;
    }

    const mainCol = el("div", { className: "lineup-select-main" }, header, stats);
    selectArea.append(mainCol, thumbImg);

    // ---- expanded content: instructions + gallery + details, one toggle (round-2 design critique 5) ----
    const steps = el("ol", { className: "lineup-steps" });
    for (const step of buildInstructions(l)) {
      steps.append(el("li", { textContent: step }));
    }
    const galleryWrap = el("div", { className: "preview-gallery" });
    const detailsBlock = el("div", { className: "lineup-more" });
    detailsBlock.append(
      el("p", { textContent: `${strings.panel.detailsFeet}: ${l.feet[0].toFixed(0)}, ${l.feet[1].toFixed(0)}, ${l.feet[2].toFixed(0)}` }),
      el("p", { textContent: `${strings.panel.detailsRest}: ${l.rest[0].toFixed(0)}, ${l.rest[1].toFixed(0)}, ${l.rest[2].toFixed(0)}` }),
      el("p", { textContent: `${strings.panel.detailsAim}: ${aimRefText(l.aimRef)}` }),
      el("p", { textContent: strings.panel.detailsHumanError(Math.round(l.humanError)) }),
    );
    for (const warning of warningsOf(l)) {
      detailsBlock.append(el("p", { className: "warn", innerHTML: icon("warning", 14) }, warning));
    }
    const expandedBlock = el("div", { className: "lineup-expanded", hidden: true }, steps, galleryWrap, detailsBlock);

    // ---- actions: primary "Скопировать" + icon "Вид игрока" + icon chevron "Подробнее" ----
    const copyInput = el("input", { type: "text", className: "copy-fallback", readOnly: true, hidden: true });
    const copyBtn = el("button", { type: "button", className: "primary", textContent: strings.panel.copyButton });
    copyBtn.addEventListener("click", () => {
      if (navigator.clipboard?.writeText) {
        navigator.clipboard.writeText(l.console).then(
          () => showToast(strings.panel.copied),
          () => showToast(fallbackCopy(copyInput, l.console) ? strings.panel.copied : strings.panel.copyFallback),
        );
      } else {
        showToast(fallbackCopy(copyInput, l.console) ? strings.panel.copied : strings.panel.copyFallback);
      }
    });
    const fpvBtn = el("button", { type: "button", className: "icon-btn btn-secondary", innerHTML: icon("eye", 16), "aria-label": strings.fpv.button, title: strings.fpv.button });
    fpvBtn.addEventListener("click", () => handlers.onFirstPerson?.(l));
    const chevronBtn = el("button", {
      type: "button",
      className: "icon-btn btn-ghost lineup-expand-btn",
      innerHTML: icon("chevron", 16),
      "aria-expanded": "false",
      "aria-label": strings.panel.moreButton,
      title: strings.panel.moreButton,
    });
    chevronBtn.addEventListener("click", () => {
      if (expandedIds.has(l.id)) {
        expandedIds.delete(l.id);
      } else {
        expandedIds.add(l.id);
      }
      syncCardExpansion(l.id, card);
    });

    const actions = el("div", { className: "lineup-actions" }, copyBtn, fpvBtn, chevronBtn, copyInput);
    card.append(selectArea, actions, expandedBlock);

    if (selectedId === l.id || expandedIds.has(l.id)) {
      expandedBlock.hidden = false;
      chevronBtn.classList.add("open");
      chevronBtn.setAttribute("aria-expanded", "true");
      chevronBtn.setAttribute("aria-label", strings.panel.lessButton);
      ensureGallery(l, galleryWrap, (aim) => {
        thumbImg.src = aim.url;
        thumbImg.hidden = false;
      });
    }
    return card;
  }

  // BLUE-16: toggling a row's selection must not re-render the whole list (that drops keyboard
  // focus) - flip the CSS/ARIA state (and the expanded block) on the two rows involved in place.
  function toggleSelected(id, row, selectArea) {
    const prevId = selectedId;
    selectedId = id === selectedId ? null : id;
    handlers.onSelect?.(selectedId);
    row.classList.toggle("selected", selectedId === id);
    selectArea.setAttribute("aria-pressed", selectedId === id ? "true" : "false");
    syncCardExpansion(id, row);
    if (prevId != null && prevId !== id) {
      const prevRow = container.querySelector(`.lineup-card[data-id="${CSS.escape(prevId)}"]`);
      if (prevRow) {
        prevRow.classList.remove("selected");
        prevRow.querySelector(".lineup-select")?.setAttribute("aria-pressed", "false");
        syncCardExpansion(prevId, prevRow);
      }
    }
  }

  // The results column's own "a solve is running" state - an indeterminate bar, not the run
  // button's own phase/counter text (that already lives in the steps column, no need to
  // duplicate it here) (round-4 integration item 2).
  function renderSearchingState() {
    const barSpan = el("span");
    const bar = el("div", { className: "progress-bar indeterminate" }, barSpan);
    return el(
      "div",
      { className: "panel-searching" },
      el("p", { className: "empty-title", textContent: strings.panel.searchingTitle }),
      el("p", { className: "hint", textContent: strings.panel.searchingHint }),
      bar,
    );
  }

  function resetFilters() {
    filters.easyOnly = false;
    filters.hiddenOnly = false;
    filters.type = "all";
    filters.position = "all";
    render();
  }

  // How many of the popover's own filters (Упор/Скрытые/тип - not "Только лёгкие", which has its
  // own always-visible chip) are currently non-default.
  function popoverFilterCount() {
    let n = 0;
    if (filters.position !== "all") n++;
    if (filters.hiddenOnly) n++;
    if (filters.type !== "all") n++;
    return n;
  }

  function renderActiveFilterPills() {
    const pills = [];
    if (filters.position !== "all") {
      const label = filters.position === "wall" ? strings.panel.positionWallOrCorner : strings.panel.positionCornerOnly;
      pills.push([`${strings.panel.positionFilterLabel}: ${label}`, () => { filters.position = "all"; render(); }]);
    }
    if (filters.hiddenOnly) {
      pills.push([strings.panel.hiddenOnly, () => { filters.hiddenOnly = false; render(); }]);
    }
    if (filters.type !== "all") {
      pills.push([TYPE_LABELS[filters.type] ?? strings.panel.typeUnknown, () => { filters.type = "all"; render(); }]);
    }
    if (pills.length === 0) {
      return null;
    }
    const row = el("div", { className: "active-filters" });
    for (const [label, onRemove] of pills) {
      const removeBtn = el("button", { type: "button", innerHTML: icon("close", 12), "aria-label": `${strings.panel.removeFilter}: ${label}` });
      removeBtn.addEventListener("click", onRemove);
      row.append(el("span", { className: "filter-pill" }, label, removeBtn));
    }
    return row;
  }

  function renderFiltersPopover() {
    const types = typeOptions();
    const content = [
      el("p", { className: "popover-row-label", textContent: strings.panel.positionFilterLabel }),
      segmented({
        name: "panel-position",
        value: filters.position,
        variant: "chips",
        ariaLabel: strings.panel.positionFilterLabel,
        options: [
          { value: "all", label: strings.panel.positionAll },
          { value: "wall", label: strings.panel.positionWallOrCorner },
          { value: "corner", label: strings.panel.positionCornerOnly },
        ],
        onChange: (v) => { filters.position = v; focusAfterRender = "popover"; render(); },
      }),
      chip({ id: "filter-hidden", label: strings.panel.hiddenOnly, checked: filters.hiddenOnly, onChange: (c) => { filters.hiddenOnly = c; focusAfterRender = "popover"; render(); } }),
    ];
    if (types.length > 1) {
      content.push(
        el("p", { className: "popover-row-label", textContent: strings.panel.typeFilterLabel }),
        segmented({
          name: "panel-type",
          value: filters.type,
          variant: "chips",
          ariaLabel: strings.panel.typeFilterLabel,
          options: [{ value: "all", label: strings.panel.typeAll }, ...types.map((t) => ({ value: t, label: TYPE_LABELS[t] ?? strings.panel.typeUnknown }))],
          onChange: (v) => { filters.type = v; focusAfterRender = "popover"; render(); },
        }),
      );
    }
    return el("div", { className: "popover", role: "dialog", "aria-label": strings.panel.filtersButton }, ...content);
  }

  // `position: fixed`, positioned from `filtersBtn`'s own (already on-screen) rect and appended to
  // `<body>` - not nested under `.popover-anchor`: `.map-results` needs `overflow-y: auto` to
  // scroll the list, and per the CSS overflow spec a non-"visible" `overflow-y` forces
  // `overflow-x` to also compute to "auto" on the very same element, clipping anything positioned
  // outside its box - including an absolutely-positioned popover nested inside it. Must run after
  // `filtersBtn` is actually attached to the live document - its rect is all-zero otherwise.
  function openFiltersPopover(filtersBtn) {
    const popover = renderFiltersPopover();
    document.body.append(popover);
    const rect = filtersBtn.getBoundingClientRect();
    const popW = popover.getBoundingClientRect().width;
    // Clamped `left`, not anchored purely via `right` - at <=1280px (where the results column is
    // narrower and the button sits close to the panel's own right edge) a `right`-anchored
    // popover this wide can run off the left edge of the viewport (round-3 review finding 3).
    const left = Math.max(8, Math.min(rect.right - popW, window.innerWidth - popW - 8));
    popover.style.position = "fixed";
    popover.style.top = `${rect.bottom + 6}px`;
    popover.style.left = `${left}px`;
    popover.style.right = "auto";
    popoverEl = popover;

    function close() {
      filtersOpen = false;
      focusAfterRender = "button";
      render();
    }
    function onDocClick(e) {
      if (!popover.contains(e.target) && e.target !== filtersBtn) {
        close();
      }
    }
    function onKey(e) {
      if (e.key === "Escape") {
        close();
      }
    }
    // Closes on the next outside click/Escape - queued via `setTimeout` so the very click that
    // opened the popover (already bubbling) doesn't immediately close it again.
    const timer = setTimeout(() => {
      document.addEventListener("click", onDocClick);
      document.addEventListener("keydown", onKey);
    }, 0);
    // Replaces whatever the previous popover's own cleanup was - `render()`/`clear()` call the
    // OLD one (if any) before ever reaching here, so this is always the only pair of listeners
    // alive at a time (round-3 review finding 2).
    popoverCleanup = () => {
      clearTimeout(timer);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("keydown", onKey);
    };
  }

  // Returns `{ node, filtersBtn }` - the caller appends `node` to the live document first, then
  // (only if `filtersOpen`) calls `openFiltersPopover(filtersBtn)` once `filtersBtn` actually has
  // a real position to anchor to.
  function renderHeader() {
    const sortSelect = el("select", { className: "select", "aria-label": strings.panel.sortLabel });
    for (const [value, label] of [["best", strings.panel.sortBest], ["easy", strings.panel.sortEasy], ["fast", strings.panel.sortFast]]) {
      sortSelect.append(el("option", { value, textContent: label, selected: value === sortMode }));
    }
    sortSelect.addEventListener("change", () => {
      sortMode = sortSelect.value;
      render();
    });
    const heading = el(
      "div",
      { className: "panel-heading-row" },
      el("h2", { textContent: `${strings.panel.heading} · ${lineups.length}` }),
      sortSelect,
    );

    const easyChip = chip({ id: "filter-easy", label: strings.panel.stableOnly, checked: filters.easyOnly, onChange: (c) => { filters.easyOnly = c; render(); } });
    const filterCount = popoverFilterCount();
    const filtersBtn = el("button", {
      type: "button",
      className: "btn-secondary",
      textContent: filterCount > 0 ? strings.panel.filtersButtonActive(filterCount) : strings.panel.filtersButton,
    });
    filtersBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      filtersOpen = !filtersOpen;
      // Rebuilding the header always replaces this very button - without this, opening or
      // closing the popover silently drops keyboard focus to `<body>` (round-3 review finding 4).
      focusAfterRender = filtersOpen ? "popover" : "button";
      render();
    });

    const toolbar = el("div", { className: "panel-toolbar" }, easyChip, filtersBtn);
    const pills = renderActiveFilterPills();
    return { node: el("div", {}, heading, toolbar, pills), filtersBtn };
  }

  // Applied at the end of every `render()` exit path once the (possibly-just-reopened) popover
  // exists - consumes `focusAfterRender`, so a render that wasn't asked to move focus never does
  // (round-3 review finding 4).
  function applyFocusAfterRender(header) {
    if (focusAfterRender === "button") {
      header?.filtersBtn?.focus();
    } else if (focusAfterRender === "popover" && popoverEl) {
      (popoverEl.querySelector("input:checked") ?? popoverEl.querySelector("input, button"))?.focus();
    }
    focusAfterRender = null;
  }

  function render() {
    // The previous popover's own outside-click/Escape listeners must go before it does - leaving
    // them meant every later click anywhere reopened this same cleanup path on a detached popover
    // (round-3 review finding 2).
    popoverCleanup?.();
    popoverCleanup = null;
    popoverEl?.remove();
    popoverEl = null;
    container.replaceChildren();
    if (lineups.length === 0) {
      container.append(
        phase === "searching"
          ? renderSearchingState()
          : emptyState({ title: emptyTitle, hint: emptyHint, iconName: phase === "welcome" ? "target" : undefined }),
      );
      focusAfterRender = null;
      return;
    }
    const header = renderHeader();
    container.append(header.node);
    if (filtersOpen) {
      openFiltersPopover(header.filtersBtn);
    }
    const visible = sortList(lineups.filter(passesFilters));
    if (visible.length === 0) {
      container.append(
        emptyState({
          title: strings.panel.emptyFilteredTitle,
          hint: strings.panel.emptyFilteredHint,
          actionLabel: strings.panel.resetFilters,
          onAction: resetFilters,
        }),
      );
      applyFocusAfterRender(header);
      return;
    }
    const list = el("ul", { className: "lineup-list" });
    for (const l of visible) {
      list.append(renderRow(l));
    }
    container.append(list);
    applyFocusAfterRender(header);
  }

  // Set by `setResult` (round-2 review finding 10) - the "no lineups at all" empty state explains
  // the server's own reason, in words, instead of a generic "nothing found". Doubles as the
  // "welcome" (never-searched) text while `phase === "welcome"` - `setResult` always overwrites
  // both before `phase` can ever go back to "welcome".
  let emptyTitle = strings.panel.emptyNoResultsTitle;
  let emptyHint = strings.panel.emptyNoResultsHint;
  // The column used to just stay blank until the first solve (round-4 integration item 2) -
  // render the "welcome" empty state right away instead.
  render();

  return {
    setResult(list, targetPoint, emptyReason) {
      lineups = list;
      target = targetPoint;
      selectedId = null;
      expandedIds.clear();
      revokeAllPreviews();
      filters.easyOnly = false;
      filters.hiddenOnly = false;
      filters.type = "all";
      filters.position = "all";
      filtersOpen = false;
      sortMode = "best";
      phase = "settled";
      if (list.length === 0) {
        emptyTitle = strings.panel.emptyReasonPrefix;
        emptyHint = `${emptyReason ?? ""} ${strings.solve.emptyHint}`.trim();
      } else {
        // The first (best-ranked) card starts expanded, so the headline preview/instructions
        // feature is visible without an extra click (round-2 design critique 5).
        expandedIds.add(list[0].id);
      }
      render();
    },
    // Called both when a new solve starts (blanks the previous result while it runs - `render()`
    // then shows the "searching" state, round-4 integration item 2) and when the map screen is
    // torn down (where nothing renders again anyway).
    clear() {
      lineups = [];
      target = null;
      selectedId = null;
      expandedIds.clear();
      filtersOpen = false;
      focusAfterRender = null;
      phase = "searching";
      popoverCleanup?.();
      popoverCleanup = null;
      popoverEl?.remove();
      popoverEl = null;
      revokeAllPreviews();
      render();
    },
    setSelected(id) {
      selectedId = id;
      render();
    },
    retryPreviews,
  };
}
