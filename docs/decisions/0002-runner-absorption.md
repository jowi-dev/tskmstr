# ADR-0002: Absorb the Lane Runner

**Status:** Accepted
**Date:** 2026-08-06
**Supersedes:** the process boundary in ADR-0001 (everything else in 0001 stands)

## Problem

ADR-0001 drew a hard line: tskmstr owns run *state* only — no spawning, no
supervision. Since then the lane runner (`j work run`, OCaml, devtools repo)
has grown inseparable from tm: it calls `tm runs start/finish`, deploys the
tm telemetry hooks, exports `TSKMSTR_RUN_ID`, and its output is consumed
through `tm runs show`. The runner cannot function without tm, and the
roadmap's board-launched audit sessions require tm to launch sessions
itself. Keeping the runner outside the repo is a boundary without a
benefit.

## Decision

1. **tskmstr may spawn and supervise Claude sessions.** Launching and
   managing task-working sessions is in scope — it is a task master; it
   needs to master tasks. The ADR-0001 non-goals list is amended
   accordingly; restart policy and concurrency limits become design
   questions rather than forbidden territory.

2. **Port, don't vendor.** The work commands are rewritten in Rust as
   `tm work ...` subcommands. The `j` CLI is an incubator — new ideas live
   there until they are big enough to graduate, and these have graduated.
   A Rust port keeps the single nix toolchain and gives the runner direct
   access to the config/runs/Jira layers instead of shelling into tm.

3. **The seam.** Anything tskmstr needs to run and do its job — session
   launching, worktree management, the telemetry hooks it deploys, run
   supervision — belongs in this repo. Anything a different tskmstr user
   would supply in their own fashion — lane prompt content, tmux layout
   preferences, personal paths — stays in personal config (devtools for
   Joe).

## What still stands from ADR-0001

- Jira remains the single source of truth for ticket data; no mirroring.
- Run state and events live in SQLite; transcripts live on disk.
- The hook-based mid-flight telemetry design.
- Keep the status vocabulary small.
