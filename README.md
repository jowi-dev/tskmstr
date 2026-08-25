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

To onboard a repo in one step, run the interactive wizard from its root:

```
tm init
```

It asks for the ticket backend (defaulting to `github` when an `origin`
remote is detected, with the slug pre-filled), writes the repo-local
`.tskmstr.toml`, scaffolds a work lane (`repo = "."`, an explicit
`base_branch`, and an optional starter prompt file), creates the GitHub
backend's `tm:status/*` labels, optionally fills in `[work.audit]` /
`[work.review_watch]`, and offers `tm work hooks install --user` when the
session hooks are absent — everything `tm board` and the board's `w`/`a`
keys need. Re-running it is a review pass: every question offers the
current value as its default, and nothing is overwritten without
confirmation. `tm init --yes` accepts every default for scripted setup.

Under the Jira backend, `tm init` hands off to `tm auth login` when no
API token resolves; you can also bootstrap auth directly:

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
| `tm init [--yes]` | Interactive wizard onboarding the current repo: backend choice, `.tskmstr.toml`, a work lane, status labels, and session assets, so `tm board` works immediately after; `--yes` accepts every default |
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
| `tm ticket comment [<KEY>] [--body <TEXT>] [--pr]` | Post a comment to ticket `<KEY>` (inferred from the current branch's PR if omitted); `--pr` also posts it to the current branch's PR |
| `tm ticket audit <KEY>` | Print `<KEY>`'s summary, status, assignee, links, last recorded audit (plus its usage, if any), and description — the material for an audit conversation |
| `tm ticket audit <KEY> --record <ready\|needs-work> [--notes]` | Record an audit verdict for `<KEY>` (offline; never touches Jira) |
| `tm ticket retro <KEY> --clean` | Record that `<KEY>` shipped clean, with no known production defect (offline; never touches Jira) |
| `tm ticket retro <KEY> --defect --severity <minor\|major\|critical> [--note]` | Record that `<KEY>` shipped a production defect, with its severity |
| `tm ticket search <TEXT>` | Search the configured default project for open (non-`Done`) tickets matching `<TEXT>`, most recently updated first |
| `tm ready` | List tickets assigned to you that are ready to pick up (To Do, no open blockers), in rank order, plus any blocked ticket that's stackable |
| `tm ready <KEY>` | Check whether ticket `<KEY>` (any assignee, any status) is ready, stackable, or blocked. Exits `0` (ready), `3` (stackable), or `1` (blocked/error) |
| `tm pr create [--title] [--body] [--base] [--auto-ticket]` | Open a PR for the current branch and associate a ticket |
| `tm pr status [--auto-ticket]` | Report the PR open for the current branch and its associated ticket |
| `tm pr watch <KEY> [--foreground]` | Poll `<KEY>`'s open PR until its review bots have posted (or the PR merges/closes), detached by default; `--foreground` runs the poll loop in this process |
| `tm` / `tm board` | Open the interactive TUI board of your assigned tickets |
| `tm runs [--kind <KIND>]` | List every recorded run in a table, optionally restricted to one `kind` (`lane`, `audit`, `create`, `review-fix`, `review-watch`, `bugbot-cleanup`) |
| `tm runs --by-outcome [--kind <KIND>]` | Print cost totals grouped by bot-findings outcome (not measured / clean / findings) instead of listing individual runs |
| `tm runs --by-retro [--kind <KIND>]` | Print cost totals grouped by shipped-ticket retro verdict (clean / defect, from `tm ticket retro`) instead of listing individual runs; tickets with a recorded verdict but no run are counted separately rather than as a `$0` run |
| `tm runs start --ticket <KEY> --lane <LANE> --worktree <PATH> [--branch] [--pid] [--kind <KIND>]` | Record the start of a run (`--kind` defaults to `lane`); prints the new run id |
| `tm runs finish <RUN_ID> --status <STATUS> [...] [--model-usage <JSON>] [--findings-count <N>]` | Record a run's terminal outcome (`done`/`failed`/`blocked`/`review`/`interrupted`), optionally with the authoritative per-model token/cost breakdown and/or the number of unresolved bot review findings (`0` for measured-clean; omit to leave it unmeasured) |
| `tm runs event <RUN_ID> --kind <KIND> [--detail <JSON>]` | Append a telemetry event to a run and bump its heartbeat |
| `tm runs reap [--stale-after <MINS>]` | Mark abandoned runs (stale heartbeat, dead pid) as failed |
| `tm runs show <KEY> [--kind <KIND>] [--json]` | Print the latest run for a ticket (optionally restricted to one `kind`), its latest checklist (if any), and its event timeline (newest first); `--json` prints one machine-readable JSON object instead (see below) |
| `tm runs resume <KEY>` | Print the session id of the latest run of a ticket, for `claude --resume`; warns on stderr (without blocking) if that run's status is terminal, pointing at `tm runs reopen` |
| `tm runs reopen <ticket-or-run-id> [--kind <KIND>] [--to queued\|running\|blocked]` | Reopen a finished run (status `done`/`failed`/`interrupted`) so it's actionable again — clears `ended_at`/`pid`/`heartbeat_at` and moves `status` to `--to` (default `queued`); `--to blocked` is for repairing a run mislabeled `done` when it was actually blocked |
| `tm runs register --kind <KIND> <KEY>` | Adopt (or start) a run for `<KEY>` under `<KIND>`, for a skill invoked directly rather than through `tm ticket audit`/`create` (no-op if `CLAUDE_CODE_SESSION_ID` is unset) |
| `tm runs watch` | Live kanban board of lane runs, polling the local run db |
| `tm runs logs <ticket-or-run-id> [--kind <KIND>] [--tail <N>] [--follow]` | Print (`--tail`, default 200 lines) or follow (`--follow`, like `tail -f`) a run's detached-process log file |
| `tm work new <name> [branch] [--from base]` | Provision a lane's worktree (if missing) and start/attach its tmux session |
| `tm work remove <name>` | Kill the worktree's tmux session (if any) and remove the worktree |
| `tm work list` | List every current tmux session with a worktree/session kind column |
| `tm work restore` | Recreate tmux sessions for every existing worktree that doesn't already have one running |
| `tm work session <KEY>` | Rebuild `<KEY>`'s `tm-<key>` tmux session and its windows from the ticket's recorded runs — after a reboot, a `tmux kill-server`, or an accidental `kill-session`. Only runs still in flight come back; never attaches, and does nothing to a healthy session. See "Per-ticket tmux sessions" below |
| `tm work clean <KEY>` | Finish with `<KEY>`: one `kill-session` on `tm-<key>` plus one worktree removal. Only a path under the configured worktree root is ever removed, so an audit run's `[work.audit].dir` is never touched |
| `tm work start [<dir>]` | Attach to (or create) the tmux session for `<dir>`, defaulting to `cwd` |
| `tm work run <lane> [ticket] [--from] [--model] [--max-turns] [--permission-mode] [--prompt] [--headless] [--fg]` | Provision (if needed) and run one Claude Code session for a configured lane, tracked in `tm runs`; interactive in a `work` window of the ticket's `tm-<key>` tmux session by default, `--headless` runs the autonomous `claude -p` pass under a detached supervisor, `--fg` runs that headless pass synchronously |
| `tm work hooks install --user [--dry-run]` | Install tm's `Stop`/`SubagentStop`/`SessionEnd` telemetry hooks into your own Claude Code settings, so interactive `tm ticket audit`/`tm ticket create` sessions get usage tracking too (see below) |
| `tm review fix <KEY> [--headless] [--fg]` | Dispatch a Claude fix pass over the `vdiff` review comments captured for `<KEY>`'s lane-run worktree, tracked as a `review-fix` run on that same worktree and branch; interactive in a `fix` window of the ticket's `tm-<key>` session by default (a repeat pass becomes `fix-2`), `--headless` uses the detached supervisor, `--fg` runs synchronously. Exits `0` (dispatched), `3` (no comments captured, no run created), or `1` (error) |

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
per status (Queued, Running, Blocked, Review, Done, Failed, Interrupted),
refreshing from the database every ~500ms. `h`/`l` move between columns, `j`/`k` move within
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

### `Interrupted` vs. `Failed`, and recovering a run

`Failed` means the agent ran and reported failure (a non-zero `claude` exit,
or an explicit `is_error: true` in its result). `Interrupted` means the run's
outcome couldn't be determined at all — its result JSON didn't parse, or it
parsed but never got an `is_error` field one way or the other. That absent
field is exactly the shape a mid-run event like a usage-limit forced model
switch can leave behind: the turn ends gracefully (`claude` exits 0) but
never records whether it succeeded, and treating that silence as success is
what used to mark a still-in-flight ticket `done`.

If a run lands on `Interrupted` (or was wrongly marked `done`/`failed`) and
you want to pick it back up, reopen it first:

```
tm runs reopen PROJ-123          # or a numeric run id; --to defaults to queued
tm runs resume PROJ-123          # claude --resume <the printed session id>
```

`tm runs reopen` only accepts a run whose status is already terminal
(`done`/`failed`/`interrupted`) — reopening a `queued`/`running`/`blocked`/
`review` run is a hard error, since those aren't finished yet. It defaults to
`--to queued` rather than `--to running`: reopening straight to `running`
would leave a pid-less row that `tm runs reap` could immediately re-mark
failed once its (inherited, already-old) `started_at` looks stale.
`tm runs resume` still works on a terminal run without reopening it first —
it just warns on stderr and points here, without blocking.

`--to blocked` is a repair target rather than a "make it actionable again"
one: use it when a run's `done`/`failed`/`interrupted` status is simply
wrong and the run is actually `blocked` (e.g. a run a bug mislabeled — see
below), not when you want to resume work on it.

#### Precedence when a run finishes itself

A headless lane run's in-session agent can finish its own run mid-session
with a deliberate status, e.g. `tm runs finish 18 --status blocked --blocker
"waiting on PROJ-408"`, before `claude -p` exits. When `tm work run`'s
supervisor then observes that exit, its own inferred status only wins over
that deliberate one when it's an unambiguous crash signal — a non-zero exit,
or an explicit `is_error: true` in the result JSON — which always marks the
run `Failed` regardless of what the session already set. An exit-0
`Done`/`Interrupted` classification, by contrast, defers to any status the
session already recorded: it fills in the telemetry only the supervisor
can see (turns, cost, session id, transcript, PR URL) without touching
`status`. This matters because a run finished twice used to always let the
second (supervisor's) write win outright, silently overwriting a
deliberate `blocked` status back to `done`.

### Log files

Every detached run (`tm pr watch <KEY>`, and `tm work run <lane> ...
--headless`) redirects its stdout/stderr to a log file, recorded on the run
row as `log_path`:

- `tm pr watch` (`kind = review-watch`): `<home>/.local/state/tskmstr/review-watch/<lowercased key>.log`
- `tm work run --headless` (`kind = lane`): `<state_dir>/<worktree name>-<timestamp>.log`, printed as the `log` line when the run starts

Interactive runs (`tm work run` and `tm review fix` without `--headless`)
have no log file: their output is the tmux window's scrollback, and their
prompt is kept at `<state_dir>/<worktree name>-<timestamp>.prompt.md`. `tm
runs show` remains the durable record of what happened.

For `tm work run`, that file can already have content in it by the time the
detached process's stdio is redirected there: the blocked-ticket
branch-off decision (stacking this run's branch on a blocking ticket's PR
instead of the normal base — see [The stack decision](#the-stack-decision)
below, shared verbatim with `tm ready`) resolves before a run row even
exists, and any warning or error it produces is appended to this same log
file as soon as its path is known — not just printed to the invoking
terminal, which for a detached run is gone the moment its launching shell
closes. A permanent `gh` failure during that resolution (see below) fails
the run outright, and that failure is logged here too before it propagates.

`tm runs logs <ticket-or-run-id> [--kind <KIND>] [--tail <N>] [--follow]`
resolves a run the same way `tm runs reopen` does (numeric row id, or ticket
key optionally disambiguated with `--kind`) and prints its log:

```
tm runs logs AX-408                    # last 200 lines
tm runs logs AX-408 --kind review-watch --tail 500
tm runs logs AX-408 --follow           # like tail -f
```

Runs started before the `log_path` column existed have it as `NULL`; for
`kind = review-watch` specifically, `tm runs logs` falls back to the same
by-convention path above, so even a run predating this feature stays
viewable. Other kinds have no derivable fallback (a lane run's filename also
bears a worktree name and timestamp that don't survive anywhere recoverable)
and report a distinct "no recorded log path" error in that case. A
zero-byte log file (the poll loop having emitted nothing) is reported
distinctly too, pointing at `tm runs show <KEY>` for the recorded event
timeline. On the board, pressing `L` on a ticket opens its latest run's log
in `less` the same way `a`/`b` attach to a live tmux session.

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

#### Estimated cost for interactive sessions

Lane runs (`tm work run`) get their cost straight from `claude -p`'s
authoritative `modelUsage.costUSD` — no guessing involved. Interactive
sessions (`tm ticket audit`, `tm ticket create`) have no equivalent: a Claude
Code transcript's `assistant` turns carry only token counts, never a dollar
figure, so `tm` derives an approximate cost from a small per-model price
table (`src/runs/pricing.rs`) instead. That table is a hand-maintained
estimate, not a vendor price list, and needs manual updates if pricing
changes — see its module docs for how the current rates were derived.

Every place a cost like this appears, it is marked so it can never be
mistaken for an authoritative figure:

- In `tm runs show`'s `cost` header line and `Model usage` section, an
  estimated cost gets a leading `~` (`cost ~$3.28 / 12 turns`, `Model usage
  (estimated)`), including any `total` line that sums in at least one
  estimated entry.
- In `tm runs show --json`, each model's object in `models` carries
  `"estimated": true` when its `costUSD` was derived rather than reported,
  and the top-level `model_usage.source` is `"estimated"` (a third value
  alongside `"final"` and `"live"`) whenever any model in the authoritative
  column is.

This estimation happens automatically when an audit/create session's run
finishes — either explicitly (`tm runs finish --model-usage <JSON>` fills in
any model missing a `costUSD`) or implicitly (recording an audit verdict
rolls up whatever `usage` events were recorded during the conversation the
same way). It never overwrites a `costUSD` that's already present, and never
estimates a model absent from the price table.

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
`runs.model_usage` column and every model's cost was reported verbatim,
`"estimated"` when it came from that same column but at least one model's
`costUSD` was derived from `src/runs/pricing.rs`'s price table (see "Estimated
cost for interactive sessions" above), or `"live"` when it fell back to the
latest `usage` event snapshot (same three-way distinction as the "Model
usage" / "Model usage (estimated)" / "Model usage (live)" section label). A
model entry itself carries `"estimated": true` when its own `costUSD` was
derived rather than reported. `tool_counts` is the same `(tool, count)` list
`tool_counts()` computes, just as objects instead of tuples.

```json
{
  "run": {
    "id": 12,
    "ticket": "PROJ-123",
    "lane": "backend",
    "kind": "lane",
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
    "age_secs": 240,
    "findings_count": null
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

`run.findings_count` is the number of unresolved bot review findings tallied
by `tm pr watch` at the run's end (see `--by-outcome` above): `null` means
"not measured" (every run kind other than `review-watch`, and any
`review-watch` run finished before this field existed), `0` means "measured,
clean". The two are never conflated.

`--auto-ticket` skips the "create a ticket?" prompt and just creates one
(in the configured default project, assigned to the configured default
assignee) when no key can be resolved from the PR's title, body, or
branch name.

## `tm work`

`tm work` provisions per-lane git worktrees and tmux sessions, and can run
one Claude Code session per lane, tracked in `tm runs`
(ported from a personal `j work` runner; see
`docs/plans/runner-port.md`/`docs/decisions/0002-runner-absorption.md`).
Each lane is configured under `[work.lanes.<name>]` in `config.toml`
(`repo` is required; `prompt_file`, `base_branch`, `model`, `max_turns`,
`permission_mode` fall back to the `[work]`-level defaults, then to
built-in defaults). `tm work run <lane>` provisions the lane's worktree if
missing, cuts a fresh timestamped branch off the resolved base for this
run, and invokes `claude` with the lane's prompt. That prompt is the lane's
`prompt_file`, resolved against the lane's repo root when it is a relative
path (so `prompt_file = "prompts/<lane>-lane.md"`, what `tm init`
scaffolds, keeps the prompt versioned alongside the code it instructs), and
falling back to `~/.claude/prompts/<lane>.md` when unset. In a repo-local
`.tskmstr.toml`, `repo` may
also be a relative path — see "Relative `repo`/`dir` paths in a repo-local
config" below.

`tm work run <lane> [ticket]` and the board's `w` key (see "Board-launched
lane runs" below) both refuse a lane whose `repo` resolves to a different
ticket backend than the current repo's own: the CLI as a hard preflight
error naming both backends, the board by filtering the lane out of the
picker (GitHub issue #5).

Interactive by default: provisioning/preflight run in the foreground so
errors surface immediately, then `claude` is launched in a `work` window of
the ticket's `tm-<key>` tmux session and the invocation returns the terminal
right away. `tmux attach -t tm-<key>` to watch or steer the run mid-flight.
The run row is finished by the session's own `SessionEnd` hook, so the
prompt opens by telling the session to run `tm runs register --kind lane
<KEY>` — that is what lets it adopt the run row `tm work run` pre-registered
for it. A second `work` run is refused while one is still live in the
session, before anything is provisioned.

`--headless` keeps the previous behavior for CI, cron, and unattended
batches: a one-shot `claude -p` under a `setsid`'d supervisor that runs
`claude`, records the outcome, and is bounded by `--max-turns`. `--fg` runs
that same headless pass synchronously and reports the run's outcome in the
command's exit code — it implies `--headless`, since an interactive session
has no outcome to report back. `--max-turns` applies only to the headless
pass; an interactive session has no turn budget. On
completion it records the PR URL, if any, on the run's `pr_url` field:
first by asking `gh` directly for the branch's open PR, falling back to
scraping the first GitHub pull-request URL out of the run's result text —
no PR is a normal outcome, not an error.

### Interactive-session hooks (`tm work hooks install --user`)

`tm work run`'s lane worktrees get tm's telemetry hooks deployed fresh on
every run (copy-on-every-run, no install step — see `src/work/hooks.rs`).
But interactive sessions like `tm ticket audit`/`tm ticket create` run in
your normal Claude Code, which reads `~/.claude/settings.json` (or
`$CLAUDE_CONFIG_DIR/settings.json`), and nothing installs tm's hooks there
automatically. Without them, those sessions' token usage and cost are
invisible to `tm runs`.

`tm work hooks install --user` closes that gap: it copies tm's hook
scripts into `${XDG_DATA_HOME:-~/.local/share}/tskmstr/hooks/` and
additively merges just three hook entries into your settings file —
`Stop`/`SubagentStop` -> `tm-usage.sh` and `SessionEnd` ->
`tm-session-end.sh`. It never removes, reorders, or rewrites any existing
entry (your settings file is likely shared with other tools), never
duplicates an entry on repeat runs, and always writes a timestamped backup
(`settings.json.bak-<timestamp>`) next to the file before changing it. An
absent, empty, or unparseable settings file is a hard error rather than
something this command will overwrite or recreate.

It deliberately does **not** wire in `guard-delegate.sh`: that hook denies
main-loop file edits while its gate is active, and installing it at user
level would start blocking your ordinary editing in every Claude Code
session, not just lane runs. `tm-event.sh`, `tm-checklist.sh`,
`tm-tasklist.sh`, and `tm-session-state.sh` are excluded too — none of them
are needed to close the interactive-session cost gap, and each adds
per-tool-call overhead to every session if wired in.

Given the blast radius of editing a settings file you rely on for
everything else, run `tm work hooks install --user --dry-run` first — it
performs every check and prints the same summary, but touches nothing.
Once the diff looks right, re-run without `--dry-run` to apply it.

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
| `A` | Open the assign picker for the selected ticket (board only) |
| `p` | Open the priority (stack-rank) view (board only) |
| `a` | Launch a ticket-audit session for the selected ticket, or attach to it if one is live (board only) |
| `w` | Launch a lane run for the selected ticket: zero backend-compatible lanes sets a status-line message, exactly one launches it directly, more than one opens a lane picker (board only); see "Board-launched lane runs" below |
| `s` | Attach to the selected ticket's `tm-<key>` tmux session — its whole action history, whatever is in it (board only). Unlike `a`, it never launches anything; if the ticket has no session yet, the status line says so |
| `b` | Arm a PR bot-findings watcher for the selected ticket, launch (or attach to) its cleanup session once the watcher finds something, or attach to a live cleanup session directly (board only) |
| `v` | Open the run-detail overlay for the selected ticket's latest run, any `kind` (board only) |
| `L` | Open the selected ticket's latest run's log file in `less` (board only); see "`tm runs logs`" below |
| `V` | Open the selected ticket's lane-run worktree in `vdiff` for review (board only); see "Board-launched vdiff review loop" below |
| `F` | Dispatch a fix pass over the review comments `vdiff` captured for the selected ticket (board only); see "Board-launched vdiff review loop" below |
| `R` | Open the retro board (board only); see "Retro board" below |
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

### Assigning a ticket from the board

Pressing `A` on the board opens a floating picker for the selected card,
listing `Me`, `Unassign`, and every user assignable in `default_project_key`
(the same lazily fetched, session-cached list the assignee filter picker
uses). `j`/`k` navigate, `Enter` applies the highlighted choice via the
ticket provider and closes the picker, `Esc`/`q` closes it without changing
anything. The option matching the card's current assignee is marked with a
leading `*`. Applying a choice updates the card and the status line in
place, with no board refetch. Under the GitHub backend this is exclusive
single-assignee semantics on top of GitHub's multi-assignee model —
multi-assignee support is deliberately out of scope here (see
`docs/plans/github-issues-backend.md`'s phase 6 notes).

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

### One tmux session per ticket

Every action tskmstr takes against a ticket runs in a window of that
ticket's own detached tmux session, named `tm-<lowercased key>` — the audit
in a window named `audit`, the bugbot-cleanup session in `bugbot`, plus a
plain `shell` window rooted where the session was created, for
`claude --resume`, manual git work, and running tests. So `tmux attach -t
tm-proj-123` shows one ticket's whole history, live windows included.

Windows are append-only: nothing renames or reorders them, so window order
is the action history. A repeat action whose window name is still taken
(by a previous run's dead window) gets a numeric suffix — `audit`,
`audit-2`, `audit-3`. Badges and the refuse-to-double-launch guard key off
*live window names*, not session existence: once one session outlives every
individual action, the session merely existing means "this ticket has been
touched".

Press `s` on the board to attach to the selected ticket's session. Every
attach — `s`, `a`, `b`, `tm work start` — is inside-tmux aware: from a
plain terminal it runs a blocking `tmux attach-session`; when `$TMUX` is
set it runs `tmux switch-client` instead, jumping your current client to
the session rather than tripping tmux's nested-session refusal. The board
keeps running in its own window — switch back to it the usual tmux ways
(`prefix + s`, `switch-client -l`).

#### Interactive windows own their process; headless ones only watch

An interactive action (`tm work run`, `tm review fix`, an audit, a
bugbot-cleanup) *is* the `claude` process in its window: attach and you can
steer it. A `--headless` run is different — its `claude` belongs to a
`setsid`'d supervisor that deliberately lives outside tmux, because it is
the only thing that will record the run's outcome when `claude` exits.
Binding it to a window would make `tmux kill-session` (or a reboot) silently
kill a run mid-flight.

So a headless run gets a window too, but a **viewer**: it runs
`tm runs logs <id> --follow` and owns nothing. Kill it, or kill the whole
session, and the run carries on. If tmux is unavailable the viewer is simply
skipped, reported as a `window none` line — the run is unaffected either way.

This mixed ownership is deliberate, not an inconsistency waiting to be
tidied up.

#### Rebuilding a session: `tm work session <KEY>`

The session is convenience; the run rows and log files are the record. tmux
scrollback is capped by `history-limit` and dies with the server, so it is
never what happened — the log file is.

After a reboot, a `tmux kill-server`, or an accidental
`tmux kill-session -t tm-proj-123`, `tm work session PROJ-123` rebuilds the
session from the ticket's runs:

- A **headless run still in flight** gets its viewer window back and keeps
  going, none the wiser — its supervisor survived whatever killed tmux.
- An **interactive run still in flight** has lost its `claude` with the
  pane. Its window comes back as a shell in the run's worktree, and the
  command prints the `claude --resume <session-id>` line for it. Resuming is
  never automatic: it would start billing and start editing unasked.
- A **finished run** gets no window, of either kind. Reconstruction restores
  working state, not history; `tm runs show`/`tm runs logs` (and the board's
  `L`) are where a finished run lives. A finished interactive run has no log
  at all — its durable artifact is its prompt file.

It never attaches, and running it against a healthy session does nothing, so
it is safe to run at any time or from a script.

#### Finishing with a ticket: `tm work clean <KEY>`

Because the session is the ticket, cleanup is one `kill-session` plus one
worktree removal. The worktree comes from the ticket's run rows, and only a
path sitting one level below the configured worktree root is ever removed —
an `audit` run records `[work.audit].dir`, your own checkout, and that can
never be handed to `git worktree remove`. A worktree that is already gone is
reported, not an error.

### Board-launched lane runs

Pressing `w` on a board ticket runs `tm work run <lane> <KEY>` for it (see
"`tm work`" above) without leaving the board: it launches directly if
there's exactly one lane, opens a picker if there's more than one, and sets
a status-line message if there are none.

"None"/"more than one" is computed only over lanes whose configured `repo`
resolves to the *same backend* as the board's own repo — same
`BackendKind`, and for GitHub the same `[backend.github].repo` slug, or for
Jira the same base URL and project key (GitHub issue #5). A lane rooted in
a differently-backed repo would launch a session that can't resolve the
selected ticket's key at all (a GitHub-backend board handing a `GH-3`
ticket to a Jira-backed lane, or vice versa), so it's filtered out of the
count and the picker rather than offered. When every configured lane gets
filtered this way, the status line says so explicitly — `no compatible
lanes (N hidden: backend mismatch)` — instead of the plain `no lanes
configured` it shows when `[work.lanes]` is genuinely empty. `tm work run
<lane> <KEY>` run directly from the CLI enforces the same check as a hard
preflight error naming both backends, before any worktree or run-row work
happens.

This is also why a lane's `repo` accepts a relative path: written in a
repo-local `.tskmstr.toml`, `repo = "."` roots the lane in that same repo,
which is guaranteed backend-compatible with the board that offers it. See
"Relative `repo`/`dir` paths in a repo-local config" under Configuration
below.

### Board-launched audit sessions

Pressing `a` on a board ticket launches a ticket-audit Claude session for
it in the `audit` window of its `tm-<key>` session — several tickets can
run concurrently. Pressing `a` again on the same ticket attaches the
terminal to that session (the board suspends, tmux takes over; detach with
`C-b d` to land back on the board). Launching requires:

```toml
[work.audit]
dir = "~/Projects/axiom"            # required: where the session runs
# prompt = "/ticket-audit {key}"    # optional; this is the default
# model = "fable"                   # optional; passed as `claude --model`
```

`dir` is the repo whose `.claude/` provides the audit skill and telemetry
hook settings; `{key}` in `prompt` is replaced with the ticket key. The
launch pre-registers a `kind = "audit"` run, and the in-session
`tm ticket audit <KEY>` adopts it (via `TSKMSTR_SESSION_RUN_ID`), so the
whole conversation's telemetry lands on one run.

If `dir`'s resolved backend doesn't match the current repo's own — the
same mismatch `w`'s lane filtering checks for (GitHub issue #5) — the
audit session falls back to launching in the current repo instead of
refusing outright, since a repo hosting the ticket's own backend always
resolves the ticket correctly. The status line notes the fallback:
`launched audit for <KEY> in the current repo (configured audit dir is
backend-incompatible) -- press a to attach`.

`model` is separate from `[work].default_model`, which only applies to
headless `tm work run` lanes. Leave it unset and the session takes whatever
model `claude` defaults to — under an enterprise-managed model pin, that is
the pinned model, not anything tskmstr configures. Setting it emits an
explicit `claude --model`, which overrides that pin. Since an audit is
where digging quality matters most, it is worth setting deliberately.

Each card with a live `audit` window (or a live audit run) shows a badge:
`audit: starting` (window up, run not registered yet), `audit: running`, `audit:
waiting` (bold yellow — Claude stopped or asked a question and is waiting
for you; attach and answer), and `audit: done` / `audit: failed` while
the window is still up. Waiting-state telemetry comes from the `Stop` /
`Notification` / `UserPromptSubmit` hooks emitting `await`/`resume`
events; `tm runs watch` renders the same state as a `waiting` marker on
running audit/create cards. The board polls the run store and tmux for
badge updates every ~2s; the ticket list itself still refreshes only on
`r`.

### Board-launched bot-findings watch

Once a ticket's PR is up for review, its bots (`review_bots`) take anywhere
from seconds to tens of minutes to post their findings. Pressing `b` on a
board ticket arms a watcher for it: `tm pr watch <KEY>` resolves the
ticket's open PR and polls `gh` in a detached process, never on the
board's own tick — the board only ever reads the local run db. The card
shows a `bots:` badge: `bots: starting` while the launcher child is still
resolving the PR, `bots: watching` once the poll loop is running, `bots:
ready` (bold — the watcher finished and left unresolved findings) when a
cleanup session is worth launching, `bots: clean` when the bots reviewed
and found nothing (no cleanup needed), and `bots: failed` if the watcher
gave up: either the PR sat open past `max_wait_mins`, or `gh` failed —
after 10 consecutive failures (~7-8 minutes of backoff at the default
`poll_secs`) for an ordinary transient failure (network, rate limit,
expired auth), or immediately, on the very first failed poll, for a
permanent one (e.g. `gh` rejecting a request tm itself built wrong) —
retrying a failure that's going to recur identically forever gains
nothing, so that case skips the backoff. Either way, `tm runs logs
<KEY> --kind review-watch` shows what `gh` actually said.

Pressing `b` again is context-sensitive, mirroring `a`'s attach-or-launch
shape:

- A live cleanup session (see below) → attach to it.
- No cleanup session, but the watcher is `bots: ready` → launch the
  cleanup session.
- The watcher is still `bots: watching` → a status-line message, no
  action.
- Otherwise (no watcher yet, or the last one is `bots: clean`/`bots:
  failed`) → arm a new watcher.

Watching requires:

```toml
[work.review_watch]
dir = "~/Projects/axiom"          # optional; falls back to [work.audit].dir
# prompt = "/bugbot-triage {key} {findings_file}"  # optional; this is the default
# model = "fable"                 # optional; falls back to [work.audit].model
# poll_secs = 45                  # optional, default 45
# max_wait_mins = 1440            # optional, default 1440 (24h)
# on_bots_done = "notify"         # optional, "notify" | "launch"; default "notify"
```

`poll_secs` is the cadence `tm pr watch`'s foreground loop sleeps between
`gh` checks; `max_wait_mins` is how long it keeps polling an open PR
before giving up. `on_bots_done` gates whether finding unresolved bot
comments spends tokens automatically: `"notify"` (the default) just
leaves the card at `bots: ready` for you to launch by hand; `"launch"`
launches the cleanup session itself the moment the watcher sees
unresolved findings. Zero unresolved findings always goes straight to
`bots: clean` — no cleanup session either way.

When there are findings, the watcher writes them to
`${XDG_DATA_HOME:-~/.local/share}/tskmstr/findings/<key>.json` before
finishing, and the cleanup session (the `bugbot` window of the ticket's
`tm-<key>` session, launched the same way the `audit` window is) runs
`prompt` with both `{key}` and `{findings_file}` substituted. Unlike the
audit session, nothing in this repo calls `tm ticket audit` for you
inside the cleanup conversation — the axiom-side `/bugbot-triage` skill's
documented first step is `tm runs register --kind bugbot-cleanup <KEY>`,
which adopts the pre-registered run via `TSKMSTR_SESSION_RUN_ID` so the
whole conversation's telemetry lands on it, the same way `tm ticket
audit`/`create` adopt theirs.

### Board-launched vdiff review loop

`vdiff` (https://github.com/jowi-dev/vdiff) is a visual PR reviewer with an
embedded nvim; its `vdiff.nvim` plugin captures per-hunk review comments into
`<git-dir>/vdiff/comments.json` as you review. `V` and `F` close that review
loop from the board without leaving the keyboard: `V` opens the review, `F`
dispatches a fix pass over whatever comments you left.

Pressing `V` on a board ticket resolves its latest `kind = "lane"` run and
opens `vdiff` with `current_dir` set to that run's `worktree` — the same
worktree `tm work run` provisioned, on its existing branch. `vdiff` detects
the PR's base branch itself, so no `--pr` flag or other resolution is
needed. Unlike `a`/`w`/`b`, this is a foreground, terminal-suspending
launch (mirroring `L`'s log viewer): the board leaves the alternate screen
and hands the terminal to `vdiff` directly, since it's an interactive
GUI/TUI that needs the real TTY, and returns to an intact board once you
quit it. A ticket with no lane run, a lane run whose worktree has since
been removed (`tm work remove`), or a `vdiff` not found on `PATH` all set a
status-line message rather than launching anything or appearing to hang.

Reviewing PRs with no local worktree (someone else's PR, not run from this
board) is out of scope for now — `vdiff` has no `--pr` flag yet.

Pressing `F` dispatches `tm review fix <KEY>` as a watched-child launch, the
same shape as `w`/`b`: `tm review fix` resolves the ticket's lane run,
renders its captured `vdiff` comments via `vdiff --export-comments`, and
dispatches a tracked run in the ticket's existing worktree and branch — no
new worktree, no new branch. The pass itself runs interactively in a `fix`
window of the ticket's `tm-<key>` session (`fix-2` for a second pass), so
`tmux attach -t tm-<key>` shows it alongside the ticket's other actions. Its run rows
use `kind = "review-fix"`, so they never shadow the lane run and show up
separately in `tm runs`; there is no board badge for them yet, so track
progress via `tm runs` or the run-detail overlay (`v`). A ticket with no
lane run sets a status-line message immediately; one with a lane run but no
captured comments launches the child, which resolves quickly (like every
watched-child launch) and reports "no comments captured" (or similar) in the
status line without leaving a `review-fix` run behind.

### Retro board

Pressing `R` on the board opens a full-screen "Retro" list: every ticket in
`default_project_key` that shipped (moved to a `Done`-category status)
within the last 30 days and has no recorded retro verdict yet (see
"Recording ship-defect retros" below), newest-resolved first. Bounded to a
recent window rather than every `Done` ticket ever, so the screen reads as
a queue to clear, not a wall of history. Once a ticket has a verdict —
recorded here or via `tm ticket retro` — it drops off the list for good.

Each row shows the ticket's key and summary plus its latest `kind = lane`
run's cost and model mix, when it has one. A ticket with no lane run at all
(common — not all shipped work goes through a lane) shows `no run`, kept
visibly distinct from a run that cost `$0.00` — that distinction is the
whole point, since it separates "shipped manually" from "shipped cheaply".

| Key | Action |
|---|---|
| `j` / `k` / arrows | Move the cursor |
| `d` | Flag a defect: opens a severity picker (`Minor`/`Major`/`Critical`), then an optional one-line note |
| `c` | Mark the highlighted ticket clean, immediately, no picker |
| `r` | Refetch the retro list from Jira |
| `o` | Open the highlighted ticket in the browser |
| `Esc` / `q` | Return to the board |
| `?` | Toggle the help overlay |

The note step is a single-line field built up a character at a time
(`Backspace` deletes, `Enter` submits — blank submits with no note, `Esc`
cancels the whole defect flow, ticket included). It doesn't shell out to
`$EDITOR` the way `tm ticket comment`'s prompter does: doing that from
inside the board would mean suspending raw mode and the alternate screen
around the child process, the same dance `a`/`L` already do for `tmux
attach`/`less`, and wiring the `EditorPrompter` trait through the TUI's
dependencies for one optional field wasn't worth the extra moving parts.
If you need a longer note, `tm ticket retro <KEY> --defect --severity
<...> --note "..."` isn't limited to one line.

A successful verdict removes the ticket from the list immediately (no
refetch) and confirms in the status line (`Recorded clean for PROJ-3`); a
failed one (e.g. the runs database is unavailable) leaves the ticket in
place and shows the error instead — the board never freezes or panics on a
failed Jira/store call here, same stance as everywhere else in the TUI.
When nothing is awaiting a verdict, the screen says so plainly rather than
rendering an empty box — that's the steady state this screen is meant to
reach, not an error.

## Configuration

`tm init` writes both files described below interactively; everything here
can also be authored by hand.

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

# [backend]                           # optional, see below; "jira" is the default
# provider = "jira"
```

A repo can override any subset of these fields with a `.tskmstr.toml` in
its root; fields it doesn't set fall back to the global config.
`jira_base_url`, `jira_email`, and `default_project_key` must resolve
between the two files whenever the Jira backend is selected, or `tm`
refuses to run; `default_assignee_account_id`, `status_on_pr`,
`status_on_create`, `review_bots`, `board_column_order`, and `[backend]`
are optional.

### Relative `repo`/`dir` paths in a repo-local config

A `[work.lanes.<name>].repo` or `[work.audit].dir` value set by a
repo-local `.tskmstr.toml` may be a plain relative path (not `~`-prefixed,
not absolute); it resolves against that repo's own root. `repo = "."` is
the common case — it resolves to the repo root itself, not `<root>/.` —
letting a repo point a lane straight at itself:

```toml
[work.lanes.tskmstr]
repo = "."
```

This is what makes a lane trivially backend-compatible with the repo that
defines it (see "Board-launched lane runs" above): the lane's repo and the
board's own repo are the same directory, so their resolved backends can
never mismatch.

The same relative value set by the *global* config
(`~/.config/tskmstr/config.toml`) is a hard error
(`RelativePathRequiresRepoConfig`, naming the field and the value) rather
than resolving against some fallback directory — the global config has no
repo of its own to resolve a relative path against, and a lane inherited
verbatim into every project (the root cause behind GitHub issue #5) is
exactly the failure mode this restriction rules out. Use an absolute or
`~`-prefixed path for any lane/audit-dir value set globally.

### `[backend]`: choosing a ticket provider

`[backend].provider` selects which ticket provider this config uses. It
defaults to `"jira"` when `[backend]` is absent from both global and repo
config, so an existing config with no `[backend]` table at all keeps
working exactly as before. `"jira"` and `"github"` are both implemented;
any other value is an invalid-provider error naming the value that was set.

Each adapter validates only its own required fields: under the Jira
provider, `jira_base_url`, `jira_email`, and `default_project_key` are
required exactly as before `[backend]` existed; under the GitHub provider,
`[backend.github].repo` (an `"owner/name"` slug) is required instead, and
none of the Jira fields are. A repo-local `.tskmstr.toml` can override
`[backend].provider` on its own, same as every other field.

```toml
[backend]
provider = "github"

[backend.github]
repo = "jowi-dev/tskmstr"   # "owner/name"; defaults to the checkout's
                             # `origin` remote when unset (see below)
```

`[backend.github].repo` can be omitted: `tm` then runs `git config --get
remote.origin.url` in the repo-local config file's directory (or the
current directory, if there's no repo-local config at all) and parses an
`"owner/name"` slug out of it, recognizing both the SSH
(`git@github.com:owner/name.git`) and HTTPS
(`https://github.com/owner/name.git`) forms GitHub hands out. If that
doesn't resolve to anything (not a git checkout, no `origin` remote, a
non-GitHub host), `tm` fails with the same missing-field error an explicit
`repo` omission would produce — it never guesses.

`[backend.jira]` is the canonical location for Jira's own fields —
`jira_base_url`, `jira_email`, `default_project_key` — mirroring
`[backend.github]`'s shape:

```toml
[backend.jira]
jira_base_url = "https://example.atlassian.net"
jira_email = "dev@example.com"
default_project_key = "PROJ"
```

The legacy flat top-level keys shown at the top of this section keep working
unchanged as a silent fallback (checked only when the corresponding
`[backend.jira]` field is absent) — existing configs, including
`~/.config/tskmstr/config.toml` in the wild, need no migration. When a field
is set in both places within the same file, `[backend.jira]` wins.

Selecting the GitHub backend changes several things: `tm auth login`/`tm
auth status` are no-ops (GitHub Issues authenticates via `gh`'s own `gh auth
login`/`gh auth status`, not a stored Jira token); ticket keys are
`GH-<issue number>` instead of a Jira project key; status lives in
`tm:status/{todo,in-progress,in-review,blocked}` labels (no label, or a
closed issue, both map to their obvious default) rather than a Jira
workflow; and `tm backend init-labels` creates those four labels in the
configured repo (idempotently — safe to re-run) so a fresh repo's board has
somewhere to put issues. `tm ticket`/`tm ready`/the board's mutating keys
work end to end under the GitHub backend: create, comment, update
description, and transitions map onto `gh issue create/comment/edit`;
associating a PR needs no remote link (the `Closes #N` line already renders
the backlink); links (`Blocks`) use GitHub's native issue-dependencies
GraphQL mutations; and rank has no GitHub equivalent, so it's tracked in a
local `ticket_rank` table in `runs.db` (unranked issues sort to the end by
issue number, same as before anything is ranked). See
`docs/plans/github-issues-backend.md` for the full design and phase status.

This repo dogfoods the GitHub backend on itself: its own `.tskmstr.toml`
sets `provider = "github"` with no `[backend.github]` section at all, so
`tm` running inside a checkout of this repo resolves `repo` from the
checkout's own `origin` remote (`jowi-dev/tskmstr`) rather than a hardcoded
slug. `tm board`/`tm ready`/`tm ticket *` run against this repo's own
issues.

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

This command also runs inside its own tracked run: if it's invoked from a
Claude Code session (an interactive `/ticket-audit` skill, not a plain
terminal command), `tm` registers an `audit`-kind run in the same runs
database `tm runs` uses, keyed by the session's identity, so the
conversation's tool and model usage get recorded exactly like a `tm work
run` lane's. `--record` (below) finishes that run. This is pure telemetry —
a plain terminal invocation, or one with no runs database available,
registers nothing and behaves exactly as before. When a finished `audit`-kind
run for `<KEY>` recorded model usage, a `Last audit usage: <model> <n>k
out / ...` line follows `Last audit: ...` (omitted otherwise, or on a
runs-DB error). `tm runs --kind audit` and `tm runs show <KEY> --kind audit`
surface these runs directly. `tm ticket create` registers a `create`-kind run
the same way, immediately after the new ticket exists.

Session registration only writes the marker file (`register_session` in
`src/runs/session.rs`) that ties a Claude Code session to its run row — it
does not, by itself, make any usage data show up. That still depends on
`hooks/tm-usage.sh` (Stop/SubagentStop) and `hooks/tm-session-end.sh`
(SessionEnd) actually firing during the conversation. `tm work run` deploys
those hooks automatically into each lane's worktree-local Claude Code
settings (see [`src/work/hooks.rs`](src/work/hooks.rs)), but an interactive
`tm ticket audit`/`create` session runs under your normal, global Claude Code
settings (`~/.claude/settings.json`), which `tm` never touches. Wiring
`tm-usage.sh` (`Stop`, `SubagentStop`) and `tm-session-end.sh` (`SessionEnd`)
into your global `settings.json` — additively, alongside whatever hooks are
already there — is a one-time, manual step; without it, `tm ticket audit`/
`create` runs register correctly but never accumulate any usage or cost.

`tm ticket audit <KEY> --record <ready|needs-work> [--notes "..."]` persists
that conversation's verdict, timestamped, to the same local SQLite database
`tm runs` uses (`$XDG_DATA_HOME/tskmstr/runs.db`, or wherever `run_db_path`
in `config.toml` points instead). Recording never touches Jira and works
fully offline; every past verdict is kept (no upsert), and the read mode
above always shows the most recent one. `--notes` only makes sense alongside
`--record`, so it requires it.

### Recording ship-defect retros

`tm ticket retro <KEY> --clean` or `tm ticket retro <KEY> --defect --severity
<minor|major|critical> [--note "..."]` records whether a ticket, once shipped,
turned out to be defective in production. This is a different signal from
`findings_count` (what review bots caught pre-merge, via `tm runs finish
--findings-count`): a retro records what production revealed *after* ship,
and the two are allowed to disagree — a ticket clean in bugbot but defective
in prod is exactly the case this exists to surface. `--severity` is required
with `--defect` and rejected with `--clean` (enforced by
`RunStore::record_retro`, not by clap — see its doc comment for why). Exactly
one of `--clean`/`--defect` is required; `--note` is optional but, if given,
must not be empty or all-whitespace.

Retros are stored in the same local SQLite database `tm runs` uses, in a
`ticket_retros` table modeled on `ticket_audits`: every call inserts a new
row rather than upserting, so a ticket can be re-recorded (marked clean, then
later found defective once a bug surfaces) while every past verdict stays
queryable, and reads always resolve to the most recent one. Recording never
touches Jira and works fully offline.

`tm runs --by-retro [--kind <KIND>]` joins retro verdicts to run cost: for
each verdict (clean/defect), it reports the number of tickets carrying that
verdict, how many of them have no recorded run at all, the number of runs
across the rest, and total/average cost over those runs. A ticket with a
verdict but no run (common — not all shipped work goes through a lane)
contributes to the ticket count but is excluded from the cost columns
entirely, rather than counted as a `$0` run.

### Commenting

`tm ticket comment [<KEY>] [--body <TEXT>] [--pr]` posts a comment to a Jira
ticket. `<KEY>` is verified to exist first, same as `rank`/`link`; if it's
omitted, it's inferred from the current branch's pull request the same way
`tm pr create` infers an existing ticket (title, body, then branch name). If
neither an explicit `<KEY>` nor a resolvable one is available — no pull
request open for the branch at all, or one exists but carries no ticket key
anywhere — it's a hard error naming the branch.

The comment body is resolved in this order: `--body`, then piped stdin (when
stdin isn't a terminal, e.g. `git log -1 --format=%B | tm ticket comment
PROJ-372`), then `$EDITOR` as a last resort (opened on a scratch file; its
saved contents become the body). An empty or all-whitespace resolved body is
rejected as a usage error, same rationale as `tm ticket search`'s empty-text
check. The body is Markdown throughout; it's converted to Jira's ADF format
for the Jira comment (the same conversion `tm ticket update`/`create --body`
use).

`--pr` **means the pull request open for the current branch** — not "the
pull request associated with the ticket". There is no reverse lookup from a
Jira issue back to a PR in this codebase (it would mean re-deriving one via
`gh pr list`), and every other explicit `tm ticket <KEY>` command already
only ever touches the current branch's PR, so `comment` follows the same
rule. When set, the same Markdown body is also posted to that PR via `gh pr
comment`, unconverted — GitHub comments are Markdown natively, unlike Jira's
ADF requirement. Like every other explicit `tm ticket` subcommand, every
failure here is a hard error (non-zero exit); there's no advisory/warning
path, since nothing has already been created or linked by the time a comment
attempt fails.

### Searching

`tm ticket search <TEXT>` searches the configured default project
(`default_project_key`) for open (non-`Done`) tickets whose text matches
`<TEXT>`, most recently updated first. It's meant for a quick sweep — e.g. a
Claude skill checking for potential blockers or duplicates before creating a
new ticket — not for browsing: it prints one line per match, `KEY  STATUS
SUMMARY`, or a friendly "no matches" message (exit 0) when nothing is found.
`<TEXT>` must not be empty or all-whitespace; unlike a missing match, that's
rejected up front as a usage error, mirroring `tm ticket assign`'s empty-name
check. Any other Jira/config failure is a hard error, same as every other
`tm ticket` subcommand.

### Readiness

A ticket is a *candidate* for readiness when it has no open `Blocks`-type
blockers: a `Blocks` link where the linked issue isn't yet Done. A Done
blocker doesn't count at this stage, and a ticket that merely blocks
something else is never "blocked" by that relationship. Every ticket that
fails this first, Jira-only check is then re-examined by the **stack
decision** below before being reported ready, stackable, or blocked — this
is what tells the difference between "genuinely stuck" and "safe to build
on top of, without waiting for the blocker to merge".

#### The stack decision

`tm ready` and `tm work run` (which cuts a lane run's branch — see
[Log files](#log-files) above) share one decision table so they can never
disagree about a ticket's blockers, instead of `tm ready` trusting Jira
status while `tm work run` trusted PR merge state, which is what happened
before this was unified: a ticket blocked in Jira by an unmerged-but-open PR
was stackable by `tm work run`'s own logic, but `tm ready` still reported it
`BLOCKED`, and an autonomous lane prompt that treats `tm ready`'s word as
final refused to touch it.

For each of a ticket's *direct* `Blocks` blockers, satisfied means EITHER of
these — a Jira status check and a PR merge-state check, either one clearing
it independently:

- Jira status category **done** → satisfied, doesn't count, regardless of
  whether it ever had a PR at all (a config change, a spike, docs, or manual
  ops work often has none to find).
- PR **merged** → satisfied, doesn't count, regardless of whether Jira's
  status has caught up yet.
- otherwise, PR **open** → unmerged, a candidate to stack on.
- otherwise, **no PR** (including a closed-but-unmerged one, or a PR whose
  branch name doesn't match the lane-branch naming convention) → unmerged,
  with nothing to stack on yet.

Then, across the ticket's unmerged blockers:

- **zero** → **ready**.
- **exactly one, with an open PR** → **stackable**: `tm work run` (with no
  `--from` override) cuts the run's branch from that PR's head branch
  instead of the normal base, rather than waiting for it to merge; `tm
  ready <KEY>` reports the same branch so a human or an autonomous agent can
  do the equivalent by hand.
- **exactly one, with no PR yet** → **blocked**: nothing exists to build on.
- **two or more** → **blocked**: a single branch can only be stacked on one
  dependency at a time, so this refuses rather than guessing which one.

`tm ready` (no key) lists tickets assigned to you that are ready, further
restricted to your "To Do" tickets (something already In Progress has
already been picked up) and printed in Jira's native backlog rank order,
`KEY  Summary` per line, followed by any candidate found stackable as `KEY
Summary  [stackable on <branch> — blocked by <BLOCKER>, PR #<N> open]`. If
any remaining candidates were excluded for being blocked (per the stack
decision above), a final `(N blocked tickets hidden)` line says so, so a
filtered list doesn't read as "this is everything assigned to you".

`tm ready <KEY>` checks one specific ticket, regardless of assignee or
status:

- **ready** — prints `KEY is ready (<status>)`, exits `0`.
- **stackable** — prints `KEY is stackable on <branch> (blocked by
  <BLOCKER>, PR #<N> open)`, exits `3` — a distinct code from both `0` and
  `1` so a script or an autonomous lane prompt can branch on "safe to
  proceed by stacking" without parsing stdout.
- **blocked** — prints `KEY is blocked by:` followed by one line per
  unmerged blocker (plus, when there's more than one, a line explaining
  that two parallel unmerged blockers can't both be stacked on), exits `1`.

Resolving the stack decision needs `gh` (to check each blocker's PR state),
but only when the ticket actually has a direct blocker — one with none never
shells out. A **transient** `gh` failure (network error, rate limit, `gh`
missing, expired auth) degrades quietly to the pre-stacking, Jira-status-only
answer, printing a `warning: could not resolve blocker PRs for KEY (...) —
falling back to Jira-only readiness check` line first (`list`'s equivalent
warning is `warning: could not check stackability: ...`, and it hides every
remaining blocked candidate rather than guessing). A **permanent** `gh`
failure — `gh` telling `tm` it asked for something nonsensical, e.g. an
invalid `--json` field — is not swallowed: `tm ready <KEY>` fails loudly with
`bug in tm itself while resolving blockers for KEY: ... (this will not
resolve on retry)` instead of silently misreporting the ticket's real
stackability.

Both forms also carry a best-effort, advisory annotation of unresolved
GitHub bot review findings (see `review_bots` above and `Bot findings`
below) on a ready ticket's associated open pull request (matched by title,
the same way as `tm ticket`/`tm pr` association) — stackable and blocked
tickets never carry this annotation. `tm ready`'s list adds `  [N unresolved
bot findings]` to a matched ready ticket's line when `N > 0`; `tm ready
<KEY>` prints a `  note: N unresolved bot findings on PR #<number>` line
after the ready message. This is purely visible, never blocks claimability,
and never changes an exit code: if the GitHub lookup fails, `tm` prints a
single `warning: could not check bot findings: ...` line and falls back to
the unannotated output rather than failing the command.

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
- **Ticket search caps at 500 results.** `POST /search/jql` is paged
  through via `nextPageToken`, but only up to 5 pages of 100 issues. Past
  that the board and rank screens show the first 500 and warn "showing
  first N tickets -- more matched; narrow the filter" in the status line.
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
