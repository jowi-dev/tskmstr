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

- [x] Phase 1
- [x] Phase 2
- [x] Phase 3

### Phase 3 notes

- **Dogfood lane**: `.tskmstr.toml` gained `[work.lanes.tskmstr]` with
  `repo = "."` (phase 2's relative-repo idiom, resolving to this repo's
  own root) and an explicit `prompt_file = "prompts/tskmstr-lane.md"`.
  `resolve_prompt_path` (`src/work/run.rs`) only understands a leading `~`
  or an absolute path — unlike `resolve_repo_path`, it was intentionally
  left out of phase 2's relative-path plumbing, and phase 3 stays in scope
  by not adding any. A plain relative `prompt_file` value is instead left
  to resolve against whatever directory the `tm` process is invoked from,
  which is always this repo's root for both `tm work run` and the board —
  noted as a config comment in `.tskmstr.toml` in case that assumption
  ever stops holding (switch to an absolute path if so).
- `prompts/tskmstr-lane.md` is a short autonomous work-lane prompt: run
  `tm ready <KEY>` first and branch on exit code (`0` ready -> proceed,
  `3` stackable -> proceed on the blocker's PR branch, `1` blocked -> stop
  and record why), work only the named ticket, TDD, run `cargo fmt
  --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`
  before finishing, no AI attribution in commits. It does not reference
  `{key}` itself — `resolve_prompt_path`'s caller already appends `"Work
  ticket: <ticket>."` to whatever the prompt file contains (see
  `src/work/run.rs`'s module doc, step 7), so the prompt file only needed
  to describe the workflow.
- **Verification**: `cargo run -- auth status` (which loads config the
  same way every other command does) succeeded with the github-backend
  message rather than erroring, proving `.tskmstr.toml` — including the
  new `[work.lanes.tskmstr]` table and its `repo = "."` resolution —
  parses cleanly under `merge_work`/`resolve_repo_path`. `cargo run --
  work list`/`board --help` also ran clean (no regressions in existing
  read-only paths). Did not run `tm work run tskmstr` for real or launch
  the board TUI, per the task's scope — those would provision a worktree/
  tmux session and are exactly what "do not run for real" ruled out.
