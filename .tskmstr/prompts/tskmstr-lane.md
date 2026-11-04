# tskmstr work lane

Autonomous work session for a single tskmstr ticket. Do not scope-creep
beyond the named ticket.

## Start

1. Run `tm ready <KEY>` and branch on its exit code:
   - `0` (ready) -> proceed.
   - `3` (stackable) -> proceed, but build on the blocking ticket's PR
     branch rather than the lane's base branch.
   - `1` (blocked) -> stop. Do not write code. Record why in your final
     message (which ticket(s) are blocking, and their state) so a human can
     unblock it.
2. Work only `<KEY>`. If you discover unrelated bugs or cleanup along the
   way, note them (a follow-up ticket, a TODO) instead of fixing them here.
3. Before reading code, run thatch `memory_recall` for `<KEY>`, for the
   area it touches, and for this lane's branch name. Prior sessions may
   already have investigated it.

## Workflow

- Investigate with the `thatch-code-archaeology` skill, then plan and
  keep a checklist with the `thatch-coding-workflow` skill.
- Write work-in-progress notes (decisions, gotchas, status) as thatch
  memories scoped to this lane's branch.
- Test-driven: write a failing test before writing the implementation that
  makes it pass, for every behavior change.
- Keep commits small and focused, one logical change per commit, imperative
  mood, no AI attribution (no `Co-Authored-By: Claude` or similar) in any
  commit message.

## Before finishing

Run, and make green, in this order:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Do not report the ticket done while any of these fail.

## Open the pull request

Once the checks above are green, the ticket is not done until its PR is
open. Push the branch and open the PR with tskmstr, which also links the
ticket and applies the configured `status_on_pr` transition:

```
git push -u origin HEAD
tm pr create --title "<KEY>: <short summary>" --body "<what changed and why>"
```

- Draft the `--body` with the `thatch-pr-description` skill.
- If `tm ready` returned `3` (stackable), add `--base <blocking PR's branch>`.
- Do not end the session without a PR URL. If `tm pr create` fails, retry
  once, then report the exact error in your final message.
- Include the PR URL in your final message.

## Wrap up

Before the final message, record what this session learned with the
`thatch-session-reflection` skill.
