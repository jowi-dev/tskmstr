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

## Workflow

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
