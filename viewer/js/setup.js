// Screen 1 - first run: pick the CS2 game directory (required) and, optionally, the cache
// directory, then hand off to the caller once `game_dir` is configured.

import { putConfig } from "./api.js?v=1";
import { strings } from "./strings.js?v=4";

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

// `root`: the container to render into. `onConfigured(config)` fires once `PUT /api/config`
// reports `configured: true`. `initialConfig`: the current `/api/config`, if this screen is
// reached from the map list's "Настройки" link rather than the first-run gate - prefills the
// fields and lets "Продолжить" work without re-checking the game dir.
export function renderSetup(root, onConfigured, initialConfig = null) {
  root.replaceChildren();
  const s = strings.setup;

  const heading = el("h1", { textContent: s.heading });
  const intro = el("p", { className: "hint", textContent: s.intro });

  // --- game dir ---
  const gameLabel = el("label", { htmlFor: "setup-game-dir", textContent: s.gameDirLabel });
  const gameHint = el("p", { className: "hint", textContent: s.gameDirHint });
  const gameInput = el("input", {
    id: "setup-game-dir",
    type: "text",
    placeholder: s.gameDirPlaceholder,
    autocomplete: "off",
    value: initialConfig?.gameDir ?? "",
  });
  const gameButton = el("button", { type: "button", textContent: s.checkButton });
  const gameStatus = el("p", { className: "status", role: "status" });
  let lastConfig = initialConfig?.configured ? initialConfig : null;

  gameButton.addEventListener("click", async () => {
    const value = gameInput.value.trim();
    gameButton.disabled = true;
    gameButton.textContent = s.checking;
    gameStatus.className = "status";
    gameStatus.textContent = "";
    const { data, error } = await putConfig({ gameDir: value });
    gameButton.disabled = false;
    gameButton.textContent = s.checkButton;
    if (error !== undefined) {
      gameStatus.className = "status status-error";
      gameStatus.textContent = error ?? strings.errors.serverDown;
      return;
    }
    const lines = [];
    if (data.gameBuild) {
      lines.push(s.gameBuildFound(data.gameBuild));
    }
    // `gameDirAdjusted` from the server is structurally always false here (`put_config` stores
    // the already-adjusted directory, then `config_response` re-validates that stored path) -
    // dead server field, should be fixed server-side later. Detect the adjustment ourselves by
    // comparing what was typed against what got saved.
    const normalize = (p) => p.trim().replace(/[\\/]+$/, "").toLowerCase();
    if (data.gameDirAdjusted || (data.gameDir && normalize(value) !== normalize(data.gameDir))) {
      lines.push(s.gameDirAdjusted(data.gameDir));
    }
    gameStatus.className = "status status-ok";
    gameStatus.textContent = lines.join(" ");
    if (data.configured) {
      lastConfig = data;
      continueButton.hidden = false;
      continueButton.focus();
    }
  });

  const gameField = el(
    "div",
    { className: "field" },
    gameLabel,
    gameHint,
    el("div", { className: "field-row" }, gameInput, gameButton),
    gameStatus,
  );

  // --- cache dir ---
  const cacheLabel = el("label", { htmlFor: "setup-cache-dir", textContent: s.cacheDirLabel });
  const cacheHint = el("p", { className: "hint", textContent: s.cacheDirHint });
  const cacheInput = el("input", {
    id: "setup-cache-dir",
    type: "text",
    placeholder: s.cacheDirPlaceholder,
    autocomplete: "off",
    value: initialConfig?.cacheDir ?? "",
  });
  const cacheButton = el("button", { type: "button", textContent: s.saveCacheButton });
  const cacheStatus = el("p", { className: "status", role: "status" });

  cacheButton.addEventListener("click", async () => {
    const value = cacheInput.value.trim();
    if (!value) {
      return;
    }
    cacheButton.disabled = true;
    const { data, error } = await putConfig({ cacheDir: value });
    cacheButton.disabled = false;
    if (error !== undefined) {
      cacheStatus.className = "status status-error";
      cacheStatus.textContent = error ?? strings.errors.serverDown;
      return;
    }
    cacheStatus.className = "status status-ok";
    cacheStatus.textContent = data.restartRequired ? s.restartRequired : s.cacheDirSaved;
  });

  const cacheField = el(
    "div",
    { className: "field" },
    cacheLabel,
    cacheHint,
    el("div", { className: "field-row" }, cacheInput, cacheButton),
    cacheStatus,
  );

  const continueButton = el("button", {
    type: "button",
    className: "primary",
    textContent: s.continueButton,
    hidden: !lastConfig,
  });
  continueButton.addEventListener("click", () => {
    if (lastConfig) {
      onConfigured(lastConfig);
    }
  });

  root.append(
    el(
      "div",
      { className: "page setup-page" },
      el("div", { className: "card" }, heading, intro, gameField, cacheField, continueButton),
    ),
  );
}