- **Docs**: README gained a `w` row in the keybindings table (previously
  absent entirely — a pre-existing gap, left as-is beyond adding this one
  row, since fully documenting `w`'s pre-issue-5 behavior is out of this
  phase's scope), a new "Board-launched lane runs" section describing the
  zero/one/many-lanes behavior and the backend-compatibility filter
  ("hidden: backend mismatch" status note) plus its `tm work run`
  preflight-error counterpart, an addition to "Board-launched audit
  sessions" documenting the audit-dir fallback and its exact status-line
  wording, and a new "Relative `repo`/`dir` paths in a repo-local config"
  subsection under Configuration covering the `repo = "."` idiom and why
  the same relative value in the global config is
  `RelativePathRequiresRepoConfig`.
- **Real bug found while dogfooding, fixed in this branch**: `nix build`
  (never previously run for this feature — phase 3 is the first phase
  that runs it) failed a test unrelated to any phase-3 change:
  `tests::run_ticket_provider_github_backend_does_not_need_a_jira_token`
  (added in phase 1, `src/main.rs`) opens a `RunStore` at the *default*
  XDG run-db path (`$HOME/.local/share/tskmstr/runs.db`) because its
  `Config` fixture left `run_db_path` unset. That default resolves fine
  under an ordinary dev shell's real, writable `$HOME`, but `nix build`
  sandboxes `$HOME` to something unwritable, so `RunStore::open` fails,
  `run_ticket_provider` swallows the error into `None` per its documented
  opportunistic contract, and the test's `is_some()` assertion fails —
  even though the production code path (`ticket_provider_for`'s github
  arm) is correct. Reproduced locally without nix by pointing `$HOME` at
  a `chmod 555` directory and running the test binary directly (`cargo
  test` itself needs a writable `$HOME` for its own registry cache, so
  `CARGO_HOME` had to stay pointed at the real one while only `$HOME` was
  swapped). Fix: the test now sets `config.run_db_path` to a
  `tempfile::tempdir()` path instead of leaving it unset, making it
  hermetic regardless of the ambient `$HOME` — no production code
  changed. Verified: the test fails against the pre-fix code under the
  `chmod 555` `$HOME` repro, and passes both there and normally after the
  fix; `nix build` failed before this commit and succeeds after it.
- Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (1983 lib tests + 3 bin tests + 0 doctests, unchanged count
  from phase 2 — the bug fix edited an existing test, not add a new one)
  all clean. `nix build` succeeds (`result -> tskmstr-0.1.0` in the
  Nix store).
- Post-merge dogfood finding (first real `w` press on this board): the lane
  launched but failed base resolution — `could not resolve a base branch
  for `tskmstr` (no --from, lane base_branch, or origin/HEAD)`. This clone's
  `origin/HEAD` symbolic ref was never set (`git remote set-head`), so
  `resolve_base`'s final fallback had nothing to resolve; the axiom lane
  never hit this because it sets `base_branch` explicitly. Fixed by adding
  `base_branch = "main"` to `[work.lanes.tskmstr]` (committed) and running
  `git remote set-head origin main` in this clone (local state, fixes the
  fallback for anything else that wants it).

### Phase 2 notes

- New module `src/config/backend_identity.rs` (re-exported from
  `crate::config`): `BackendIdentity` (`Jira { base_url, project_key }` /
  `Github { repo }`, derived infallibly from an already-merged `Config` via
  `BackendIdentity::from_config`), the `BackendIdentityResolver` trait
  (`resolve(&self, dir) -> Result<BackendIdentity, ConfigError>`), a
  production `FsBackendIdentityResolver` (wraps `config::load` pointed at an
  arbitrary directory instead of the process's cwd — the exact same I/O
  `load` already does, just parameterized), and a `FakeBackendIdentityResolver`
  test double. Two pure functions built on top, both taking `&dyn
  BackendIdentityResolver` so callers can fake the I/O: `compatible_lane_names`
  (board filter) and `resolve_audit_host_dir` (audit fallback).
- **Design decision, not explicitly spelled out in the plan**: all
  filtering/fallback decisions are resolved *eagerly at startup* in
  `main.rs` (`run_board`/`run_work`), not lazily per keypress or via a new
  `Cmd` variant. `run_board` computes the board's own `BackendIdentity` once,
  partitions `config.work.lanes` into compatible names before `TuiDeps` is
  even constructed, and pre-resolves the effective audit host dir the same
  way (baking the fallback directly into the `AuditConfig` handed to
  `TuiDeps`, plus a plain `audit_dir_fallback: bool` for status-line
  wording). This keeps `tui::app`'s reducer and `tui::event`'s `TuiDeps`
  free of any new I/O seam — `App`/`update` only ever see the *result*
  (`lane_names`, `hidden_lane_count`, `audit_dir_fallback`), matching the
  "resolve at TUI startup where `lane_names` is currently wired" option the
  task offered. `tm work run`'s preflight is the one place identity
  resolution stays live (via `RunLaneDeps::backend_identity_resolver`) since
  it's a single directory, checked once, immediately before use.
- **`tm work run` preflight**: `RunLaneDeps` gained
  `current_repo_dir`/`current_backend_identity`/`backend_identity_resolver`;
  `prepare_run_lane` resolves the lane repo's identity and compares it to
  the invoking repo's right after resolving `repo_root`, before the
  prompt-file check — the earliest point, so a backend mismatch is reported
  before any other preflight failure. `RunLaneError::BackendMismatch` is
  `Box<BackendMismatchInfo>` (not inline fields): clippy's `result_large_err`
  flagged every `Result<_, RunLaneError>` function once the two
  `BackendIdentity`s + two `PathBuf`s were added inline, so the payload is
  boxed and `Display`s via a dedicated `BackendMismatchInfo` type instead of
  thiserror's field interpolation. `RunDeps` (`src/cli/work.rs`) grew the
  same three fields to pass through. This touched 40 existing
  `RunLaneDeps { .. }` test literals in `src/work/run.rs` plus 9 in
  `src/cli/work.rs`; all were mechanically extended with a shared
  `compatible_test_identity()`/`compatible_test_resolver()` pair (a
  `BackendIdentityResolver` that always reports the same fixed identity
  regardless of directory) so tests unrelated to backend compatibility
  don't have to care about it.
- **Board lane filtering**: `App` gained `hidden_lane_count` +
  `with_hidden_lane_count` (mirroring `lane_names`/`with_lane_names`).
  `lane_run_action`'s zero-lane status message and `draw_lane_picker`'s
  title both branch on it: "no compatible lanes (N hidden: backend
  mismatch)" instead of "no lanes configured" / a bare "Lane" title, when
  lanes exist but were all filtered out. `App::lane_names`'s stale "not yet
  wired into main.rs" doc comment (dating to before `board-lane-runs.md`
  landed) was corrected while touching the adjacent method.
