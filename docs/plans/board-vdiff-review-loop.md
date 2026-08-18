# Board vdiff review loop (issue #1)

Status: planned.

Close the code-review loop from the `tm board` TUI without leaving the
keyboard: open the focused ticket's PR in `vdiff` for review, then dispatch a
Claude fix pass over the review comments `vdiff` captured.

Upstream context: `vdiff` (https://github.com/jowi-dev/vdiff) is a visual PR
reviewer with an embedded nvim; `vdiff.nvim` captures per-hunk review comments
into `<git-dir>/vdiff/comments.json`, and `vdiff --export-comments` renders
them as agent-ready markdown grouped by file with `path:start-end` anchors.

## Decisions

- **Keybinding `V`** ("vdiff") on the Board screen opens the focused ticket's
  worktree in `vdiff`. Lowercase `v` is already the run-detail overlay, so the
  shifted variant sits next to it rather than displacing it.
- **Keybinding `F`** ("fix") on the Board screen dispatches the fix pass over
  the captured review comments.
- **Both keys are ungated by column/status**, matching `a` (audit) and `w`
  (lane run). There is no per-status gating precedent in the board today and
  no "Code Review" constant in code — `board_column_order` is display
  ordering only. Gating is instead *state-driven*: both keys degrade to a
  status-line message when the ticket has no lane run (and therefore no
  worktree), which is the same information the column would have conveyed.
- **No `vdiff --pr`**. `vdiff` has no `--pr` flag (vdiff#2). It doesn't need
  one here: every ticket worked from the board already has a worktree at
  `~/Worktrees/<repo>/<lane>` provisioned by `prepare_run_lane`, and the run
  row records its exact path. `V` resolves the ticket's latest `kind = "lane"`
  run via `RunStore::latest_run_for_ticket_kind` and runs `vdiff` with
  `current_dir` set to `Run.worktree` — `vdiff` detects the base branch
  itself. Reviewing *other people's* PRs (no local worktree) stays blocked on
  vdiff#2 and is out of scope.
- **`V` is a foreground, terminal-suspending launch**, following `L`
  (`view_logs`, `src/tui/event.rs`): leave the alternate screen, disable raw
  mode, block on the child, then restore and clear. `vdiff` is an interactive
  GUI/TUI that needs the real TTY, so the `LaneLauncher` watched-child seam
  (used by `w`/`b`) is wrong for it.
- **`F` is a watched-child launch** through the existing `LaneLauncher` seam,
  spawning `tm review fix <KEY>` — the same shape as `w`'s
  `["work", "run", <lane>, <key>]` and `b`'s `["pr", "watch", <key>]`.
  Rationale is identical: the subcommand does all preflight in the foreground
  (no run row until it succeeds) and then detaches, so a nonzero exit means
  preflight failed and its stderr is worth surfacing in the status line.

## `tm review fix <KEY>` — the dispatch subcommand

New top-level subcommand group `review` in `src/cli/review.rs`, following the
`pr`/`runs`/`work` module convention (plain functions over trait-object deps,
no direct I/O).

Steps:

1. Resolve the ticket's latest `kind = "lane"` run
   (`crate::cli::runs::resolve_run` / `latest_run_for_ticket_kind`). No run →
   error: the ticket has no worktree to fix in.
2. Read the captured review comments for that worktree. `vdiff` stores them
   under the worktree's git dir; `vdiff --export-comments` renders them. Shell
   out to `vdiff --export-comments` with `current_dir` set to `Run.worktree`
   through a `VdiffOps` trait seam (real + fake, following `GitOps`/`GhCli`),
   so the CLI layer stays testable without the real binary.
3. Empty export ("No comments.") → exit with a distinct code and a clear
   message rather than dispatching a no-op Claude run.
4. Build the prompt: the exported markdown wrapped in fix-pass instructions.
5. Dispatch a tracked, detached run **in the ticket's existing worktree and on
   its existing branch** — no new worktree, no new branch.

**Reuse boundary.** `prepare_run_lane` (`src/work/run.rs`) is *not* reusable
as-is: steps 3-6 always compute a worktree name, provision a worktree if
absent, and cut a brand-new branch. That is actively wrong for a fix pass on
an open PR's branch. Instead add a sibling `prepare_review_fix` beside it that
takes the already-resolved worktree/branch plus a prompt `String` and produces
a `PreparedRun`, then reuses the unchanged tail:

- `build_claude_invocation` (`src/work/claude.rs`) — takes `prompt: String`
  directly, no file coupling. **Always route through it**: it owns the
  `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`/`CLAUDECODE` env stripping that
  keeps runs off raw-API billing.
- `hooks::deploy_hooks` for the settings path, `RunStore::start_run` for the
  run id.
- `run_claude_and_finish` (fg) / `src/work/detach.rs`'s supervisor path
  (detached), both of which need only a `PreparedRun`.

Run rows use `kind = "review-fix"`, distinct from `"lane"` so the fix pass
never shadows the lane run in `latest_run_for_ticket_kind` lookups and shows
up separately in `tm runs`.

## Out of scope

- Posting the comments back to the GitHub PR (vdiff#7).
- Reviewing PRs with no local worktree (needs vdiff#2's `--pr`).
- A board badge for the fix-pass run. The `w` badge machinery is per-`kind`
  and could be extended later; the first cut relies on `tm runs` and the
  existing run-detail overlay.

## Manual verification

The `current_exe` argv path and the real `vdiff` launch are untestable
in-process, matching `RealLaunchHandle`'s and `RealDetachSpawner`'s existing
carve-outs. Verify by hand:

- `V` on a ticket with a lane run opens `vdiff` in that worktree; quitting it
  returns to an intact board.
- `V` on a ticket with no lane run sets a status-line message and launches
  nothing.
- `V` with no `vdiff` on `PATH` sets a status-line message rather than
  appearing to hang.
- `F` after capturing comments in `vdiff` starts a `review-fix` run visible in
  `tm runs`, working in the ticket's existing worktree on its existing branch.
- `F` with no captured comments sets a status-line message and creates no run
  row.
