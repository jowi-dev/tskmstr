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
| `tm ticket <KEY>` | Associate Jira issue `<KEY>` (e.g. `AX-372`) with the PR open for the current branch |
| `tm pr create [--title] [--body] [--base] [--auto-ticket]` | Open a PR for the current branch and associate a ticket |
| `tm pr status [--auto-ticket]` | Report the PR open for the current branch and its associated ticket |
| `tm` / `tm board` | Open the interactive TUI board of your assigned tickets |

`--auto-ticket` skips the "create a ticket?" prompt and just creates one
(in the configured default project, assigned to the configured default
assignee) when no key can be resolved from the PR's title, body, or
branch name.

## TUI keybindings

| Key | Action |
|---|---|
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `Enter` | Drill into the selected ticket, or apply the selected transition |
| `Esc` / `q` | Go back a screen, or quit from the board |
| `r` | Refresh from Jira |
| `o` | Open the selected ticket in the browser |
| `?` | Toggle the help overlay (any other key closes it; `q` still quits) |

## Configuration

Global config lives at `~/.config/tskmstr/config.toml`:

```toml
jira_base_url = "https://home-solutions.atlassian.net"
jira_email = "joe.williams@homesolutions.com"
default_project_key = "AX"
default_assignee_account_id = "..."   # filled in by `tm auth login`
```

A repo can override any subset of these fields with a `.tskmstr.toml` in
its root; fields it doesn't set fall back to the global config. All four
fields must resolve between the two files or `tm` refuses to run.

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
3. The branch name (e.g. `ax-372-fix` or `feature/ax-372-fix`),
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
