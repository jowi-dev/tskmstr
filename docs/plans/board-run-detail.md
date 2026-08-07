# In-progress run visibility from the board (roadmap stream 6)

Status: implemented 2026-08-07 (commits `93e67c0`, `82846da`). Deviations
from the plan below: the `v view run` hint landed in the status bar's
`hint_for` (where `a`/`w`/`b` are documented) rather than the modal help
overlay, which never listed board-only keys; the Usage panel's title falls
back to a literal `"Usage"` when no model usage has loaded, keeping the
panel frame stable; the middle row is fixed at 8 rows. Manual
verification still open: a real `v` press against a live run on the
board.

For a ticket with an active (or recent) run: inspect it without leaving the
board. Per the roadmap decision, lane runs stay headless and visibility is
the watch screen's existing `RunDetail` floating window, opened on the board
for the selected ticket's latest run. This stream also redesigns that window
— it is a single vertically-stacked paragraph today — into grouped panels
with the theme's accent colors, which upgrades the watch screen for free
since the renderer is shared.

## Decisions

- **Keybinding `v`** ("view run") on the Board screen, ungated by column or
  status (same policy as `a`/`w`/`b`; the badges already tell the user which
  tickets have runs).
- **Latest run, any kind**: the overlay shows `latest_run_for_ticket(key)` —
  not just `kind = "lane"`. The window's title already carries the kind, and
  the most recent run is what the user is watching regardless of whether it
  was an audit, a lane run, or a bugbot cleanup. No run for the ticket →
  status-line message, overlay does not open.
- **Ticket-keyed load Cmd**: a new `Cmd::LoadTicketRunDetail { key: String }`
  handled by the board's `execute` (which, unlike `execute_watch`, must be
  lenient about `store == None`). It resolves ticket → latest run → the same
  `run_to_detail` pipeline the watch screen uses, emitting the existing
  `Msg::RunDetailLoaded` / `Msg::RunDetailFailed`. `Cmd::LoadRunDetail`
  (run-id-keyed) stays watch-only and stays in the board's unreachable arm.
  Refreshes re-resolve by ticket, so a newer run launched while the overlay
  is open replaces the content — that is the desired "watching the ticket"
  behavior, not a bug.
- **Refresh cadence**: while the overlay is open on the board, `tick()`
  emits `LoadTicketRunDetail` every 2nd tick (~500ms), matching the watch
  screen's detail cadence. The ~2s badge polling is untouched. This is a
  local SQLite read, never a network call.
- **Failure closes an empty overlay**: `Msg::RunDetailFailed` keeps setting
  the status line, and additionally closes the overlay when nothing has
  loaded yet (`run_detail == None`) so the user is never stuck on a
  "Loading..." window for a ticket with no runs. A refresh failure after a
  successful load leaves the loaded content up.
- **Key gating**: the existing run-detail keymap branch (scroll j/k, close
  Esc/q, refresh r) currently applies only on `Screen::Runs`; it widens to
  the Board screen. `back()` on the board closes the overlay before its
  quit behavior, mirroring the watch arm. While the overlay is open the
  board's selection cannot change (j/k are captured for scrolling), so the
  refresh can safely use the currently selected ticket.

## Overlay redesign

The current window is one `Paragraph` built as a flat `Vec<Line>`: ~10
label-value header lines, then Model usage, Agent usage, Checklist, and the
event timeline stacked vertically, one shared scroll offset. Replacement
layout (shared by board and watch), inside a `centered_rect(90, 80)` outer
block whose title keeps the `Run {id}: {ticket} ({kind})` form:

- **Header grid** (fixed height): the short label-value facts arranged as a
  three-column grid instead of a vertical list — identity (lane, kind,
  status with the `(waiting)` marker), timing (started, ended, turns), and
  cost/process (cost, pid, session). Labels `DIM`, values default; status
  value colored via `run_status_style`, kind via `kind_style`. The long
  paths (worktree, branch, pr, blocker) stay full-width lines under the
  grid — they don't fit a column.
- **Middle row** (fixed height, horizontal split): a `Usage` panel (model
  usage lines, then agent usage lines) beside a `Checklist` panel
  (`{done}/{total}` in the panel title, green `[x]` / dim `[ ]` items).
  Each panel is its own bordered block with a `SECTION_HEADER`-styled
  title. Content taller than the panel truncates with a dim `… +N more`
  final line. A section with no data shows a dim placeholder ("no usage
  yet" / "no checklist") rather than collapsing, so the frame is stable
  across refreshes. Tool counts render as a single dim line in the Usage
  panel footer.
- **Events panel** (remaining height, full width): the timeline, newest
  first, `j`/`k` scrolling applies here and only here. Timestamps stay
  `DIM`; event kinds get accent colors consistent with the badge families
  (`await` bold yellow like `AWAITING_INPUT`, `finish`/`done` green,
  `error`/`fail` red, others default).

Colors follow the `theme.rs` doctrine: fg-only accents, no backgrounds. Any
new styles land as `theme.rs` constants with the style-contract test
pattern (distinct fg where meaningful, `bg == None`).

The scroll-behavior change (whole-window scroll → events-only scroll) is
deliberate: header, usage, and checklist are bounded summaries; the
timeline is the unbounded part.

## Implementation shape

**Pure side (`app.rs`, `keymap.rs`)**

- No new `App` fields: `show_run_detail`, `run_detail`, and
  `run_detail_scroll` are reused; they were watch-only by convention, not
  by type.
- `Msg::ViewRunAction` (the `v` key, Board only) → sets `show_run_detail`,
  clears `run_detail`/scroll, emits `Cmd::LoadTicketRunDetail` for the
  selected ticket. No selected ticket → no-op.
- `tick()` Board arm additionally pushes `LoadTicketRunDetail` every 2nd
  tick while `show_run_detail`.
- `keymap.rs`: the `show_run_detail` gating branch drops its
  `screen == Runs` condition in favor of `Runs | Board`; `v` joins the
  board-only bindings; the help overlay gains a `v view run` line.
- `back()` Board arm closes the overlay first.

**Executor side (`event.rs`)**

- `Cmd::LoadTicketRunDetail` in `execute`: `store` absent → `RunDetailFailed
  ("run database unavailable")`; no run → `RunDetailFailed("no runs for
  KEY")`; otherwise `run_by… → events_for_run → run_to_detail` exactly as
  `load_run_detail` does (factor the shared tail into a helper both call).
  The watch dispatcher's unreachable arm gains the new Cmd.

**Rendering (`ui.rs`, `theme.rs`)**

- `draw_run_detail_window` rebuilt around a `Layout` as described above;
  helper functions per panel to keep arity in check. New theme constants
  for event-kind accents.

## Testing

TDD throughout, templated on the existing suites: keymap tests (`v` on
Board, not elsewhere; gating branch active on Board), reducer tests
(open/no-ticket/close ordering, tick refresh emission, failure-closes-empty
vs failure-keeps-loaded), executor tests against a tempdir `RunStore`
(store-less, no-runs, latest-of-several, kind-agnostic), and `TestBackend`
rendering tests for the new panel layout (existing run-detail render tests
updated to the new geometry, plus truncation `+N more`, placeholder text,
and events-only scroll).

## Out of scope

- Attach-to-run (rejected in the roadmap: headless `setsid` runs have no
  controlling terminal).
- A second hosting mode for lane runs.
- Run *selection* (history browsing) from the board — latest run only.
