# Roadmap

Prioritized workstreams as of 2026-08-06. Ordering within this file is
rough priority; dependencies are called out per stream.

## 1. Absorb the `j work` lane runner into tskmstr

The lane runner (`j work run` and friends, currently OCaml in the devtools
repo) is now intertwined with tm at several layers: it calls `tm runs
start/finish`, deploys the tm telemetry hooks, exports `TSKMSTR_RUN_ID`,
and its behavior is analyzed through `tm runs show`. The integration
surface has grown to the point where the runner should live here.

Open questions to settle before starting:

- **Supersedes decision 0001's boundary rule.** `docs/decisions/0001-run-state.md`
  deliberately scoped tskmstr to run *state* only — no process spawning or
  supervision. Moving the runner in reverses that. Write a superseding
  decision doc (0002) rather than silently violating 0001.
- Port to Rust as `tm work ...` subcommands vs. vendoring the OCaml? A Rust
  port keeps the nix-only single-toolchain property and lets the runner
  share the config/runs/Jira layers directly.
- What stays in devtools: lane prompt files, tmux layout conventions, and
  the claude-hooks scripts could move here too (they are tm telemetry
  hooks) or stay behind — decide the seam explicitly.

## 2. Cost and usage analytics for `tm ticket audit` / `tm ticket create`

Ticket audit and ticket create are Claude-driven processes (axiom skills
call tm), but their token cost is invisible today — usage telemetry only
exists for lane runs (`tm runs`, keyed by TSKMSTR_RUN_ID). Goal: the same
per-model usage capture for audit/create sessions, so the cost of grooming
a ticket is as measurable as the cost of working it.

Likely shape: a lightweight session-record analog to runs (or a run
`kind` discriminator) plus hook gating that recognizes audit/create
sessions. Needs design: what identifies such a session, and where the
Stop-hook usage snapshot lands.

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
