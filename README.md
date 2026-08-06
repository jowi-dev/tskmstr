# tskmstr

`tm` is a personal CLI + TUI for managing Jira tickets and GitHub pull
requests from the terminal. It links a PR to a Jira issue (title prefix +
Jira remote link), can spin up a ticket automatically when you open a PR
without one, and gives you a small `ratatui` board of your assigned Jira
tickets with vim-style keys.

## Requirements

- [Nix](https://nixos.org/) with flakes enabled, and (recommended)
  [direnv](https://direnv.net/)
- [`gh`](https://cli.github.com/), authenticated (`gh auth login`)
- A Jira Cloud API token for your Atlassian account

## Setup

```
direnv allow      # or: nix develop
```

Either drops you into a shell with `cargo`, `rustc`, `clippy`, `rustfmt`,
and `gh` on `PATH`.

Then bootstrap auth:

```
tm auth login
```

If `~/.config/tskmstr/config.toml` doesn't exist yet, `login` walks you
through creating it (Jira base URL, email, default project key). It then
prompts for a Jira API token — create one at
https://id.atlassian.com/manage-profile/security/api-tokens — validates it
against `GET /myself`, and stores it in the macOS keychain. On first
success it also fills in `default_assignee_account_id` in the config so
auto-created tickets get assigned to you.

Check things are working any time with:

```
tm auth status
```

## Commands

| Command | What it does |
|---|---|
| `tm auth login` | Bootstrap config if needed, validate a Jira API token, store it in the keychain |
| `tm auth status` | Report config, token source, and whether Jira auth + the default project resolve |
| `tm ticket <KEY>` | Associate Jira issue `<KEY>` (e.g. `PROJ-123`) with the PR open for the current branch |
| `tm ticket create [--title] [--body] [--status <STATUS>\|--no-transition]` | Create a new ticket in the configured default project. No PR required or touched |
| `tm ticket transition <KEY> <STATUS>` | Move ticket `<KEY>` to `<STATUS>`. Fails (non-zero exit) if no transition matches or the Jira API call fails |
| `tm ticket transition <KEY>` | List ticket `<KEY>`'s current status and available transitions |
| `tm ticket assign <KEY> <NAME>` | Assign ticket `<KEY>` to the assignable user matching `<NAME>` (exact displayName match, else an unambiguous substring match). Fails if no user or more than one matches |
| `tm ticket assign <KEY> --me` | Assign ticket `<KEY>` to you (cached account ID from `tm auth login`, or the Jira `myself` endpoint) |
| `tm ticket assign <KEY> --unassign` | Clear ticket `<KEY>`'s assignee |
| `tm ticket rank <KEY> --above <OTHER>` | Rank ticket `<KEY>` above `<OTHER>` in Jira's native backlog rank |
| `tm ticket rank <KEY> --below <OTHER>` | Rank ticket `<KEY>` below `<OTHER>` in Jira's native backlog rank |
| `tm ticket link <KEY> --blocks <OTHER>` | Create a `Blocks` link: `<KEY>` blocks `<OTHER>` |
| `tm ticket link <KEY> --blocked-by <OTHER>` | Create a `Blocks` link: `<KEY>` is blocked by `<OTHER>` |
| `tm ticket link <KEY>` | List `<KEY>`'s existing links, of any link type |
| `tm ticket unlink <KEY> <OTHER>` | Remove the `Blocks` link(s) between `<KEY>` and `<OTHER>`, either direction |
| `tm ticket update <KEY> --body <BODY>` | Replace ticket `<KEY>`'s description with `<BODY>` (GitHub-flavored Markdown, converted to Jira's ADF format) |
| `tm ticket audit <KEY>` | Print `<KEY>`'s summary, status, assignee, links, last recorded audit, and description — the material for an audit conversation |
| `tm ticket audit <KEY> --record <ready\|needs-work> [--notes]` | Record an audit verdict for `<KEY>` (offline; never touches Jira) |
| `tm ready` | List tickets assigned to you that are ready to pick up (To Do, no open blockers), in rank order |
| `tm ready <KEY>` | Check whether ticket `<KEY>` (any assignee, any status) is ready to pick up. Fails (non-zero exit) if it's blocked |
| `tm pr create [--title] [--body] [--base] [--auto-ticket]` | Open a PR for the current branch and associate a ticket |
| `tm pr status [--auto-ticket]` | Report the PR open for the current branch and its associated ticket |
| `tm` / `tm board` | Open the interactive TUI board of your assigned tickets |
| `tm runs` | List every recorded lane run in a table |
| `tm runs start --ticket <KEY> --lane <LANE> --worktree <PATH> [--branch] [--pid]` | Record the start of a lane run; prints the new run id |
| `tm runs finish <RUN_ID> --status <STATUS> [...] [--model-usage <JSON>]` | Record a run's terminal outcome (`done`/`failed`/`blocked`/`review`), optionally with the authoritative per-model token/cost breakdown |
| `tm runs event <RUN_ID> --kind <KIND> [--detail <JSON>]` | Append a telemetry event to a run and bump its heartbeat |
| `tm runs reap [--stale-after <MINS>]` | Mark abandoned runs (stale heartbeat, dead pid) as failed |
| `tm runs show <KEY> [--json]` | Print the latest run for a ticket, its latest checklist (if any), and its event timeline (newest first); `--json` prints one machine-readable JSON object instead (see below) |
| `tm runs resume <KEY>` | Print the session id of the latest run of a ticket, for `claude --resume` |
| `tm runs watch` | Live kanban board of lane runs, polling the local run db |

## `tm runs`

`tm runs` inspects and records autonomous lane runs. Ticket data still lives
in Jira and process lifecycle is owned by whatever supervises the runner
(systemd, launchd, a shell loop) — `tm` never spawns or supervises a runner
itself; see `docs/decisions/0001-run-state.md`. State lives in a local
SQLite database at `$XDG_DATA_HOME/tskmstr/runs.db` (falling back to
`~/.local/share/tskmstr/runs.db` when `XDG_DATA_HOME` isn't set), or wherever
`run_db_path` in `config.toml` points instead.

A runner (or its hooks) is expected to call `start`/`event`/`finish` around
its own lifecycle:

```
run_id=$(tm runs start --ticket PROJ-123 --lane backend --worktree /path/to/wt --pid $$)
tm runs event "$run_id" --kind tool_use --detail '{"file":"a.rs"}'
tm runs finish "$run_id" --status done --session-id sess-abc --cost-usd 1.23 --num-turns 7
```

`tm runs reap` (also run automatically on `tm runs watch` startup and every
~30s while it's open) marks a run `failed` if its status is `running`, its
last heartbeat is older than `--stale-after` minutes (default 10), and its
recorded pid is no longer alive — a crashed runner otherwise leaves a row
reading `running` forever.

### The `checklist` event convention

A `checklist` event reports a run's current todo list (Claude's own
checklist) so `tm runs watch` can show fine-grained progress rather than
just coarse status-column moves. `--detail` must be a full snapshot of the
whole checklist, not a diff — each new `checklist` event replaces the
previous one entirely, and `tm` always renders the newest one it can parse:

```
tm runs event abc123 --kind checklist --detail '{"items":[{"text":"write tests","done":true},{"text":"implement","done":false}]}'
```

`items` is a list of `{"text": string, "done": bool}` objects, in display
order. An event whose `detail` isn't valid JSON, or doesn't match this
shape, is skipped in favor of the next-newest `checklist` event that does
parse — a malformed emission never crashes the watch board, it just falls
back.

`tm runs watch` opens a full-screen kanban board of every run, one column
per status (Queued, Running, Blocked, Review, Done, Failed), refreshing from
the database every ~500ms. `h`/`l` move between columns, `j`/`k` move within
a column, `Enter` opens a floating window with the selected run's full
detail and event timeline (`j`/`k` scroll it, `Esc`/`q` closes it), `r`
refreshes immediately, and `q`/`Esc` quits when no detail window is open. A
`Running` card whose heartbeat is more than 10 minutes stale is marked with
a red `!`. A run's latest checklist (see above), if it has emitted one, is
rendered as a `[x]`/`[ ]` section above the event timeline in the detail
window, and as a terse `{done}/{total}` marker on its kanban card. `tm runs
show` renders the same checklist section, in the same place, for the
non-interactive one-shot view.

Both `tm runs show` and the watch detail window print the event timeline
newest first — the most recent event is always the first line, so you don't
have to scroll to see what a run just did.

### Per-model token/cost usage

Two complementary sources feed the "Model usage" breakdown shown by `tm runs
show` and the watch detail window:

- **Live snapshots, via `usage` events.** A runner's Stop hook can report
  running token counts the same way it reports a checklist: a full snapshot
  each time, latest-parseable-wins, garbage-tolerant.

  ```
  tm runs event abc123 --kind usage --detail '{"models":{"claude-fable-5":{"inputTokens":146,"outputTokens":58564,"cacheReadInputTokens":6535803,"cacheCreationInputTokens":203983}}}'
  ```

  `models` maps model name to `{inputTokens, outputTokens,
  cacheReadInputTokens, cacheCreationInputTokens}` (all default to `0` if
  omitted). These live events never carry `costUSD` — cost isn't known until
  the run finishes.

- **The authoritative breakdown, via `tm runs finish --model-usage`.** A
  runner wrapper passes the `claude -p` result's `modelUsage` map verbatim
  (a bare object, no `"models"` wrapper) when the run ends:

  ```
  tm runs finish "$run_id" --status done --model-usage '{"claude-fable-5":{"inputTokens":146,"outputTokens":58564,"cacheReadInputTokens":6535803,"cacheCreationInputTokens":203983,"costUSD":12.996}}'
  ```

  Unlike every other `finish` flag, `--model-usage` is validated eagerly:
  since `finish` is an explicit, one-shot command (not a best-effort hook),
  a value that isn't a JSON object is a hard error and nothing is stored.
  Omitting the flag leaves any previously recorded `model_usage` untouched,
  same as `finish`'s other optional flags.

`tm runs show` and the watch detail window render a `Model usage` section
using the authoritative column when the run has finished with one, falling
back to the latest `usage` event's live snapshot (labeled `Model usage
(live)`) while the run is still running; the section is omitted entirely
when neither is available. Cache tokens are always shown alongside
input/output — on a cache-heavy run they dominate the real cost, and hiding
them would misrepresent it:

```
Model usage
claude-fable-5   $13.00  out 58.6k, in 146, cache-read 6.5M, cache-write 204.0k
claude-sonnet-5  $2.81   out 30.7k, in 150, cache-read 5.4M, cache-write 191.0k
total            $15.81
```

The `total` line is only added when at least one model carries a `costUSD`
(i.e. the authoritative column, never a live snapshot). A `usage` event also
gets a compact one-line rendering in the event timeline itself, e.g. `fable-5
89.2k out / sonnet-5 30.7k out` (a leading `claude-` is stripped from each
model name for brevity).

### Friendly event rendering

`tool`, `checklist`, and `usage` events render as a short human-readable
summary instead of raw detail JSON, in both `tm runs show` and the watch
detail window:

- A `tool` event's `detail` is `{"tool": string, "summary"?: string,
  "agent"?: string}` — `summary` and `agent` are optional (older events may
  carry only `tool`). It renders as the tool name, prefixed with
  `[<agent>]` when `agent` is present, suffixed with ` — <summary>` when
  `summary` is present: `{"tool":"Bash","summary":"cargo test"}` renders as
  `Bash — cargo test`; `{"tool":"Read","summary":"src/main.rs","agent":
  "Explore"}` renders as `[Explore] Read — src/main.rs`.
- A `checklist` event renders as `{done}/{total} done`, using the same
  `items` shape as the checklist section above.
- A `usage` event renders as `{model} {out} out`, joined by ` / ` for
  multiple models, using the same `models` shape as the per-model usage
  section above (see "Per-model token/cost usage").

Any other event kind, or a `tool`/`checklist` event whose `detail` doesn't
match the shape above, falls back to the raw `{at}  {kind}  {detail}` line
used before this convention existed.

`tm runs show` also prints a `Tools: <name> ×<count>, ...` summary line —
counting every `tool` event by tool name, sorted by count descending then
name ascending — right after the run's header fields and before the
checklist section, omitted entirely when the run has emitted no `tool`
events. The watch detail window shows the same line in its header block.

### `tm runs show --json`

`tm runs show <KEY> --json` prints a single pretty-printed JSON object to
stdout instead of the rendering above, and nothing else (errors, e.g. an
unknown ticket, still go to stderr with a non-zero exit). It's meant for
scripts and LLM tooling that would otherwise have to parse the human-oriented
prose.

Two things differ deliberately from the human rendering:

- **`events` is oldest-first** (chronological order), not newest-first — the
  newest-first reversal `tm runs show` uses is a display-only convenience.
- **Each event's `detail` is the raw stored string verbatim**, never the
  friendly one-line rendering (see "Friendly event rendering" above).

Every optional field on `run` is present as `null` rather than omitted, so
the schema is stable regardless of which fields a given run happens to have
set. `checklist` and `model_usage` are `null` when the run has none;
`model_usage.source` is `"final"` when it came from the authoritative
`runs.model_usage` column, or `"live"` when it fell back to the latest
`usage` event snapshot (same distinction as the "Model usage" / "Model usage
(live)" section label). `tool_counts` is the same `(tool, count)` list
`tool_counts()` computes, just as objects instead of tuples.

```json
{
  "run": {
    "id": 12,
    "ticket": "PROJ-123",
    "lane": "backend",
    "status": "done",
    "session_id": "sess-abc",
    "worktree": "/path/to/wt",
    "branch": null,
    "pid": null,
    "transcript": null,
    "started_at": "2026-08-05 12:00:00",
    "heartbeat_at": null,
    "ended_at": "2026-08-05 12:04:00",
    "exit_code": null,
    "num_turns": 3,
    "cost_usd": 1.5,
    "blocker": null,
    "pr_url": "https://example.invalid/pr/1",
    "age_secs": 240
  },
  "checklist": {
    "done": 1,
    "total": 2,
    "items": [
      {"text": "write tests", "done": true},
      {"text": "implement", "done": false}
    ]
  },
  "model_usage": {
    "source": "final",
    "models": {
      "claude-fable-5": {
        "inputTokens": 146,
        "outputTokens": 58564,
        "cacheReadInputTokens": 6535803,
        "cacheCreationInputTokens": 203983,
        "costUSD": 12.996
      }
    }
  },
  "tool_counts": [{"tool": "Bash", "count": 2}],
  "events": [
    {"at": "2026-08-05 12:00:01", "kind": "tool", "detail": "{\"tool\":\"Bash\"}"},
    {"at": "2026-08-05 12:04:00", "kind": "stop", "detail": null}
  ]
}
```

`--auto-ticket` skips the "create a ticket?" prompt and just creates one
(in the configured default project, assigned to the configured default
assignee) when no key can be resolved from the PR's title, body, or
branch name.

## TUI keybindings

The board lays tickets out as columns, one per Jira status, ordered by
status category (new, then indeterminate, then done) and alphabetically by
status name within a category. Set `board_column_order` in `config.toml` to
override this ordering (see Configuration below). Drilling into a ticket or
its transitions opens a centered floating window on top of the board rather
than replacing it, so the board stays visible behind the detail and "Move
to" windows.

| Key | Action |
|---|---|
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `h` / `Left` | Move to the previous column (board only) |
| `l` / `Right` | Move to the next column (board only) |
| `Enter` | Drill into the selected ticket, or apply the selected transition |
| `Esc` / `q` | Go back a screen, or quit from the board |
| `r` | Refresh from Jira |
| `o` | Open the selected ticket in the browser |
| `f` | Open the assignee filter picker (board only) |
| `p` | Open the priority (stack-rank) view (board only) |
| `?` | Toggle the help overlay (any other key closes it; `q` still quits) |

### Filtering the board by assignee

By default the board shows only your own open tickets (`assignee =
currentUser()`), same as always. Pressing `f` on the board opens a floating
picker listing `Me`, `Unassigned`, `Everyone`, and every user assignable in
`default_project_key` (fetched from Jira the first time the picker opens and
cached for the rest of the session). `j`/`k` navigate the list, `Enter`
applies the highlighted filter and refetches the board, `Esc`/`q` closes the
picker without changing anything. The currently active filter is marked with
a leading `*`.

### Priority view (stack-ranking)

Pressing `p` on the board opens a full-screen "Priority" list of every open
ticket in `default_project_key`, in Jira's own backlog rank order, regardless
of assignee. This is a separate list from the board: leaving the priority
view with `Esc`/`q` shows the board exactly as it was, with no refetch.

| Key | Action |
|---|---|
| `j`/`k` / arrows | Move the cursor (or, if a ticket is grabbed, move the ticket itself) |
| `Enter` / `Space` | Grab the highlighted ticket, or drop it if already grabbed |
| `Esc` / `q` | Cancel the grab and restore the original order, or (if nothing is grabbed) return to the board |
| `r` | Refetch the priority list from Jira (inert while a ticket is grabbed) |
| `o` | Open the highlighted ticket in the browser |
| `?` | Toggle the help overlay |

Grabbing a row (`Enter`/`Space`) marks it with a leading `><` and lets `j`/`k`
reorder it in place, with the cursor following it. Dropping it (`Enter`/`Space`
again) re-ranks it in Jira: relative to its new neighbors, it's anchored
`Before` the ticket now below it, or `After` the ticket above it if it landed
at the bottom of the list. Dropping it back at its starting position sends no
request. A successful re-rank shows a confirmation like `Ranked PROJ-3 above
PROJ-7` in the status bar and keeps the already-reordered list; a failed one
shows the error and refetches the list so the display returns to Jira's
actual order. `q` never quits from the priority view — while a ticket is
grabbed it cancels the grab instead of leaving the screen, matching `Esc`.

Every filter other than `Me` scopes the query to `default_project_key`
(`Me` keeps the board's original, project-independent query). When a
non-`Me` filter is active, the status line shows `Filter: <name>` and each
ticket card also shows its assignee (`Assignee: <name>` or `Assignee:
Unassigned`).

## Configuration

Global config lives at `~/.config/tskmstr/config.toml`:

```toml
jira_base_url = "https://example.atlassian.net"
jira_email = "dev@example.com"
default_project_key = "PROJ"
default_assignee_account_id = "..."   # filled in by `tm auth login`
# status_on_pr = "In Review"          # optional, see below
# status_on_create = "In Progress"    # optional, see below
# review_bots = ["cursor[bot]"]       # optional, see below; this is the default
# board_column_order = ["To Do", "In Progress", "Code Review"]  # optional, see below
```

A repo can override any subset of these fields with a `.tskmstr.toml` in
its root; fields it doesn't set fall back to the global config.
`jira_base_url`, `jira_email`, and `default_project_key` must resolve
between the two files or `tm` refuses to run; `default_assignee_account_id`,
`status_on_pr`, `status_on_create`, `review_bots`, and `board_column_order`
are optional.

`review_bots` lists the GitHub bot logins (e.g. `cursor[bot]`) whose PR
review comment threads count as "bot findings" for `tm pr status` and
`tm ready`. Defaults to `["cursor[bot]"]` when unset in both global and
repo config.

`tm pr status` reports these as a `Bot findings:` line: `Bot findings: 2
unresolved (of 3)` when at least one bot-authored review thread exists, or
`Bot findings: none` when there are none. If the GitHub lookup itself fails,
`tm` prints `warning: could not check bot findings: ...` instead and
continues with the rest of `tm pr status` — this is informational and never
fails the command.

`status_on_pr` names the workflow status (e.g. `"In Review"`) to move a
ticket to when `tm pr create` gives it a ticket, whether that ticket was
just auto-created or already existed (e.g. one made by `tm ticket create`
and picked up via the branch/title/body). If the ticket already sits in
the target status, `tm pr create` leaves it alone and prints nothing extra.
`tm pr status --auto-ticket` also applies it to a freshly auto-created
ticket. Jira's create-issue API can't set status directly, so without this
setting an auto-created ticket is left in the workflow's initial status
(typically Backlog/To Do). When set, `tm` looks up the ticket's available
transitions and applies the first one whose target status matches,
case-insensitively; if none match, or the transition call itself fails,
`tm` prints a warning and continues — the ticket is still created/linked
either way. `tm ticket <KEY>` (plain association, no PR being created)
never changes an existing ticket's status.

`status_on_create` names the workflow status (e.g. `"In Progress"`) to
move a ticket to right after `tm ticket create` makes it. It's matched the
same way as `status_on_pr` (available transitions, case-insensitive,
warn-and-continue on no match or API failure) and is independent of it —
set one, both, or neither depending on your workflow.

`tm ticket create` takes two flags to control this per invocation:
`--status <STATUS>` transitions the new ticket to `<STATUS>` instead of
`status_on_create`, with the same case-insensitive, warn-and-continue
matching; `--no-transition` creates the ticket with no transition at all,
even if `status_on_create` is configured. They're mutually exclusive.

`tm ticket transition <KEY> <STATUS>` uses the same case-insensitive
matching rule (target status name, falling back to the transition's own
name), but unlike `status_on_pr`/`status_on_create` it's a hard failure
if nothing matches or the API call fails, since the command is an explicit
request rather than an automatic side effect of creating/linking a ticket.
If `<KEY>` is already in `<STATUS>`, it prints a message and exits 0
without calling the transition API. Omit `<STATUS>` to list the ticket's
current status and available transitions instead.

`board_column_order` lists workflow status names (case-insensitive match)
in the order the board's columns should appear, e.g. `["To Do", "In
Progress", "Code Review"]`. Listed statuses sort first, in that order;
any status not listed keeps the board's default ordering (status category,
then alphabetically by name) and sorts after every listed column. Useful
when two statuses share a category but Jira's alphabetical fallback puts
them in the wrong order for your workflow (e.g. "Code Review" sorting
before "In Progress"). Unset (the default) leaves the board's ordering
unchanged.

`tm ticket assign <KEY> <NAME>` resolves `<NAME>` against the assignable
users of `<KEY>`'s *own* project (not `default_project_key` — assigning a
ticket in another project still works): a case-insensitive exact match on
displayName wins first, falling back to a case-insensitive substring match,
but only when exactly one user matches it. Zero or more than one substring
match is a hard failure listing the candidates found (or every assignable
user in the project, when none matched at all). `--me` assigns to you,
preferring the `default_assignee_account_id` cached by `tm auth login` over
an extra Jira `myself` call; `--unassign` clears the assignee. Like `tm
ticket transition`, every failure here is a hard error (non-zero exit) —
this is an explicit command, not an automatic side effect.

### Ranking

`tm ticket rank <KEY> (--above|--below) <OTHER>` moves `<KEY>` to a new
position in Jira's native backlog rank (the `Rank` field Jira's own
backlog/board drag-and-drop uses), relative to `<OTHER>`. `<KEY>` is
verified to exist first, so a typo'd primary key gives the same friendly
"not found" error as every other `tm ticket` subcommand; a typo'd `<OTHER>`
surfaces from the rank request itself. Ranking `<KEY>` relative to itself
is rejected as a usage error. Like `tm ticket transition`/`tm ticket
assign`, this is an explicit command, so every failure is a hard error
(non-zero exit).

### Linking

`tm ticket link <KEY> (--blocks|--blocked-by) <OTHER>` creates a `Blocks`
link between two tickets: `--blocks <OTHER>` records that `<KEY>` blocks
`<OTHER>`, `--blocked-by <OTHER>` records that `<KEY>` is blocked by
`<OTHER>` — getting these backwards writes inverted dependency data, so the
direction is worth double-checking. `<KEY>` is verified to exist first, same
as `rank`; a typo'd `<OTHER>` surfaces from the link request itself. Linking
`<KEY>` to itself is rejected as a usage error. Giving neither flag lists
`<KEY>`'s existing links instead of creating one — a discovery view that
includes every link type, not just `Blocks`. Like `tm ticket rank`, creating
a link is an explicit command, so every failure is a hard error (non-zero
exit).

`tm ticket unlink <KEY> <OTHER>` is the inverse: it removes the `Blocks`
link(s) between the two tickets, direction-agnostic — you don't need to
remember which of `--blocks`/`--blocked-by` was used to create it. It only
ever touches `Blocks`-type links; if no `Blocks` link exists between
`<KEY>` and `<OTHER>`, it's a hard error, and if a link of some other type
(e.g. `Relates`) exists between the pair instead, the error names it so you
can see what's actually there. Unlinking `<KEY>` from itself is rejected as
a usage error, same as `link`.

### Auditing

`tm ticket audit <KEY>` prints the raw material for a pre-handoff audit
conversation — a ticket's summary, status, assignee, existing links (in the
same style as `tm ticket link <KEY>`'s bare listing, omitted entirely when
there are none), its last recorded audit verdict (or `Last audit: never`),
and its description. `tm` only owns the state/data side of an audit: the
actual human + Claude conversation that decides whether a ticket is ready
for an autonomous run is a Claude skill, not something `tm` runs itself.

