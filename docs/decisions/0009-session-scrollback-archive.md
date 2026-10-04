# ADR-0009: Archive a Ticket Session's Scrollback Before Killing It

**Status:** Accepted
**Date:** 2026-10-04

## Problem

One tmux session per ticket (`tm-<scope>-<key>`) holds the ticket's action
history, and nothing ever killed it automatically. `tm merge` cleaned up the
worktree and local branch but left the session behind, so finished tickets
piled up as live sessions whose panes sat in deleted directories. GitHub
issue #78 asks `tm merge` to kill the session as part of its cleanup.

Killing the session throws away the only human-readable transcript tm has
for some windows. A headless run has a log file. A finished interactive run
has only its prompt file. `shell` and manual windows have nothing at all.
The kill therefore needs a durable copy of the scrollback first.

## Decision

1. **Plain files plus a `runs.db` table.** Each window's scrollback
   (`tmux capture-pane -p -S - -t <session>:<window>`) is written to
   `<state_dir>/archive/<scope-slug>/<KEY>/<YYYYMMDD-HHMMSS>-<window>.log`.
   Here `<state_dir>` is `~/.local/state/tskmstr/work`, the directory that
   already holds prompt files and run logs. Each file gets one
   `session_archives` row (scope, ticket, session, window, path,
   captured_at), added as migration 13. Rows are keyed by ticket, not run,
   because a window such as `shell` belongs to no run. The rows and files
   survive `tmux kill-server`. This keeps the feature tm-native: thatch or
   anything else can index the files later, but tm does not depend on them.

2. **Archive, then kill, in one shared helper.**
   `crate::work::session_gc::archive_and_kill` is the only code path. It
   runs in `tm merge`'s best-effort cleanup (single and batch, after the
   worktree and branch steps) and in `tm work clean`. It captures every
   window before writing anything, then writes the files, records the rows,
   and kills the session last.

3. **Conservative skips, never failures.** The session is left running,
   with a `warning:` line giving the manual `tmux kill-session` command, when:
   - a run hosted in it is live. "Hosted" and "live" mean exactly what
     ADR-0005's `live-run` tier means (`run_hosted_in` + `run_is_live`: a
     `running` row with a live or unrecorded pid, or any `hibernated` row).
   - it is the session the command is running in (`current_session_name`).
   - a capture, file write, or row insert fails. Nothing is killed
     unarchived.
   - no runs store is available to record the archive.
   - in a batch merge, it hosts the shared conflict window while some ticket
     is still handed back to it.

   None of these change the exit code. A ticket with no session is a quiet
   no-op.

4. **Read back with `tm runs scrollback <KEY> [--window <NAME>]`.** With no
   window it lists the archives. With a window it prints that window's
   newest archive file. This is a separate command rather than part of `tm
   runs logs`, because `logs` resolves one *run's* log file and archives
   belong to the ticket.

5. **No opt-out toggle.** No operator has asked to keep sessions after a
   merge, and the archive keeps the history the session used to hold. A
   `[work.merge]` key can be added if that changes.

## Consequences

- ADR-0005's classification is unchanged. This ADR reuses its `live-run`
  predicate rather than adding a new tier.
- The archive is best-effort history. `capture-pane` only sees what is
  still inside tmux's `history-limit`. For agent windows, the agent's own
  transcript (via the run's recorded `session_id`) stays authoritative.
- Archive files are never pruned. They are small text files, and pruning
  can be a follow-up if it ever matters.
- Sessions of tickets closed outside `tm merge` (merged on GitHub, closed by
  hand) are not swept. A future `tm work gc` can reuse `archive_and_kill`.
