# GH-113: proactive Claude Code usage signal (spike)

Issue: https://github.com/jowi-dev/tskmstr/issues/113

ADR-0008 learns that `claude` is exhausted by failing a run, because it
assumed no quota signal existed. That assumption is wrong. Claude Code
exposes the subscription's 5-hour and 7-day utilization in three places
that tm can read. This doc records what was verified, recommends which
signals to use, and proposes the runs.db snapshot schema that GH-114 and
the dispatcher (GH-115) build on.

## Ground truth

Verified 2026-10-18 on Claude Code 2.1.280 (Linux, Max subscription),
from the shipped CLI bundle and from live captures.

All three sources below are fed from the same in-process cache. That cache
is filled from the `anthropic-ratelimit-unified-*` response headers on
every API call the process makes, including `-5h-utilization`, `-5h-reset`,
`-7d-utilization`, `-7d-reset`, `-status` and the overage headers. So a
value is only as fresh as the last API call made by *that* process.

### 1. Statusline input: `rate_limits`

This answers question 1. The statusline command receives this on stdin,
captured from a live interactive session:

```json
"rate_limits": {
  "five_hour": { "used_percentage": 34, "resets_at": 1792365600 },
  "seven_day": { "used_percentage": 7,  "resets_at": 1792735200 }
}
```

- `used_percentage` is utilization × 100, rounded to 0.1. `resets_at` is
  in epoch seconds.
- The key is omitted entirely when no window is known: before the first
  API response, or under an API key, Bedrock or Vertex. On a gateway, a
  third `spend_limit` window is also present.
- The field exists in 2.1.148, the oldest bundle still in the local nix
  store, so the floor is ≤ 2.1.148. The exact first version wasn't pinned.
- **Headless `claude -p` never renders a statusline**, so this source only
  covers interactive sessions.

### 2. `--output-format stream-json`: `rate_limit_event`

This answers question 3, and it is the important find. Headless runs emit
this line, captured from `claude -p --output-format stream-json --verbose`:

```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed",
 "resetsAt":1792365600,"rateLimitType":"five_hour","overageStatus":"rejected",
 "overageDisabledReason":"out_of_credits","isUsingOverage":false,
 "unifiedWindows":{"five_hour":{"utilization":0.31,"resetsAt":1792365600},
                   "seven_day":{"utilization":0.07,"resetsAt":1792735200}}},
 "uuid":"…","session_id":"…"}
```

- The CLI's own schema describes it as "emitted when rate limit info
  changes". It appeared in a one-turn run, so every run sees at least one.
- `status` (`allowed` / `allowed_warning` / `rejected`) is the server's
  own verdict. `rejected` is the proactive version of what ADR-0008 scrapes
  from result text.
- `unifiedWindows` is marked `@internal` in the CLI schema. It is absent
  from the 2.1.161 bundle and present in 2.1.280. The top-level
  `status`/`resetsAt`/`rateLimitType`/`utilization` describe only the
  "representative" window, so treat `unifiedWindows` as optional and fall
  back to the top-level fields.
- `utilization` is a fraction: usually 0–1, but it can exceed 1 under
  overage. The statusline uses a percentage instead.
- **tm's current `--output-format json` contains none of this.** The single
  result object has `usage`/`modelUsage` and no rate-limit fields. Getting
  the signal from lane runs means switching the claude adapter to
  `stream-json --verbose`. The final `{"type":"result",…}` line has the
  same shape `parse_outcome` reads today, so the change is: read stdout as
  JSONL, take the last `result` line for the outcome, and take the last
  `rate_limit_event` for the snapshot.

### 3. `/api/oauth/usage` (what `/usage` reads)

The `/usage` panel fetches `GET /api/oauth/usage`. It returns
`five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`,
`extra_usage` and a `limits[]` array. Calling it means reading Claude
Code's OAuth token out of its credentials store and hitting an internal,
undocumented endpoint that has unannounced query flags (`at_wall`,
`cedar_ember`). **Do not depend on it.** Sources 1 and 2 give the same
two numbers through supported output surfaces.

## Staleness (question 2)

All sources have the same shape: a value observed at `observed_at`, valid
until `resets_at`.

- **While usage happens, the value only rises.** Any snapshot is a lower
  bound on the current value, because usage from claude.ai web, desktop,
  mobile or another host lands between observations.
- **Once `now ≥ resets_at`, the window has rolled over.** The real value
  is then ≥ 0 and almost certainly small, so a stale-but-expired snapshot
  is still informative.
- **If you aren't working interactively, a statusline-only design goes
  stale.** Nothing refreshes it until you next type. Unattended dispatch
  would act on a number from hours ago, which is exactly the case
  ADR-0008 can't handle.

That makes the statusline shim insufficient on its own, and also not
necessary. Lane runs already refresh the snapshot at least once per run
via source 2. When the snapshot is too old to trust, a **probe** fixes it
for almost nothing:

