# GitHub Issues as a ticket backend (issue #3), phases 1-7

Status: **all seven phases complete.** Phases 1-6 (and the phase 5 prep
refactor) are merged to `main` (merge commits `f86ff4f`, `5ae20bc`,
`c00e95e`, `e0aaf33`, and `d6c5f4d`; the underlying work is `1446509`,
`721a331`, phase 3's commits below, `59efbed`, `877296d`/`b77baaf`, phase
5's commits below, and phase 6's commits below). **Phase 7** (dogfood) is
complete on branch `issue-3-phase-7-dogfood` (commit below), not yet
merged.

Let `tm` treat GitHub Issues as a first-class ticket backend, selected per
repo, so a GitHub-only project (tskmstr itself included) gets the full
board, `tm ready`, lane runs, audits, and retros without a single Jira
artifact. See GitHub issue #3 for the full design (label taxonomy, `GH-123`
key shape, `TicketQuery`/ADF abstraction, dependencies via GraphQL, etc.);
this document tracks phase-by-phase implementation status and records
anything a phase revealed that the issue's design didn't anticipate.

## Phase 1 — provider trait, Jira behind it (complete, commit `1446509`)

Introduced `src/ticketing/provider.rs`:

- `TicketProvider`, a backend-agnostic trait carrying the same fourteen
  operations `JiraClient` exposes today, unchanged in shape (a ticket key is
  still a plain `&str`, a description is still an ADF `serde_json::Value`,
  every error is still `JiraError`). Abstracting JQL/ADF further is phase 2's
  job, not this one's.
- `JiraProvider`, a thin wrapper around a boxed `JiraClient` used by
  production wiring (`main.rs`'s `jira_client_for`/`build_ticketing_deps`,
  `cli::auth`'s injected factory, `tui::event`'s `TuiDeps` construction) to
  turn a live `HttpJiraClient` into a `Box<dyn TicketProvider>`.
- A direct `impl TicketProvider for FakeJiraClient` (delegating to the
  `JiraClient` impl it already has), rather than routing the fake through
  `JiraProvider`.

Retyped every `&dyn JiraClient` / `Box<dyn JiraClient>` caller onto
`&dyn TicketProvider` / `Box<dyn TicketProvider>`: `src/ticketing/mod.rs`,
`src/cli/{ticket,ready,work,auth}.rs`, `src/tui/event.rs`'s `TuiDeps`, and
`src/work/run.rs`. `src/cli/pr.rs` needed no changes at all — it never named
`JiraClient` directly, only `TicketingContext`, whose field type flowed
through automatically. `src/blocker_stacking.rs` also needed no changes: it
operates purely on `Issue`/`LinkedIssue` values, not a client trait, despite
being named in the issue's phase-1 caller list — that list was slightly
stale against the tree at implementation time. `src/main.rs` needed only two
function signatures touched (`jira_client_for`, `build_ticketing_deps`); every
one of its ~15 call sites downstream picked up the new type automatically.

Pure refactor, zero behavior change. Full test suite (1810 tests), clippy,
and fmt all pass.

### The one real design decision this phase forced

The issue's design assumed a conventional wrapper (`JiraProvider` wraps
`JiraClient`, callers take `&dyn TicketProvider`, done). That breaks on
contact with this codebase's actual test style: `FakeJiraClient` is a plain
public struct, not `#[cfg(test)]`-gated, and roughly 150 tests across
`src/ticketing/mod.rs`, `src/cli/ticket.rs`, `src/cli/pr.rs`,
`src/cli/ready.rs`, and `src/work/run.rs` construct one, pass a *reference*
into a context struct, run the operation under test, and then call
`fake.some_method_calls()` on that same reference afterward to assert what
was recorded.

A `JiraProvider(Box<dyn JiraClient>)` that owns its client — the natural
reading of "wraps `JiraClient`" — takes the fake by value, so wrapping it at
the construction site moves it into the box and the test's later
`.calls()` assertion no longer has anything to call it on. Rewriting ~150
tests to route through a wrapper handle (the pattern `src/cli/auth.rs`
already used once, for its factory closure, via a `Rc`-backed
`FakeJiraClientHandle`) would have been exactly the "test rewritten
wholesale" the task explicitly ruled out.

The fix: `FakeJiraClient` implements `TicketProvider` directly, alongside
its existing `JiraClient` impl, rather than exclusively through
`JiraProvider`. Both impls exist side by side (disambiguated with
fully-qualified `JiraClient::method(...)` syntax in the one place that
needed it, `cli::auth`'s `FakeJiraClientHandle`, which still implements the
older `JiraClient` trait to plug into `JiraProvider`). `JiraProvider` itself
stays exactly as the issue described, and is still what production wiring
uses. This is the same shape as the issue's own suggested escape hatch ("a
`FakeTicketProvider` that delegates") — implemented as a second trait impl
on the existing type instead of a new wrapper struct, since that's what let
the ~150 tests keep compiling completely unmodified.

## Phase 2 — `TicketQuery` + description abstraction (complete, commit `721a331`)

Added `TicketQuery` to `src/ticketing/provider.rs`: `MyOpen`, `Unassigned`,
`Everyone`, `Assignee`, `Ranked`, `Search`, `ShippedAwaitingRetro`, plus
`ReadyCandidates` (see below). `TicketProvider::search` takes a `&TicketQuery`
instead of a JQL string; a private `render_jql` in `provider.rs` renders it
via `src/jira/jql.rs`'s existing builders, which stay exactly where they were
and unchanged. Both `JiraProvider::search` and `FakeJiraClient`'s
`TicketProvider` impl call `render_jql`, so no caller outside `src/jira/`
builds a JQL string anymore -- `src/tui/app.rs`'s `jql_for_filter` became
`query_for_filter`, returning a `TicketQuery`; `Cmd::FetchTickets`/
`FetchRankTickets`/`FetchRetroTickets` carry a `TicketQuery` instead of a
`jql: String`; `src/ticketing/mod.rs`'s `search_tickets`/`ready_tickets`
build `TicketQuery::Search`/`TicketQuery::ReadyCandidates` instead of calling
`ticket_search_jql`/`ready_candidates_jql` directly.

