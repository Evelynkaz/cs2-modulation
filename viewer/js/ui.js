// Small shared UI-building blocks for the S6m redesign (segmented control, toggle chips, toast,
// collapsible section) - `main.js` and `panel.js` both need the same look and keyboard behavior
// for these, so they live here once instead of drifting apart across the two modules.

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

// A single-select control backed by real radio inputs (keyboard/focus-ring behavior comes free) -
// replaces both radio groups and 2-4 option `<select>`s per the S6m design system. `options`:
// `[{value, label}]`. `variant`: "" (joined pill group) | "chips" (individual rounded chips, same
// single-select behavior - used for the results filters).
export function segmented({ name, options, value, onChange, ariaLabel, variant = "" }) {
  const wrap = el("div", {
    className: variant ? `segmented segmented-${variant}` : "segmented",
    role: "radiogroup",
    "aria-label": ariaLabel ?? "",
  });
  const inputs = [];
  for (const opt of options) {
    const id = `${name}-${opt.value}`;
    const input = el("input", {
      type: "radio",
      name,
      id,
      value: opt.value,
      checked: opt.value === value,
      className: "segmented-input",
    });
    const label = el("label", { htmlFor: id, className: "segmented-label", textContent: opt.label });
    input.addEventListener("change", () => onChange(opt.value));
    wrap.append(input, label);
    inputs.push(input);
  }
  wrap.setValue = (v) => {
    for (const input of inputs) {
      input.checked = input.value === v;
    }
  };
  return wrap;
}

// A multi-select toggle chip, backed by a checkbox (same reasoning as `segmented` above).
// `iconName`, when given, prepends `icons.js`'s SVG in front of the label text.
export function chip({ id, label, checked, onChange, disabled = false, title, iconName }) {
  const input = el("input", { type: "checkbox", id, checked, disabled, className: "chip-input" });
  const text = el("label", { htmlFor: id, className: "chip" });
  text.innerHTML = iconName ? icon(iconName, 14) : "";
  text.append(label);
  if (title) {
    text.title = title;
  }
  input.addEventListener("change", () => onChange(input.checked));
  const wrap = el("span", { className: "chip-wrap" }, input, text);
  wrap.input = input;
  return wrap;
}

// A collapsible section that remembers open/closed in `localStorage` (best-effort - a blocked
// store just falls back to `defaultOpen` every time, same wrapped-try/catch idiom as `state.js`).
export function collapsible({ summary, content, storageKey, defaultOpen = false }) {
  let open = defaultOpen;
  try {
    const stored = localStorage.getItem(storageKey);
    if (stored != null) {
      open = stored === "1";
    }
  } catch {
    // Ignored - falls back to `defaultOpen` for this session.
  }
  const chevron = el("span", { className: "collapsible-chevron", innerHTML: icon("chevron", 16) });
  const btn = el(
    "button",
    { type: "button", className: "collapsible-summary", "aria-expanded": String(open) },
    chevron,
    el("span", { textContent: summary }),
  );
  const body = el("div", { className: "collapsible-body" }, content);
  body.hidden = !open;
  btn.classList.toggle("open", open);
  btn.addEventListener("click", () => {
    open = !open;
    body.hidden = !open;
    btn.setAttribute("aria-expanded", String(open));
    btn.classList.toggle("open", open);
    try {
      localStorage.setItem(storageKey, open ? "1" : "0");
    } catch {
      // Ignored - the section still toggles for the rest of this session.
    }
  });
  return el("div", { className: "collapsible" }, btn, body);
}

let toastTimer = null;

// Shows the shared toast at the bottom of the viewport (`#toast` in index.html). Re-triggers the
// CSS fade-in on every call, even for the same text twice in a row.
export function showToast(text) {
  const node = document.getElementById("toast");
  if (!node) {
    return;
  }
  node.textContent = text;
  node.hidden = false;
  node.classList.remove("show");
  // Force a reflow so removing/re-adding `.show` restarts the CSS transition on a repeated toast.
  void node.offsetWidth;
  node.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    node.classList.remove("show");
    node.hidden = true;
  }, 1800);
}

// A small empty-state block (no results yet, or a filter hid everything) - `title` + `hint` in
// words, and an optional action button (`s6m_consumer_redesign.md`: "explains what to loosen").
export function emptyState({ title, hint, actionLabel, onAction, iconName }) {
  const children = [];
  if (iconName) {
    children.push(el("div", { className: "empty-state-icon", innerHTML: icon(iconName, 28) }));
  }
  children.push(
    el("p", { className: "empty-title", textContent: title }),
    el("p", { className: "hint", textContent: hint }),
  );
  if (actionLabel) {
    const btn = el("button", { type: "button", className: "btn btn-secondary", textContent: actionLabel });
    btn.addEventListener("click", onAction);
    children.push(btn);
  }
  return el("div", { className: "empty-state" }, ...children);
}

export { el };
