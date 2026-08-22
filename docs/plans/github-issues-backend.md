# GitHub Issues as a ticket backend (issue #3), phases 1-7

Status: **phases 1-3 complete** and merged to `main` (merge commits
`f86ff4f`, `5ae20bc`, `c00e95e`; the underlying work is `1446509`, `721a331`,
and phase 3's commits below). **Phase 4 complete** on branch
`issue-3-phase-4-gh-issue-ops` (commit `59efbed`), not yet merged. **Phase 5
prep complete** on branch `issue-3-phase-5-prep-provider-types` (commits
`877296d`, `b77baaf`), not yet merged. Phases 5-7 are specified below per the
issue but not started.

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

## Phase 5 — `GithubProvider`, read path (not started)

The six board methods (`search`, `transitions`, `transition`,
`assignable_users`, `rank`, `get_issue`) plus `get_issue`, so `tm board`,
`tm ticket search`, and `tm ready` work against GitHub Issues.
`tm backend init-labels` ships here.

## Phase 6 — `GithubProvider`, write path (not started)

Transitions as label swap + close/reopen, assign, comment, create, update
body, links, local rank table.

## Phase 7 — dogfood (not started)

`.tskmstr.toml` with `provider = "github"` in this repo, existing issues
labeled, tskmstr's own work driven off `tm board`.

## Carry-forward decisions for phases 5-7

Open questions phases 1-3 surfaced, recorded here so they get decided rather
than defaulted:

- **Config shape.** Phase 3 kept `jira_base_url`/`jira_email`/
  `default_project_key` flat and top-level to avoid breaking a live config, so
  the config surface is only half adapter-keyed: `[backend] provider` selects
  the adapter, but Jira's own fields sit outside any adapter table. When phase
  5 adds `[backend.github].repo`, document `[backend.jira]` as the canonical
  location for Jira's fields while `merge` keeps reading the flat keys as a
  silent fallback. That gives every adapter the same shape without a breaking
  migration.
- ~~**The trait is still Jira-shaped.**~~ Resolved by the phase 5 prep work
  above: `TicketProvider` returns `ProviderError` and the provider-owned
  types in `src/ticketing/types.rs`, not `JiraError`/`crate::jira::types::*`.
  The one item that work explicitly deferred rather than folded in: the
  `"Blocks"`-string-becomes-a-constant question (see that section's last
  bullet).
- **Fat trait vs. capability traits.** One 14-method trait forces
  `GithubProvider` to answer for operations GitHub has no equivalent of (rank,
  transitions). If phases 5-6 produce more than two or three
  `Unsupported`-style stubs, split the trait into narrower capabilities
  (`TicketRead`/`TicketWrite`/`Rankable`/`Linkable`/`Transitionable`) so the
  board can hide a keybinding instead of failing on it at runtime. Below that
  threshold, the fat trait is the cheaper shape and should stay.
- **`NewTicket.issue_type_name`** is a Jira-only field sitting on the
  provider-boundary struct. Phase 6 must decide whether it goes optional,
  gets ignored by non-Jira adapters, or moves into an adapter-specific extras
  bag.
- **Test doubles.** Both `JiraProvider` and any future adapter's fake should be
  exercised by a shared conformance suite rather than each fake getting an
  ad-hoc direct `TicketProvider` impl (phase 1 took the ad-hoc route for
  `FakeJiraClient` deliberately, to avoid rewriting ~150 tests).
