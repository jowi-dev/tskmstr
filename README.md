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
| `tm ticket create [--title] [--body]` | Create a new ticket in the configured default project. No PR required or touched |
| `tm ticket transition <KEY> <STATUS>` | Move ticket `<KEY>` to `<STATUS>`. Fails (non-zero exit) if no transition matches or the Jira API call fails |
| `tm ticket transition <KEY>` | List ticket `<KEY>`'s current status and available transitions |
| `tm ticket assign <KEY> <NAME>` | Assign ticket `<KEY>` to the assignable user matching `<NAME>` (exact displayName match, else an unambiguous substring match). Fails if no user or more than one matches |
| `tm ticket assign <KEY> --me` | Assign ticket `<KEY>` to you (cached account ID from `tm auth login`, or the Jira `myself` endpoint) |
| `tm ticket assign <KEY> --unassign` | Clear ticket `<KEY>`'s assignee |
| `tm ticket rank <KEY> --above <OTHER>` | Rank ticket `<KEY>` above `<OTHER>` in Jira's native backlog rank |
| `tm ticket rank <KEY> --below <OTHER>` | Rank ticket `<KEY>` below `<OTHER>` in Jira's native backlog rank |
| `tm pr create [--title] [--body] [--base] [--auto-ticket]` | Open a PR for the current branch and associate a ticket |
| `tm pr status [--auto-ticket]` | Report the PR open for the current branch and its associated ticket |
| `tm` / `tm board` | Open the interactive TUI board of your assigned tickets |

`--auto-ticket` skips the "create a ticket?" prompt and just creates one
(in the configured default project, assigned to the configured default
assignee) when no key can be resolved from the PR's title, body, or
branch name.

## TUI keybindings

The board lays tickets out as columns, one per Jira status, ordered by
status category (new, then indeterminate, then done) and alphabetically by
status name within a category. Drilling into a ticket or its transitions
opens a centered floating window on top of the board rather than replacing
it, so the board stays visible behind the detail and "Move to" windows.

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
| `r` | Refetch the priority list from Jira |
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
```

A repo can override any subset of these fields with a `.tskmstr.toml` in
its root; fields it doesn't set fall back to the global config.
`jira_base_url`, `jira_email`, and `default_project_key` must resolve
between the two files or `tm` refuses to run; `default_assignee_account_id`,
`status_on_pr`, and `status_on_create` are optional.

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

`tm ticket transition <KEY> <STATUS>` uses the same case-insensitive
matching rule (target status name, falling back to the transition's own
name), but unlike `status_on_pr`/`status_on_create` it's a hard failure
if nothing matches or the API call fails, since the command is an explicit
request rather than an automatic side effect of creating/linking a ticket.
If `<KEY>` is already in `<STATUS>`, it prints a message and exits 0
without calling the transition API. Omit `<STATUS>` to list the ticket's
current status and available transitions instead.

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
