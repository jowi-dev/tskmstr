# Board-launched lane runs (roadmap stream 5)

Status: planned 2026-08-07.

From the `tm board` TUI, launch the headless `tm work run <lane> <key>` flow
for the selected ticket with one keypress, and show the run's status on the
card — the execution counterpart to stream 4's `a` audit action.

## Decisions

- **Keybinding `w`** ("work") on the Board screen, ungated by column/status
  (same policy as `a`; the user knows which tickets are ready).
- **Lane selection**: a floating lane picker overlay (the board's overlay
  idiom, same shape as the assignee filter picker) listing
  `config.work.lanes` keys. If exactly one lane exists, skip the picker and
  launch directly. Zero lanes → status-line message, no picker. No
  `default_lane` config key — the picker is cheap and explicit.
- **Launch mechanism**: spawn `tm work run <lane> <key>` (via
  `std::env::current_exe()`) as a *watched child process* with piped
  stdout/stderr, and poll `Child::try_wait` from the existing event loop.
  Rationale: `prepare_run_lane` deliberately creates **no run row until
  preflight succeeds** (`src/work/run.rs` doc comment), so a fire-and-forget
  detach would swallow preflight failures (unknown lane, dirty worktree,
  missing prompt file). The launcher process itself already detaches the
  supervisor via `setsid` re-exec and exits within seconds, so the watched
  child resolves quickly: exit 0 → run row exists, badge polling takes over;
  nonzero → stderr (read after exit, truncated to one line) shown in the
  status line. The TUI stays single-threaded — no channels, no threads.
- **Badges**: a parallel `lane run` badge alongside the audit badge, reusing
  the same building blocks (`list_runs_filtered(Some("lane"))` per-ticket
  latest run, `is_awaiting_input`, the 8-tick ~2s poll cadence). Not merged
  into the audit poll — a ticket can legitimately carry both an audit
  session and a lane run, and two cheap local SQLite queries per 2s beat
  entangling the kinds.
- **One active lane run per ticket**: if the ticket's lane-run indicator is
  Starting/Running/Waiting, `w` sets a status message ("lane run already
  active for KEY") instead of opening the picker. Terminal runs (Done/
  Failed) do not block a relaunch.

## Indicator mapping

`RunIndicator { Starting, Running, Waiting, Done, Failed }` derived from the
ticket's latest `kind = "lane"` run plus the pending-launch set:

| Source state | Indicator |
| --- | --- |
| launch child in flight (no run row yet) | Starting |
| `Queued` / `Running`, last event not `await` | Running |
| `Running` + last event `await`, or `Blocked` | Waiting |
| `Review` / `Done` | Done |
| `Failed` | Failed |

Unlike audit badges there is no tmux-session liveness input; the indicator
comes purely from the run row (which outlives the process), so terminal
badges persist until a newer run replaces them. Styling mirrors
`audit_indicator_style`: label `run: <state>`, same per-state colors
(Waiting = bold `AWAITING_INPUT` yellow), no background.

## Implementation shape

**Pure side (`app.rs`, `keymap.rs`, `ui.rs`, `theme.rs`)**

- `App` gains: `show_lane_picker: bool`, `lane_picker_selected: usize`,
  `lane_names: Vec<String>` (threaded from config at construction; BTreeMap
  order), `pending_lane_launches: HashSet<String>` (ticket keys with a
  launcher child in flight), `lane_run_status: HashMap<String, RunIndicator>`.
- `Msg`: `LaneRunAction` (the `w` key), `LanePickerUp/Down/Select/Close`,
  `LaneRunLaunched { key }` → insert into pending, `LaneRunLaunchResult
  { key, result: Result<(), String> }` → remove from pending + status
  message, `LaneRunStatusLoaded(HashMap<String, RunIndicator>)`.
- `Cmd`: `LaunchLaneRun { lane: String, key: String }`,
  `LoadLaneRunStatus`.
- `tick()` emits `LoadLaneRunStatus` on the same 8-tick multiple as
  `LoadAuditStatus` on `Screen::Board`.
- `lane_run_indicator(pending: bool, run: Option<(RunStatus, bool)>) ->
  Option<RunIndicator>` as a pure table-tested function, mirroring
  `audit_indicator`.
- Picker rendering mirrors `draw_filter_picker`; data is synchronous (no
  lazy fetch, no error line). Esc/q close the picker without quitting.

**Executor side (`event.rs`, `main.rs`)**

- `Cmd::LaunchLaneRun` is intercepted in `run_cmds` (like `AttachAudit`)
  because it needs mutable launcher-registry state: the event loop owns a
  `Vec<PendingLaunch { key: String, child: std::process::Child }>`; each
  loop iteration `try_wait`s entries and feeds `Msg::LaneRunLaunchResult`
  into `update` for completed ones. Spawn failure itself becomes an
  immediate `LaneRunLaunchResult` error.
- Child spawn seam: a small trait (e.g. `LaneLauncher`) with a real impl
  wrapping `Command::new(current_exe())` and a fake for tests, following
  the `TmuxOps` pattern; `try_wait` polling is tested against the fake.
- `load_lane_run_status(deps)` mirrors `load_audit_status`: lenient on
  `store == None`, queries `list_runs_filtered(Some("lane"))`, first-seen
  per ticket wins, maps through `lane_run_indicator` (pending set is
  applied reducer-side, not here — the executor doesn't know it).
- `main.rs` threads `config.work.lanes.keys()` into the TUI (via `TuiDeps`
  or an `App` constructor argument, whichever the existing construction
  favors).

## Testing

TDD throughout, templated on the stream 4 suites: `lane_run_indicator`
table tests; `w` keymap tests (Board yes, other screens no); reducer tests
for picker open/navigate/select/close, single-lane fast path, zero-lane
message, active-run guard, pending insert/remove, status map replacement;
`load_lane_run_status` executor tests (store-less, running, waiting via
`await` event, terminal); fake-launcher spawn/`try_wait` tests; `ui.rs`
`cell_at` badge render + REVERSED-survives tests; `theme.rs` style-contract
test (distinct fg per variant, `bg == None`).

## Out of scope

- Run-detail overlay from the board (stream 6).
- Any gating on ticket status/column.
- Merging audit and lane badge polls into one query.
