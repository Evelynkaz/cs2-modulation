// The lineup list: filters (stable/hidden/type), one row per lineup with its numbers, hover/select
// wiring back to the map, and the copy-`console`-string button.

import { strings } from "./strings.js?v=1";

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
  RunJumpThrow: strings.solveParams.typeRunJumpThrow,
};

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
// `onHoverLeave()`.
export function createPanel(container, handlers) {
  let lineups = [];
  let target = null;
  let selectedId = null;
  const filters = { stableOnly: false, hiddenOnly: false, type: "all" };

  function passesFilters(l) {
    if (filters.stableOnly && l.stability < 1) {
      return false;
    }
    if (filters.hiddenOnly && l.exposed) {
      return false;
    }
    if (filters.type !== "all" && l.type !== filters.type) {
      return false;
    }
    return true;
  }

  function typeOptions() {
    const seen = new Set(lineups.map((l) => l.type));
    return [...seen];
  }

  function renderRow(l) {
    const dist = target ? distance3(l.rest, [target.x, target.y, target.z]) : null;
    const row = el("li", {
      className: "lineup-row" + (l.id === selectedId ? " selected" : ""),
    });
    row.dataset.id = l.id;
    const selectArea = el("div", {
      className: "lineup-select",
      tabIndex: 0,
      role: "button",
      "aria-pressed": l.id === selectedId ? "true" : "false",
    });
    selectArea.addEventListener("click", () => toggleSelected(l.id, row, selectArea));
    // Only Enter/Space that started on `selectArea` itself toggle it - a key that bubbled up
    // from the copy button or the fallback input must not also trigger row selection.
    selectArea.addEventListener("keydown", (e) => {
      if (e.target !== selectArea) {
        return;
      }
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        toggleSelected(l.id, row, selectArea);
      }
    });
    selectArea.addEventListener("pointerenter", () => handlers.onHoverEnter?.(l.id));
    selectArea.addEventListener("pointerleave", () => handlers.onHoverLeave?.());

    const title = el(
      "div",
      { className: "lineup-title" },
      el("span", { textContent: l.how }),
      el("span", { className: "pill", textContent: TYPE_LABELS[l.type] ?? l.type }),
    );
    const stats = el(
      "div",
      { className: "lineup-stats" },
      el("span", { textContent: `${strings.panel.distance}: ${dist != null ? dist.toFixed(0) : "?"}` }),
      el("span", { textContent: `${strings.panel.stability}: ${(l.stability * 100).toFixed(0)}%` }),
      el("span", { textContent: `${strings.panel.humanError}: ${l.humanError.toFixed(1)}°` }),
      el("span", { textContent: `${strings.panel.bounces}: ${l.Bounces}` }),
      el("span", { textContent: `${strings.panel.flightTime}: ${l.flightTime.toFixed(2)} с` }),
    );
    const flags = el("div", { className: "lineup-flags" });
    if (l.pin === "corner") {
      flags.append(el("span", { className: "pill", textContent: strings.panel.pinCorner }));
    } else if (l.pin === "wall") {
      flags.append(el("span", { className: "pill", textContent: strings.panel.pinWall }));
    }
    flags.append(
      el("span", {
        className: "pill",
        textContent: l.exposed ? strings.panel.exposedYes : strings.panel.exposedNo,
      }),
    );

    const copyStatus = el("span", { className: "hint" });
    let copyStatusTimer = null;
    function setCopyStatus(text) {
      copyStatus.textContent = text;
      if (copyStatusTimer) {
        clearTimeout(copyStatusTimer);
      }
      copyStatusTimer = setTimeout(() => {
        copyStatus.textContent = "";
        copyStatusTimer = null;
      }, 2000);
    }
    const copyInput = el("input", { type: "text", className: "copy-fallback", readOnly: true, hidden: true });
    const copyBtn = el("button", { type: "button", textContent: strings.panel.copyButton });
    copyBtn.addEventListener("click", () => {
      if (navigator.clipboard?.writeText) {
        navigator.clipboard.writeText(l.console).then(
          () => {
            setCopyStatus(strings.panel.copied);
          },
          () => {
            setCopyStatus(fallbackCopy(copyInput, l.console) ? strings.panel.copied : strings.panel.copyFallback);
          },
        );
      } else {
        setCopyStatus(fallbackCopy(copyInput, l.console) ? strings.panel.copied : strings.panel.copyFallback);
      }
    });

    selectArea.append(title, stats, flags);
    row.append(
      selectArea,
      el("div", { className: "lineup-copy" }, copyBtn, copyStatus, copyInput),
    );
    return row;
  }

  // BLUE-16: toggling a row's selection must not re-render the whole list (that drops keyboard
  // focus) - flip the CSS/ARIA state on the two rows involved in place instead.
  function toggleSelected(id, row, selectArea) {
    const prevId = selectedId;
    selectedId = id === selectedId ? null : id;
    handlers.onSelect?.(selectedId);
    row.classList.toggle("selected", selectedId === id);
    selectArea.setAttribute("aria-pressed", selectedId === id ? "true" : "false");
    if (prevId != null && prevId !== id) {
      const prevRow = container.querySelector(`.lineup-row[data-id="${CSS.escape(prevId)}"]`);
      if (prevRow) {
        prevRow.classList.remove("selected");
        prevRow.querySelector(".lineup-select")?.setAttribute("aria-pressed", "false");
      }
    }
  }

  function renderFilters() {
    const stableCb = el("input", { type: "checkbox", id: "filter-stable", checked: filters.stableOnly });
    stableCb.addEventListener("change", () => {
      filters.stableOnly = stableCb.checked;
      render();
    });
    const hiddenCb = el("input", { type: "checkbox", id: "filter-hidden", checked: filters.hiddenOnly });
    hiddenCb.addEventListener("change", () => {
      filters.hiddenOnly = hiddenCb.checked;
      render();
    });
    const typeSelect = el("select", { id: "filter-type", "aria-label": strings.panel.typeFilterLabel });
    typeSelect.append(el("option", { value: "all", textContent: strings.panel.typeAll }));
    for (const t of typeOptions()) {
      typeSelect.append(el("option", { value: t, textContent: TYPE_LABELS[t] ?? t, selected: t === filters.type }));
    }
    typeSelect.value = filters.type;
    typeSelect.addEventListener("change", () => {
      filters.type = typeSelect.value;
      render();
    });

    return el(
      "div",
      { className: "panel-filters" },
      el("label", { htmlFor: "filter-stable" }, stableCb, ` ${strings.panel.stableOnly}`),
      el("label", { htmlFor: "filter-hidden" }, hiddenCb, ` ${strings.panel.hiddenOnly}`),
      typeSelect,
    );
  }

  function render() {
    container.replaceChildren();
    if (lineups.length === 0) {
      return;
    }
    const visible = lineups.filter(passesFilters);
    container.append(
      el("h2", { textContent: strings.panel.heading }),
      el("p", { className: "hint", textContent: strings.panel.count(visible.length) }),
      renderFilters(),
    );
    const list = el("ul", { className: "lineup-list" });
    for (const l of visible) {
      list.append(renderRow(l));
    }
    container.append(list);
  }

  return {
    setResult(list, targetPoint) {
      lineups = list;
      target = targetPoint;
      selectedId = null;
      filters.type = "all";
      render();
    },
    clear() {
      lineups = [];
      target = null;
      selectedId = null;
      container.replaceChildren();
    },
    setSelected(id) {
      selectedId = id;
      render();
    },
  };
}
