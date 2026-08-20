# One tmux session per ticket (issue #2), phases 1-3

Status: phases 1 and 2 implemented 2026-08-20 (commits `df7c576`..`f547a63`);
phase 3 implemented 2026-08-20 (commits `1876a3f`..HEAD). Phases 4-5 of
issue #2 (viewer windows and `tm work session`, board attach/cleanup
unification) are **not** started; this document covers only what landed and
where it deviates from the issue's design.

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

## Phase 3 — interactive work and fix runs by default

`tm work run` and `tm review fix` now host `claude` in a window of the
ticket's `tm-<key>` session by default. `--headless` keeps the previous
`setsid` supervisor and `claude -p` path byte-for-byte.

- **`claude::RunMode`** (`Headless` | `Interactive`) forks
  `build_claude_invocation`. Interactive: prompt positional (`args[0]`), no
  `--output-format json`, no `--max-turns`, and the run id travels as
  `TSKMSTR_SESSION_RUN_ID`. Headless: unchanged, `TSKMSTR_RUN_ID`.
  `--model`/`--settings`/`--permission-mode` are common to both — `--settings`
  especially, since it is what deploys the SessionEnd hook that finishes an
  interactive run.
- **`work::interactive`** owns the tmux seam: `resolve_action_window` (pure,
  over a `list_windows` snapshot) picks the window name via
  `unique_window_name` and refuses `AlreadyRunning`;
  `launch_interactive_run` writes the prompt file and creates-or-appends the
  window; `tmux_command_line` renders the `$SHELL -c` string.
- **`cli::work::Dispatch`** resolves `--headless`/`--fg` into
  `Interactive` | `Headless` | `HeadlessForeground`, shared by both
  commands.
- Interactive runs are pre-registered with `pid = None`, exactly like an
  audit launch. Window existence, not a live pid, is the liveness signal.

### The env-var split, and how it is pinned

`hooks/tm-session-end.sh` exits 0 when `TSKMSTR_RUN_ID` is set, and
`register_session`/`finish_session` short-circuit on it: all three read it as
"a supervisor owns this run's lifecycle". Interactive runs have no
supervisor, so setting it would leave every one of them at `running` until
`tm runs reap` — with nothing else visibly wrong. Two tests pin it:
`claude::tests::run_mode_decides_which_run_id_env_var_claude_receives`
asserts the *whole* env set of both modes (so a swap, or setting both,
fails), and
`interactive::tests::launch_interactive_run_never_passes_the_supervisor_owned_run_id_var`
re-checks it at the tmux seam.

### Decisions and deviations

- **`--fg` implies `--headless`.** `--fg` has always meant "run
  synchronously and report the outcome in my exit code", and an interactive
  session cannot honor that: no result JSON, no bounded turn count, and a
  human may still be typing at it. Redefining `--fg` as "launch the window
  and attach" would silently change what existing scripts get and would
  duplicate phase 5's board-attach work. So `--fg` keeps its meaning and
  thereby *selects* the headless path; `--headless --fg` is the same request
  said twice and is deliberately not a conflict. Interactive is the default
  precisely because it is the one thing `--fg` cannot express.
- **Adoption needs an in-prompt instruction.** `register_session` reads
  `CLAUDE_CODE_SESSION_ID`, which only exists *inside* a Claude Code
  session, so nothing outside the session can adopt the pre-registered row.
  A board-launched audit gets this for free (its prompt is
  `/ticket-audit <KEY>`, and that skill runs `tm ticket audit` first); work
  and fix prompts have no such skill in front of them, so
  `interactive::registration_preamble` prepends `tm runs register --kind
  <kind> <TICKET>` to the prompt. This is the softest link in the chain — it
  depends on the session obeying its first instruction — but adoption is
  telemetry: if it is skipped the work still happens and the row is reaped
  stale rather than finished. The alternative considered and rejected was a
  new `SessionStart` hook: hooks receive `session_id` on stdin, not in the
  environment, so it would have needed its own out-of-band contract with
  `tm runs register`, plus two new env vars to carry kind and ticket.
- **Adoption's ticket match is now case-insensitive.** `tm runs register`
  uppercases its key while a lane run's row records the ticket exactly as
  typed (or the bare lane name for a ticket-less run), and a case-only
  mismatch is indistinguishable from success: the session starts a *second*
  run and leaves the first stuck at `running`. Ticket keys are
  case-insensitive everywhere else in the CLI, so the comparison in
  `register_session` follows.
- **The double-launch guard runs before provisioning, not at launch.**
  `launch_audit` can guard and then create its row from one snapshot; a work
  run cannot, because by the time it has a `PreparedRun` it has already
  provisioned a worktree, cut a branch, and started a row. So
  `resolve_action_window` takes the snapshot and makes the refusal, and the
  CLI calls it *first*. Still one snapshot per launch.
- **A second concurrent `fix` pass is refused, not suffixed.**
  `unique_window_name` gives repeat passes `fix-2`/`fix-3`, but only once the
  previous window is gone or dead. Two live `claude` sessions editing one
  worktree is never right, and `prepare_review_fix`'s dirty-worktree check
  would fail the second one anyway — better an explicit refusal that leaves
  no run row.
