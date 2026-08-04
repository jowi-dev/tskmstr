# ADR-0001: Run State Tracking

**Status:** Proposed
**Date:** 2026-08-04
**Depends on:** `tskmstr ready` + Jira link support (built)
**Upstream context:** axiom `docs/claude/decisions/0001-agent-orchestration.md` (D10)

## Problem

A lane run is opaque until it exits. `claude -p --output-format json` emits one
blob at the end, so there is no way to see that a ticket has been stuck re-running
the same failing test for forty minutes, or that a run tripped the Boberdoo guard
and is spinning.

## Decision

tskmstr gains a SQLite-backed run store and query/TUI surface over it.
**tskmstr does not spawn or supervise processes.**

### Ownership boundaries

| Data | Owner | Rationale |
|---|---|---|
| Ticket fields, status, dependency links | **Jira** | Single source of truth (axiom ADR-0001, D8) |
| Run state, events, cost, session IDs | **SQLite** | Ephemeral execution data, not ticket data |
| Process lifecycle, restart, supervision | **systemd / launchd** | Already solved; reimplementing is a project |
| Full transcripts | **Filesystem** | DB stores the path, not the text |

Do not mirror Jira ticket data into SQLite. `tskmstr ready` already queries Jira;
the run store joins to it by ticket key only.

## Schema

```sql
PRAGMA journal_mode = WAL;      -- one writer + many readers; set before concurrency
PRAGMA busy_timeout = 5000;

CREATE TABLE runs (
  id           INTEGER PRIMARY KEY,
  ticket       TEXT    NOT NULL,          -- 'AX-411'
  lane         TEXT    NOT NULL,          -- 'partner-integrations'
  status       TEXT    NOT NULL,          -- queued|running|blocked|review|done|failed
  session_id   TEXT,                      -- from claude -p JSON; enables --resume
  worktree     TEXT    NOT NULL,
  branch       TEXT,
  pid          INTEGER,
  transcript   TEXT,                      -- path on disk
  started_at   TEXT    NOT NULL,
  heartbeat_at TEXT,
  ended_at     TEXT,
  exit_code    INTEGER,
  num_turns    INTEGER,
  cost_usd     REAL,
  blocker      TEXT,                      -- escalation text when status='blocked'
  pr_url       TEXT
);

CREATE TABLE run_events (
  id      INTEGER PRIMARY KEY,
  run_id  INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  at      TEXT    NOT NULL,
  kind    TEXT    NOT NULL,   -- tool_use|subagent_start|subagent_stop|
                              -- commit|gate_blocked|escalation|stop
  detail  TEXT                -- JSON
);

CREATE INDEX idx_events_run ON run_events(run_id, at);
CREATE INDEX idx_runs_status ON runs(status);
```

Keep the status vocabulary small. Resist adding states until a query needs them.

## Mid-flight telemetry via hooks

Hooks fire *during* the run. This is what makes in-flight visibility possible
without parsing a stream.

The runner exports `TSKMSTR_RUN_ID` before invoking `claude -p`; every hook reads
it and attributes its row.

| Hook | Writes |
|---|---|
| `PostToolUse` (`Edit\|Write`) | `tool_use` event; bump `heartbeat_at` |
| `PreToolUse` (blocked, exit 2) | `gate_blocked` event with the guard's message |
| `SubagentStart` / `SubagentStop` | `subagent_start` / `subagent_stop` |
| `Stop` | final `stop` event |

The existing `guard-paths.sh` and `guard-bash.sh` gain a single append call on the
block path — they become guards *and* telemetry.

Optionally add `--output-format stream-json` piped to a tail writer for the full
transcript. Hooks give structured state; the stream gives narrative. Start with
hooks only.

## Liveness

A crashed runner leaves a row reading `running` forever.

- `heartbeat_at` bumped on every `PostToolUse`
- `tskmstr runs reap` marks a run `failed` when `heartbeat_at` is older than a
  threshold **and** `pid` is no longer alive
- Run reap on TUI startup and on a timer; do not trust `status` alone

## Commands

```
tskmstr runs                 # table: ticket, lane, status, age, last event
tskmstr runs watch           # TUI, polls SQLite (500ms is fine)
tskmstr runs show AX-411     # event timeline for the latest run
tskmstr runs reap            # reconcile dead runs
tskmstr runs resume AX-411   # print session_id for `claude --resume`
```

## Non-goals

Explicitly out of scope. Each one is where this turns into a job scheduler:

- Spawning or supervising `claude` processes
- Restart / retry policy
- Concurrency limiting (the init system or a semaphore file handles this)
- Signal handling and orphan cleanup beyond `reap`
- Storing transcripts in the database
- Any mirror of Jira ticket state

## Acceptance

- Two lanes run concurrently for 30 minutes with zero `SQLITE_BUSY` errors
- `tskmstr runs watch` shows tool activity within ~2s of it happening
- Killing a runner with `SIGKILL` leaves a row that `reap` correctly marks `failed`
- A run that trips the Boberdoo guard shows a `gate_blocked` event with the message
- `tskmstr runs resume AX-411` returns a session ID that `claude --resume` accepts
