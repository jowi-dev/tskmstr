# GH-120: import GitHub review threads into the vdiff comment store

`tm review import <KEY>` copies the unresolved human review threads on a
ticket's pull request into the worktree's vdiff comment store,
`<git-dir>/vdiff/comments.json`. From there they show up in `V` next to
local comments, and an unchanged `tm review fix` addresses them. There is
one comment store and one fix path, whatever the comment came from.

## What one import does

1. tm finds the ticket's latest lane run, its branch, and the open PR for
   that branch.
2. It fetches every review thread on the PR with all of its comments
   (`gh api graphql`, first 100 threads and first 50 comments each).
3. It skips:
   - resolved threads;
   - comments from a `review_bots` login, since `tm pr watch` handles
     those;
   - comments tm or vdiff posted (see "Echoes" below);
   - threads whose remaining comments are all in the dedup sidecar.
4. Each remaining thread becomes one comment-store entry, written with the
   same schema-v1 writer `tm review auto` uses (`work::comment_store`). The
   text is `[github · @author] <body>`, then each reply as
   `↳ @author: <body>`, then the thread URL.
5. It records every imported GitHub comment id in
   `<git-dir>/vdiff/imported-threads.json`, mapped to the local `c<n>` id
   and the thread's node id. A later import never imports that comment
   again.
6. If anything was imported, it adds a `review_import` event
   (`<n> imported`) to the lane run. The overview shows the newest such
   event on the ticket's row as `gh 2 imported`.

**Outdated threads** are kept, not dropped. When GitHub reports no line on
the PR head because the code moved, the entry is anchored at the thread's
original line, and its text says `(outdated: was L<n> @ <sha>)`.

**A new reply on an imported thread** imports the thread again, with every
human comment in it. The earlier entry was normally archived by the fix
pass it fed, so the new entry is the only copy the next fix pass sees.

## Echoes

With `[work.auto_review] publish = true`, `vdiff --publish-comments` posts
local comments to the PR as you, and the next import would read them back.
vdiff's own sidecar, `published-comments.json`, records only local ids,
never the GitHub ids the post created. So tm recognises an echo by its
body: vdiff posts a comment's `text` unchanged. A GitHub comment is skipped
when its body equals the text of any comment in the store or in any
archive under `vdiff/archive/`, or when it starts with `[auto-review ·` or
`[github ·`.

## `--fix`: the fix pass

`tm review import <KEY> --fix` imports, then runs `tm review fix` in the
foreground when the store holds any imported comment:

- **Cap.** `[dispatch] max_fix_rounds` (default 3) counts every
  `review-fix` run the ticket has had, whatever started it. At the cap, no
  fix pass starts. The lane run is marked `review` if it has finished, and
  a `capped` event is recorded. `tm review auto` checks the same cap before
  each of its fix rounds (`work::fix_cap`).
- **After a successful pass**, the whole store moves to
  `vdiff/archive/import-<n>.json`, where `<n>` is the ticket's fix-pass
  count. The name differs from `tm review auto`'s `<round>.json`, so
  neither overwrites the other. A `fix` event is recorded; a failed pass
  records `fix-failed` and leaves the store in place.
- **`[review] resolve_imported`** (default false): after the pass, if HEAD
  moved and matches its upstream (the pass pushed), tm replies
  `Addressed in <sha>.` on each imported thread it hasn't resolved yet and
  resolves it. If the pass didn't push, the threads stay open, because the
  commit wouldn't exist on GitHub yet.

## Who runs it

Without `--fix`, an import only adds comments and the badge; `F` stays one
keypress away. That is the behavior for a scope without autopilot.

An autopilot host runs `tm review import <KEY> --fix` for a scope with
autopilot on. Human comments always trigger a fix pass; there is no
severity threshold.

## Not in this change

- **Calling it on a schedule.** The overview poller (`src/overview_poll`)
  is still not wired into `tm overview`, and the per-scope autopilot flag
  arrives with #115 (PR #138, still open). The hosts from #116/#117 should
  call `tm review import` for each open autopilot PR, with `--fix` when
  autopilot is on.
- **`[dispatch] auto_merge_on_approve`.** Adding an approved PR to the
  overview's merge queue needs the overview to have PR review state, which
  arrives with the poller wiring. A config key that did nothing until then
  would mislead, so it is not registered yet.
- **GitHub ids in vdiff's publish sidecar.** If vdiff later records the
  GitHub comment ids it posts, the echo check can match ids instead of
  bodies.
