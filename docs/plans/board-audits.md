# Plan: Board-launched ticket-audit sessions

Roadmap stream 4. From the `tm board` TUI: select a ticket, press one key,
and a ticket-audit Claude session starts in a detached tmux session. The
board shows a per-ticket indicator (starting / running / waiting for input
/ done) and the same key attaches to the session when one is live. Several
audits can run concurrently.

## Ground truth

Verified 2026-08-07 against the current tree.

- **`tm ticket audit` never spawns Claude.** It runs *inside* an existing
  interactive session (an axiom skill calls it) and retroactively claims
  the session via a marker file (`register_session`,
  `src/runs/session.rs`). Nothing in tm launches an audit today.
- **The tmux layer has lifecycle ops but no command-carrying session
  creation.** `TmuxOps::new_session(name, dir, primary_window)` starts a
  default shell; launching `claude "<prompt>"` as the pane command needs a
  new op. `attach` exists and is a blocking stdio handoff
  (`src/work/tmux.rs:296`), only ever called from plain CLI commands.
- **No "waiting for input" signal exists anywhere.** Heartbeats bump on
  *any* event (`add_event`, `src/runs/mod.rs:1052`), so an interactive
  session idling at a question looks identical to a hung one. `Stop`
  fires when Claude finishes a turn (which, interactively, means "now
  waiting for the user"), `Notification` fires on permission prompts and
  idle nags, and `UserPromptSubmit` fires when the user replies — none of
  the three are wired for state (only `Stop` → `tm-usage.sh` for tokens).
- **Status vocabulary stays small (ADR-0001).** `RunStatus` gains no
  variant. "Waiting" is a *derived, display-only* state: a running run
  whose most recent event says Claude stopped or asked. The listing SQL
  already computes `last_event_kind` per run (`list_runs_filtered`).
- **The board and the runs store are fully partitioned today.** `TuiDeps`
  has no `RunStore` or `TmuxOps`; the board's event loop never emits
  `Msg::Tick` (`src/tui/event.rs:93-111`); `Cmd::LoadRuns` is
  `debug_assert!`-unreachable from the board's `execute`. Stream 4 opens
  this seam deliberately.
- **Attaching from inside ratatui needs a new primitive.** `TerminalGuard`
  restores raw mode / alt screen once, on drop. Handing the terminal to
  `tmux attach` mid-session means: leave alt screen + disable raw mode,
  run attach with inherited stdio, re-enable both, `terminal.clear()`,
  redraw. No precedent in the codebase; gets a manual test checklist like
  `detach.rs`.
- **Pre-registration interacts with reap.** A run row created at launch
  has no pid until the in-session `tm ticket audit` runs (~seconds later,
  since the audit prompt is the session's first turn). Reap staleness is
  minutes, so the gap is safe; after adoption the run carries
  `pid = CLAUDE_PID` and survives idling.
- **`TSKMSTR_RUN_ID` must stay lane-only.** It gates `guard-delegate.sh`
  (denies interactive edits) and inverts `tm-session-end.sh`. The launch
  env var must be a *different* name that no existing gate reads.
- **The audit conversation is personal config.** The `/ticket-audit`
  skill lives in the axiom repo; per ADR-0002's seam, tm owns launching
  and supervising, while the working directory and prompt content are
  user config.

## Design

### Launch: detached tmux session running interactive `claude`

Ticket audits are interactive conversations (unlike headless lane runs),
so they are tmux-hosted: started detached, attached on demand. New config:

```toml
[work.audit]
dir = "~/Projects/axiom"            # required to enable launching
prompt = "/ticket-audit {key}"      # optional, this is the default
```

`dir` is where the session runs (the repo whose `.claude/` provides the
audit skill and hook settings). `{key}` in `prompt` is replaced with the
ticket key. Launching without `[work.audit].dir` is a status-line error,
not a crash.

New module `src/work/audit.rs`:

- `audit_session_name(key) -> String` = `tm-audit-<lowercased key>`.
  Deterministic, so the board can map tmux sessions back to tickets and
  attach by name alone.
- `launch_audit(store, tmux, cfg, key)`:
  1. Refuse if `tmux.has_session(name)` (one live audit per ticket).
  2. `store.start_run` with `kind = "audit"`, `lane = "audit"`, the
     ticket key, `pid = None` → run id.
  3. `tmux.new_session_with_command(name, dir, window, command)` where
     the command runs `claude "<prompt>"` and the environment carries
     `TSKMSTR_SESSION_RUN_ID=<run id>` (tmux `-e` flag; requires tmux ≥
     3.2, which is fine — nix pins tmux).

New `TmuxOps` op: `new_session_with_command(name, dir, window_name,
env: &[(String, String)], command: &str)`, faked like the rest.

### Adoption: the in-session `tm ticket audit` claims the launched run

`SessionEnv` gains `session_run_id: Option<i64>` read from
`TSKMSTR_SESSION_RUN_ID`. In `register_session`, before the existing
marker/new-run paths: if `session_run_id` points at a `Running` run whose
`kind` and `ticket` match, adopt it — write the marker file with that run
id, `update_session_id`, `update_pid(CLAUDE_PID)`. Mismatch or missing
run falls through to the existing behavior (the env var is advisory, the
marker stays the source of truth). This closes the launch→registration
gap without a second registration path: the rest of the telemetry
pipeline (marker-fallback hooks, `--record` finish, SessionEnd finisher)
is untouched.

If the session dies before adopting (claude fails to boot), the
pre-registered pid-NULL run goes stale and reap marks it `failed` — the
failure is visible instead of silent.

### Waiting-for-input: `await`/`resume` events, derived display state

New hook script `hooks/tm-session-state.sh`, wired to three events in
`settings_json()` (`Stop`, `Notification`, `UserPromptSubmit`). It is
session-marker-gated like `tm-session-end.sh`: exits immediately when
`TSKMSTR_RUN_ID` is set (lane runs are headless; awaiting input is
meaningless there), otherwise resolves the run id via the marker file.
Dispatch on `hook_event_name` from stdin:

- `Stop` → `tm runs event <id> --kind await` (turn ended; interactively
  that means "waiting for the user").
- `Notification` → `--kind await --detail {"message": ...}` (permission
  prompt / idle nag).
- `UserPromptSubmit` → `--kind resume` (user replied; Claude is working).

Derivation is pure and read-side: a `Running` run whose
`last_event_kind` is `await` is *awaiting input*. Any later event —
`resume`, `tool`, `usage` — flips it back. No schema change, no new
`RunStatus`; `add_event`'s heartbeat bump keeps reap semantics intact.
`RunCard` gains `awaiting_input: bool`; the watch screen renders it too
(audit/create cards), since the derivation is shared.

### Board integration

`TuiDeps` gains `store: Option<RunStore>` and `tmux: Box<dyn TmuxOps>`
(lenient: a broken runs DB never blocks the Jira board — matching
`AuditStoreStatus`'s stance). The board loop starts emitting `Msg::Tick`
on poll timeouts like the watch loop; the board tick polls audit status
every ~2s (every 8th 250ms tick), leaving Jira refresh manual.

New plumbing: `Cmd::LoadAuditStatus` → reads
`list_runs_filtered(Some("audit"))` + `tmux.list_sessions()` → `Msg::
AuditStatusLoaded(HashMap<String, AuditIndicator>)` keyed by ticket key.

```rust
enum AuditIndicator { Starting, Running, Waiting, Done, Failed }
```

Precedence per ticket (session = live `tm-audit-<key>` tmux session,
run = latest audit-kind run for the ticket):

- run Running + last event `await` → `Waiting`
- run Running → `Running`
- session exists, no live run yet → `Starting`
- run Done/Failed *and* session still exists → `Done`/`Failed`
  (attachable aftermath; once the session is gone the badge disappears —
  history lives in `tm runs` and the audit verdict, not the board)

Card rendering: a badge line/suffix on the card, styled via new theme
entries (see color pass): `Waiting` is the loud one (bold yellow),
`Running` cyan, `Starting` dim, `Done` green, `Failed` red.

Keymap: `a` on the board is the single audit action for the selected
ticket — if a `tm-audit-<key>` session exists, attach; otherwise launch
(status line: `launched audit for KEY — press a to attach`). Launch and
attach are `Cmd`s executed board-side only.

### Attach: suspend/restore the terminal

`Cmd::AttachAudit { session }` is handled specially in the board's event
loop (like quit, not like ordinary commands): leave alternate screen,
disable raw mode, `tmux.attach(name)` (blocking, inherited stdio), then
re-enable raw mode, re-enter the alternate screen, `terminal.clear()`,
and resume the loop with a status-line note. Detaching tmux (`C-b d`)
lands the user back on the board. Mechanics get a manual test checklist
in the module docs (same stance as `detach.rs`).

### Board color pass (rides along)

The recent accent pass covered only the watch screen; the board is bare.
Same doctrine — fg accents and modifiers only, never backgrounds,
selection stays `Modifier::REVERSED` (cell-level test contract):

- `theme::ticket_status_style(status_category)` — Jira category is the
  stable key (`new` → blue, `indeterminate` → cyan, `done` → green);
  board column titles get it (bold + color), mirroring the watch board.
- Card: key stays bold; `Assignee:` line dims (`theme::DIM`); audit
  badge styled per `AuditIndicator`.
- Ticket detail overlay: bold-cyan `SECTION_HEADER` labels, status text
  colored by category, URL dimmed.
- Rank screen: hint/summary text dimmed where the watch screen dims.

## Steps

Each step is TDD'd and lands as one commit; tests + clippy green via
`nix develop -c cargo ...`.

1. **Board color pass.** `ticket_status_style` + theme tests; column
   titles, dim assignee/detail accents, `cell_at`-based render tests.
2. **Session-state telemetry.** `hooks/tm-session-state.sh`, wiring for
   `Stop`/`Notification`/`UserPromptSubmit` in `settings_json()`,
   `HOOK_SOURCES` parity tests (8 scripts), lane-gate polarity test.
3. **Derived waiting state.** `awaiting_input` on `RunSummary`/`RunCard`,
   watch-screen waiting marker, unit tests over event orderings.
4. **Launch machinery.** `TmuxOps::new_session_with_command` (+ fake),
   `src/work/audit.rs` (`audit_session_name`, `launch_audit`), config
   `[work.audit]` parsing, tests against fakes.
5. **Adoption.** `SessionEnv::session_run_id`, adopt path in
   `register_session` with kind/ticket guard, tests (adopt, mismatch,
   missing, absent-env no-op).
6. **Board wiring.** `TuiDeps` store/tmux, board tick + `LoadAuditStatus`
   poll, `AuditIndicator` precedence (pure fn + tests), card badges,
   keymap `a`, launch path, reducer/keymap/render tests.
7. **Attach.** Terminal suspend/restore around `tmux.attach` in the board
   loop, manual test checklist in docs.
8. **Docs.** README (board keys, config), ROADMAP stream 4 → done, this
   file gains a Status section.

## Out of scope

- Restoring audit tmux sessions after a reboot (`tm work restore`
  analog) — audits are short-lived; relaunching from the board is cheap.
- Board-launched *create* sessions — same machinery would generalize;
  do it when wanted.
- A `Notification`-based OS-level alert (sound/badge) when a session
  starts waiting — the board indicator is the alert for now.
- Emitting `await` telemetry for lane runs (headless; nothing to wait
  on).

## Status (2026-08-07)

All eight steps landed 2026-08-07 (commits `b95c8d4`..HEAD; 1104 tests,
clippy clean). Resolutions worth recording:

- Step 1 added a `Status: ` label to the ticket detail overlay (the bare
  status line gave the `SECTION_HEADER` style nothing to attach to).
- `launch_audit` takes `home: &Path` for tilde expansion, matching
  `expand_tilde`'s pure-caller convention; the tmux command string is the
  one place in the codebase needing shell quoting (tmux hands it to
  `$SHELL -c`), handled by a local `shell_quote`.
- The board reuses `watch_tick` for its poll counter and `Msg::Tick` now
  handles `Screen::Board` (audit status every 8th tick); Detail /
  TransitionMenu overlays pause the polling — acceptable, the badges
  freeze only while an overlay is open.
- `AuditStatusEntry` carries `has_session` separately from the indicator
  because attach-vs-launch keys off session existence alone.
- `Cmd::AttachAudit` is intercepted in `run_cmds` (which went generic
  over the ratatui backend for testability) rather than `execute`, since
  it needs `&mut Terminal`. Suspend is the exact reverse of setup
  (`LeaveAlternateScreen` then `disable_raw_mode`) — note this is the
  reverse of `TerminalGuard::drop`'s order; the symmetric order is
  deliberate since this pair must compose with re-entry.
- Hook E2E smoke-tested against a scratch `XDG_DATA_HOME`:
  `Stop`/`Notification`/`UserPromptSubmit` payloads piped through
  `tm-session-state.sh` produced exactly `await`, `await`+message
  detail, and `resume`; `TSKMSTR_RUN_ID`-gated and unknown-event calls
  emitted nothing.
- The attach suspend/restore has a manual test checklist on
  `attach_audit` in `src/tui/event.rs`; run it once on a real terminal
  before relying on attach day-to-day.

## Operational follow-up

The axiom repo's hook copies and `settings.json` must be re-synced after
step 2 (adds `tm-session-state.sh` and three hook wirings) — same
outstanding chore as streams 2/3. Board-launched sessions get their
hooks from the *audit dir's* own Claude settings, so the sync is what
makes waiting-state telemetry actually flow.