- **Audit-dir fallback**: implemented entirely in `main.rs`, not
  `work::audit::launch_audit` — `resolve_audit_host_dir`'s result is baked
  into the `AuditConfig.dir` string handed to `TuiDeps` before construction,
  so `launch_audit`'s own signature/tests are untouched. `TuiDeps` gained a
  plain `audit_dir_fallback: bool` `launch_audit_cmd` (`src/tui/event.rs`)
  checks to word its success message differently ("launched audit for
  {key} in the current repo (configured audit dir is backend-incompatible)
  -- press a to attach") — no new I/O in the TUI event layer.
- **Relative lane `repo` / `[work.audit].dir` paths**: resolved at
  config-merge time, not at consumption time (unlike `~`-expansion, which
  stays the caller's job per existing convention) — `merge_with_repo_dir`
  already threads a `repo_dir: Option<&Path>` through for
  `[backend.github].repo`'s origin-remote default, so `merge_work`/
  `merge_audit` reuse it as the "defining directory" for a relative value,
  *provided that specific value actually came from the repo-local config*
  (checked via `repo.lanes.contains_key(name)` for lanes, and `repo.dir.is_some()`
  for audit — not merely "repo_dir happens to be `Some`", since `load()`
  passes a cwd-fallback `repo_dir` even when no repo-local `.tskmstr.toml`
  exists at all). A relative value that falls back to the global config is
  `ConfigError::RelativePathRequiresRepoConfig`, naming the dotted field
  path (`work.lanes.<name>.repo` / `work.audit.dir`) and the offending
  value. `repo = "."` is special-cased to resolve to the defining dir
  itself (not `<dir>/.`); any other relative value is a plain
  `dir.join(value)` (so `"../sibling"` resolves un-normalized, same as a
  naive `PathBuf::join` would — no lexical `..`-collapsing was added, since
  nothing needed it).
- Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (1983 lib tests + 3 bin tests + 0 doctests, up from phase 1's
  1963) all clean.

### Phase 1 notes

- `RunLaneDeps`/`RunDeps`'s `jira` field is renamed to `ticket_provider`
  (already typed as `Option<&dyn TicketProvider>`, so this was a rename, not
  a retyping) throughout `src/work/run.rs` and `src/cli/work.rs`.
  `resolve_ticket_slug` and `resolve_blocker_stacking` take
  `ticket_provider` instead of `jira` and are otherwise unchanged — they
  were already backend-agnostic; the bug was entirely in `main.rs`'s
  wiring, which unconditionally called `jira_client_for` instead of
  branching on `config.backend`.
- Fix: extracted `run_ticket_provider` in `src/main.rs`, which wraps the
  existing `ticket_provider_for` (the same construction `tm ticket`/`tm
  board` already use) as `.ok()` — preserving the prior opportunistic
  contract (`None` on any construction/auth failure, never a hard error for
  `tm work run`). `WorkCmd::Run` now calls this instead of hand-rolling a
  Jira-only client.
- Deviation from a literal read of "nothing in `src/work/` should name Jira
  types": `FakeJiraClient` (a generic `TicketProvider` test double already
  shared crate-wide, per its own doc comment) remains in `src/work/run.rs`'s
  *existing* tests unrelated to backend selection (e.g. blocker-stacking
  fixtures). Interpreted the constraint as scoped to production code (the
  run path) — confirmed via grep that no non-test code in `src/work/` names
  a concrete Jira type. Added one new test,
  `run_lane_fg_uses_github_summary_slug_for_branch_name_when_available`,
  using a real `GithubProvider` over `FakeGhCli` (mirroring
  `github_provider.rs`'s own test setup) to prove GH-N slug resolution
  actually works end to end.
- TDD note: the routing bug lives in `main.rs`, which has no prior test
  culture (no injectable `GhCli`/keychain seam beyond what
  `ticket_provider_for` already offers, and phase 1 was scoped to reuse that
  construction rather than add a new one). Wrote `run_ticket_provider` first
  with the old Jira-only behavior, added a failing test asserting a
  github-backend config with no Jira token still yields a provider, watched
  it fail, then swapped the implementation to call `ticket_provider_for`
  and watched it pass. Two more tests cover the Jira backend's existing
  None-without-a-token / Some-with-a-token behavior is unchanged.
- Gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (1960 lib tests + 3 new bin tests + doctests) all clean.