```
claude -p 'ok' --model haiku --max-turns 1 \
  --output-format stream-json --verbose
```

The probe costs one Haiku turn. The measurement was 22k cache-read tokens
and about $0.003 list price, run from a directory with no project context,
and it consumes a negligible slice of the window. It must use the same
billing-safety env stripping as lane runs (no `ANTHROPIC_API_KEY`).

## Token-budget fallback (question 4)

A rolling budget summed from runs.db `model_usage` was not measured
against real utilization, because one afternoon of snapshots isn't enough
data. It is structurally wrong in two ways:

- **It can't see usage outside tm.** That includes claude.ai, mobile,
  other machines, and Claude Code sessions not registered with tm. In this
  spike the 5-hour window moved 31% → 34% between two observations while
  tm recorded only two tiny Haiku calls. The difference was this spike's own
  Opus session, and nothing in `model_usage` would have
  explained it.
- **Utilization is not linear in raw tokens.** It is weighted by model and
  by token kind (cache reads are cheap). A raw token budget needs
  per-model weights tm has no source for.

Recommendation: GH-114 should become **snapshot-driven gating**, with the
token budget kept only as the gate for runners that have no
signal: opencode, or `claude` under an API key, where `rate_limits` is
absent. If a budget is kept for claude, calibrate it from snapshot deltas
across runs (Δutilization ÷ Δweighted tokens) rather than from a configured
constant.

## Recommendation

1. **Primary: lane runs record `rate_limit_event`.** Switch the claude
   adapter's headless output to `stream-json --verbose`, keep
   `parse_outcome` on the final `result` line, and upsert the last
   `rate_limit_event` into `usage_snapshots` at finish. This is free, and
   it refreshes the snapshot on every unattended run.
2. **Before an unattended launch: probe if stale.** If the newest
   snapshot for a window is older than a max age (start with 10 min) and
   its `resets_at` is still in the future, run the probe above and
   upsert. If the probe itself fails, fall back to ADR-0008's reactive
   path.
3. **Optional: statusline shim.** A `tm usage statusline` subcommand
   reads stdin, upserts `rate_limits` (source `statusline`), and then
   execs the user's real statusline command so it doesn't replace it.
   This keeps the snapshot fresh while you work interactively, and so
   reduces probes. It is opt-in and not needed for correctness.
4. **Keep ADR-0008's reactive classification** as the backstop. Signals
   can disappear: the `@internal` field, API-key sessions, a CLI change.

### Snapshot schema (proposed runs.db migration)

```sql
CREATE TABLE usage_snapshots (
    agent       TEXT NOT NULL,     -- 'claude'
    window      TEXT NOT NULL,     -- 'five_hour' | 'seven_day' | 'seven_day_overage_included'
    utilization REAL NOT NULL,     -- fraction; may exceed 1.0 (statusline % ÷ 100)
    resets_at   INTEGER NOT NULL,  -- epoch seconds
    observed_at INTEGER NOT NULL,  -- epoch seconds, when tm read it
    source      TEXT NOT NULL,     -- 'stream' | 'probe' | 'statusline'
    status      TEXT,              -- allowed | allowed_warning | rejected (stream/probe only)
    PRIMARY KEY (agent, window)
);
```

- There is one row per window, and the latest observation wins. Upsert
  only when `observed_at` is newer than the stored row, because concurrent
  runs finish out of order. History isn't needed for gating. If calibration
  ever wants it, `run_events` can carry each snapshot as an event.
- The table is keyed by agent rather than by account. runs.db is
  per-host, and one host has one Claude login.
- Reading rule for the gate: `effective = now >= resets_at ? 0 :
  utilization`, plus `age = now - observed_at`. The thresholds (for
  example, hold unattended launches at five_hour ≥ 0.8 to keep headroom
  for interactive work) belong to GH-114/GH-115 config, not here.
- A `rejected` status from stream/probe should also write
  `agent_windows.exhausted_until = resets_at`. That way the existing
  ADR-0008 routing honours it without a second code path.

### Proof-of-concept

The two captures above are the proof-of-concept snapshots. To reproduce:

```sh
claude -p 'ok' --model haiku --max-turns 1 --output-format stream-json --verbose \
  | jq -c 'select(.type == "rate_limit_event") | .rate_limit_info.unifiedWindows'
# {"five_hour":{"utilization":0.31,"resetsAt":1792365600},"seven_day":{"utilization":0.07,"resetsAt":1792735200}}
```

Statusline: point `statusLine.command` at a script that runs `cat > file`,
then `jq .rate_limits file`.

## Follow-ups

- GH-114: rescope to snapshot-driven gating as above, with the token
  budget only for signal-less runners.
- New ticket: switch the claude adapter to `stream-json` and record
  `rate_limit_event` (the migration plus the parse change).
- Optional ticket: the `tm usage statusline` shim.
