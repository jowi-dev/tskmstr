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
