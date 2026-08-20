# One tmux session per ticket (issue #2), phases 1-5

Status: **complete**. Phases 1 and 2 implemented 2026-08-20 (commits
`df7c576`..`f547a63`); phase 3 implemented 2026-08-20 (commits
`1876a3f`..`467b62e`); phases 4 and 5 implemented 2026-08-20 (commits
`a1e98ed`..HEAD). This document covers what landed and where it deviates
from the issue's design.

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
  with phase 5's board work. **Done in phase 5**: it is now `window_live`.
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

## Phase 4 — viewer windows and session reconstruction

`src/work/viewer.rs` and `src/work/session.rs` are the two new modules;
`tm work session <KEY>` is the new command.

- **Viewer windows.** A `--headless` run now also gets a window in the
  ticket's session, running `tm runs logs <id> --follow`. `viewer_command`
  builds it; `launch_viewer_window` creates-or-appends with the same
  session-vs-window fork `launch_interactive_run` uses;
  `launch_and_report_viewer` wraps both and prints the outcome. Both headless
  call sites (`cli::work::run`, `cli::review::fix`) resolve their window from
  a `list_windows` snapshot *before* provisioning, then launch the viewer
  *after* `spawn_detached`.
- **Reconstruction.** `RunStore::runs_for_ticket` (the one query here that
  returns rows oldest-first, because window order is the action history)
  feeds `session::plan_session`, a pure function over the rows plus a
  `list_windows` snapshot. `reconstruct_session` executes the plan;
  `cli::work::session` is the I/O around them.

### Decisions and deviations

- **The viewer follows a run *id*, not `<KEY> --kind <kind>`.** The issue
  sketches the latter, but that resolves to "the latest run of that kind",
  which need not be the run the window was opened for: start a second `fix`
  pass while an older viewer is still open and the old window silently
  re-targets the new run's log. The launcher always knows the row id it just
  created, and `tm runs logs` takes a numeric id in the same position.
- **The viewer names the launching binary by `current_exe()`, not `tm`.** A
  `cargo run` build, or a nix store path not on `$PATH`, must still be able
  to launch a viewer that works.
- **A viewer launch failure is printed, never returned.** By the time the
  viewer can be created the supervisor is already running, so failing the
  command would report a broken run for a run that is fine — and invite the
  user to start it again. On a machine with no tmux server or no `tmux`
  binary (CI, cron), `tm work run --headless` must keep working exactly as
  before. The output line is `window    none — no log viewer (<err>); the run
  itself is unaffected`. This is the one place in the feature where a `tmux`
  error is swallowed, and it is deliberate; reconstruction, by contrast,
  *is* the tmux operation, so there it stays fatal.
- **Headless runs are now subject to the double-launch refusal too.** This is
  a side effect of needing a window name, and it is the right behavior: "a
  work run for this ticket is already live" is equally true whether the live
  window hosts `claude` or tails its log. The refusal still happens before
  provisioning, so it leaves no worktree, branch, or run row — there are
  tests for exactly that on both paths. Note this is a *behavior change* to
  `--headless`: previously two concurrent headless runs for one ticket were
  possible. With no tmux server the window list is empty, so CI and cron are
  unaffected.
- **`--fg` gets no window at all.** It has no log file to follow and its
  output is already going to the caller's terminal.
- **Only in-flight runs are reconstructed.** This is the phase-4 question
  the issue leaves open, and the reasoning is in `work::session`'s module
  docs. A headless run in flight reattaches its viewer and keeps going. An
  interactive run in flight has lost its `claude` with the pane, so its
  window comes back as a **plain shell rooted in the run's worktree**, and
  the command *prints* `claude --resume <session-id>` rather than running it
  — resuming starts billing and starts editing, and if the process did
  somehow survive elsewhere, resuming would drive one session twice.
- **A finished run gets no window, interactive or headless.** Reconstruction
  restores working state; history lives in the run table and the log files.
  A ticket with five finished runs would otherwise come back as five dead
  panes whose only content is a log `tm runs logs` and the board's `L`
  already open on demand — and a finished *interactive* run has no log at
  all (its durable artifact is its prompt file), so its window would be an
  empty shell claiming to be an action. This also keeps the command honest
  about what tmux is for: after reconstruction the window list is no longer
  the full action history, and cannot be — the DB is.
