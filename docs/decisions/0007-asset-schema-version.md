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
- **Fixing drift is `tm update`'s job (GitHub issue #39).** `tm check`
  never writes; `tm update` applies the *additive* fixes — scaffolding a
  configured lane's missing prompt file, bumping the stamp, offering the
  agent-assisted setup session (issue #30's machinery) for only the new
  assets — and never overwrites an existing file or config value, for the
  same reason content is never diffed. Drift that isn't additively fixable
  (a user-supplied skill that exists nowhere, a stamp newer than the
  running binary) stays a report. `tm check --quiet` doubles as the
  direnv-style shell-entry nudge: it compares only the stamp, so it stays
  cheap enough to run on every `cd`, and the full scan stays behind
  on-demand `tm check`/`tm update`.

## Addendum: expected config keys (GitHub issue #74)

The stamp alone says *that* a repo is behind, never *which* config key it
lacks. `src/config/manifest.rs` now also holds `EXPECTED_CONFIG_KEYS`: one
entry per key a feature expects (dotted path, the schema version it arrived
in, `Required` or `Recommended`, and a per-backend suggested value). A new
feature that ships a key registers it there.

- `tm check` reports each registered key set in neither the repo-local nor
  the global config as a `MissingConfigKey` finding naming the key and its
  suggested value. Still presence-only: an existing value is never judged.
- **Required** keys are drift (exit `1`, and the `--quiet` nudge surfaces
  them). **Recommended** keys are advisory: reported, but they never change
  the exit code or reach the quiet nudge, since a default is already
  assumed.
- A recommended key with no safe default for the repo's backend (Jira status
  names belong to the board) isn't reported at all: an unfixable advisory on
  every run would just be noise. A required key with no default is still
  reported, for the operator to set by hand.
- `tm update` lists the keys it can fill and writes all their suggested
  values after one batch confirmation (`--yes` skips the question), only
  where no value exists. A declined key stays an advisory finding.
- `--quiet` now parses the raw global config file as well as the repo one,
  so a key set globally isn't misreported. It still does no merged config
  load, so it stays cheap enough for every `cd`.
- Registering a key does not by itself bump `CURRENT_SCHEMA_VERSION`; the
  key's presence is checked directly. The keys shipped with this change
  (`status_on_pr`, `status_on_run_start`, both recommended, GitHub defaults
  `In Review` / `In Progress`) predate it and are tagged with schema 2.