- **Interactive runs have no log file.** `log_path` stays `NULL`: nothing
  redirects an interactive pane's output, and inventing a file that only
  ever holds the tmux command's stderr would make `tm runs logs` lie. The
  prompt *is* persisted, at
  `<state_dir>/<wt_name>-<timestamp>.prompt.md`, which doubles as the
  record of what the run was asked to do. Phase 4's viewer windows are where
  the log-follow story belongs.
- **The prompt goes through a file for both actions, not just fix.** The
  issue only requires it for fix prompts (unbounded `vdiff` markdown), but
  lane prompt files have no length bound either, and one code path is better
  than a length heuristic. `tmux_command_line` reads it back with
  `"$(cat '<path>')"`, quoted so the shell cannot word-split it.
- **`env -u` is re-expressed inside the command string.** `tmux -e` can set
  variables but never unset them, and `ClaudeInvocation::env_remove`'s three
  variables are billing-safety critical (see that field's docs). The
  window's command is therefore `env -u ... claude ...`, which is what
  `work.ml` did originally.
- **`shell_quote` is applied to flag names too**, rather than trying to
  recognize which arguments are flags. `'--model' 'fable'` is valid shell
  and the uniform rule has no edge cases.
- **The `shell` window is worktree-rooted when a work run creates the
  session**, closing phase 2's deviation for the case where a worktree path
  is actually known. A session first created by an audit still has its
  `shell` rooted in `[work.audit].dir`; windows are append-only, so nothing
  moves.
- **The board needs no changes.** `w` and `F` already shell out to `tm work
  run <lane> <key>` / `tm review fix <key>`, so both became interactive with
  the CLI. Both still exit quickly (launch, then return), which is what the
  watched-child launcher expects.

## Out of scope (phases 4-5, untouched)

- Viewer windows for headless runs; `tm work session <KEY>` reconstruction.
- Board attach keybinding changes and `tm work clean` unification.

## Manual verification (open)

### Phases 1-2

1. `a` on a fresh board ticket: `tmux list-windows -t tm-<key>` shows `audit`
   then `shell`, with `audit` selected.
2. `a` again: attaches; badge stays `audit: running`.
3. Kill the `audit` window only, leaving `shell`: badge clears, and `a`
   appends a new `audit` window instead of erroring on a duplicate session.
4. With `remain-on-exit on`, let an audit exit: the badge must not report it
   as live, and `a` must launch `audit-2`.
5. `b` on a ticket with findings after an audit: `bugbot` window joins the
   same session.

### Phase 3

Nothing below can be unit-tested: it needs a real tmux server, a real
`claude`, and the SessionEnd hook actually firing.

1. **The whole point.** `tm work run <lane> PROJ-1`, then `tmux attach -t
   tm-proj-1`: a `work` window with `claude` mid-conversation, in the run's
   worktree. Type at it; it responds.
2. **Adoption.** Immediately after step 1, `tm runs show PROJ-1` — the `lane`
   run should have a `session_id` and a `pid` within a turn or two (the
   session's first action is `tm runs register`). If it never gets one, the
   preamble is being ignored and the run will only ever be reaped, not
   finished.
3. **The env-var split, end to end.** In the `work` window, `echo
   $TSKMSTR_RUN_ID` must print nothing and `echo $TSKMSTR_SESSION_RUN_ID`
   must print the run id. Then exit the session: `tm runs show PROJ-1` must
   reach `done` on its own, with a `model_usage` snapshot, without `tm runs
   reap`.
4. **Billing safety.** In the `work` window, `echo "$ANTHROPIC_API_KEY
   $CLAUDECODE"` must be empty even when they are set in the shell you
   launched from.
5. **Prompt file, not command string.** `tmux list-panes -t tm-proj-1 -F
   '#{pane_start_command}'` shows a `$(cat ...)` read, and the file it names
   contains the register preamble plus the lane prompt.
6. **Double-launch refusal.** With that window live, `tm work run <lane>
   PROJ-1` again must refuse without provisioning: no new run row, no new
   branch.
7. **Repeat fix passes.** Capture comments in `vdiff`, `tm review fix
   PROJ-1`, let it finish, capture more, run it again: `fix` then `fix-2`,
   both in `tm-proj-1`.
8. **`--headless` is untouched.** `tm work run <lane> PROJ-2 --headless`:
   still returns immediately with a `log` line, still writes to that log,
   still finishes via the supervisor, still creates no tmux window. Run
   `docs`-adjacent step 4-7 of `src/work/detach.rs`'s own manual test plan
   (close the terminal mid-run) to confirm the supervisor still survives.
9. **`--fg` still reports outcomes.** `tm work run <lane> --fg --max-turns 1`
   exits non-zero on a failed run and zero on a successful one.
10. **From the board.** `w` and `F` on a ticket produce windows in that
    ticket's session, and the launcher's status line still reports quickly
    rather than appearing to hang.