`tm ticket audit <KEY> --record <ready|needs-work> [--notes "..."]` persists
that conversation's verdict, timestamped, to the same local SQLite database
`tm runs` uses (`$XDG_DATA_HOME/tskmstr/runs.db`, or wherever `run_db_path`
in `config.toml` points instead). Recording never touches Jira and works
fully offline; every past verdict is kept (no upsert), and the read mode
above always shows the most recent one. `--notes` only makes sense alongside
`--record`, so it requires it.

### Readiness

A ticket is "ready" when it has no open `Blocks`-type blockers: a `Blocks`
link where the linked issue isn't yet Done. A Done blocker doesn't count,
and a ticket that merely blocks something else is never "blocked" by that
relationship.

`tm ready` (no key) lists tickets assigned to you that are ready, further
restricted to your "To Do" tickets (something already In Progress has
already been picked up) and printed in Jira's native backlog rank order,
`KEY  Summary` per line. If any of your To Do tickets were excluded for
having an open blocker, a final `(N blocked tickets hidden)` line says so,
so a filtered list doesn't read as "this is everything assigned to you".

`tm ready <KEY>` checks one specific ticket, regardless of assignee or
status: it prints `KEY is ready (<status>)` on success, or `KEY is blocked
by:` followed by one line per open blocker on failure, exiting non-zero so
scripts can branch on it.