`TicketProvider::create_issue` now takes a new `NewTicket` struct (mirroring
`CreateIssueRequest` but with `description: String`, plain Markdown) instead
of Jira's `CreateIssueRequest`; `update_description` and `add_comment` take
`&str` (Markdown) instead of `&serde_json::Value` (ADF). `JiraProvider` and
`FakeJiraClient`'s `TicketProvider` impl both convert via
`crate::jira::adf::text_to_adf` before delegating to the wrapped
`JiraClient`, so `FakeJiraClient`'s recorded calls (`create_issue_calls()`,
`update_description_calls()`, `add_comment_calls()`) still carry ADF and
every test asserting on that content needed no changes. A new
`TicketProvider::description_text(&Issue) -> String` method covers the read
direction (`src/cli/ticket.rs`'s `audit_read`, `src/tui/event.rs`'s
`to_ticket_summary`), rendering via `crate::jira::adf::adf_to_text`
internally instead of callers calling it themselves. `src/jira/adf.rs`
itself is unchanged.

Full test suite (1818 tests, up from 1810 at phase 1's tip -- phase 2 added
provider-level tests for `render_jql` and the ADF/Markdown conversions),
clippy, and fmt all pass.

### The one real design decision this phase forced

The issue names seven `TicketQuery` variants, but `src/jira/jql.rs` has an
eighth builder, `ready_candidates_jql` (used by `tm ready`'s no-key search:
the current user's "To Do" tickets in rank order), that doesn't map onto any
of the seven. It's not `Ranked` (that's project-wide with no assignee
filter) and not `MyOpen` (that's every open status, not just "To Do"). Since
leaving it as a bare JQL-string call site would have broken the phase's own
goal ("no caller outside `src/jira/` builds a JQL string"), this adds an
eighth variant, `TicketQuery::ReadyCandidates`, not in the issue's list.

### What phase 2 revealed about phases 3-7

`CreateIssueRequest` (`src/jira/types.rs`) stays exactly as the issue
described it -- Jira's wire-shaped request, still used unchanged by
`JiraClient::create_issue` and every existing Jira-level test. The new
`NewTicket` struct in `provider.rs` is a separate, smaller type that only the
`TicketProvider` layer sees; `JiraProvider` builds a `CreateIssueRequest` out
of one, rather than `CreateIssueRequest` itself growing a Markdown
`description` field. A future `GithubProvider` (phase 5/6) will need its own
analogous "new issue" translation, since GitHub has no `issue_type_name`
concept and `NewTicket` currently carries one straight through -- phase 6
should decide whether that field becomes optional/ignored for GitHub or
whether `NewTicket` itself needs to shed Jira-specific fields at that point.

## Phase 3 — config: `[backend]` provider selection (complete, commit `87774a7`)

Added `[backend]` (`provider = "jira" | "github"`, defaulting to `jira`)
to `src/config/mod.rs`. A new `BackendKind` enum (`Jira`, `Github`) is
parsed from the `provider` string by `merge_backend`, then dispatched by
exactly one `match` inside `merge`: the `Jira` arm requires
`jira_base_url`/`jira_email`/`default_project_key` exactly as before
`[backend]` existed (`ConfigError::MissingField` on absence, unchanged);
the `Github` arm returns a new `ConfigError::ProviderNotImplemented`
unconditionally, since no `GithubProvider` exists yet. A provider string
that doesn't parse into `BackendKind` at all (anything but `"jira"`/
`"github"`) is a new `ConfigError::InvalidProvider`. `Config` gained a
`backend: BackendKind` field, always `Jira` on any `Config` that
successfully merged (selecting `github` fails merging outright, so a
`Config` carrying `Github` can never exist yet).

