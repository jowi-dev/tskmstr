# GH-80: ticket/run status drift audit

Issue: https://github.com/jowi-dev/tskmstr/issues/80

Tickets finish (run `done`, PR open or merged) while the tracker still shows
them in To Do, and nothing notices. Every status move tskmstr makes is an
advisory, one-shot transition fired by one specific command. This doc lists
every such hook and the paths that bypass it (the drift matrix), and
describes `tm drift`, the reconciliation check that makes the resulting
drift visible.

## Transition hooks

| Hook | Fired by | Interactive / headless | Failure mode |
|---|---|---|---|
| `status_on_create` | `tm ticket create` (unless `--status`/`--no-transition`) | n/a | warn and proceed |
| `status_on_run_start` | `tm work run` / board launch, in `prepare_run_lane` once the run row exists | both (one placement covers both modes) | warn and proceed; already-in-target is silent; ticketless runs skip it |
| `status_on_pr` | `tm pr create` only (auto-created and pre-existing tickets) | whichever session runs the command | warn and proceed; already-in-target is silent |
| `status_on_merge` | `tm merge` and the board's merge key | n/a | warn and proceed; already-in-target is silent (closing-keyword race, #32) |

Each key is optional per repo (`.tskmstr.toml` over the global
`config.toml`). An unset key never transitions and never warns (#74 makes
`tm check` flag missing keys).

## Drift matrix

Path → status the ticket should reach → what actually happens.

| Path | Expected | Actual | `tm drift` reports |
|---|---|---|---|
| Lane started, `status_on_run_start` unset | In Progress | stays in To Do | `RunningStillNew` while the run is live, then `FinishedStillNew` |
| Lane started, ticket provider can't be built (no token, config error) | In Progress | **silently** stays in To Do: `tm work run` builds its provider with `.ok()`, so the hook is skipped with no warning | same as above |
| Lane started, transition fails (offline, no matching transition) | In Progress | stays put; warning in the run log only | same as above |
| Agent opens its PR with `gh pr create` instead of `tm pr create` | In Review | stays wherever run start left it | `FinishedStillNew` if still To Do (open PR found by key in title/branch/body) |
| PR associated after the fact with `tm ticket <KEY>` | In Review | association never transitions (by design) | `FinishedStillNew` if still To Do |
| Headless run finishes having opened a PR (supervisor resolves `pr_url`) | In Review | nothing: the supervisor records `pr_url` but doesn't apply `status_on_pr` | `FinishedStillNew` if still To Do |
| Interactive session finished by hand, or reaped as `interrupted` after doing the work | In Review | nothing | `FinishedStillNew` when a PR is open; otherwise `Stalled` once past `--stall-hours` |
| `status_on_pr` unset | In Review | stays in In Progress / To Do | `FinishedStillNew` if still To Do |
| PR merged with `tm merge` / board | Done | `status_on_merge`, plus GitHub's closing keyword | — |
| PR merged in the GitHub UI or with `gh pr merge` (GitHub backend, PR into the default branch) | Done | `Closes #N` in the PR body closes the issue, which reads as Done; stale `tm:status/*` label (#75, fixed) | — |
| PR merged outside tm into a **non-default** base (a stacked PR) | Done | closing keywords only fire on the default branch, so the issue stays open | `MergedNotDone` |
| PR merged outside tm, Jira backend | Done | nothing | `MergedNotDone` |
| PR deliberately written with `Refs #N` (the ticket needs more than the PR, e.g. a manual acceptance step) | stays open on purpose | stays open | `MergedNotDone`: a **true positive for the rule, but not drift**. Don't fix it: pass the keys you do want moved to `tm drift --fix KEY...` |
| Ticket In Progress, run died, no PR | a human decision | stays In Progress | `Stalled` (never auto-fixed) |

## `tm drift`

`detect_drift` (`src/ticketing/drift.rs`) is a pure function over
`(ticket status category, latest lane run status, PR lifecycle)`. It checks
these rules in severity order:

1. PR merged, ticket not done → `MergedNotDone` (fix: `status_on_merge`, else `Done`)
2. ticket new-category and (PR open, or run `done`/`review`) → `FinishedStillNew` (fix: `status_on_pr`, else `In Review`)
3. ticket new-category and run `running`/`hibernated` → `RunningStillNew` (fix: `status_on_run_start`, else `In Progress`)
4. ticket in progress, no live run (`queued`/`running`/`hibernated`/`blocked`), no open PR, latest run started more than `--stall-hours` ago (default 24) → `Stalled` (no fix)

A done-category ticket never drifts. A ticket with no runs has no age, so it
is never reported as stalled.

**Projects.** Lanes are configured in each repo's own `.tskmstr.toml`, so
the run store is the only cross-project registry. `tm drift` takes every
ticket scope with lane runs, resolves the main repo root from that scope's
newest worktree still on disk, adds the current repo, and loads each with
its own effective config and ticket provider. If a project's lookups fail,
`tm drift` prints a warning and skips it. The summary line counts only the
projects actually checked.

**Candidates and PRs.** It checks each project's open tickets (one search).
A ticket's PR is its open PR found by key (`find_pr_for_ticket`, which
catches PRs opened outside `tm pr create`). Failing that, it is any PR whose
head branch or number matches the latest lane run's recorded branch or
`pr_url`, which is how merged PRs are found. Known gap: a PR merged by hand
from a branch no lane run recorded is invisible.

**`--fix`** applies the suggested transition through `reconcile_status`,
the body `apply_status_on_merge` already used. It is advisory and never
errors, and a ticket already in the target status is reported as
`<KEY> already in <STATUS>`, not warned about. `--fix` is
operator-triggered, not run on the board poll:
silently moving tickets would hide the bugs this command exists to find.

## Root causes found

The reconciliation is a safety net. These gaps were filed as their own
tickets:

- Headless lane supervisor resolves the run's PR but never applies
  `status_on_pr`, so a PR opened with `gh pr create` leaves the ticket
  behind.
- `tm work run` skips `status_on_run_start` silently when its ticket
  provider can't be built.
- A stacked PR merged outside `tm merge` never closes its issue (closing
  keywords only fire on the default branch).

Already tracked: #74 (missing config keys), #75 (stale status labels on
closed issues, fixed), #79 (false "no transition" warning when already in
target).
