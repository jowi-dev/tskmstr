# Plan: Usage analytics for `tm ticket audit` / `tm ticket create` sessions

Roadmap stream 2. Ticket audit and ticket create are interactive
Claude-driven processes (axiom skills calling tm), but their token cost is
invisible: every telemetry hook exits immediately unless `TSKMSTR_RUN_ID`
is set, and only lane runs set it. Goal: the cost of grooming a ticket is
as measurable as the cost of working it, using the machinery that already
exists for lane runs.

## Ground truth

Verified 2026-08-07 on Claude Code 2.1.220 and the current tree.

- **Session identity exists in the environment.** Bash-tool subprocesses
  receive `CLAUDE_CODE_SESSION_ID` (the session UUID, matching the
  `session_id` hooks receive on stdin) and `CLAUDE_PID` (the Claude
  process pid). Both are *observed but undocumented* — the docs only
  guarantee `session_id`/`transcript_path` on hook stdin. Anything reading
  these env vars must degrade to a silent no-op when they are absent.
- **Hooks get `session_id` on stdin.** All hook events receive
  `session_id`, `transcript_path`, `cwd` in their stdin JSON (documented).
  A `SessionEnd` event exists ("fires when session terminates"; exact
  firing conditions — `/clear`, exit — are not documented). It is not
  wired in `settings_json()` today (`src/work/hooks.rs:145`).
- **The runs store needs no new event machinery.** `latest_usage`,
  `collect_agent_usage`, `tool_counts`, `latest_checklist` are pure
  event-kind matchers over `events_for_run` — a run that receives the
  same `usage`/`agent_usage`/`tool`/`checklist` events renders Model
  usage / Agent usage / tool counts / checklists in `tm runs show` and
  the watch detail with zero changes. Only the `runs` schema, the
  listing layers (`RunSummary`, `RunCard`), and registration are new.
- **Reap hazard.** `RunStore::reap` reaps a stale-heartbeat running run
  *on staleness alone* when `pid` is NULL, and skips it when the pid is
  alive (`src/runs/mod.rs:1084`). An interactive session idles for long
  stretches, so a session run registered without a pid would be reaped
  `failed` after 10 quiet minutes. Session runs must record
  `pid = CLAUDE_PID`.
- **`finish_run` tolerates repeat finishes.** All-`Option` fields write
  through `COALESCE`, status/ended_at are overwritten, no double-finish
  guard (`src/runs/mod.rs:925`). A `SessionEnd` finisher can safely stamp
  status and `model_usage` after `--record` already finished the run.
- **`tm runs show` resolves the latest run of any kind for a ticket**
  (`latest_run_for_ticket`, `started_at DESC LIMIT 1`) — an audit session
  can shadow a lane run in `show`, so the kind must be visible and
  filterable.
- **`guard-delegate.sh` shares the `TSKMSTR_RUN_ID` gate.** It denies
  main-loop edits during lane runs. It must NOT learn the session-marker
  fallback below, or it would start denying edits in registered
  interactive sessions.
- **Cost is tokens-only for sessions.** `costUSD` comes only from
  `claude -p`'s result JSON; interactive sessions have no result JSON and
  tskmstr has no rate table (per-agent-usage plan). Session runs carry
  token counts; `cost_usd` stays NULL. That matches how usage is actually
  budgeted here (subscription limits are token-denominated).

## Design

### Runs, not a parallel store

A session becomes a **run with a `kind` discriminator** — the roadmap's
"run kind" option. Migration 4:

```sql
ALTER TABLE runs ADD COLUMN kind TEXT NOT NULL DEFAULT 'lane';
```

Kind is free-form TEXT at the store layer (same stance as `lane` and
event `kind`); the current vocabulary is `lane`, `audit`, `create`.
`StartRun` gains `kind: String`; `tm runs start` gains an optional
`--kind` (default `lane`). Session runs set `lane` to the kind name,
`worktree` to the cwd, `session_id` and `pid` from the environment.

### Session registration: marker files

New module `src/runs/session.rs`. Registration is keyed by the session
UUID via a marker file:

