# Roadmap

Prioritized workstreams as of 2026-08-06. Ordering within this file is
rough priority; dependencies are called out per stream.

## 1. Absorb the `j work` lane runner into tskmstr — done

The lane runner (`j work run` and friends, formerly OCaml in the devtools
repo) was intertwined with tm at several layers: it calls `tm runs
start/finish`, deploys the tm telemetry hooks, exports `TSKMSTR_RUN_ID`,
and its behavior is analyzed through `tm runs show`. The integration
surface had grown to the point where the runner needed to live here.

Decided 2026-08-06 — see `docs/decisions/0002-runner-absorption.md`:
session spawning is in scope (supersedes 0001's boundary), the runner is
ported to Rust as `tm work ...` rather than vendored, and the seam is
"tskmstr owns what it needs to do its job; personal config stays in the
user's own tooling."

Completed 2026-08-07: full `tm work` CLI surface (`new`/`remove`/`list`/
`restore`/`start`/`run`), config-driven lanes, hooks, and PR-URL recording
landed per `docs/plans/runner-port.md` (see that file's steps 1-12).
`devtools`' `j work` still exists unchanged pending dogfooding, per the
plan's migration path (§5).

## 2. Cost and usage analytics for `tm ticket audit` / `tm ticket create` — done

Ticket audit and ticket create are Claude-driven processes (axiom skills
call tm), but their token cost was invisible — usage telemetry only
existed for lane runs (`tm runs`, keyed by TSKMSTR_RUN_ID). Goal: the same
per-model usage capture for audit/create sessions, so the cost of grooming
a ticket is as measurable as the cost of working it.

Implemented 2026-08-07 per `docs/plans/session-usage.md`: sessions became
runs with a `kind` discriminator (`lane`/`audit`/`create`), identified via
`CLAUDE_CODE_SESSION_ID` and registered by the ticket commands themselves
through marker files the telemetry hooks read as a fallback gate; a new
`SessionEnd` hook finishes abandoned session runs. Surfaces: `tm runs
--kind`, a `KIND` column, `tm ticket audit`'s `Last audit usage:` line,
and kind badges in the watch TUI (which also gained a color pass).

Remaining operational step: sync the axiom repo's hook copies and its
`settings.json` (SessionEnd wiring) so interactive sessions pick up the
new gating.

## 3. Per-agent cost breakdown in run telemetry — done

`tm runs` broke usage down by model only. Added a second dimension:
by agent — each subagent invocation (elixir-implementer, Explore, ...)
keyed by `(agent_type, model)`. This shows where in the pipeline tokens
burn and where to optimize (e.g. is review or implementation the
expensive phase?).

Implemented 2026-08-07 per `docs/plans/per-agent-usage.md` (see its
status section): `hooks/tm-event.sh` emits an `agent_usage` event per
completed `Agent`/`Task` call from the `PostToolUse` payload's usage
summary; `tm runs show` renders an always-on "Agent usage" section
(human, `--json`, and the watch run-detail overlay). Tokens only — no
per-agent cost exists anywhere upstream, so none is invented.

The devtools hook mirror is synced, so the next lane run produces the
events; interactive audit/create sessions need stream 2's remaining
axiom hook/settings sync. The reconciliation gap between `model_usage`
and summed `agent_usage` should be eyeballed on the first few real
runs (see the plan's status section).

## 4. Board-launched ticket-audit sessions — done

Once streams 1-3 are in place: from the `tm board` TUI, select a ticket
and launch a ticket-audit Claude session for it in a tmux session
(attach/detach), enabling several concurrent audits. Requires:

- The runner living in tm (stream 1) so the board can launch sessions.
- A status channel back to the board so the user knows when a session is
  blocked waiting on human input vs. safely running — the existing run
  event/heartbeat store is the natural transport.
- Board UI: per-ticket session indicator (running / waiting-for-input /
  done), attach action.

Implemented 2026-08-07 per `docs/plans/board-audits.md` (see its status
section): `a` on a board ticket launches a detached `tm-audit-<key>` tmux
session running interactive `claude` with the audit prompt (config
`[work.audit]`), pre-registering a `kind = "audit"` run that the
in-session `tm ticket audit` adopts via `TSKMSTR_SESSION_RUN_ID`; a new
`tm-session-state.sh` hook (`Stop`/`Notification`/`UserPromptSubmit`)
emits `await`/`resume` events from which a display-only waiting state is
derived (no new `RunStatus`); cards show starting/running/waiting/done
badges polled every ~2s, and `a` attaches (terminal suspend/restore)
when the session is live. The board also gained the accent-color pass
the watch screen got in stream 2.

Operational step remaining: the axiom repo's hook copies and its
`settings.json` need the new script and the three hook wirings before
waiting-state telemetry flows in real sessions (superset of the stream
2/3 sync chore).

## 5. Board-launched lane runs — done

Stated 2026-08-07. The board's lifecycle story so far covers grooming
(stream 4's `a` audit action); the next column over is execution: from a
ready-for-work ticket, launch the normal headless `tm work run` flow with
one keypress, and see its status on the card.

Implemented 2026-08-07 per `docs/plans/board-lane-runs.md`: `w` on a board
ticket launches `tm work run <lane> <key>`, via a floating lane picker when
more than one lane is configured (direct launch for exactly one). The
launcher runs as a watched child process polled with `try_wait` from the
existing event loop — chosen over fire-and-forget detach because
`prepare_run_lane` creates no run row until preflight succeeds, so preflight
failures (dirty worktree, missing prompt) must surface via the child's
captured stderr in the status line. Cards carry a `run:
starting/running/waiting/done/failed` badge alongside the audit badge,
polled on the same ~2s cadence from `kind = "lane"` runs; an active run
guards against double-launch.

## 6. In-progress run visibility from the board — done

Stated 2026-08-07. For a ticket with an active (or recent) run: inspect
it without leaving the board.

Decided 2026-08-07: lane runs **stay headless**, and visibility is the
watch screen's existing `RunDetail` floating window (header, checklist,
model/agent usage, event timeline) opened on the board for the selected
ticket's latest run — build on the infrastructure that exists rather
than adding a second hosting mode. Small delta: the board already
carries a lenient `RunStore`, and the overlay rendering exists; it needs
a keybinding, a `LoadRunDetail`-by-ticket path, and the watch screen's
~500ms detail-refresh tick while open.

(Attach-to-run was considered and set aside: headless `setsid` runs
have no controlling terminal, so attaching would require tmux-hosting
work runs — a second hosting mode the overlay makes unnecessary.
Interactive flows that genuinely need a terminal — audits — already
have one via stream 4.)

Implemented 2026-08-07 per `docs/plans/board-run-detail.md`: `v` on a
board ticket opens the run-detail overlay on the ticket's latest run of
any kind, loaded via a ticket-keyed `Cmd::LoadTicketRunDetail` (lenient
when the runs DB is unavailable; a load failure before anything renders
closes the overlay via the status line instead of sticking on
"Loading..."), refreshed every ~500ms while open. The overlay itself was
rebuilt from a single scrolling paragraph into grouped panels — a
three-column header grid (identity / timing / cost), side-by-side Usage
and Checklist panels with truncation markers and stable placeholders,
and a full-width Events timeline that is now the only scrolling region —
with the run's status color as the window accent and per-kind event
accents (`await` yellow, `bots_done` green, `give_up`/`poll_error` red).
The watch screen shares the renderer, so `tm runs watch` got the same
redesign for free.

## 7. Bugbot follow-through on code review — done

Stated 2026-08-07. When a ticket reaches code review its PR sits waiting
on bot review (`review_bots`, e.g. cursor bugbot); today noticing that
the bots finished — and cleaning up their findings — is manual. Goal:
from the board, arm a watcher that notices bot completion and gets the
findings fixed.

- **The poller is a plain tm process, not a Claude session** (decided
  2026-08-07): bot-completion detection is deterministic and automatable
  through `gh`, so no tokens are spent waiting. tm already knows the
  bots and how to read their reviews (the `tm pr` bot-findings
  machinery) — the watcher is a detached tm process polling `gh` on a
  slow cadence (30-60s; never the board's 2s tick — the board must not
  make network calls per tick), recorded as a run row (`kind =
  "review-watch"`?) so the existing badge/event machinery shows
  `bots: pending / done` on the card for free.
- **On completion, then spend tokens**: launch a cleanup session against
  the bot findings (a lane run whose prompt consumes the findings, or an
  interactive tmux session like stream 4's audits — decide during
  design; the findings-to-prompt plumbing is the real work).
- **Auto-launch is config-gated**: `on_bots_done = "notify" | "launch"`,
  default `notify` — an unattended trigger that spawns Claude sessions
  is an explicit opt-in. The notify path flips the board badge (and the
  waiting-style bold accent) so cleanup is still one keypress.
- Watcher lifecycle questions for design: dies with the PR merging/
  closing; dedup (one watcher per PR); what happens on bot findings =
  zero (badge straight to done, no cleanup session).

Implemented 2026-08-07 per `docs/plans/bugbot-watch.md` (see its status
section): `tm pr watch <KEY>` resolves the ticket's open PR via a widened
`pr_list`, dedups against a running `review-watch` run, and detaches via
the same `setsid` re-exec idiom the lane runner uses (`--foreground` runs
the poll loop in-process). The loop polls `gh` every `poll_secs` (default
45) for PR lifecycle and whether every configured bot has reviewed; zero
unresolved findings finishes the run `Done`, findings write a
`$XDG_DATA_HOME/tskmstr/findings/<key>.json` file and finish the run
`Review`, and `on_bots_done = "launch"` auto-launches the cleanup session
(default `"notify"` just flips the badge). The cleanup session is a
detached `tm-bugbot-<key>` tmux session, structurally identical to
stream 4's audit launch, running `/bugbot-triage {key} {findings_file}`
by default. On the board, `b` arms the watcher, attaches to a live
cleanup session, launches one once the watcher is ready, or reports a
status-line message while still watching; cards carry `bots:` and
`clean:` badges alongside the audit/run badges. Operational remainder:
the axiom-side `/bugbot-triage` skill needs its documented first step,
`tm runs register --kind bugbot-cleanup {key}`, and reads the findings
file itself; no new hook syncing is expected beyond streams 2/4's, since
the await/resume/session-end hooks are reused unchanged.

Streams 5-7 together make the board the control surface for the whole
ticket lifecycle: groom (audit) → execute (lane run) → observe (run
overlay) → land (bot cleanup). All three streams have landed.

## Non-tskmstr chores tracked elsewhere

- Devtools nixpkgs pin update (April 2026 → current): operational task in
  the devtools repo, in progress 2026-08-06.
- Graphify A/B evaluation on the axiom lane: baseline and first-run
  results live in thatch memory (`graphify A/B baseline`, `graphify A/B
  run 1`); integration levers (lane prompt step 3, graphify-nudge hook)
  landed 2026-08-06.