Both forms also carry a best-effort, advisory annotation of unresolved
GitHub bot review findings (see `review_bots` above and `Bot findings`
below) on a ready ticket's associated open pull request (matched by title,
the same way as `tm ticket`/`tm pr` association). `tm ready`'s list adds
`  [N unresolved bot findings]` to a matched ticket's line when `N > 0`;
`tm ready <KEY>` prints a `  note: N unresolved bot findings on PR #<number>`
line after the ready message. This is purely visible, never blocks
claimability, and never changes an exit code: if the GitHub lookup fails,
`tm` prints a single `warning: could not check bot findings: ...` line and
falls back to the unannotated output rather than failing the command.

The Jira API token itself is never stored in either config file — it
lives in the macOS keychain (service `tskmstr`, account `jira`), or comes
from the `JIRA_API_TOKEN` environment variable, which always takes
precedence over the keychain.

## How PR/ticket association works

`tm ticket <KEY>` and `tm pr create`/`tm pr status --auto-ticket` both
converge on the same association step: prefix the PR title with
`[KEY]` (idempotent — a no-op if it's already there) and post a Jira
remote link pointing at the PR.

Looking up an existing key on a PR checks, in order, stopping at the
first match:

1. A `[KEY-123]` prefix on the title, or a bare `KEY-123` token
   elsewhere in the title.
2. A `KEY-123` token in the body.
3. The branch name (e.g. `proj-123-fix` or `feature/proj-123-fix`),
   normalized to uppercase.

Title and body matches are trusted outright — someone wrote them on
purpose. A branch-derived key is only inferred, so it's validated with
`GET /issue/<key>` first; if Jira 404s, it's treated as no key found at
all rather than an error.

## Known limitations

- **macOS-only keychain.** Token storage shells out to `security`. On any
  other platform, set `JIRA_API_TOKEN` in the environment instead.
- **Single-page ticket search.** The board only fetches the first page of
  `POST /search/jql` results; `nextPageToken` pagination isn't
  implemented, so boards with a very large number of open tickets will be
  truncated.
- **PR title edits are last-write-wins.** `tm` doesn't read back the
  title before prefixing it, so a concurrent edit to the PR title can be
  clobbered.
- **`/search/jql` response shape is unverified against live Jira.** The
  parsing code follows Atlassian's documented contract (`issues` plus an
  optional `nextPageToken`), but hasn't yet been exercised against a real
  Jira Cloud instance — only against `httpmock` fixtures.

## Development

```
nix develop -c cargo test
nix develop -c cargo clippy
nix develop -c cargo fmt
```

TDD: write a failing test first, then the minimum code to pass it, then
refactor with tests green. Commits are checkpoints — one small working
change per commit, no `Co-Authored-By` trailers.

`nix build` produces `./result/bin/tm` from a fully pinned dependency
tree (`Cargo.lock` via `rustPlatform.buildRustPackage`), independent of
whatever toolchain is on `PATH`.