An existing config with no `[backend]` table needs zero changes: absence
in both global and repo config defaults to `Jira` via `BackendKind`'s
`#[derive(Default)]`, so behavior for every pre-phase-3 config is
identical before and after. `tm auth login`'s bootstrap
(`cli::auth::bootstrap_config`) still writes the pre-phase-3 flat shape
(no `[backend]` table at all) and still works, for the same reason. Full
test suite (1829 tests, up from 1818 -- 12 new tests added, one incidental
net change from an existing test's assertion style caught by `cargo fmt`),
clippy, and fmt all pass.

### The one real design decision this phase forced

The issue's own `[backend]` sketch put `repo` (a GitHub-only setting)
flat alongside `provider`, and implied Jira's fields might eventually
move into a matching `[backend.jira]` table. Phase 3 deliberately does
neither: `repo` isn't added at all yet, since nothing consumes it before
`GithubProvider` exists (phase 5/6's job, once there's a config-shape
decision an implementation can actually validate against) and adding an
unused field now would be speculative. And Jira's `jira_base_url`/
`jira_email`/`default_project_key` stay exactly where they've always
been -- flat, top-level, outside `[backend]` entirely -- rather than
moving under `[backend.jira]`, because the repo owner's live
`~/.config/tskmstr/config.toml` already has them at top level and moving
them would be a breaking migration for zero behavioral gain. See
ADR-0003's addendum for the full reasoning; `docs/decisions/0003-ticket-providers.md`
is amended rather than superseded.

### What phase 3 revealed about phases 4-7

Phase 5/6, when they add `GithubProvider`, will need to decide `[backend.github]`'s
shape from scratch (at minimum a `repo` field, defaulting to the origin
remote per the issue) and add a new arm to `merge`'s `BackendKind` match
that validates it -- that match is the one and only place in the codebase
a third phase (or a fourth adapter) needs to touch on the config side.
Nothing about phase 4 (`GhCli` issue operations, which doesn't touch
config at all) changes.

## Phase 4 — `GhCli` issue operations (complete, commit `59efbed`)

Added six methods to `GhCli` (`src/github/gh_cli.rs`), all `Result<T,
GhError>` and classified by the existing `is_permanent()` convention
unchanged:

- `issue_view(repo, number)` — `gh issue view <number> -R <repo> --json
  number,url,title,body,state,labels,assignees`, parsed into a new
  `IssueInfo` (labels/assignees flattened from `gh`'s `{name}`/`{login}`
  object shape to plain strings).
- `issue_list(repo, filter)` — `gh issue list -R <repo> --state ... --limit
  ... [--label ...] [--assignee ...] --json ...`. `IssueListFilter` always
  carries an explicit `limit` (default 200, matching `pr_list`'s bound) —
  `gh issue list` defaults to 30, same pitfall as `gh pr list`.
- `issue_create(repo, req)` — `gh issue create -R <repo> --title ... --body
  ... [--label ...] [--assignee ...]`. `gh issue create` has no `--json`
  support and prints only the created issue's URL, so the number is parsed
  out of that URL and a follow-up `issue_view` fetches the full `IssueInfo`
  — the same two-step shape `pr_create` uses via `pr_view`, adapted because
  there's no "current issue" analogous to "the PR for the current branch" to
  re-resolve by.
- `issue_edit(repo, number, req)` — label/assignee changes via `gh issue
  edit -R <repo> --add-label/--remove-label/--add-assignee/
  --remove-assignee` (skipped entirely if `req` carries none, since `gh
  issue edit` errors on zero flags and a state-only edit is common), plus,
  if `req.state` is set, a following `gh issue close`/`gh issue reopen -R
  <repo>` — two separate `gh` subcommands under the hood, matching the
  design doc's transition model ("a label swap ... plus, for Done/Reopen, a
  close/reopen").
- `issue_comment(repo, number, body)` — `gh issue comment <number> -R
  <repo> --body <text>`, plain Markdown, mirroring `pr_comment`.
- `issue_dependencies(repo, number)` — `gh api graphql` querying the
  `Issue.blockedBy`/`Issue.blocking` connections GitHub's native issue
  dependencies feature exposes (confirmed against the live GraphQL schema
  via introspection during this phase — see below), fetching up to 100
  issues per side, the same single-page bound `pr_review_threads` and
  `pr_bot_finding_details` already use.

`FakeGhCli` gained matching `with_*`/`*_calls()` builders for all six
methods, following the existing per-method builder pattern exactly (see
`with_issue_view`/`with_issue_dependencies` for the two places this phase's
unconfigured-default choice departs from the norm, below).

Full test suite (1862 tests, up from 1829 — 33 new tests: arg construction,
JSON/GraphQL response parsing, error classification, and `FakeGhCli`
call-recording, in the same style already used for `pr_*`), clippy, and
fmt all pass.

### The one real design decision this phase forced

Every new method takes `repo: &str` (an `"owner/name"` slug) instead of the
`dir: &Path` the `pr_*` methods take. The `pr_*` methods resolve their
target repository from a git checkout (`gh repo view` run with
`.current_dir(dir)`), because a PR is only ever meaningful relative to a
branch actually checked out somewhere. Issue operations have no such
anchor: `GithubProvider` (phase 5/6) will be driven entirely by
`[backend].repo` from config, and the board or a lane run may target that
repo without it being the invoking process's cwd, or checked out locally at
all. Requiring a checkout just to shell out `gh issue view` would be a real
new constraint the design doc never asked for. Every new method instead
passes `-R <repo>` directly; only `issue_dependencies` (which needs owner
and name as separate GraphQL variables, not a single flag) splits the slug
first, via a new pure `split_repo_slug` that classifies a malformed slug as
`GhError::Parse` naming the offending string — a `tm`-side config/caller
bug, not something `gh` itself would ever say on stderr.

A smaller decision: this phase's issue-dependencies GraphQL query
(`ISSUE_DEPENDENCIES_QUERY`) was written against the schema confirmed live
via `gh api graphql` introspection (`Issue.blockedBy`/`Issue.blocking`,
each an `IssueConnection` taking `first: Int`), rather than assumed from
the design doc's prose — GitHub's native issue-dependencies feature reached
general availability only in August 2025, after this design doc's own
Jira-parity table was written, so its exact field names weren't a safe
guess.

### What phase 4 revealed about phases 5-7

`GithubProvider`'s methods (phase 5/6) can call every `GhCli` issue method
with just `[backend].repo` — no git checkout, `dir`, or cwd resolution
needed anywhere in the read or write path. This confirms the carry-forward
note's assumption that a repo config field is sufficient; nothing further
needs deciding on that front.

`FakeGhCli::issue_view`'s unconfigured-number default is `Err`, not the
`Ok(trivially empty)` default every other unconfigured `Fake*` lookup in
this file uses — an issue number is always caller-supplied (unlike, say, a
PR-for-branch lookup where "none" is a normal outcome), so a silent blank
result would mask a phase 5/6 test's forgotten `with_issue_view` setup
rather than model a real "not found" case. `issue_dependencies` keeps the
existing empty-default convention instead, since "no dependencies" *is* a
normal, common result. Phase 5/6's own test setup should follow this same
split when deciding what an unconfigured fake should do, rather than
defaulting to "empty" uniformly.

`IssueEditRequest.state` bundles a close/reopen into the same request shape
as label/assignee edits, issuing up to two separate `gh` calls internally.
Phase 6's transition-as-label-swap logic can call `issue_edit` once per
transition (labels plus, for `Done`/`Reopen`, `state`) rather than
sequencing `issue_edit` and a separate close/reopen method itself — that
sequencing already lives inside `GhCli::issue_edit`.

## Phase 5 prep — provider-owned types (complete, commits `877296d`, `b77baaf`)

Resolved the first carry-forward bullet ("the trait is still Jira-shaped")
before `GithubProvider` exists, per that bullet's own instruction. Pure
refactor, zero behavior change: 1870 tests (up from 1862 at phase 4's tip —
8 new tests for the error conversion below), clippy, and fmt all pass.

- **`ProviderError`** (`src/ticketing/error.rs`) replaces `JiraError` as
  every `TicketProvider` method's error type. It's a one-to-one mirror of
  every `JiraError` variant — same names, same fields — so every existing
  `match`/`matches!` on a classification (`NotFound`, `ProjectNotFound`,
  `Unauthorized`, `Api { status, message }`, `RankNotFound`,
  `RankPartialFailure`, `LinkNotFound`, `LinkIdNotFound`) kept working after
  a mechanical rename to the `ProviderError` path. The one field that
  changed shape: `Http` carries a formatted `String` instead of a live
  `reqwest::Error`, so a non-HTTP adapter (a `gh` shell-out) never needs to
  fabricate one. `JiraProvider` and `FakeJiraClient`'s `TicketProvider` impl
  convert via `From<JiraError> for ProviderError` at the boundary (each
  method body became `Ok(self.0.method(...)?)`, letting `?` invoke the
  conversion). `TicketingError::Jira` and `AuthCliError::Jira` were renamed
  to `...::Provider` to carry `ProviderError` instead.
- **Read-path types moved, not wrapped.** `Issue`, `IssueFields`,
  `IssueLink`, `IssueLinkType`, `LinkedIssue`, `LinkedIssueFields`, `Status`,
  `StatusCategory`, `UserRef`, `JiraUser`, `Myself`, `Transition`,
  `SearchResult`, `RemoteLinkRequest`, and `CreateLinkRequest` moved
  bodily from `src/jira/types.rs` to `src/ticketing/types.rs`, together with
  their deserialization tests — no re-export shim, no field renaming. Every
  `use crate::jira::types::X` outside `src/jira/` became
  `use crate::ticketing::types::X`, a mechanical import-path swap;
  `FakeJiraClient`-based tests needed no rewriting beyond that (the phase 1
  lesson this was deliberately designed around) plus five error-message
  string assertions that changed wording ("Jira API error" → "ticket
  provider API error"). `RemoteLinkRequest::to_payload`/
  `CreateLinkRequest::to_payload` — genuinely Jira-specific wire mapping
  (ADF-adjacent JSON shaping, the inward/outward link direction quirk) —
  stayed behind as inherent impls in `src/jira/types.rs`, legal because
  inherent impls only require the type to live in the current crate, not
  the current module. `CreateIssueRequest` stayed in `src/jira/types.rs`
  outright: phase 2 already replaced it at the `TicketProvider` boundary
  with the backend-neutral `NewTicket`, so it never leaked through the
  trait and had nothing to move.
- **What still names a Jira type, on purpose.** `JiraProvider::create_issue`
  (`src/ticketing/provider.rs`) builds a `crate::jira::types::CreateIssueRequest`
  to call the wrapped `JiraClient` — inherent to being the Jira→provider
  boundary, not a leak. `src/ticketing/error.rs` names
  `crate::jira::client::JiraError` for the same reason: it's the boundary
  conversion's input type. `src/cli/auth.rs`'s test-only
  `FakeJiraClientHandle` implements the raw `JiraClient` trait directly (a
  `tm auth login`/`status` test-plumbing detail predating this refactor), so
  it necessarily names `JiraError` and the Jira wire types too. None of
  these are reachable from `GithubProvider`'s side of the trait.
- **`"Blocks"` stayed hardcoded — deferred, not folded in.** The issue's
  design flags `open_blockers` (`src/ticketing/mod.rs`) and
  `direct_blockers` (`src/blocker_stacking.rs`) hardcoding the literal
  `"Blocks"` as something that becomes a provider-supplied constant. Left
  alone here: both are pure functions taking `&Issue` with no
  `TicketProvider` reference in scope, called from `src/cli/ready.rs` and
  `src/work/run.rs` — threading a provider-supplied constant through would
  mean a real signature change to every call site, not a mechanical
  import-path swap, and risks scope creep into behavior this phase
  promised to leave untouched. Phase 5/6 should decide it alongside
  `GithubProvider`'s own link-type story (native GitHub issue dependencies
  have no `"Blocks"`-named type at all — see the issue's dependencies
  section), since that's when the right shape for the constant (a
  `TicketProvider` method? a module-level default some adapters override?)
  becomes answerable from real usage instead of guessed at.

### What phase 5 prep revealed about phases 5-7

`GithubProvider`'s methods can now return `ProviderError` and the types in
`src/ticketing/types.rs` directly — no Jira type needs impersonating, and
nothing about `TicketProvider`'s signatures needs to change again for that
reason. The `"Blocks"`-constant question (previous bullet) and the
`NewTicket.issue_type_name` and fat-trait-vs-capability-traits questions
below are the remaining open items before/during phase 5-6 implementation.

## Phase 5 — `GithubProvider`, read path (complete, branch `issue-3-phase-5-github-read-path`)

Commits, in order: `29b9f3b` (`GhCli::repo_assignees`/`label_create`),
`9494fe8` (`GithubProvider`), `cf2b544` (`[backend.github]`/`[backend.jira]`
config), `26424fd` (main.rs wiring + `tm backend init-labels`), `bd8f1f6`
(routing every `tm ticket`/`tm ready` code path through the configured
backend, not just the board). Full test suite 1929 tests (up from 1870 at
phase 5 prep's tip — 59 new tests), clippy, and fmt all pass.

**`GhCli` gained two methods** (`src/github/gh_cli.rs`), following phase 4's
exact conventions: `repo_assignees(repo)` (`gh api repos/{repo}/assignees`,
parsed to a `Vec<String>` of logins) and `label_create(repo, name, color,
description)` (`gh label create ... --force`, idempotent by construction).
`FakeGhCli` gained matching `with_repo_assignees`/`repo_assignees_calls` and
`with_label_create_result`/`label_create_calls` builders.

**`GithubProvider`** (`src/ticketing/github_provider.rs`) implements
`TicketProvider` over `&dyn GhCli` + a configured `repo` slug, borrowing
rather than owning its `GhCli` for the same reason phase 1's
`FakeJiraClient` bypassed `JiraProvider` — tests construct a `FakeGhCli`,
pass a reference into `GithubProvider::new`, and inspect its recorded calls
by that same reference afterward.

- **Status synthesis** (`synthesize_status_slug`): a closed issue is always
  `Done`, regardless of any `tm:status/*` label it still carries; an open
  issue with more than one status label (not a state `tm` itself produces,
  but not one it can prevent) resolves by fixed priority `blocked` >
  `in-review` > `in-progress` > `todo`; no label, or an unrecognized label,
  both mean `todo`.
- **Transitions** are synthesized, never fetched: a closed issue offers only
  `Reopen` (→ `todo`); an open issue offers every status but its current one,
  plus `Done`. Applying one (`transition`) is a `tm:status/*` label swap via
  `issue_edit`, plus a close/reopen for `Done`/`Reopen` — `issue_edit`
  already sequences both `gh` calls internally (phase 4's design), so this
  is one call site, not two.
- **`search`** renders each of the eight `TicketQuery` variants to an
  `issue_list` call plus, where `gh issue list`'s filter shape doesn't cover
  it, client-side work: `Unassigned` filters out issues with any assignee
  after the fact (no "has no assignee" filter exists); `Ranked` and
  `ReadyCandidates` sort ascending by issue number (no local rank table this
  phase — see below); `Search` filters on summary substring,
  case-insensitively; `ShippedAwaitingRetro` returns every closed issue with
  no lookback-window filtering, since `IssueInfo` carries no closed-at
  timestamp to filter by (a real gap, not a placeholder — see below).
- **Dependencies** (`get_issue`) come from `GhCli::issue_dependencies` and
  become `LinkedIssue`s under the link type name `"Blocks"`, matching the
  hardcoded string `src/blocker_stacking.rs` and `open_blockers` already key
  off of — the "Blocks"-becomes-a-constant carry-forward item from phase 5
  prep is still open (see below), but this phase's shape doesn't block it: a
  future constant would replace this one literal, not restructure the
  method.
- **`assignable_users`** ignores its `project` argument (GitHub has no
  concept narrower than the whole repo) and maps `repo_assignees`'s logins
  to `{ id: login, display_name: login }`, per the issue's design.
- **Every write-path method is a stub**: `create_issue`, `add_remote_link`,
  `assign`, `rank`, `create_link`, `delete_link`, `update_description`,
  `add_comment` all return a distinct `ProviderError::Api { status: 501,
  message: "<method> is not yet implemented for the github backend ..." }`
  rather than panicking or silently no-oping — phase 6's job.
- **`description_text`** reads the GitHub issue body back out of
  `Issue.fields.description`, which `GithubProvider` populates as
  `serde_json::Value::String(body)` (no ADF; GitHub bodies are already
  Markdown) rather than `None` for an empty body.

**Config**: `[backend.github]` gained a `repo` field (`"owner/name"`,
required — `ConfigError::MissingField` naming `backend.github.repo` if
absent and undefaulted). It can be omitted: `load` (not `merge`, which stays
a pure function of its `RawConfig` arguments and never shells out) tries
`git config --get remote.origin.url` in the repo-local config's directory
(or the cwd, if there's no repo-local config), parsing both the SSH and
HTTPS URL shapes GitHub hands out. `merge`'s `Github` arm, previously an
unconditional `ConfigError::ProviderNotImplemented`, now validates `repo`
the same way the `Jira` arm validates its three fields. Per the carry-forward
decision, `[backend.jira]` is now the canonical (documented) location for
`jira_base_url`/`jira_email`/`default_project_key`, with the legacy flat
top-level keys read as a silent fallback when the canonical field is absent
— see ADR-0003's phase 5 addendum.

**`tm backend init-labels`** (`src/cli/backend.rs`, new `Command::Backend`/
`BackendCmd` clap surface) creates the four `tm:status/*` labels via
`gh label create ... --force`, one `gh` call per label — `--force` alone
makes it idempotent, so no check-then-create round trip was needed.
Running it under the Jira backend is `BackendCliError::NotGithubBackend`
without calling `gh` at all.

**Wiring**: `main.rs` gained `ticket_provider_for(config, keychain,
env_token)`, the one place that branches on `config.backend` to build either
a real Jira client (as before) or a `GithubProvider` over a leaked
`&'static dyn GhCli` (see that function's doc comment for why leaking a
zero-sized `ShellGhCli` is fine for a short-lived CLI process).
`build_ticketing_deps` and every inline `tm ticket`/`tm ready` dependency
construction in `main.rs` now goes through it instead of calling
`jira_client_for` directly, so `tm board`, `tm ticket search`, `tm ready`,
and every other `tm ticket <subcommand>` all honor the configured backend —
not just the board, which is all the issue's own phase 5 scope named.
`tm auth login`/`tm auth status` print a one-line "not applicable to the
github backend, use `gh auth login`/`gh auth status`" message and return
under the GitHub backend rather than running the Jira-specific flow (which
would otherwise fail outright with no Jira config to bootstrap); a fuller
GitHub-aware `tm auth` (e.g. actually shelling out to `gh auth status`) is
deferred to phase 7, per this phase's own scope note allowing that.

### The one real design decision this phase forced

`GithubProvider` needing to inspect a `FakeGhCli`'s recorded calls after
running an operation — the same shape phase 1 hit with `FakeJiraClient` and
`JiraProvider` — has a different fix here, because the two constraints don't
actually collide the way they did in phase 1. Phase 1's fix (implement the
trait a second time, directly on the fake) was needed because `JiraProvider`
was specified to *wrap* `JiraClient`, and ~150 existing tests already
constructed a `FakeJiraClient` by reference; rewriting them to route through
an owning wrapper was explicitly ruled out. Neither constraint applies to a
brand-new type: no `GithubProvider`-based test existed yet to preserve, so
there was nothing wrong with just giving `GithubProvider` a lifetime
parameter and a borrowed `&'a dyn GhCli` field from the start — tests pass a
`&FakeGhCli` straight in and keep their own reference for `.calls()`
afterward, no second trait impl needed. The cost shows up only in
production wiring, where `Box<dyn TicketProvider>` demands `'static`: fixed
by leaking a freshly constructed `ShellGhCli` (a zero-sized unit struct)
once per `tm` invocation, which is a real trade worth naming but not one
that costs anything in practice for a process that exits after one command.

### What phase 5 revealed about phases 6-7

- **Fat trait vs. capability traits: the threshold is now crossed.**
  `GithubProvider` has eight stub methods (`create_issue`,
  `add_remote_link`, `assign`, `rank`, `create_link`, `delete_link`,
  `update_description`, `add_comment`) against phase 5 prep's carry-forward
  note that named "more than two or three" as the signal to split
  `TicketProvider` into capability traits (`TicketRead`/`TicketWrite`/
  `Rankable`/`Linkable`/`Transitionable`). Phase 6 implementing several of
  these will shrink the stub count, but not below the threshold on its own
  (`rank` has no GitHub equivalent to implement at all — see below) — phase
  6 should make the split call explicitly rather than let the stub count
  quietly stay past the line the previous phase drew.
- **The local rank table didn't land this phase — and that's a real,
  user-visible gap, not a formality.** `TicketQuery::Ranked` and
  `ReadyCandidates` both sort by plain ascending issue number; there is no
  `runs.db` `ticket_rank` table yet, so "unranked issues fall to the end"
  degenerates to "every issue is unranked, so everything sorts by number."
  Phase 6 (or a dedicated sub-phase before it) needs to actually add the
  local rank table the issue's design describes, wire `GithubProvider::rank`
  to write to it, and wire `search`'s `Ranked`/`ReadyCandidates` branches to
  read from it instead of issue-number order.
- **`ShippedAwaitingRetro` has no time-window filtering.** `IssueInfo`
  carries no closed-at/updated-at timestamp, so `GithubProvider::search`
  returns every closed issue rather than ones that shipped within the retro
  lookback window the Jira JQL builder honors. Fixing this needs a new
  `GhCli` field (GitHub's REST/GraphQL APIs both expose `closedAt`) threaded
  through `issue_list`'s JSON fields and `IssueInfo`.
- **The `"Blocks"`-string-becomes-a-constant question, carried forward from
  phase 5 prep, is still open.** `GithubProvider::get_issue` hardcodes
  `"Blocks"` the same way `src/blocker_stacking.rs`/`open_blockers` already
  do, for the same reason phase 5 prep left it alone: no call site threading
  a provider reference through those two pure functions yet exists, and this
  phase's job was the read path, not that refactor.
- **`NewTicket.issue_type_name`** is still unresolved — `create_issue` is a
  stub this phase, so nothing yet exercises whether it should go optional
  or be ignored for GitHub. Phase 6 (which has to actually implement
  `create_issue`) is where this gets decided for real, from working code
  rather than a signature guess.
- **`Config`'s Jira fields are unconditionally present (as empty strings)
  under the GitHub backend**, rather than the whole `Config` struct being
  reshaped so Jira-only and GitHub-only fields can't coexist meaninglessly.
  This was a deliberate minimal-blast-radius choice (`jira_base_url` etc.
  stay `String`, not `Option<String>`, so the ~15 existing call sites that
  read them don't all need an `.unwrap()`/pattern-match added) rather than a
  considered design position — every one of those call sites is already
  Jira-specific and now runs only on the Jira path via `ticket_provider_for`,
  so the empty strings are never actually read under the GitHub backend, but
  a future phase splitting `Config` into a common part plus a
  `backend`-keyed enum of provider-specific fields would be a cleaner shape
  if a third adapter ever arrives.

## Phase 6 — `GithubProvider`, write path (complete, branch `issue-3-phase-6-github-write-path`)

Commits, in order: `7200dd5` (`IssueEditRequest.body` +
`create_issue_dependency`/`delete_issue_dependency` GraphQL mutations on
`GhCli`), `4613f3c` (the local `ticket_rank` table in `RunStore`), `0c1fb57`
(every `GithubProvider` write-path method), `e54a671` (wiring the rank store
into `main.rs`'s `GithubProvider` construction). Full test suite 1959 tests
(up from 1929 at phase 5's tip — 30 new tests), clippy, and fmt all pass.

**`GhCli` gained a `body` field on `IssueEditRequest`** (`src/github/gh_cli.rs`),
wired into `issue_edit_args` as `--body`: phase 4 only needed label/assignee/
state edits, so the body flag wasn't added until `update_description` needed
it.

**`GhCli` gained `create_issue_dependency`/`delete_issue_dependency`**,
GitHub's native issue-dependencies write side. Confirmed live via `gh api
graphql` introspection during this phase (the read-side query phase 4
introspected covers `blockedBy`/`blocking`, but says nothing about how to
write them): `addBlockedBy`/`removeBlockedBy` mutations, each taking
`input: { issueId, blockingIssueId }` — both opaque GraphQL node ids, not
issue numbers, unlike every other `GhCli` method. Each call is two `gh api
graphql` round trips: a query resolving both issues' node ids by number
(`ISSUE_NODE_IDS_QUERY`, aliasing `blocker`/`blocked` in one request), then
the mutation itself. `FakeGhCli` gained matching
`with_create_issue_dependency_result`/`create_issue_dependency_calls` and
`with_delete_issue_dependency_result`/`delete_issue_dependency_calls`
builders, following phase 4/5's exact pattern.

**The local `ticket_rank` table** landed in `src/runs/mod.rs` as migration
8: `ticket_rank(ticket_key TEXT PRIMARY KEY, rank REAL NOT NULL)`, plus three
`RunStore` methods following the `ticket_audits`/`ticket_retros` convention
exactly — `set_ticket_rank` (upsert, unlike the audit/retro tables' append-only
history, since rank is a single current position not an event log),
`ticket_rank` (single lookup), and `all_ticket_ranks` (every ranked ticket,
ascending by rank, for `GithubProvider::search` to consult).

**`GithubProvider`** (`src/ticketing/github_provider.rs`) implements every
write-path method:

- **`create_issue`**: `gh issue create` with `tm:status/todo` applied as a
  label at creation time (matching the label taxonomy every other issue
  carries) and the requested assignee, if any. See "the one real design
  decision" below for `issue_type_name`.
- **`assign`**: exclusive single-assignee semantics (matching Jira's
  `Option<&str>` signature) built on top of GitHub's multi-assignee model —
  every existing assignee other than the requested one is removed, and the
  requested one is added only if not already present, avoiding a redundant
  add-and-remove-the-same-login round trip. `None` removes every assignee.
  Already-correct state (the requested assignee is already the sole
  assignee) makes no `gh` call at all.
- **`add_comment`**/**`update_description`**: `gh issue comment`/`gh issue
  edit --body`, Markdown pass-through, no ADF involved.
- **`create_link`**/**`delete_link`**: thin wrappers over
  `GhCli::create_issue_dependency`/`delete_issue_dependency`, parsing issue
  numbers out of ticket keys (`create_link`) or the link id itself
  (`delete_link` — see below).
- **`rank`**: writes to the local `ticket_rank` table via a new
  `compute_new_ranks` pure function — fractional-index interpolation between
  the anchor's rank and its nearest neighbor on the requested side (a fresh,
  unranked anchor is first assigned a rank past the current maximum, i.e.
  logically "at the end," matching the design doc's "unranked issues fall to
  the end" for the anchor's own case). Returns `ProviderError::Api` if no
  rank store is attached, rather than silently no-oping.
- **`search`**'s `Ranked`/`ReadyCandidates` branches now call a new
  `apply_local_rank_order` helper: issues with a recorded rank sort first
  (ascending by rank), everything else follows sorted by issue number —
  degenerating to phase 5's plain issue-number order when nothing is ranked
  or no store is attached, so every phase 5 test for those two queries kept
  passing unmodified.
- **`add_remote_link`** is a deliberate no-op returning `Ok(())` — see "the
  one real design decision" below.
- The **error-mapping fix** flagged in phase 5's review: `get_issue`/
  `transitions`/`transition` previously mapped every `issue_view` failure to
  `ProviderError::NotFound` unconditionally, swallowing the difference
  between "the issue doesn't exist" and "`gh` isn't installed" / "the
  process timed out." A new `map_issue_view_error` helper narrows this: only
  `GhError::Command` (the process ran and `gh` itself reported failure —
  the only way `issue_view` fails in practice for a missing issue) becomes
  `NotFound`; `GhError::Spawn`/`Parse`/`Timeout` pass through via
  `ProviderError::from` instead.
- `closedAt` threading for `ShippedAwaitingRetro` (the other phase 5 review
  flag) was **not** done this phase — see "what phase 6 revealed" below.

### The one real design decision this phase forced

Phase 5's synthesized link ids (`gh-dep-blocked-by-<neighbor>`/
`gh-dep-blocking-<neighbor>`) only encoded the *neighbor's* issue number, not
the issue being viewed. That's enough to render a link in `get_issue`'s
result, but `TicketProvider::delete_link` receives only the id, with no
other context (unlike `create_link`, which gets both keys directly) — there
was no way to recover which two issues to call
`GhCli::delete_issue_dependency` on from a one-sided id. The fix: a new id
shape, `link_id(blocker_number, blocked_number) -> "gh-dep-<blocker>-blocks-<blocked>"`,
deliberately symmetric regardless of which of `issue_dependencies`'
connections (`blocked_by` or `blocking`) the link was discovered from — a
`Blocks` relationship is one thing between two issues, not two things, so
both directions produce the same id for the same pair. `parse_link_id` is
the exact inverse, used by `delete_link`; an id that doesn't parse (e.g. a
link fetched under phase 5's old shape, never re-fetched before being
deleted under phase 6) is `ProviderError::LinkIdNotFound`, the same error a
genuinely unknown id would produce, rather than a panic or a silently wrong
mutation.

Two smaller decisions this phase also had to make, both flagged as open in
phase 5's carry-forward list:

- **`NewTicket.issue_type_name` is ignored for GitHub.** An issue is just an
  issue — there's no GitHub concept an issue type could map onto, and
  encoding it as a label (the taxonomy's only extensibility point) would
  conflate two unrelated label namespaces. `create_issue` simply doesn't
  read the field. `NewTicket` itself keeps the field rather than becoming
  `Option`-al or Jira-only via a wrapper type: the field is harmless dead
  weight on the GitHub path, and reshaping the boundary struct now, with
  exactly one adapter that ignores one field, would be solving a problem
  that doesn't exist yet (revisit if a third adapter's needs disagree with
  both existing ones).
- **`add_remote_link` is a no-op, not a body-append.** GitHub already
  renders the PR↔issue backlink for free from the `Closes #123` line
  `associate` (`src/ticketing/mod.rs`) puts in the PR body — nothing
  `add_remote_link` could post would add information already visible on the
  issue page. A body-append was the only real alternative considered, and
  was rejected as strictly worse: it would restate a link GitHub already
  displays natively, and would need to be idempotency-checked against
  `associate`'s own re-invocation, for zero benefit.

### What phase 6 revealed about phase 7

- **The local rank store's lifetime is now the concrete pattern a third
  adapter would follow.** `GithubProvider` gained an `Option<&'a RunStore>`
  field, attached via a `with_rank_store` builder — `None` by default, so
  every phase 5 test that doesn't touch `rank`/`Ranked`/`ReadyCandidates`
  needed no changes. Production wiring (`main.rs`'s `ticket_provider_for`)
  leaks a freshly opened `RunStore` the same way it already leaks a
  `ShellGhCli`, for the same `'static`-required-by-`Box<dyn TicketProvider>`
  reason. `JiraProvider` gained no equivalent field at all — Jira has its
  own native rank and never touches this table, confirming the carry-forward
  note's expectation that this wouldn't need to be threaded through the
  trait itself, only through `GithubProvider`'s own constructor.
- **Fat trait vs. capability traits: the threshold is no longer crossed —
  the fat trait stays.** Phase 5 counted eight stub methods against the
  carry-forward's "two or three" split signal. After this phase, zero
  `TicketProvider` methods on `GithubProvider` return a not-implemented
  stub: every method is either a real implementation or (for
  `add_remote_link`) a deliberate, permanent, documented no-op —
  categorically different from "not built yet." Splitting `TicketProvider`
  into `TicketRead`/`TicketWrite`/`Rankable`/`Linkable`/`Transitionable` now
  would be a refactor in search of a problem: there is no caller that wants
  a narrower trait than the one both adapters already fully implement. This
  carry-forward item is resolved, not deferred — revisit only if a third
  adapter can't implement some slice of the trait at all (as opposed to
  implementing it differently, which both existing adapters already do
  freely).
- **`ShippedAwaitingRetro`'s missing time-window filtering is still open**,
  carried forward unchanged from phase 5: `IssueInfo` still carries no
  closed-at timestamp. This phase touched `create_issue`/`issue_edit`/issue
  dependencies, none of which read or need `closedAt`, so there was no
  natural point to add it without scope creep into a query path this phase
  wasn't otherwise touching. Left for phase 7 (or a dedicated follow-up) —
  needs a new `GhCli` field threaded through `issue_list`'s JSON fields and
  `IssueInfo`.
- **The `"Blocks"`-string-becomes-a-constant question is still open**,
  also carried forward unchanged: `to_issue_links` still hardcodes the
  literal, for the same reason phase 5 left it — no call site threading a
  provider reference through `src/blocker_stacking.rs`/`open_blockers`
  exists yet, and this phase's link work (`create_link`/`delete_link`) only
  ever *sends* dependency mutations, it doesn't touch the two functions that
  read the hardcoded string back.
- **Test doubles remain ad hoc, not a shared conformance suite** — phase 5's
  carry-forward flagged this as worth weighing once a second implementation
  existed to compare against. Having now written the write-path tests, the
  overlap is real (transition synthesis, key parsing, and error-mapping
  shape are structurally identical exercises against `FakeGhCli` vs.
  `FakeJiraClient`) but the two fakes' call-recording APIs differ enough
  (`FakeGhCli`'s per-method `with_*`/`*_calls()` vs. `FakeJiraClient`'s
  broader surface) that extracting a shared suite now would mean designing
  a third abstraction on top of both fakes rather than just writing tests
  against the trait. Left for phase 7 or later to decide with the dogfooding
  workload as a forcing function, rather than speculatively here.

## Phase 7 — dogfood (complete, branch `issue-3-phase-7-dogfood`)

Commits: `469d2d0` (`.tskmstr.toml`), plus this section's doc update. No
source changes were needed — phases 1-6 already made every code path this
phase exercises live. Full test suite still 1959 tests (unchanged from
phase 6's tip), clippy, fmt, and `nix build` all pass.

**Config.** `.tskmstr.toml` at the repo root is just:

```toml
[backend]
provider = "github"
```

No `[backend.github].repo` at all — `[backend.github].repo`'s
defaulting-from-`origin`-remote path (`detect_origin_repo`, added in phase 5)
was verified live rather than assumed: running `./target/debug/tm ready
GH-3` from a checkout of this repo with only `provider = "github"` set
resolved the repo to `jowi-dev/tskmstr` and fetched the real issue
correctly. No bug found; the explicit-`repo`-in-config fallback the task
brief flagged as a possible workaround wasn't needed.

**`tm backend init-labels`**, run live against `jowi-dev/tskmstr`: created
all four `tm:status/*` labels (confirmed via `gh label list`). Re-running it
immediately after is idempotent — `gh label create --force` means the second
run reports the same four "Created label ..." lines with no error and no
duplicate labels (`gh label list` still shows exactly one of each
afterward).

**Existing issues labeled.** Issue #3 (this phase's own in-flight work) is
`tm:status/in-progress`. Issues #1 and #2 are `tm:status/todo` — #1 got there
via `tm ticket transition GH-1 "To Do"` (a no-op reporting "already in To
Do" the first time it was tried, since an unlabeled open issue already
synthesizes as `todo`; #2 hit the same no-op path); the label itself was
then applied to both directly via `gh issue edit --add-label` so the design's
"existing issues labeled" holds literally rather than relying on the
implicit no-label-means-todo reading. No issue was closed, retitled, or had
its body edited.

**Live verification, all via the real `jowi-dev/tskmstr` repo and the real
`gh` CLI:**

- `tm ticket audit GH-3` printed the live issue's title, status, assignee,
  and full Markdown body — confirms `get_issue`/`description_text` work
  live, not just against `FakeGhCli`.
- `tm ready GH-3` returned "GH-3 is ready (To Do)" before the round-trip
  below, and "GH-3 is ready (In Progress)" after — confirms the dependency
  read path (`open_blockers`/`get_issue`'s `Blocks` links) runs live with no
  crash even though GH-3 has no dependencies to find.
- `tm ready` (no key) correctly printed "No ready tickets." — every open
  issue here is unassigned, so the `assignee = @me` filter legitimately
  empties the list; this is the expected result, not a bug.
- Transition round-trip on GH-1 (chosen as the lowest-stakes open issue):
  `tm ticket transition GH-1 "In Progress"` → confirmed via `gh issue view 1
  --json labels` that `tm:status/in-progress` was applied → `tm ticket
  transition GH-1 "To Do"` → confirmed `tm:status/todo` replaced it. Original
  state (no explicit label, reading as `todo`) is restored in spirit (an
  explicit `tm:status/todo` label now, per the labeling pass above, but the
  synthesized status is identical either way).
- `tm ticket transition GH-3` (no target) printed the correct four remaining
  transitions for an in-progress issue (`To Do`, `In Review`, `Blocked`,
  `Done`).

**No live-found bugs.** Every read and write path exercised behaved exactly
as phases 5/6's design predicted; the only thing this phase's live run
actually tested that hadn't been exercised outside unit tests before was
`detect_origin_repo`'s git-remote-URL parsing against this repo's own real
`git@github.com:jowi-dev/tskmstr.git` origin, which worked on the first try.

**tskmstr's own work driven off `tm board`.** Mechanically true as of this
phase (the board reads/writes real issues in this repo), but not yet
exercised as a workflow — no board session was run interactively this phase
beyond the CLI commands above. That's a process change for future sessions
to adopt, not something this phase could "complete" any further by itself.

## Carry-forward decisions for phases 5-7

Open questions phases 1-3 surfaced, recorded here so they get decided rather
than defaulted. Phase 7 was dogfooding, not implementation — it exercised
these paths live but didn't have natural scope to resolve any of the three
still-open items below; they carry forward past phase 7 as real follow-up
work, not phase-7 gaps.

- ~~**Config shape.**~~ Resolved by phase 5: `[backend.github].repo` exists
  (required, defaultable from the checkout's `origin` remote), and
  `[backend.jira]` is documented as `jira_base_url`/`jira_email`/
  `default_project_key`'s canonical location, with the legacy flat top-level
  keys read as a silent fallback (see ADR-0003's phase 5 addendum). Every
  adapter now has the same `[backend.<name>]` shape; nothing about the flat
  keys had to break.
- ~~**The trait is still Jira-shaped.**~~ Resolved by the phase 5 prep work:
  `TicketProvider` returns `ProviderError` and the provider-owned types in
  `src/ticketing/types.rs`, not `JiraError`/`crate::jira::types::*`. The
  `"Blocks"`-string-becomes-a-constant item that work deferred is still open
  — see below.
- ~~**Fat trait vs. capability traits.**~~ Resolved by phase 6, the other
  direction from what phase 5 expected: rather than crossing further past
  the "two or three" split threshold, implementing the remaining methods
  brought the stub count to *zero* (every method is either a real
  implementation or `add_remote_link`'s deliberate, permanent no-op). The
  fat trait stays — see phase 6's "what phase 6 revealed" section for the
  reasoning. Revisit only if a future third adapter genuinely can't
  implement some slice of the trait at all.
- ~~**The local rank table doesn't exist yet.**~~ Resolved by phase 6: the
  `runs.db` `ticket_rank` table exists (migration 8), `GithubProvider::rank`
  writes to it via `compute_new_ranks`, and `search`'s `Ranked`/
  `ReadyCandidates` branches read from it via `apply_local_rank_order`.
- ~~**`NewTicket.issue_type_name`.**~~ Resolved by phase 6: ignored for
  GitHub (an issue has no type concept to map it onto); the field stays on
  `NewTicket` rather than becoming `Option`-al, since it's harmless dead
  weight with only one adapter ignoring it so far. See phase 6's "the one
  real design decision" section.
- **`ShippedAwaitingRetro` still has no time-window filtering** — `IssueInfo`
  carries no closed-at timestamp, so `search` returns every closed issue
  under the github backend. Needs a new `GhCli` field (`closedAt`) threaded
  through `issue_list` before this query variant means the same thing under
  both backends. Not touched by phase 6 (see that phase's report); still
  open for phase 7 or a dedicated follow-up.
- **`"Blocks"`-string-becomes-a-constant** is still open: `GithubProvider`
  hardcodes it the same way `src/blocker_stacking.rs`/`open_blockers`
  already do, for the same reason every prior phase left it alone — no call
  site threading a provider reference through those two pure functions
  exists yet, and phase 6's link work only sends dependency mutations, it
  never reads the hardcoded string back.
- **Test doubles remain ad hoc, not a shared conformance suite.** Phase 6
  confirmed real structural overlap now exists (transition synthesis, key
  parsing, error-mapping shape) between `GithubProvider`'s and
  `JiraProvider`'s tests, but the two fakes' call-recording APIs differ
  enough that extracting a shared suite now would mean designing a third
  abstraction rather than just writing tests against the trait. Left for
  phase 7 or later, with the dogfooding workload as a forcing function.
- **`Config`'s Jira fields are always present (as empty strings) under the
  GitHub backend**, rather than `Config` being reshaped so Jira-only and
  GitHub-only fields can't coexist meaninglessly — a deliberate
  minimal-blast-radius choice phase 5 made explicitly rather than solved;
  see that phase's report for the reasoning and the cleaner shape it
  suggests if a third adapter ever arrives. Not touched by phase 6.
