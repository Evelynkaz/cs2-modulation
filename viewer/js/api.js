// Fetch wrappers. No DOM access here; callers own status text and overlays.
//
// Every function resolves to `{ data }` or `{ error }` - `error` is the server's own message
// (`{"error": "..."}`, shown verbatim per `s6f1_viewer_shell.md`) or, when the server itself
// doesn't answer, `null` so the caller can tell "server down" apart from "server said no".

async function readError(res) {
  try {
    const body = await res.json();
    return typeof body.error === "string" ? body.error : `HTTP ${res.status}`;
  } catch {
    return `HTTP ${res.status}`;
  }
}

async function getJson(path) {
  let res;
  try {
    res = await fetch(path, { cache: "no-cache" });
  } catch {
    return { error: null };
  }
  if (!res.ok) {
    return { error: await readError(res) };
  }
  return { data: await res.json() };
}

export async function fetchConfig() {
  return getJson("/api/config");
}

export async function putConfig(patch) {
  let res;
  try {
    res = await fetch("/api/config", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    });
  } catch {
    return { error: null };
  }
  if (!res.ok) {
    return { error: await readError(res) };
  }
  return { data: await res.json() };
}

export async function fetchMaps() {
  return getJson("/api/maps");
}

export async function fetchRadar(map) {
  return getJson(`/api/radar?map=${encodeURIComponent(map)}`);
}

export function radarPngUrl(map) {
  return `/data/maps/${encodeURIComponent(map)}/viewer-map.png`;
}

export async function fetchLevels(map, x, y) {
  return getJson(`/api/levels?map=${encodeURIComponent(map)}&x=${x}&y=${y}`);
}

// `s6f3b_viewer3d.md` F3b-1a: `render*` files served under `/data/maps/<map>/`, same whitelist
// and ETag/304 machinery as `viewer-map.png`.
export function renderGlbUrl(map) {
  return `/data/maps/${encodeURIComponent(map)}/render.glb`;
}

// `s6f3a6_native_tex.md` review fix item 11: `render.json`'s own `formatVersion` this viewer
// requires - materials before it reference glTF's own (now-unpopulated) `baseColorTexture`/
// `normalTexture` fields instead of `render.json`'s `textures[]`, so an older export would render
// completely untextured rather than erroring. `/api/maps`' own `renderVersion` (`null` when there's
// no render.glb at all, or render.json doesn't parse) is checked against this before ever treating
// a map as "has a usable 3D export".
export const MIN_RENDER_FORMAT_VERSION = 3;

/** True only when `mapSummary` (an `/api/maps` entry) has a `render.glb` *and* its `render.json`
 * is at least `MIN_RENDER_FORMAT_VERSION` - the single gate both `main.js` (the 2D/3D toggle) and
 * `scene3d.js` (whether to even fetch render.json/render.glb) use. */
export function hasUsableRender(mapSummary) {
  return !!mapSummary?.hasRender && (mapSummary.renderVersion ?? 0) >= MIN_RENDER_FORMAT_VERSION;
}

export async function fetchRenderJson(map) {
  return getJson(`/data/maps/${encodeURIComponent(map)}/render.json`);
}

// Any other whitelisted `render*` file (`render_lm_*.bin`, `render_sky_cube.bin`, `render_lut.bin`,
// the fallback PNGs, ...) - same route as `renderGlbUrl`, just a different file name
// (`s6f3b2_lighting_shader.md` §3/§4: lightmaps, sky cube, LUT).
export function renderAssetUrl(map, file) {
  return `/data/maps/${encodeURIComponent(map)}/${file}`;
}

export function meshUrl(map) {
  return `/api/mesh?map=${encodeURIComponent(map)}`;
}

// `GET /api/trajectory`: a single throw's flight ticks. `spec`: `{x,y,z,type,pitch,yaw,strength,
// runDeg,broken}` (feet + throw parameters, same names the query string uses).
export async function fetchTrajectory(map, spec) {
  const params = new URLSearchParams({
    map,
    x: spec.x,
    y: spec.y,
    z: spec.z,
    type: spec.type,
    pitch: spec.pitch,
    yaw: spec.yaw,
    strength: spec.strength,
  });
  if (spec.runDeg) {
    params.set("runDeg", spec.runDeg);
  }
  if (spec.broken && spec.broken.length > 0) {
    params.set("broken", spec.broken.join(","));
  }
  return getJson(`/api/trajectory?${params.toString()}`);
}