```
${XDG_DATA_HOME:-~/.local/share}/tskmstr/sessions/<session_id>
```

containing the bare run id (tm's usual bare-output style; trivially
`cat`-able from shell hooks). Base dir resolution mirrors
`default_db_path` (honors `XDG_DATA_HOME`).

`register_session(store, kind, ticket, env)`:

- No-op (Ok) when `CLAUDE_CODE_SESSION_ID` is unset — plain terminal
  invocations register nothing — or when `TSKMSTR_RUN_ID` is set (a lane
  run already owns telemetry; the wrapper owns start/finish).
- If a marker exists and points at a *running* run with the same kind and
  ticket → reuse (idempotent re-audit of the same ticket in one session).
- If it points at a running run for a *different* ticket → finish that
  run `done` (the session moved on; its usage snapshots are already
  recorded) and start fresh.
- Otherwise start a run (`kind`, ticket, `lane = kind`, `worktree = cwd`,
  `pid = CLAUDE_PID`, `session_id`) and write the marker.
- Opportunistic hygiene: sweep sibling markers whose run is no longer
  running (bounded: one small dir, local SQLite lookups).

`finish_session(store, env, status)` finishes the marker's run and
unlinks the marker.

Registration and finishing are telemetry — every failure path degrades
silently; the ticket command's own output and exit code are never
affected.

### Wiring into the ticket commands

- `tm ticket audit KEY` (read mode): after the lenient `RunStore::open`
  succeeds (`run_ticket_audit`, `src/main.rs:409`), register kind
  `audit`. This runs at the *start* of the /ticket-audit skill, so tool
  and agent-usage events flow for the whole investigation.
- `tm ticket audit KEY --record ...`: after recording, finish the
  session run `done`. Recording a verdict is the natural end of an
  audit.
- `tm ticket create`: inside `ticket::create()` immediately after
  `create_ticket()` succeeds (the first point the new key exists),
  register kind `create` with the new key. The store is opened leniently
  in the dispatch arm and threaded in as `Option<&RunStore>`; a broken
  runs DB must never block ticket creation.
- Create-session caveat: registration happens late (at the create call),
  so `tool`/`agent_usage` events from earlier in the conversation are
  not captured. Model usage is still complete: the Stop-hook snapshot
  aggregates the *entire transcript* and fires at the end of the turn
  that ran the create. Documented limitation; a transcript back-scan for
  agent usage is a possible follow-up if the gap matters in practice.

### Hook gating: marker fallback

The telemetry hooks — `tm-event.sh`, `tm-usage.sh`, `tm-checklist.sh`,
`tm-tasklist.sh` — replace their first line gate with:

```sh
RUN_ID="${TSKMSTR_RUN_ID:-}"
if [ -z "$RUN_ID" ]; then
  SESSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tskmstr/sessions"
  [ -n "$(ls -A "$SESSIONS_DIR" 2>/dev/null)" ] || exit 0
  SESSION_ID=$(... jq -r .session_id from stdin ...)
  [ -f "$SESSIONS_DIR/$SESSION_ID" ] || exit 0
  RUN_ID=$(cat "$SESSIONS_DIR/$SESSION_ID")
fi
```

then use `$RUN_ID` where they used `$TSKMSTR_RUN_ID`. Cost profile:
unregistered interactive sessions with no active session anywhere pay
one directory listing (the dir is empty or absent almost always); while
an audit session is active elsewhere, other sessions additionally pay
one stdin read + jq per hook fire. Lane runs are entirely unaffected
(`TSKMSTR_RUN_ID` short-circuits first).

Explicitly excluded: `guard-delegate.sh` (would deny interactive edits)
and `graphify-nudge.sh` (unrelated to run telemetry).

### Session end: finishing abandoned runs

New hook `hooks/tm-session-end.sh` on `SessionEnd`:

- Exit 0 when `TSKMSTR_RUN_ID` is set (the lane wrapper owns finish).
- Exit 0 when no marker exists for the payload's `session_id`.
- Otherwise aggregate the transcript into a bare per-model map (same jq
  as `tm-usage.sh`, but shaped as `parse_model_usage` expects — no
  `{"models": ...}` wrapper) and run
  `tm runs finish "$RUN_ID" --status done --model-usage "$MAP"`, then
  unlink the marker.

