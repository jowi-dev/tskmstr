# ADR-0003: Ticket Providers

**Status:** Accepted
**Date:** 2026-08-20
**Amends:** ADR-0001 and ADR-0002's "Jira is the single source of truth for
ticket data" wording (the rule itself is unchanged — see below)

## Problem

Every ticket-shaped operation in tskmstr goes through `src/jira/client.rs`'s
`JiraClient` trait, and `~/.config/tskmstr/config.toml` requires Jira
credentials unconditionally. That means a GitHub-only project — including
tskmstr's own repo — cannot get a board, `tm ready`, lane runs, or audits
without standing up a parallel Jira project it doesn't otherwise need. GitHub
issue #3 tracks making GitHub Issues a first-class ticket backend, selected
per repo; that work has to start by giving tskmstr's ticketing layer a
backend-agnostic seam to plug a second implementation into.

## Decision

1. **A `TicketProvider` trait replaces `JiraClient` at every call site.**
   `src/ticketing/provider.rs` defines `TicketProvider`, carrying the same
   fourteen operations `JiraClient` exposes today. Every function and struct
   field in `src/ticketing/mod.rs`, `src/cli/{ticket,ready,work,auth}.rs`,
   `src/tui/event.rs`'s `TuiDeps`, and `src/work/run.rs` that took
   `&dyn JiraClient` / `Box<dyn JiraClient>` now takes the provider instead.

2. **Jira is a `TicketProvider` implementation, not `TicketProvider`
   itself.** `JiraProvider` wraps a boxed `JiraClient` and forwards every
   call unchanged — today it's the only implementation, but the trait itself
   carries no Jira-specific assumption a second backend couldn't also
   satisfy (a JQL string and an ADF `serde_json::Value` still flow through
   its signatures for now; abstracting those further is a later phase's
   job, not this one's).

3. **This phase is a pure retype, not a redesign.** No new features, no
   GitHub code, no config changes. Behavior is identical before and after;
   the existing test suite is the spec, and it passes unchanged throughout.

## Addendum: the config-side adapter contract (phase 3)

Phase 1's decision left one thing unsettled: what does *selecting* a
provider look like in config, and where does a third theoretical adapter
(Linear, Shortcut, a plain-file backend, whatever) plug in on that side?
The answer, added in phase 3 without revisiting decisions 1-3 above:

4. **One discriminant enum, one dispatch site.** `src/config/mod.rs`'s
   `BackendKind` (`Jira` | `Github`) is parsed from an optional `[backend]`
   `provider` string, defaulting to `Jira` when `[backend]` is absent so
   every config written before this phase keeps working unchanged. Exactly
   one `match` on `BackendKind` exists in the whole codebase, in `merge`:
   each arm validates and assembles that adapter's own required fields
   (Jira's arm requires `jira_base_url`/`jira_email`/`default_project_key`,
   as it always has) and nothing outside that arm needs to know what the
   adapter requires. A provider string that doesn't parse into a
   `BackendKind` at all is `ConfigError::InvalidProvider`; a recognized
   variant with no `TicketProvider` impl yet (`Github`, until issue #3's
   later phases land one) is `ConfigError::ProviderNotImplemented` —
   config loading fails cleanly either way rather than panicking or
   silently defaulting to Jira.

5. **Jira's fields stay flat and top-level, not nested under
   `[backend.jira]`.** The issue's own sketch put a future adapter's
   settings in `[backend.<name>]` sub-tables, which is the right shape for
   `Github` (and any later adapter) once one exists — but Jira's
   `jira_base_url`/`jira_email`/`default_project_key` already exist as
   flat top-level keys in every config in the wild, including the repo
   owner's live one. Moving them into a nested table to match the new
   adapters' shape would be a breaking migration for zero behavioral
   gain, so phase 3 deliberately left them where they are: "each adapter
   owns its own config shape" is satisfied by the Jira arm of the `merge`
   match validating exactly the fields Jira needs, wherever in the TOML
   they happen to live, not by every adapter's fields sharing one nesting
   convention.

Adding a real third adapter later means: a new `BackendKind` variant, a new
arm in `merge` that validates that adapter's own fields (nested under
`[backend.<name>]` if it has no legacy flat fields to preserve), a new
`TicketProvider` impl, and registering it at whichever single site
constructs the live provider from a loaded `Config`. Nothing else in
config, the TUI, or any `tm ticket`/`tm ready` command changes.

## What still stands from ADR-0001 and ADR-0002

The no-mirroring rule itself is unchanged: **the configured ticket
provider** (Jira today; a future `GithubProvider` per issue #3) **is the
single source of truth for ticket data; tskmstr never mirrors it.** Where
ADR-0001 and ADR-0002 named "Jira" specifically, read "the configured ticket
provider" — the boundary they drew was never about Jira as a product, it was
about tskmstr not caching remote ticket state locally. Everything else in
both ADRs — run state and events in SQLite, transcripts on disk, hook-based
telemetry, tskmstr spawning and supervising sessions, a small status
vocabulary — stands as written.
