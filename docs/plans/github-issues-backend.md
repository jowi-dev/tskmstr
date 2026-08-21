# GitHub Issues as a ticket backend (issue #3), phases 1-7

Status: **phase 1 complete** (commit `1446509`). Phases 2-7 are specified
below per the issue but not started.

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

## Phase 2 — `TicketQuery` + description abstraction (not started)

Move JQL construction (`src/tui/app.rs`'s raw JQL strings, `src/jira/jql.rs`)
and ADF conversion (`src/jira/adf.rs`) behind the provider, via a
`TicketQuery` enum each provider renders to its own query shape.

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
