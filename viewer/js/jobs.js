// Runs background jobs (`extract`, `standspots`, `viewerdata`) and reports their progress.
// Screen 2 uses this both for a single "extract this new map" job and for the chained
// "Подготовить" sequence that runs whichever of the three is still missing.

import { postJob, streamJob, deleteJob } from "./api.js?v=1";
import { strings } from "./strings.js?v=1";

const STAGE_LABELS = {
  extract: strings.maps.stageExtract,
  standspots: strings.maps.stageStandspots,
  viewerdata: strings.maps.stageViewerdata,
};

export function stageLabel(kind) {
  return STAGE_LABELS[kind] ?? kind;
}

// Runs one job kind for `map` to completion. `handlers`:
// - onJobId(jobId): the job id, as soon as it's known (so a cancel button can act on it).
// - onLine(msg): every NDJSON line (`{stage,done,total}` progress lines included).
// Resolves to `{ done: true }`, `{ error: message }`, `{ cancelled: true }`, or
// `{ broken: true, jobId }` if the stream dropped before a terminal line arrived.
export async function runOneJob(kind, map, handlers = {}) {
  const { data, error } = await postJob(kind, map);
  if (error !== undefined) {
    return { error: error ?? strings.errors.serverDown };
  }
  const jobId = data.job;
  handlers.onJobId?.(jobId);
  let terminal;
  try {
    terminal = await streamJob(jobId, (msg) => handlers.onLine?.(msg));
  } catch {
    return { broken: true, jobId };
  }
  if (terminal.error) {
    return { error: terminal.error };
  }
  if (terminal.status === "cancelled") {
    return { cancelled: true };
  }
  return { done: true, result: terminal.result };
}

// Re-attaches to an already-running (or already-finished) job, e.g. after `runOneJob` reported
// `broken`. Same resolved shape as `runOneJob`, minus `onJobId` (the caller already has it).
export async function reconnectJob(jobId, handlers = {}) {
  let terminal;
  try {
    terminal = await streamJob(jobId, (msg) => handlers.onLine?.(msg));
  } catch {
    return { broken: true, jobId };
  }
  if (terminal.error) {
    return { error: terminal.error };
  }
  if (terminal.status === "cancelled") {
    return { cancelled: true };
  }
  return { done: true, result: terminal.result };
}

// Chains `extract` -> `standspots` -> `viewerdata` for `map`, stopping at the first failure,
// cancellation, or broken stream. `handlers` gets `onStage(kind)` before each job starts, plus
// everything `runOneJob` takes. Returns a controller: `promise` resolves to the last job's
// outcome (or `{ cancelled: true }` if `cancel()` was called first); `cancel()` requests the
// currently running job be cancelled and stops the chain from starting the next one.
export function startPrepare(map, handlers = {}, kinds = ["extract", "standspots", "viewerdata"]) {
  let cancelled = false;
  let currentJobId = null;

  const promise = (async () => {
    for (const kind of kinds) {
      if (cancelled) {
        return { cancelled: true };
      }
      handlers.onStage?.(kind);
      const outcome = await runOneJob(kind, map, {
        onJobId: (id) => {
          currentJobId = id;
          handlers.onJobId?.(kind, id);
          if (cancelled) {
            deleteJob(id);
          }
        },
        onLine: (msg) => handlers.onLine?.(kind, msg),
      });
      if (!outcome.done) {
        return outcome;
      }
    }
    return { done: true };
  })();

  return {
    promise,
    cancel: async () => {
      cancelled = true;
      if (currentJobId) {
        await deleteJob(currentJobId);
      }
      // Else: the POST hasn't resolved yet. `onJobId` below notices `cancelled` once the id
      // arrives and issues the `DELETE` itself, instead of the cancel request being lost.
    },
  };
}