- **Reconstruction is idempotent and never attaches.** Every planned window
  is skipped if a live window for that action already exists (via
  `has_live_window`, so a live `fix-2` counts as `fix` being present), so
  running it against a healthy session is a no-op that touches tmux zero
  times beyond the snapshot. Not attaching matches `tm work restore`, and
  makes it scriptable across several tickets.
- **A ticket with no runs is an error, not an empty session.** There is
  nothing to rebuild *from*, and a bare `tm-<key>` holding one shell rooted
  nowhere in particular would be a worse answer than saying so
  (`WorkCliError::NoRunsForTicket`).
- **The rebuilt `shell` window is rooted at the *newest* run's worktree.** An
  audit run is rooted in `[work.audit].dir` (pre-worktree) and a lane run in
  the worktree, so newest-wins gives the most useful root available. This
  finally closes the phase-2 deviation about `shell`'s root for the
  reconstruction path.
- **Unknown run kinds fall back to the kind as the window name.**
  `action_window_for_kind` maps the four action kinds explicitly (`lane` →
  `work`, `review-fix` → `fix`, `bugbot-cleanup` → `bugbot`, `audit` →
  `audit`); anything else — a future kind, or `review-watch` — uses the kind
  verbatim rather than being dropped, so a new kind shows up in a rebuilt
  session immediately under a name that is at worst unlovely.

## Phase 5 — board integration and cleanup unification

- **`s` on the board attaches to the selected ticket's session.** It reuses
  `attach_session`'s suspend/restore dance unchanged. `Cmd::AttachAudit` was
  renamed `Cmd::AttachSession` and `attach_audit` to `attach_session`, since
  all three attaching keys (`a`, `b`, `s`) go through it and none of it was
  ever audit-specific. `Msg::SessionAttachResult` is now the result `Msg` for
  all three; `Msg::AuditActionResult` stays for *launch* outcomes, which are
  audit-specific. Documented in the README keybindings table and the board's
  key-hint footer.
- **`tm work clean <KEY>`** is one `kill-session` plus one worktree removal.
- **`AuditStatusEntry::has_session` → `window_live`.** Renamed; see below.

### Decisions and deviations

- **`s` was chosen because it is free and mnemonic.** `V`, `F`, `v`, `w`,
  `b`, `a`, `L`, `R`, `O`, `o`, `f`, `p`, `r`, `q`, `h`/`j`/`k`/`l` are all
  taken on the board; `s` for "session" was unbound on every screen.
- **`s` attaches unconditionally and never launches.** `a`'s
  attach-or-launch precedence is right for an action; `s` means "show me this
  ticket's session", and starting an action as a side effect of asking to
  look would be a surprise.
- **`s` consults no liveness map, by design.** `audit_status`/
  `cleanup_status` answer "is this *action's window* live", which is a
  different question from "does the session exist" — a session holding only
  a `work` window, or only `shell`, is perfectly attachable. Answering the
  session question properly would mean another polled board map; instead
  `tmux attach-session` answers it and its failure becomes the status line.
  Cheaper, and it cannot go stale between poll and keypress. The cost is
  that pressing `s` on an untouched ticket briefly suspends the board to
  show an attach failure; that is in the manual-verification list.
- **`tm work clean` finds its worktree through the run rows, with a guard.**
  The rows know where the worktree actually is, so nothing is re-derived from
  a lane name. But not every run's `worktree` *is* a worktree — an `audit`
  run records `[work.audit].dir`, the user's own checkout. Two conditions
  must both hold: the run's `lane` names a configured lane (giving a repo
  root for `git worktree remove`), and the path passes
  `naming::worktree_path_has_expected_parent`, the same guard `tm work
  new`/`remove` use. Condition two is what makes a checkout un-removable: it
  is not under the worktree root at all. There is a dedicated test for it,
  because this is the one operation in the feature that destroys data.
- **`tm work clean` is idempotent and tolerant.** A missing session, a
  worktree already gone, and a ticket with no qualifying worktree are all
  reported and exit zero. Cleanup that fails the second time you run it is
  worse than useless.
- **`tm work clean` is a new command, not a rename of `tm work remove`.**
  They key off different things — `remove` takes a lane/worktree name and is
  the lane-level operation, `clean` takes a ticket key and is the
  ticket-level one. `remove` is untouched.
