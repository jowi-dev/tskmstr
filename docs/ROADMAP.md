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

## 3. Per-agent cost breakdown in run telemetry

`tm runs` currently breaks usage down by model. Add a second dimension:
by agent — the orchestrating session vs. each subagent (elixir-implementer,
elixir-reviewer, Explore, audit-hook-logging, ...). This shows where in
the pipeline tokens burn and where to optimize (e.g. is review or
implementation the expensive phase?).

Building blocks that already exist: tool events carry an `agent` field,
and the SubagentStop hook fires per subagent — the usage snapshot needs
the same attribution. Surface as `tm runs show --by-agent` (and in the
`--json` output).

## 4. Board-launched ticket-audit sessions (future)

Once streams 1-3 are in place: from the `tm board` TUI, select a ticket in
Prioritized for Dev and launch a ticket-audit Claude session for it in a
tmux session (attach/detach), enabling several concurrent audits. Requires:

- The runner living in tm (stream 1) so the board can launch sessions.
- A status channel back to the board so the user knows when a session is
  blocked waiting on human input vs. safely running — the existing run
  event/heartbeat store is the natural transport.
- Board UI: per-ticket session indicator (running / waiting-for-input /
  done), attach action.

Not started; captured here so the earlier streams are designed with this
destination in mind.

## Non-tskmstr chores tracked elsewhere

- Devtools nixpkgs pin update (April 2026 → current): operational task in
  the devtools repo, in progress 2026-08-06.
- Graphify A/B evaluation on the axiom lane: baseline and first-run
  results live in thatch memory (`graphify A/B baseline`, `graphify A/B
  run 1`); integration levers (lane prompt step 3, graphify-nudge hook)
  landed 2026-08-06.
