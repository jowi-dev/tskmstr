# GitHub Issues as a ticket backend (issue #3), phases 1-7

Status: **phases 1-2 complete** (commits `1446509`, `721a331`). Phases 3-7
are specified below per the issue but not started.

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

## Phase 3 — config: `[backend]` provider selection (not started)

Add the `[backend]` table (`provider = "jira" | "github"`, `repo`), make the
Jira config fields conditionally required, add `ConfigError::InvalidProvider`.

## Phase 4 — `GhCli` issue operations (not started)

`issue_view`, `issue_list`, `issue_create`, `issue_edit`, `issue_comment`,
plus the dependencies GraphQL query, following the existing `pr_*` methods'
`Result<T, GhError>` + `is_permanent()` conventions.

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
