# ADR-0009: Cross-Project Overview — Ticket-Centric Attention Queue

**Status:** Accepted
**Date:** 2026-10-04

## Problem

Inside one repo, `tm board` answers "what's going on". Across repos nothing
does. `tm runs watch` reads the shared `runs.db` and so sees every repo's
runs, but it is organised by **run** status (Queued / Running / Blocked /
Review / Done / Failed / Interrupted), not by where the **ticket** is in its
lifecycle. At current lane volume the operator's questions are ticket
questions:

- Which tickets have a session at all, and is it live?
- Which are waiting on me (awaiting input, needs review, ready to merge)?
- Which are stuck or drifted (finished work still in To Do, #80)?
- Can the machine take another lane right now?

Answering any of them today means attaching to sessions one by one or
opening `tm board` in each repo. GitHub issue #81 asks for a layout
decision and a slice plan; this ADR is that decision. Implementation lands
as the follow-up tickets listed under **Slices**.

## Options considered

1. **Lifecycle columns, project-tagged cards.** Columns by ticket stage
   (Not started · Running · Needs input · Needs review · Ready to merge ·
   Stuck), each card prefixed with its project. Closest to the board, so
   it's familiar, but it answers "what does everything look like", not
   "what do I touch next". Six columns of mixed projects also squeeze card
   width on an ordinary terminal.
2. **Project swimlanes × lifecycle columns.** Best "is project X healthy"
   read. Rejected as the default: vertical space runs out past ~5 projects,
   most cells are empty at any moment, and per-project health is already
   what `tm board` in that repo is for.
3. **Attention queue + per-project summary strip.** One list sorted by how
   much the ticket needs the operator, with a one-line counts strip per
   project above it. Most actionable, least spatial.

## Decision

1. **Default layout is (3), with (1) as a toggle.** The view opens on the
   attention queue. A single key switches to lifecycle columns over the
   same rows, for when the operator wants the spatial read. (2) is not
   built. Both layouts render one data model, so the toggle is presentation
   only and costs no extra fetches.

   Queue order, highest first:

   | Rank | Stage            | Meaning                                             |
   |------|------------------|-----------------------------------------------------|
   | 1    | Needs input      | live run with the awaiting-input marker             |
   | 2    | Ready to merge   | PR open, approved/mergeable, checks green           |
   | 3    | Needs review     | PR open, no approving review yet                    |
   | 4    | Conflicted       | PR open, merge state conflicted or checks failing   |
   | 5    | Stuck / drifted  | #80 drift, a failed/interrupted latest run, or a stale heartbeat |
   | 6    | Running          | live run, nothing needed from the operator          |
   | 7    | Not started      | in the operator's ready list, no run recorded       |

   Ties break by age in current stage, oldest first. A ticket appears
   exactly once, in its highest-ranked stage.

2. **New entry point `tm overview`; `tm runs watch` stays as it is.**
   The two views have different nouns. Watch is per **run**: it shows
   audit, retro, review-fix and bot-watch runs, several runs per ticket,
   failed attempts, and is the right tool for "why did that run die". The
   overview is per **ticket** and collapses all of that into one row per
   `(scope, ticket)`. Replacing watch would either lose the run-level view
   or turn the overview into watch with a different sort. Both run on the
   same TUI loop, and each gets a key that switches to the other with the
   highlighted ticket kept, so the operator moves between them in one
   motion. `overview` over `meta`: it says what the view is.

3. **Rows are keyed by `(scope, ticket)` and built from three sources with
   separate refresh budgets.** This is the main technical risk in #81, so
   the budget is fixed here rather than left to the implementation:

   - **`runs.db`: every tick.** Local SQLite, already polled by watch at
     the 250 ms loop. Supplies run status, live/heartbeat, awaiting input,
     checklist progress, `pr_url`, memory, and the set of known scopes.
   - **Tracker status and PR state: per repo, background, 60 s.** One
     batched call per repo per refresh, never one per ticket. For a GitHub
     scope that's one `gh pr list --json
     number,headRefName,reviewDecision,mergeStateStatus,statusCheckRollup`
     plus one issue query for the tracked keys. For Jira, one JQL `key in
     (...)` search. Fetches run off the render thread and the view shows
     the last good snapshot with its age. `r` forces a refresh. Nothing in
     the codebase fetches review/merge state today (`RunCard` only has
     `pr_url`), so this is new code, not reuse.
   - **Which repos get polled:** only scopes with a live run or a run that
     ended in the last 7 days. A repo the operator stopped working in
     drops out on its own, so N stays at "repos in flight", not every repo
     ever onboarded.

   A scope whose fetch fails keeps its runs-only rows and marks the
   project strip entry stale. It never blanks the view.

4. **"Not started" is limited to the operator's ready list.** Tickets with
   no run don't exist in `runs.db`, so the overview needs the tracker for
   them. It uses the same query as `tm ready` (assigned to me, unblocked),
   per polled scope, capped per project. It never lists the full backlog.
   This keeps the overview from becoming a second `tm board`: rank,
   assign, retro, audit, and anything else that needs the full ticket list
   stays board-only.

5. **Runs record their repo root.** Board actions assume cwd is the repo.
   The overview must route each action by the row's own repo, and today a
   run row can't name it: `runs.repo` is the repo *directory name* (the
   memory-estimate key from #66), and `worktree` is the lane worktree,
   which `tm merge` cleans up. A new nullable `runs.repo_root` column,
   stamped at run start next to `agent`/`repo`, holds the main checkout's
   absolute path. For rows without it, fall back to `git -C <worktree>
   rev-parse --git-common-dir` while the worktree exists. A row that
   resolves neither way stays visible with its actions disabled, and the
   status line says why. This is a forward-only migration with no backfill.

6. **Actions reuse the board's implementations, run with the row's repo
   root as cwd.** `s` attach already routes by scope (#25). `v` detail and
   `L` logs need only the run id. `V` vdiff review, `F` fix pass, and `M`
   merge are spawned as the same child `tm` invocations the board uses, with
   `current_dir(repo_root)`. `Space` batch-merge queues are kept **per
   scope**, and confirming fires one `tm merge K1 K2 …` per repo, because a
   batch shares one conflict session (#67) and that session lives in one
   repo. `B` opens `tm board` in the row's repo in a new tmux window.

7. **System header.** One line: CPU and memory summed across live lanes,
   host totals, the `[work]` memory budget against estimated lane cost
   (#66), and `build_slots` in use/total. That answers "can I launch
   another lane". Per-lane memory already comes from `runs.db`. CPU is new
   and is sampled with the same process-tree walk `footprint.rs` uses for
   memory.

## Slices

Each slice leaves `tm overview` usable on its own. Signals land before
actions, and read-only actions land before ones that change state.

1. **Data model + `repo_root` column.** A pure `OverviewRow` join over
   `runs.db` keyed by `(scope, ticket)`, with stage derivation and queue
   ordering as unit-tested pure functions. Adds the `runs.repo_root`
   migration and stamping.
2. **`tm overview` render: attention queue + project strip.** Runs-only
   signals (needs input, running, stuck from run status/heartbeat), `s`
   attach, and the toggle to and from `tm runs watch`. Useful before any
   network fetch exists.
3. **Tracker + PR state fetch with refresh budget.** The background
   per-repo batched poller from decision 3. Lights up Needs review, Ready
   to merge, Conflicted and Not started. Drift glyph once #80 lands.
4. **Lifecycle-columns toggle.** Layout (1) over the same rows.
5. **System resource header.** Decision 7.
6. **Scope-aware read actions: `v`, `L`, `B`.**
7. **Scope-aware write actions: `V`, `F`, `M`, `Space`.** Per-repo batch
   queues.

## Dependencies

- #80 (status drift audit) supplies the `Drift` signal for the Stuck stage.
  Slice 3 ships without it and adds the glyph when #80 lands. Nothing else
  waits on it.
- #67 / #68 batch merge, reused by slice 7.

## What this does not change

`tm runs watch` keeps its screen, keys, and filters. `tm board` keeps every
action it has, including those the overview borrows. The run db gains one
nullable column and nothing else. Tracker and PR snapshots live only in
memory for the life of the overview process and are not persisted.
