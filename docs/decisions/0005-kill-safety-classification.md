# ADR-0005: Kill-Safety Classification for tmux Sessions

**Status:** Accepted
**Date:** 2026-09-08

## Problem

A session picker's kill key (jowi-dev/devtools#15) destroys a tmux session
and removes its worktree. Whether that is dangerous depends on facts only
tm holds: whether an agent run is live in the session (runs store), whether
the ticket's PR is merged (`gh`), and which sessions are per-project hub
sessions rather than disposable ticket sessions (the `@root_session`
jump-back contract of GitHub issue #19 / devtools#8). GitHub issue #26 asks
tm to publish that classification so the picker can decide when to demand
confirmation — without either repo importing the other, per the established
producer/consumer split.

The same issue's other half — a killed session stranding its run row and
blocking the board's lane guard — is solved in the runs store itself
(liveness-aware `RunStore::reap`, see that method's docs) and needs no
contract; this ADR pins the picker-facing half.

## Decision

**The contract is a query command, not a stamped tmux option.** Safety is
dynamic (runs start and finish, PRs merge) and a stamped option would need a
refresher process to stay honest; a query answered at confirmation time is
always fresh, and the picker shells out to tm exactly once per kill
keypress.

### Command

```
tm runs kill-safety <SESSION_NAME>
```

`SESSION_NAME` is a tmux session name, exactly as `tmux list-sessions`
prints it (e.g. `tm-<scope-slug>-<key>`). The command works from any cwd:
the runs DB lives at its XDG-resolved path and every other lookup is
anchored on data recorded in the run rows themselves.

### Output

- **stdout line 1** — exactly one of the four tier tokens below. This line
  is the machine-readable contract; the picker branches on it verbatim.
- **stdout line 2** — a one-line human-readable reason, suitable for
  embedding in a confirmation prompt.
- **exit code** — `0` whenever classification ran, including `unknown`.
  Non-zero means tm itself failed (e.g. an unopenable runs DB); the picker
  must treat any non-zero exit as `unknown`.

### Tiers

| Token | Meaning | Suggested picker behavior |
|---|---|---|
| `live-run` | A run hosted in this session is still `running` in the runs store and not provably dead (recorded pid alive, or no pid recorded yet). Covers starting, running, and waiting-for-input agents. | Prompt. |
| `root-session` | The session is some session's `@root_session` target: a per-project hub session (where the board runs), not a ticket session. | Always prompt. |
| `safe` | No hosted run is live, and the newest hosted run's recorded branch has a **merged** PR and no open one (checked via `gh pr list` in that run's worktree). | Kill silently. |
| `unknown` | Everything else: a session tm has no runs for, no recorded branch, an open or missing PR, a deleted worktree, or a `gh`/`tmux` failure. | Prompt (treat like `live-run`). |

Tier precedence is the table order: `root-session` is checked first (a hub
session never hosts runs, but if it somehow matched both, the stricter
always-prompt tier must win is the intent — in the implementation the root
check simply runs first), then liveness, then the merged-PR check.

### How tm maps a session name to runs

A run row is "hosted in" the queried session when any of these match, so
the classification works for every session shape tm launches:

1. the `runs.tmux_session` column, stamped by interactive `tm work run` /
   `tm review fix` launches at start;
2. the ticket session name recomputed from the row's `scope` + `ticket`
   (`tm-<scope-slug>-<key>`), covering runs adopted inside a session via
   `tm runs register` (audits, registered lane runs), which never get a
   stamp;
3. for `create`-kind runs, the scope's keyless creation session
   (`tm-<scope-slug>-create`).

Sessions tm never launched (personal scratch sessions, other tools'
sessions) match nothing and classify `unknown` — the picker prompts, which
is the correct default for foreign sessions.

## Consequences

- devtools#15 implements the picker side against this file: parse stdout
  line 1, map non-zero exit to `unknown`, and never parse line 2.
- `root-session` detection reads `@root_session` values from tmux at query
  time (`tmux list-sessions -F '#{@root_session}'`); the picker does not
  need to forward its own root-session knowledge, though it may short-cut
  with it before shelling out.
- The `safe` tier is deliberately strict: an *open* PR classifies
  `unknown`, not `safe` — pushed-but-unmerged work still warrants a prompt
  since the kill also removes the worktree.
- Classification never mutates anything. Reaping stranded rows is the
  board's and `tm runs reap`'s job, not the picker's.
