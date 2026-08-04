# PLAN.md — Run State Tracking

Adds mid-flight observability for autonomous lane runs. Spec:
`docs/decisions/0001-run-state.md`.

**Prerequisite:** don't start this until the axiom side reaches Phase 3 and has
produced real transcripts. The event vocabulary should come from observed runs,
not from guessing. See `~/Worktrees/axiom/claude-agent/PLAN.md`.

---

## The one-line summary

tskmstr owns **state**. systemd/launchd owns **process lifecycle**. Jira owns
**ticket data**. Nothing crosses those lines.

The moment tskmstr spawns and supervises children, you've signed up for restart
policy, orphan reaping, signal handling, crash recovery, and concurrency limits —
that's a job scheduler, and the init system already is one.

---

## Build order

Each step is independently useful. Stop at any point and you still have something.

### 1. Schema + `runs` table

DDL is in the ADR. Two things not to defer:

```sql
PRAGMA journal_mode = WAL;    -- one writer, many readers
PRAGMA busy_timeout = 5000;
```

Set these before there are two concurrent lanes. The failure mode when you forget
is `SQLITE_BUSY` surfacing as an apparently flaky agent — you'll debug the wrong
layer for an afternoon.

- [x] `runs` and `run_events` tables, indexes
- [x] `tskmstr runs start` → prints a run id
- [x] `tskmstr runs finish` → status, exit code, session id, cost, turns
- [x] `tskmstr runs` → table view

### 2. Wire the runner

`bin/axiom-lane` in the axiom worktree has `TSKMSTR:` markers at the three
integration points. Uncomment as the commands land.

- [ ] Export `TSKMSTR_RUN_ID` before `claude -p` so hooks can attribute events

### 3. First event: `gate_blocked`

Smallest change, highest signal. `guard-paths.sh` and `guard-bash.sh` already
isolate the block path — each needs one append before its `exit 2`.

A run that trips the Boberdoo guard and then spins is exactly the failure that's
currently invisible.

- [x] `tskmstr runs event <run-id> --kind gate_blocked --detail <json>`
- [ ] Append call in both hooks

### 4. Remaining hook telemetry

| Hook | Event |
|---|---|
| `PostToolUse` (`Edit\|Write`) | `tool_use`; bump `heartbeat_at` |
| `SubagentStart` / `SubagentStop` | `subagent_start` / `subagent_stop` |
| `Stop` | `stop` |

### 5. Liveness

A crashed runner leaves a row reading `running` forever.

- [x] `tskmstr runs reap` — stale `heartbeat_at` **and** dead `pid` → `failed`
- [x] Reap on TUI startup and on a timer. Never trust `status` alone.

### 6. TUI

- [x] `tskmstr runs watch` — poll SQLite, 500ms is fine
- [x] `tskmstr runs show <ticket>` — event timeline
- [x] `tskmstr runs resume <ticket>` — print session id for `claude --resume`

---

## Non-goals

Each is where this turns into a job scheduler:

- Spawning or supervising `claude` processes
- Restart / retry policy
- Concurrency limiting (init system or a semaphore file)
- Signal handling and orphan cleanup beyond `reap`
- Storing transcripts in the database — store the path
- **Any mirror of Jira ticket state.** `tskmstr ready` already queries Jira; the
  run store joins by ticket key only.

---

## Acceptance

- [ ] Two lanes run concurrently 30 min, zero `SQLITE_BUSY`
- [ ] `runs watch` reflects tool activity within ~2s
- [ ] `SIGKILL` a runner → `reap` marks it `failed`
- [ ] Boberdoo-guard trip shows a `gate_blocked` event carrying the message
- [ ] `runs resume AX-411` returns a session id `claude --resume` accepts

The `SIGKILL` test is the one that gets skipped. Every crashed-run bug lives in
that path, and it stays invisible until the day the TUI confidently shows three
tickets in progress that died an hour ago.
