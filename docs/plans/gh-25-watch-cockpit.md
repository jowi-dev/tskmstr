# GH-25: runs watch cockpit — attach, swap-back, view filters

Issue: https://github.com/jowi-dev/tskmstr/issues/25

Make `tm runs watch` actionable at high run volume: attach to the tmux
session behind the highlighted run card, get back to the watch screen in one
motion, and slice the wall of cards by kind or repo scope.

## Design decisions

### Attach (`s` on the highlighted card)

The board already solved attach mechanics (issue #6): `Cmd::AttachSession`
is intercepted outside the pure executor because it needs `&mut Terminal`
to suspend/restore the alternate screen around the blocking
`tmux attach-session` (or the immediate `switch-client` when inside tmux).
The watch loop gains the same interception in `run_watch_cmds`, reusing the
board's `attach_session` helper verbatim.

Resolving a card to a session name is the new part. Ticket sessions are
named `tm-<slug>-<key>` where `<slug>` is
`BackendIdentity::session_slug()` — but the watch screen lists runs from
*every* repo (`list_runs` is unscoped), so the invoking repo's slug is the
wrong one for another repo's run. Each run row already stores its
`scope` string (`github:<repo>` / `jira:<base_url>:<key>`, issue #10),
which carries exactly the inputs `session_slug()` uses. So:

- `BackendIdentity::session_slug_for_scope(scope)` parses a stored scope
  string back to the slug, guaranteed to match `session_slug()` for the
  identity that produced the scope.
- `RunSummary`/`RunCard` gain a `scope` field so the reducer can resolve
  the highlighted card without a store round-trip.
- Legacy rows (`scope = ""`, pre-#10) fall back to the invoking repo's own
  slug (threaded into `App::session_slug` like the board does); if that is
  also unavailable the status line explains and no attach is attempted.

Like the board's `s`, there is no pre-flight liveness check: a dead or
never-created session surfaces as the `tmux` error in the status line with
the screen fully restored — the issue's "dead-session degrade" criterion is
the same restore path as a successful detach.

### Swap-back (`@root_session`, the #19 / devtools#8 picker contract)

Issue #19 defined the contract: tm stamps a per-session tmux user option
`@root_session` naming the session to jump back to; the devtools session
picker owns the keybinding that reads it. For the watch cockpit the right
"back" target is *wherever the watch client is right now*, not the
project's root session — so the watch stamps at **attach time**: before
switching, it reads the current client's session name
(`tmux display-message -p '#{session_name}'`) and best-effort sets
`@root_session` on the target session. Outside tmux there is nothing to
stamp (and nothing to need it: detach returns to the watch naturally).

Stamping is best-effort per #19's acceptance criteria: a failed
`set-option` never blocks the attach.

### View filters (`f` kind, `F` scope)

Two independent cycling filters, both client-side over the already-loaded
cards (the store's `list_runs_filtered` exists, but filtering in
`App::runs_in_col` keeps the full list warm so cycling is instant and the
existing selection-preservation logic applies unchanged):

- `f` cycles the kind filter: all → each distinct kind present → all.
- `F` cycles the scope filter the same way.

All rendering, selection clamping, and column counts already flow through
`runs_in_col`, so filtering there is the single change point. Active
filters are shown persistently as a status-bar prefix (mirroring the
board's assignee-filter prefix).

## Out of scope

- Kill/close/confirmation on runs (issue #26 / devtools#15).
- Creation-time `@root_session` stamping on ticket sessions (#19's
  producer role) — this ticket stamps only on watch-screen attach.
- The picker keybinding itself (devtools#8).
