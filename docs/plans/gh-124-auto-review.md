# GH-124: thatch auto-review session

`tm review auto <KEY>` runs a thatch review over a ticket's PR branch, writes
the confirmed findings into the worktree's vdiff comment store, and drives
`tm review fix` rounds until the review is clean or a round cap is hit.

## Shape

`tm review auto <KEY>` follows `tm pr watch`'s shape: the plain invocation
re-execs a detached `tm review auto <KEY> --foreground` child that logs to
`~/.local/state/tskmstr/auto-review/<key>.log`, and the foreground child
runs the loop synchronously. The loop has no tmux window of its own. Each
review round is a headless agent run in the ticket's existing worktree,
recorded as `kind = "auto-review"`. Each fix round is an unchanged
`tm review fix <KEY> --fg`, recorded as `kind = "review-fix"`.

One round:

1. Pick the prompt. Round 1 renders `{skill}` as `thatch-code-review`.
   Later rounds render it as `thatch-review-followup` and point `{archive}`
   at the previous round's archive file.
2. Run the review agent. It writes its confirmed findings to
   `{findings_file}`, a JSON array of
   `{path, start_line, end_line, severity, specialist, body}`, anchored to
   the head SHA.
3. tm appends the findings to `<git-dir>/vdiff/comments.json` (schema v1,
   see below).
4. With `publish = true`, tm runs `vdiff --publish-comments <pr>`.
5. If no finding is at or above `fix_threshold`, the loop ends **clean**.
   LOW findings stay in the store for the human to see in `V`.
6. If this was round `max_review_rounds`, the loop ends **capped**. The
   round's run row is finished as `review` and gets an `auto_review_capped`
   event, and the store is left in place for the human.
7. Otherwise tm runs `tm review fix <KEY> --fg`. Then it moves the whole
   store to `<git-dir>/vdiff/archive/<round>.json` and leaves an empty
   store behind.

Each round's run row gets an `auto_review_round` event whose detail is
`<round>/<max>`, plus `capped` on the last one when the cap was hit. The
overview row shows the newest auto-review run's detail next to its run
column.

## Writing `comments.json`

tm follows schema v1 exactly:

- It resolves the store through `git rev-parse --git-dir` in the worktree,
  which is safe in a linked worktree.
- It allocates `c<n>` ids one past the highest existing `n`.
- It stamps `created_at` as ISO-8601 UTC.
- It re-sorts by `(path, start_line)` and pretty-prints.
- It never touches existing comments. A human's `V` comments survive, and
  unknown fields on them round-trip.

The text is prefixed with provenance and severity, for example
`[auto-review · state-flow · MEDIUM] ...`, so human and agent comments are
distinguishable in `V` and in the fix prompt. No `author` field was added;
the prefix needs no schema change.

## Config: `[work.auto_review]`

| key                 | default            | meaning                                                |
|---------------------|--------------------|--------------------------------------------------------|
| `prompt`            | built-in           | prompt template (`{key}`, `{skill}`, `{findings_file}`, `{archive}`, `{round}`) |
| `prompt_file`       | unset              | file form of `prompt`; setting both is an error        |
| `fix_threshold`     | `"medium"`         | lowest severity that triggers a fix round              |
| `max_review_rounds` | `2`                | round cap                                              |
| `publish`           | `false`            | run `vdiff --publish-comments <pr>` after each round   |
| `auto`              | `false`            | chain from a `review-watch` that finishes              |

The severities, low to high, are `low`, `medium`, `high` and `blocking`
(thatch's synthesizer scale). An unknown severity on a finding counts as
`low`, so a malformed finding is shown to the human but never forces a fix.

## Chaining

With `auto = true`, a `tm pr watch` that ends because every bot finished
clean, or because its cleanup session launched, spawns a detached
`tm review auto <KEY>`. #119 ("Awaiting review") will key off the
auto-review run's outcome.

## Runner decision

`thatch-code-review` says "opencode only (requires sub-agent support)".
Its fan-out instructions only say to "dispatch a sub-agent using the Task
tool". Claude Code provides that tool natively (the Task/Agent tool), so
the skill runs under Claude Code unchanged. **Decision:** auto-review uses
the repo's configured runner, the same one `tm review fix` uses, and adds
no `runner` key. A runner without subagents falls back to the skill's
inline-synthesis path ("Alternatively, perform the synthesis yourself").
The skill's "opencode only" note should be relaxed upstream in thatch.

## Not in this change

- **vdiff schema amendment.** vdiff's `docs/comments-schema.md` says
  `vdiff.nvim` is the only writer. It needs a short amendment allowing
  additional writers that follow the same rules (ids, ordering,
  pretty-printing, never rewriting others' comments). That lives in the
  vdiff repo and is a follow-up there.
- **Usage admission and `max_lanes`.** Usage admission (#114) has not
  landed, so review rounds are not admission-gated yet.
- **The `max_fix_rounds` bound.** The cap's bound by #120's
  `max_fix_rounds` arrives with #120.
