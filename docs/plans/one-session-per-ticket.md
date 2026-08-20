# One tmux session per ticket (issue #2), phases 1-2

Status: phases 1 and 2 implemented 2026-08-20 (commits `df7c576`..HEAD).
Phases 3-5 of issue #2 (interactive work/fix runs, viewer windows and
`tm work session`, board attach/cleanup unification) are **not** started;
this document covers only what landed and where it deviates from the issue's
design.

Consolidate every action taken against a ticket into a single long-lived tmux
session, `tm-<lowercased key>`, with one named window appended per action, and
move liveness — badges and the refuse-to-double-launch guard — from session
existence to live window names.

## Ground truth (verified 2026-08-20 against the tree at `de57141`)

- Audit and bugbot-cleanup each owned a session: `tm-audit-<key>` and
  `tm-bugbot-<key>`, created by `TmuxOps::new_session_with_command` with a
  single window (`audit` / `bugbot-cleanup`).
- Board badge liveness came from `tmux list-sessions`, prefix-matched back to
  a ticket key by `session_ticket_key` in `src/tui/event.rs`, consumed by
  `load_audit_status` and `load_cleanup_status`.
- The double-launch guard was `tmux.has_session(&session_name)` in both
  `launch_audit` and `launch_cleanup`.
- `TmuxOps` had no window-listing op and no way to run a command in a window
  of an existing session.

## Phase 1 — `TmuxOps::list_windows` + window-name liveness

- `TmuxOps::list_windows` shells out to
  `tmux list-windows -a -F '#{session_name}:#{window_name}:#{pane_dead}'`,
  parsed into `TmuxWindow { session, name, dead }` with
  `parse_list_sessions_output`'s existing tolerance: no server running yields
  an empty list, malformed rows are dropped. The session name is taken up to
  the *first* `:` and `pane_dead` from after the *last* one, so a window name
  containing a colon (tmux permits it; only session names forbid it) survives.
- `dead` is part of the signal, not decoration: with `remain-on-exit` set a
  window outlives its pane, and that is aftermath, not a running action.
- `live_action_tickets(windows, session_prefix, window_name)` in
  `src/tui/event.rs` replaced `session_ticket_key`, feeding both badge maps.
- The `AlreadyRunning` guard in `launch_audit`/`launch_cleanup` now asks
  `has_live_window(&windows, &session, ACTION)`. Both error variants gained a
  `window_name` field, and the board's status line reads
  `audit already running (tm-proj-1:audit)`.

### Deviation: the append path landed in phase 1, not phase 2

Moving the guard to window names without also teaching the launchers to append
into an existing session would have produced a broken intermediate: a session
that exists with no live `audit` window would have sent
`new_session_with_command` at an already-taken session name, which tmux
rejects. So `TmuxOps::new_window_with_command` and the
create-session-vs-append-window fork landed with the guard. Each commit is
still independently green; only the phase boundary moved.

`new_window_with_command` targets `<session>:{end}` with `-a` rather than
tmux's default insertion point (after the *current* window), so window order
is the action history regardless of which window happened to be selected.
`{end}` needs tmux ≥ 3.1; the `-e` flag the launchers already depend on needs
≥ 3.2, so this adds no new floor.

## Phase 2 — consolidate to `tm-<key>`

- `naming::ticket_session_name(key) -> tm-<lowercased key>` replaced
  `audit_session_name` and `cleanup_session_name`, both deleted. Lowercasing
  stays the only transformation, because `live_action_tickets` recovers the
  key by uppercasing the stripped suffix.
- The bugbot window is named `bugbot`, for the action, not `bugbot-cleanup`,
  for the run `kind` — it sits next to `audit`/`work`/`fix` in a window list
  read as history. The run `kind` is untouched.
- `unique_window_name(desired, existing)` is the pure naming rule: `desired`,
  else `desired-2`, `desired-3`, … resolved against the session's live window
  names, unit-tested against plain name lists the way
  `window_creation_sequence` is. `window_action(name)` is its inverse, mapping
  `fix-2` back to the action `fix`; only an all-digit suffix counts as a
  repeat marker, so multi-word names survive. `has_live_window` and
  `live_action_tickets` both match on the action, so a live `audit-2` blocks a
  new audit and lights the badge.
- Creating a ticket session also provisions a `shell` window, then reselects
  the action window (tmux's `new-window` steals focus).
- `LaunchOutcome` gained `window_name`, since with suffixing the session name
  no longer identifies where `claude` is running. Phase 5's window-targeted
  attach will want it.

### Deviations and decisions

- **`shell` is rooted where the session was created, not in the worktree.**
  The issue asks for "a `shell` window rooted in the worktree". Audit runs in
  `[work.audit].dir` *before* any worktree exists (per `board-audits.md`), and
  in phases 1-2 the audit is the only thing that creates a ticket session, so
  there is no worktree path to root it at. `-c` is per-window, so phase 3 —
  where `tm work run` becomes a window in this same session and does know the
  worktree — can add a worktree-rooted window without touching this one.
  Windows are append-only, so nothing needs to move.
- **`AuditStatusEntry::has_session` keeps its name** while now meaning "the
  action's window is live". Renaming it reaches into `app.rs`, `ui.rs`,
  `event.rs` and a large block of render tests for no behavior change; the
  field's doc comment states the current meaning instead. A rename belongs
  with phase 5's board work.
- **A dead window's name is not reused.** `unique_window_name` suffixes past
  it rather than reusing the name, keeping "one window per action attempt"
  literally true. It *does* reuse a suffix freed by a killed window, since a
  killed window leaves no trace to preserve.
- **One `list_windows` snapshot per launch** answers both "already running?"
  and "does the session exist yet?", so the two decisions cannot disagree
  about a session that appeared or vanished between two probes. This is why
  `session_window_names` exists rather than a second `has_session` call.
- **`session_path` is whatever created the session first**, as the issue
  anticipated. The only reader of that field is `tm work list`'s
  worktree/session kind column, which reported `session` for `tm-audit-<key>`
  and still reports `session` for `tm-<key>`: no change.
- **Old `tm-audit-*` / `tm-bugbot-*` sessions are not migrated.** Nothing
  reads them any more; they linger until killed, and their windows cannot be
  mistaken for a ticket's (`tm-audit-proj-1` maps to the nonexistent key
  `AUDIT-PROJ-1`). Kill them by hand once.

## Out of scope (phases 3-5, untouched)

- Interactive `tm work run` / `tm review fix` and the
  `TSKMSTR_RUN_ID` → `TSKMSTR_SESSION_RUN_ID` flip.
- Viewer windows for headless runs; `tm work session <KEY>` reconstruction.
- Board attach keybinding changes and `tm work clean` unification.

## Manual verification (open)

1. `a` on a fresh board ticket: `tmux list-windows -t tm-<key>` shows `audit`
   then `shell`, with `audit` selected.
2. `a` again: attaches; badge stays `audit: running`.
3. Kill the `audit` window only, leaving `shell`: badge clears, and `a`
   appends a new `audit` window instead of erroring on a duplicate session.
4. With `remain-on-exit on`, let an audit exit: the badge must not report it
   as live, and `a` must launch `audit-2`.
5. `b` on a ticket with findings after an audit: `bugbot` window joins the
   same session.
