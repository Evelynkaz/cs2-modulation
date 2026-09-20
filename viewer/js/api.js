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
