# Issue #5: Board-launched work sessions route GitHub-backend tickets into the inherited Jira lane

Tracking: https://github.com/jowi-dev/tskmstr/issues/5

## Problem recap

Pressing `w` on `tm board` in this repo (github backend) launches `tm work run axiom GH-3`,
which provisions a session in the *axiom* checkout (Jira-backed). Backend resolution is
cwd-driven, so everything inside the session speaks Jira and 404s on `GH-3`.

Root causes (verified, see issue body for the full chain):

1. `[work.lanes.*]` and `[work.audit]` inherit verbatim from the global config into any
   repo that doesn't override them (`merge_with_repo_dir`, `src/config/mod.rs`), so the
   only lane this repo's board can offer is `axiom`.
2. The launched session's cwd is the lane's repo, and nothing checks that the lane repo's
   resolved backend can serve the ticket being launched.
3. `WorkCmd::Run` wiring (`src/main.rs:352-359`) builds a Jira client unconditionally;
   `RunLaneDeps` has a field literally named `jira` (`src/work/run.rs:266`), and
   `resolve_ticket_slug` (`src/work/run.rs:434`) calls `jira.get_issue("GH-3")` and
   silently swallows the failure.

## Design decisions

- **Refuse backend-mismatched lanes rather than defaulting lane repo to cwd.** Lanes keep
  their required explicit `repo` (the no-cwd-fallback decision from the runner port
  stands). Instead, a lane is *compatible* with the current repo only if the lane repo's
  resolved backend matches the current repo's resolved backend identity — same
  `BackendKind`, and for github the same `[backend.github].repo` slug; for jira the same
  base URL + project key. Incompatible lanes are filtered from the board picker and
  hard-rejected by `tm work run` preflight with a clear error.
- **Audit dir falls back to the current repo on mismatch** instead of refusing, so the `a`
  audit action keeps working here without config changes. Backend resolution is
  cwd-driven, so an audit session hosted in the current repo resolves correctly.
- **Fix 2 from the issue ("cwd = the ticket's own repo") falls out of the compatibility
  check**: once a lane must share the ticket provider's identity, the session cwd is a
  repo where the ticket's key resolves correctly.
- **Lane `repo` accepts relative paths** resolved against the repo that defines the lane,
  so this repo can commit a portable `[work.lanes.tskmstr] repo = "."` in `.tskmstr.toml`
  and dogfood the whole path.
- Skill-level fixes (`/ticket-audit` speaking only `tm` verbs) are downstream and out of
  this repo's scope, per the issue.

## Phases

Each phase: its own branch off local main, TDD, gates (`cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`), orchestrator review, merge
`--no-ff`. `nix build` once at the end. No pushes.

### Phase 1: provider-agnostic `run_work`

Branch `issue-5-phase-1`. Replace the unconditional Jira client in the `WorkCmd::Run`
wiring with the ticket provider selected by `config.backend` (reuse the same provider
construction the `ticket`/`board` commands use). Rename the `RunLaneDeps.jira` seam to a
provider-agnostic one; `resolve_ticket_slug` goes through the provider. Under the github
backend, slug resolution must actually resolve `GH-N` summaries instead of silently
failing.

### Phase 2: lane/audit backend-compatibility guard

Branch `issue-5-phase-2`. Introduce a backend-identity resolution for a directory
(kind + github repo slug / jira base URL + project key) and a compatibility predicate.
Apply it in three places: board lane offering (filter, with a status-line note when lanes
were hidden), `tm work run` preflight (hard error naming both backends), and audit-dir
resolution (fall back to current repo on mismatch). Lane `repo` gains relative-path
resolution against its defining config's repo dir.

### Phase 3: dogfood + docs

Branch `issue-5-phase-3`. Add `[work.lanes.tskmstr]` with `repo = "."` to `.tskmstr.toml`
(prompt file decision may need a repo-local prompt); update README/docs for the new
compatibility behavior, relative lane repos, and audit-dir fallback. Live-verify: `w` on a
GH ticket from this repo's board offers/launches the tskmstr lane in a tskmstr worktree.

## Status

- [ ] Phase 1
- [ ] Phase 2
- [ ] Phase 3
