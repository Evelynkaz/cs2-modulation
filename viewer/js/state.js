// One state object for the whole viewer. Screens read it and call explicit render functions;
// nothing here reacts on its own (`s6f1_viewer_shell.md`: "никакой реактивности «по волшебству»").

export const state = {
  screen: "loading", // "loading" | "setup" | "maps" | "map"
  config: null, // last GET/PUT /api/config response
  maps: [], // last GET /api/maps response
  currentMap: null, // map name shown on screen 3
  theme: "light",
  // map name -> running-job record (see main.js's `runJobFlow`). Survives a map-list re-render
  // (a re-render only replaces the DOM, never this) so switching maps, saving settings, or a
  // reload (via `GET /api/jobs`) never orphans a job that's still going on the server.
  activeJobs: new Map(),
};

const THEME_KEY = "cs2-modulation-theme";
const LIGHTING_MODE_KEY = "cs2-modulation-lighting-mode";

// localStorage throws in some private-browsing modes; every touch of it is wrapped so a blocked
// store never crashes the page (`s6f1_viewer_shell.md`).
export function loadStoredTheme() {
  try {
    return localStorage.getItem(THEME_KEY);
  } catch {
    return null;
  }
}

export function storeTheme(theme) {
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    // Ignored - the toggle still works for the rest of this session.
  }
}

export function resolveInitialTheme() {
  const stored = loadStoredTheme();
  if (stored === "light" || stored === "dark") {
    return stored;
  }
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function applyTheme(theme) {
  state.theme = theme;
  document.documentElement.dataset.theme = theme;
}

// The "game"/"simple" lighting toggle (`s6f3b2_lighting_shader.md` §7) - same wrapped-localStorage
// pattern as the theme above, kept independent of it since a slow GPU may want simple lighting in
// either theme.
export function loadStoredLightingMode() {
  try {
    return localStorage.getItem(LIGHTING_MODE_KEY);
  } catch {
    return null;
  }
}

export function storeLightingMode(mode) {
  try {
    localStorage.setItem(LIGHTING_MODE_KEY, mode);
  } catch {
    // Ignored - the toggle still works for the rest of this session.
  }
}
