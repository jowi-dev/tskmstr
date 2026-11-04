# ADR-0011: tm Is the Control Plane, thatch Is the Agent Plane

**Status:** Accepted
**Date:** 2026-11-04

## Problem

tm launches agent sessions, and its session prompts already depend on
[thatch](https://github.com/sysread/thatch) for memory and methods: the
create, audit and review prompts recall thatch memory and run thatch
skills, and auto-review runs thatch's review pipeline. An audit on
2026-11-04 compared the two codebases. It found almost no duplication,
only a ~100-line desktop notifier.

The risk is in what tm has not built yet. The features next on tm's path
are lanes talking to each other, supervising agents while they work, and
reacting to PR events inside a session. thatch already does all of these
with cross-session chat, its coordinator workflow, and watchers. With no
recorded boundary, each of those features gets decided ticket by ticket,
and a lane can build a weaker copy of something thatch does well.

## Decision

**tm owns the control plane. thatch owns the agent plane.**

tm owns everything that must be deterministic, cheap to run on a timer,
or tied to tickets, processes and git:

- ticket backends, status transitions, readiness, blockers, epics and the
  audit gate;
- dispatch and autopilot, admission (memory budget, build slots, usage
  gate) and runner routing;
- worktrees, branches, tmux sessions, liveness, kill-safety and
  hibernation;
- the run store, cost and footprint telemetry, PR creation, merge and
  drift.

thatch owns what happens inside an agent session and between sessions:

- memory, predictions and behaviors;
- methods, as skills: planning, refinement, review, review response,
  PR and ticket writing, walkthroughs and reflection;
- cross-session chat, and coordination of agents by an agent.

### Check thatch first

Before tm adds a feature that changes how an agent thinks, writes, reviews,
remembers or talks to another agent, check whether thatch already does it.
If it does, tm schedules and supervises it (launches the session, records
the run, applies the gates) and the agent runs thatch's skill or tool. If
thatch nearly does it, prefer a change to thatch over a copy in tm.

tm keeps its own copy only when thatch's version cannot serve tm's case.
Name the reason in the ticket. One example: thatch's PR watchers only wake
the session that created them, and only on opencode, so tm's detached
review watch stays.

### Integration contract

tm reaches thatch only through thatch's public surfaces:

- the `thatch` CLI and its documented output (`thatch chat list`,
  `thatch chat tail`, `thatch chat broadcast`, `thatch --version`);
- skills and slash commands named in tm's session prompts;
- thatch's MCP tools, called by the agent sessions tm launches.

tm never reads thatch's SQLite database, config file or queue files, and
never links against thatch code. This is the same neutral-contract rule tm
follows with `gh` and tmux. Either tool can change its internals without
breaking the other.

## Consequences

- An agent-side feature in tm needs a ticket that names the thatch surface
  it builds on, or the reason thatch cannot serve it.
- `tm check` should report a missing or outdated thatch and skills that do
  not support the configured runner (GitHub issue #207), because tm's
  sessions now depend on them.
- Where tm needs something thatch lacks, the change goes to thatch first.
  For example, the thatch CLI cannot write a memory, so a tm feature that
  needs one launches an agent session that writes it.
- thatch's coordinator workflow (`thatch-coordination`, `session_tab`) does
  not replace tm's dispatcher. It is opencode-v2-only, runs as a model tool
  tm cannot call, spends tokens on every scheduling decision, and has no
  ticket, admission or merge gates. Its supervision approach (one tracked
  entry per worker, chat, death notices, checking work before accepting it)
  is the model for an epic-level coordinator that drives tm (GitHub issue
  #209).
