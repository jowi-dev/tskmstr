# GH-78: Kill a ticket's tmux session after `tm merge`, archiving it first

Decision record: `docs/decisions/0009-session-scrollback-archive.md`.

## Steps (each one commit, test-first)

1. **`TmuxOps::capture_pane`.** Runs `tmux capture-pane -p -S - -t
   <session>:<window>`. `FakeTmuxOps` records a `CapturePane` call and
   returns canned text, and `with_capture_pane_failure(window, err)` makes
   one window fail.
2. **`session_archives` table** (migration 13) with
   `RunStore::record_session_archive` and
   `RunStore::session_archives_for_ticket`. Scope filtering works like the
   other ticket-keyed tables.
3. **`work::session_gc::archive_and_kill`.** Checks, in order: the session
   exists, it is not the caller's own session, a store is present, no live
   run is hosted in it (ADR-0005's predicate), and its windows can be
   listed. It then captures every window, writes the files, records the
   rows, and kills the session. Every failure becomes a `warning:` line
   and the session is skipped.
4. **`tm merge`.** `finish_merge` calls the helper after the worktree and
   branch cleanup. In a batch, the shared conflict session's name comes
   back from `run_batch_conflict_session` and is passed as `kept_session`
   while any ticket is handed back.
5. **`tm work clean`.** Its unconditional `kill-session` is replaced by the
   helper. The worktree step still runs whatever the helper decides.
6. **`tm runs scrollback <KEY> [--window <NAME>]`.** The read command.
7. **Docs.** README sections ("One tmux session per ticket", "Finishing
   with a ticket", the new "Archived scrollback", "Merging a ticket's PR",
   batch merge, and the command table), ADR-0009, and this plan.

## Settled questions

- **Read command shape:** a new `tm runs scrollback`. Archives belong to the
  ticket rather than a run, so `tm runs logs` is the wrong home.
- **Opt-out toggle:** none (see ADR-0009 §5).
- **Clean when the session is skipped:** the worktree removal still runs.
  This matches `tm merge`, which removes the worktree before the session
  step.

## Out of scope

- Sweeping sessions of tickets closed outside `tm merge`. A follow-up
  `tm work gc` can reuse `archive_and_kill`.
- Pruning old archive files.