- **The `has_session` rename was worth doing.** Phase 2 deferred it as
  "reaches into `app.rs`, `ui.rs`, `event.rs` and a large block of render
  tests for no behavior change", which was true but is the wrong trade: the
  field had come to mean the opposite of what it said. Session existence is
  now explicitly *not* a liveness signal (that is the whole point of phase
  1), so a field named `has_session` that actually reports "this action's
  window is live" invites exactly the wrong inference at every call site. It
  is a pure mechanical rename with the tests green throughout, and the struct
  docs now record what the old name was and why it went.

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
   and still finishes via the supervisor. (As of phase 3 it created no tmux
   window; phase 4 gives it a *viewer* window — see that phase's own steps.)
   Run
   `docs`-adjacent step 4-7 of `src/work/detach.rs`'s own manual test plan
   (close the terminal mid-run) to confirm the supervisor still survives.
9. **`--fg` still reports outcomes.** `tm work run <lane> --fg --max-turns 1`
   exits non-zero on a failed run and zero on a successful one.
10. **From the board.** `w` and `F` on a ticket produce windows in that
    ticket's session, and the launcher's status line still reports quickly
    rather than appearing to hang.

### Phase 4 manual verification

1. **Viewer window.** `tm work run <lane> PROJ-1 --headless`, then `tmux
   attach -t tm-proj-1`: a `work` window tailing the run's log, and `tmux
   list-panes -F '#{pane_start_command}'` shows `runs logs <id> --follow`,
   not `claude`.
2. **The viewer owns nothing.** With that run mid-flight, kill the `work`
   window (`C-b &`). Confirm the run still finishes — `tm runs show PROJ-1`
   reaches `done` — and the log file kept growing after the window died.
   Then repeat with `tmux kill-session -t tm-proj-1`: same outcome. This is
   the property the whole viewer design exists to protect.
3. **No tmux, no problem.** With no tmux server running at all (`tmux
   kill-server`), `tm work run <lane> PROJ-2 --headless` must still start the
   run and print a `window    none — no log viewer (...)` line, exiting zero.
4. **Headless double-launch refusal.** With a live headless `work` window,
   `tm work run <lane> PROJ-1 --headless` again must refuse, and `git
   worktree list` plus `tm runs show PROJ-1` must show nothing new.
5. **Reconstruction after a server death.** Start a headless run, then `tmux
   kill-server`. Run `tm work session PROJ-1`: the `work` viewer and `shell`
   come back, the viewer is following the *same* run's log (check the id),
   and the run still finishes on its own.
6. **Reconstruction of a live interactive run.** Start an interactive `tm
   work run`, `tmux kill-server`, then `tm work session PROJ-1`: the `work`
   window is a plain shell in the run's worktree, and the command printed a
   `resume:   claude --resume <id>` line. Confirm running that line by hand
   picks the conversation back up. Nothing should have resumed by itself.
7. **Reconstruction skips finished runs.** On a ticket with several finished
   runs and nothing in flight, `tm work session PROJ-1` creates a session
   with only a `shell` window.
8. **Idempotence.** Run `tm work session PROJ-1` twice in a row: the second
   prints "already has every window its runs call for" and changes nothing
   (`tmux list-windows` identical).
9. **`--fg` is unchanged.** `tm work run <lane> --fg --max-turns 1` creates
   no tmux window at all.

### Phase 5 manual verification

1. **Board attach.** `s` on a ticket with a live session: the board
   suspends, tmux takes over, `C-b d` returns to a cleanly redrawn board with
   `detached from tm-<key>` on the status line.
2. **`s` where `a` would launch.** On a ticket whose session holds a `work`
   window but no `audit` one, `s` must attach; `a` on the same ticket must
   launch an audit. This is the distinction the two keys exist for.
3. **`s` on an untouched ticket.** No session exists: confirm the status line
   reports the `tmux attach-session` failure and the board is fully usable
   afterwards (raw mode, alternate screen, redraw) — the accepted cost of not
   polling a session-existence map.
4. **Cleanup, the happy path.** After finishing a ticket with an audit, a
   work run, and a fix pass: `tm work clean PROJ-1` kills one session and
   removes one worktree. `tmux list-sessions` and `git worktree list` are
   both clean, in one command.
5. **Cleanup safety.** On a ticket whose *only* run is an audit, `tm work
   clean PROJ-1` must kill the session and report "No lane-run worktree
   recorded" — and `[work.audit].dir` must still exist. Verify the directory
   afterwards; this is the destructive-operation guard.
6. **Cleanup idempotence.** Run `tm work clean PROJ-1` twice: the second
   reports no session and an already-gone worktree, exiting zero.
7. **Badges still work after the rename.** `a` and `b` badges must light,
   clear, and attach exactly as before — the `window_live` rename is
   mechanical, but it touches every badge call site.
