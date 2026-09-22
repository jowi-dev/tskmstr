// tskmstr opencode telemetry plugin.
//
// The opencode analog of claude's hook scripts (hooks/tm-*.sh), restoring the
// three telemetry surfaces claude runs have but opencode runs lacked (GH-43,
// see docs/plans/gh-43-opencode-telemetry.md for the verified opencode 1.15.13
// plugin contract this binds to):
//
//   1. Session identity: the `shell.env` hook exports OPENCODE_SESSION_ID into
//      every tool subprocess (opencode itself exports OPENCODE_PID but no
//      session-id var). This is the var OpencodeRunner::session_env_vars names,
//      so an in-session `tm runs register` records the session id via
//      SessionEnv::from_process_env.
//   2. Per-model usage: the `event` hook accumulates per-model token/cost usage
//      from assistant `message.updated` events (the only place opencode names
//      the model), keyed by `provider/model`.
//   3. Interactive auto-finish: on process exit, a registered interactive
//      session's run row is finished as `done` with a --model-usage snapshot,
//      the way tm-session-end.sh does for claude — so an opencode interactive
//      run does not sit at `running` until the liveness reap.
//
// Gating mirrors the claude hooks' TSKMSTR_RUN_ID contract exactly:
//   - TSKMSTR_RUN_ID set  => headless lane run. A supervisor owns finish, so
//     this plugin only emits live `tm runs event --kind usage` snapshots (like
//     tm-usage.sh); the supervisor folds the latest one into runs.model_usage.
//   - TSKMSTR_SESSION_RUN_ID set (and TSKMSTR_RUN_ID unset) => registered
//     interactive session. This plugin finishes the run on exit (like
//     tm-session-end.sh's inverted lane-run gate).
//   - neither set => a session tm did not launch. No-op beyond the harmless
//     OPENCODE_SESSION_ID export.
//
// A telemetry plugin must never disturb the session: every tm invocation is a
// detached, output-suppressed, error-swallowed spawn, and nothing here throws.

import { spawn, spawnSync } from "node:child_process";

const SESSION_ID_ENV = "OPENCODE_SESSION_ID";

// Per-message-id latest-wins usage, so re-emitted `message.updated` events for
// the same message overwrite rather than double-count. Each entry records the
// model key plus that message's cumulative counters; the finished/emitted map
// sums across message ids per model.
const perMessage = new Map();

function usageKey(model) {
  if (!model) return null;
  const provider = model.providerID || "";
  const id = model.id || "";
  if (!id) return null;
  return provider ? `${provider}/${id}` : id;
}

// Record one assistant message's cumulative usage. opencode reports each
// assistant message's tokens/cost as running totals for that message, so latest
// wins per message id.
function recordMessage(info) {
  if (!info || info.role !== "assistant") return;
  const key = usageKey(info.model);
  if (!key) return;
  const tokens = info.tokens || {};
  const cache = tokens.cache || {};
  perMessage.set(info.id, {
    key,
    inputTokens: tokens.input || 0,
    outputTokens: tokens.output || 0,
    cacheReadInputTokens: cache.read || 0,
    cacheCreationInputTokens: cache.write || 0,
    costUSD: typeof info.cost === "number" ? info.cost : 0,
  });
}

// Aggregate the per-message records into a bare per-model map of the shape
// `tm runs finish --model-usage` accepts: {"<provider/model>": {inputTokens,
// outputTokens, cacheReadInputTokens, cacheCreationInputTokens, costUSD}}.
function aggregate() {
  const out = {};
  for (const m of perMessage.values()) {
    const acc = (out[m.key] = out[m.key] || {
      inputTokens: 0,
      outputTokens: 0,
      cacheReadInputTokens: 0,
      cacheCreationInputTokens: 0,
      costUSD: 0,
    });
    acc.inputTokens += m.inputTokens;
    acc.outputTokens += m.outputTokens;
    acc.cacheReadInputTokens += m.cacheReadInputTokens;
    acc.cacheCreationInputTokens += m.cacheCreationInputTokens;
    acc.costUSD += m.costUSD;
  }
  return out;
}

function isEmpty(obj) {
  for (const _ in obj) return false;
  return true;
}

// Fire-and-forget `tm` for live lane-run usage events; never blocks the
// session, never surfaces output or errors.
function tmDetached(args) {
  try {
    const child = spawn("tm", args, {
      stdio: "ignore",
      detached: true,
    });
    child.on("error", () => {});
    child.unref();
  } catch {
    // swallow — telemetry must never disturb the session
  }
}

export default async () => {
  const laneRunId = process.env.TSKMSTR_RUN_ID;
  const sessionRunId = process.env.TSKMSTR_SESSION_RUN_ID;

  // Interactive auto-finish gate: mirror tm-session-end.sh's inverted lane
  // gate — a registered interactive session (TSKMSTR_SESSION_RUN_ID) with no
  // supervising lane wrapper (TSKMSTR_RUN_ID) is the one this plugin finishes.
  const finishRunId = !laneRunId && sessionRunId ? sessionRunId : null;

  let finished = false;
  function finishOnce() {
    if (finished || !finishRunId) return;
    finished = true;
    const map = aggregate();
    const args = ["runs", "finish", finishRunId, "--status", "done"];
    if (!isEmpty(map)) {
      args.push("--model-usage", JSON.stringify(map));
    }
    // Synchronous: an exit handler's event loop will not drain an async spawn.
    try {
      spawnSync("tm", args, { stdio: "ignore" });
    } catch {
      // swallow
    }
  }

  if (finishRunId) {
    process.once("beforeExit", finishOnce);
    process.once("exit", finishOnce);
    for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
      process.once(sig, () => {
        finishOnce();
        // Re-raise the default disposition so opencode still shuts down.
        process.kill(process.pid, sig);
      });
    }
  }

  return {
    // Export the live session id into every tool subprocess so an in-session
    // `tm runs register` can adopt it. The bash/shell tool path supplies
    // `sessionID` (the terminal path does not); guard accordingly.
    "shell.env": async (input, output) => {
      if (input && input.sessionID && output && output.env) {
        output.env[SESSION_ID_ENV] = input.sessionID;
      }
    },

    // Every bus event. Assistant `message.updated` events are the only place
    // opencode names the model, so per-model usage is accumulated here.
    event: async ({ event }) => {
      if (!event || event.type !== "message.updated") return;
      const info = event.properties && event.properties.info;
      recordMessage(info);

      // Live lane-run usage snapshot (like tm-usage.sh): full snapshot, latest
      // wins, wrapped in {"models": ...}. Only for headless lane runs.
      if (laneRunId) {
        const map = aggregate();
        if (!isEmpty(map)) {
          tmDetached([
            "runs",
            "event",
            laneRunId,
            "--kind",
            "usage",
            "--detail",
            JSON.stringify({ models: map }),
          ]);
        }
      }
    },
  };
};
