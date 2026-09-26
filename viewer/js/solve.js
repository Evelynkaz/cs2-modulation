// Builds a `POST /api/lineup` body from the map screen's picked target/origin and its params
// panel, runs the streaming request, and parses a pasted in-game `setpos` line into feet
// coordinates. No DOM here - `main.js` owns the UI, this module owns the request.

import { solveLineup } from "./api.js?v=1";

const SERVER_DEFAULTS = { tolerance: 80, minStability: 0.4 };
const ALL_TYPES = ["Stand", "Crouch", "JumpThrow", "CrouchJumpThrow", "RunJumpThrow"];
const ALL_STRENGTHS = [0, 0.5, 1];

// An empty `types`/`strengths` selection is not "use the server's default" - it is impossible to
// distinguish from that once serialised (both cases omit the field, see `buildQuery` below), and
// the server then silently solves with every type/strength. Callers must block the run instead.
export function selectionError(params) {
  if (!params.types || params.types.length === 0) {
    return "types";
  }
  if (!params.strengths || params.strengths.length === 0) {
    return "strengths";
  }
  return null;
}

// `target`: `{x,y,z}` or `null` when `targetArea` is used instead (`s6g2_target_area.md`).
// `targetArea`: `{polygon:[[x,y],...], zMin, zMax}` or `null` - mutually exclusive with `target`
// (the UI never sets both). `origin`: `{x,y,reach}` (from a right-click) or `null`. `originArea`:
// `{polygon:[[x,y],...], zMin, zMax}` (from the origin-area tool, `s6g_origin_area.md`) or
// `null` - mutually exclusive with `origin` (the UI never sets both). `params`: the params
// panel's current selections. Fields left at the server's own default are omitted, per
// `s6f2_solve_ui.md` ("параметры, равные умолчанию, в запрос не класть").
export function buildQuery(map, target, targetArea, origin, originArea, params) {
  const body = { map };
  if (target) {
    body.target = [target.x, target.y, target.z];
  } else if (targetArea) {
    body.targetArea = targetArea.polygon;
    if (targetArea.zMin != null) {
      body.targetZMin = targetArea.zMin;
    }
    if (targetArea.zMax != null) {
      body.targetZMax = targetArea.zMax;
    }
  }
  if (origin) {
    body.origin = [origin.x, origin.y];
    if (origin.reach != null && origin.reach !== 300) {
      body.originReach = origin.reach;
    }
  } else if (originArea) {
    body.originArea = originArea.polygon;
    if (originArea.zMin != null) {
      body.zMin = originArea.zMin;
    }
    if (originArea.zMax != null) {
      body.zMax = originArea.zMax;
    }
  } else if (params.scope === "spawns") {
    body.scope = "spawns";
  }
  if (params.tolerance != null && params.tolerance !== SERVER_DEFAULTS.tolerance) {
    body.tolerance = params.tolerance;
  }
  if (params.minStability != null && params.minStability !== SERVER_DEFAULTS.minStability) {
    body.minStability = params.minStability;
  }
  if (params.fineScan) {
    body.fineScan = true;
  }
  if (params.types && params.types.length > 0 && params.types.length < ALL_TYPES.length) {
    body.types = params.types;
  }
  if (params.strengths && params.strengths.length > 0 && params.strengths.length < ALL_STRENGTHS.length) {
    body.strengths = params.strengths;
  }
  if (params.broken && params.broken.length > 0) {
    body.broken = params.broken;
  }
  if (params.originPin) {
    body.originPin = params.originPin;
  }
  return body;
}

// Starts the solve; resolves handlers as the stream is read. `handlers`:
// - onLine(msg, elapsedMs): every progress line (`phase`/`checked`/`verified`).
// - onResult(data, streamed): the final `result` envelope (also fires for a cache-hit
//   single-line answer); `streamed` is whether any non-terminal line preceded it.
// - onError(message): the server's own error text.
// - onCancelled(): the request was aborted via `controller.cancel()`.
// Returns `{ cancel() }`.
export function runSolve(body, handlers) {
  const controller = new AbortController();
  const startedAt = Date.now();
  (async () => {
    try {
      const { data, error, aborted, streamed } = await solveLineup(
        body,
        (msg) => handlers.onLine?.(msg, Date.now() - startedAt),
        controller.signal,
      );
      if (aborted) {
        handlers.onCancelled?.();
        return;
      }
      if (error !== undefined) {
        handlers.onError?.(error);
        return;
      }
      handlers.onResult?.(data, streamed);
    } catch (e) {
      handlers.onError?.(e?.message ?? null);
    }
  })();
  return {
    cancel: () => controller.abort(),
  };
}

// Parses a copy-pasted in-game console line (`setpos x y z; setang pitch yaw roll`) into feet
// coordinates. `setpos` gives the eye position; feet = eye z - 64.06 (the server does the same
// conversion CLI-side for `--getpos`-style input).
const SETPOS_RE = /setpos\s+(-?[\d.]+)\s+(-?[\d.]+)\s+(-?[\d.]+)/i;

export function parseSetpos(text) {
  const m = SETPOS_RE.exec(text);
  if (!m) {
    return null;
  }
  const x = parseFloat(m[1]);
  const y = parseFloat(m[2]);
  const eyeZ = parseFloat(m[3]);
  if (![x, y, eyeZ].every(Number.isFinite)) {
    return null;
  }
  return { x, y, z: eyeZ - 64.06 };
}
