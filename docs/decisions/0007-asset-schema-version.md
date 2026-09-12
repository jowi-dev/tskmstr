# ADR-0007: A Schema-Version Stamp, Plus a Read-Only `tm check`

**Status:** Accepted
**Date:** 2026-09-12

## Problem

`tm init` decides what a repo needs purely by presence — is `.tskmstr.toml`
there, is `[work.audit]` there, is the lane's prompt file there. A re-run
audits and repairs that presence, which is exactly right for keeping one
repo's assets working. It has no way to answer a different question,
though: **has this repo been onboarded by a tskmstr new enough to have
today's full set of expected assets at all?**

That gap shows up whenever tskmstr grows a new expected asset kind — a new
default `[work.*]` session type, a newly-expected skill, a new config key
existing repos must gain. A repo onboarded before that change has no marker
distinguishing "deliberately hasn't got this" from "was never told to get
this." `tm init` re-run *would* catch it up (its presence checks are
already exhaustive over what it knows to check), but there was no
lightweight way to *ask* first, across a fleet of repos, without either
re-running the interactive wizard everywhere or reading every repo's
`.tskmstr.toml` by hand.

## Decision

Two pieces, split across two changes (this repo's git history has them as
separate commits, but they are one ADR because neither is useful alone):

1. **A schema-version stamp.** [`crate::config::manifest::CURRENT_SCHEMA_VERSION`]
   is the asset/config schema revision this binary expects. `tm init` writes
   it into `.tskmstr.toml`'s top-level `schema_version` key on every run
   (including a no-op re-run that changes nothing else), so a config file
   carries a legible marker of which tskmstr last onboarded it.
   [`crate::config::manifest::stamp_status`] compares a found stamp against
   the constant, producing `Current` / `Missing` / `Stale(v)` / `Newer(v)`.

2. **`tm check`, a read-only consumer.** It reads a repo's stamp and reports
   `StampStatus` in plain language, and separately re-runs the same
   *structural presence* checks `tm init` already performs and discards —
   a configured lane's missing prompt file, a configured session's missing
   skill — as drift findings instead of offers to fix. It writes nothing to
   disk. Exit code `0` up to date, `1` drift found, `2` error (e.g. the repo
   was never onboarded — that's a hard error, not a `Missing` finding,
   because `tm check` presupposes an onboarded repo).

Structural presence and the stamp together are the *entire* definition of
"up to date." There is no third, content-based pass.

## Consequences

- **Bumping `CURRENT_SCHEMA_VERSION` is a real decision, not routine.** It
  should happen exactly when a newer tskmstr expects an asset kind an
  existing onboarded repo won't have — mirroring how `[work.audit]`,
  `[work.create]`, and `[work.manual]` were each added. A change that adds
  no new *expected* asset (a bug fix, a prompt wording tweak) does not bump
  it.
- **User-editable asset content is never compared, on purpose.** Lane
  prompts and skill bodies are scaffolded once and then edited — normal
  operation, not drift (see `docs/decisions/0006-repo-local-assets.md` and
  GitHub issue #30's agent-assisted setup, which edits them deliberately).
  A content diff would flag every such edit as a problem. `tm check` only
  ever asks "does the file exist," never "does it still read like the
  template."
- **`tm check` presupposes an onboarded repo, `tm init` doesn't.** Running
  `tm check` against a repo with no `.tskmstr.toml` is an error naming the
  path and suggesting `tm init`, not a `StampMissing` finding — there is
  nothing to report drift against.
- **Fixing drift is still `tm init`'s job.** `tm check` never writes; every
  finding's remediation is "run `tm init`." A dedicated `tm update` (or a
  direnv-style nudge to run `tm check` automatically on shell entry) is
  useful future work but out of scope here — this ADR only covers the
  stamp and the read-only report.