This gives session runs an authoritative `model_usage` column like lane
runs get from `claude -p`, and closes runs the user never `--record`ed.
If the process dies without `SessionEnd`, reap handles it (pid dead →
`failed`), and the stale marker is swept at the next registration.
Repeat finish after `--record` is safe (ground truth above): status stays
`done` and `model_usage` lands via COALESCE.

`settings_json()` in `src/work/hooks.rs` gains the `SessionEnd` wiring
and the script joins `HOOK_SOURCES` (+ parity tests). Deployment beyond
`tm work` is operational: the axiom repo's checked-in hook copies and its
`settings.json` must be synced to pick this up for interactive sessions
(same byte-identical mirroring discipline as before).

### Surfaces

- `tm runs`: new `KIND` column; `--kind <k>` filter. Same filter on
  `tm runs show` to disambiguate the latest-run-shadowing case; `show`
  header and `--json` gain `kind`.
- `tm ticket audit KEY` (read): a `Last audit usage:` line after
  `Last audit:`, from the latest *finished* `audit`-kind run for the
  ticket — one compact line (`<model> <n>k out / ...`), reusing the
  existing compact usage formatting. Omitted when there is none;
  runs-DB failure degrades exactly like `Last audit:` does.
- `tm runs watch`: cards carry `kind` (`RunSummary` → `RunCard`); the
  lane line already shows it (`lane = kind` for sessions), and the
  detail window header names the kind. Audit sessions appearing on the
  watch kanban is deliberate — it is the seed of roadmap stream 4's
  board-visible audit sessions.

### TUI color/organization pass (rider)

The TUI is monochrome today (only the yellow selected-column border and
the red stale-`!` marker). While the watch surface is being touched,
introduce `src/tui/theme.rs` — named style constants, no ad-hoc inline
styles — and apply:

- Run-status → color mapping used by watch column titles and a card
  status accent: queued dim gray, running cyan, blocked red, review
  magenta, done green, failed red.
- Kind badge styling on run cards/detail (audit yellow, create blue,
  lane default).
- Board: column titles bold + colored count, ticket key bold, selected
  card keeps `REVERSED` (a cell-level test pins that contract).
- Detail windows: section headers bold, checklist `[x]` green / `[ ]`
  dim, event timestamps dim.
- Status bar: hints dimmed so the message stands out.

Buffer-substring tests are style-insensitive and survive; the one
cell-level styling test is extended, not weakened.

## Implementation steps (TDD, checkpoint commit each)

1. **Store**: migration 4 (`kind`), `StartRun.kind`, `kind` threaded
   through `Run`/`RunSummary`/list projection, `list_runs`/`show` kind
   filter helpers, `latest_finished_run_for_ticket_kind`.
2. **Session registration**: `src/runs/session.rs` — marker dir
   resolution, register/finish/sweep, env injected for tests.
3. **Hooks**: marker-fallback gating in the four telemetry scripts,
   new `tm-session-end.sh`, `settings_json` SessionEnd wiring, parity
   tests updated to seven scripts.
4. **CLI**: `tm runs start --kind`, `KIND` column, `--kind` filters,
   `show`/`--json` kind, audit/create registration wiring, audit
   `Last audit usage:` line.
5. **TUI**: theme module, kind on `RunCard` + detail, color pass.
6. **Docs**: README (session tracking section), ROADMAP stream 2 status.

## Acceptance

- A real `/ticket-audit` session in axiom (after hook sync) produces a
  run: `tm runs --kind audit` lists it, `tm runs show KEY` renders Model
  usage, Agent usage (the skill's Sonnet explore agents), and tool
  counts; `--record` or session exit finishes it.
- `tm ticket audit KEY` afterwards prints the `Last audit usage:` line.
- A plain interactive session in any repo with the synced hooks shows no
  new runs, and hook overhead stays imperceptible.
- Lane runs are byte-for-byte unaffected (`TSKMSTR_RUN_ID` path).