// `GET /api/smoke`: the resting cloud's voxel cell centers around `(x,y,z)`.
export async function fetchSmoke(map, x, y, z) {
  return getJson(`/api/smoke?map=${encodeURIComponent(map)}&x=${x}&y=${y}&z=${z}`);
}

// Runs `POST /api/lineup` and reads its NDJSON stream. `onLine(msg)` fires for every progress
// line (`phase`/`checked`/`verified`); the terminal `result`/`error` line is not passed to it -
// it becomes this function's own resolution instead. `signal` aborts the fetch and the read loop
// together (the server notices the dropped connection and stops the solve on its own).
export async function solveLineup(body, onLine, signal) {
  let res;
  try {
    res = await fetch("/api/lineup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal,
    });
  } catch (e) {
    if (e.name === "AbortError") {
      return { aborted: true };
    }
    return { error: null };
  }
  if (!res.ok) {
    return { error: await readError(res) };
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let result;
  let error;
  // Whether any non-terminal line was actually read - the caller uses this (not "no points were
  // drawn") to tell a genuine first solve that streamed nothing (e.g. a target inside solid
  // geometry) apart from a cache hit answered in a single line.
  let streamed = false;
  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      buf += decoder.decode(chunk.value, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        if (!line.trim()) {
          continue;
        }
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          continue;
        }
        if (msg.result !== undefined) {
          result = msg.result;
        } else if (msg.error !== undefined) {
          error = msg.error;
        } else {
          streamed = true;
          onLine(msg);
        }
      }
    }
  } catch (e) {
    if (e.name === "AbortError" || signal?.aborted) {
      return { aborted: true };
    }
    return { error: null };
  }
  if (error !== undefined) {
    return { error };
  }
  if (result !== undefined) {
    return { data: result, streamed };
  }
  return { error: null };
}

const JOB_KINDS = { extract: "extract", standspots: "standspots", viewerdata: "viewerdata" };

export async function postJob(kind, map) {
  let res;
  try {
    res = await fetch(`/api/jobs/${JOB_KINDS[kind]}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ map }),
    });
  } catch {
    return { error: null };
  }
  if (!res.ok) {
    return { error: await readError(res) };
  }
  return { data: await res.json() };
}

export async function fetchJobs() {
  return getJson("/api/jobs");
}

export async function deleteJob(id) {
  try {
    await fetch(`/api/jobs/${encodeURIComponent(id)}`, { method: "DELETE" });
  } catch {
    // The caller has already given up on this job either way.
  }
}

// Attaches to `GET /api/jobs/{id}`'s NDJSON stream (or re-attaches, replaying lines already
// sent) and calls `onLine` for every parsed line. Resolves with the terminal line
// (`{result:...}`/`{error:...}`/`{status:...}`) once the stream ends normally; rejects with
// `{ broken: true }` if the connection drops before a terminal line arrived, so the caller can
// offer to reconnect.
export async function streamJob(id, onLine) {
  let res;
  try {
    res = await fetch(`/api/jobs/${encodeURIComponent(id)}`, { cache: "no-cache" });
  } catch {
    throw { broken: true };
  }
  if (!res.ok || !res.body) {
    throw { broken: true };
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let terminal = null;
  for (;;) {
    let chunk;
    try {
      chunk = await reader.read();
    } catch {
      await reader.cancel().catch(() => {});
      throw { broken: !terminal };
    }
    if (chunk.done) {
      break;
    }
    buf += decoder.decode(chunk.value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (!line.trim()) {
        continue;
      }
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.result || msg.error || msg.status) {
        terminal = msg;
      }
      onLine(msg);
    }
  }
  if (!terminal) {
    await reader.cancel().catch(() => {});
    throw { broken: true };
  }
  return terminal;
}
