# Bugbot follow-through on code review (roadmap stream 7)

Status: implemented 2026-08-07 (commits `ddda3e3`..`2d3ae5a`). Steps 1-12
below all landed; steps 13-14 remain open operational (axiom-side) work.
Deviations from the design as written:

- **Step 3: `PrSummary` didn't survive.** Rather than folding `PrSummary`
  into `PrInfo` alongside a separate caller, `PrSummary` was removed
  outright — its one real caller, `find_issue_key_with_source`'s use in
  `src/cli/ready.rs`, now takes the full `PrInfo` `pr_list` returns, same
  as `pr_view`/`pr_create` already did. No type carries the old
  `{number, title}` shape anymore.
- **Step 10: `launch_cleanup` does not (re)write the findings file.**
  The findings file is written once, by the poll loop
  (`src/work/review_watch.rs`'s `poll_once`), before it finishes the run
  as `Review`. `launch_cleanup` only reads the path back (via
  `findings_file_path`) to fill the prompt's `{findings_file}`
  placeholder — the design section's phrasing ("write the findings
  file") is corrected here; the file already exists by the time
  `launch_cleanup` runs.
- **Step 11: board wiring, minor shape deviations.**
  - The pending-launch overlay for the `bots:` badge lives in a
    ui-side set, `App::pending_bot_watch_launches`, mirroring
    `pending_lane_launches` rather than threading a flag through the
    loaded status map.
  - `PendingLaunch` (the event loop's in-flight watched-child registry)
    gained a `kind: PendingLaunchKind` field (`LaneRun` | `BotWatch`)
    instead of a parallel struct, since `LaneLauncher::spawn` is now
    shared by both launch paths.
  - The audit-session-name-to-ticket-key helper was generalized to
    `session_ticket_key(session_name, prefix)` (parametrized over
    `AUDIT_SESSION_PREFIX`/`CLEANUP_SESSION_PREFIX`) rather than adding a
    second bespoke parser for `tm-bugbot-<key>`.
  - Cleanup-launch failures from the `b` key surface through a new
    `Msg::BotsActionResult(String)`, the bot-watch counterpart of
    whatever status-line messaging the lane-run path already used,
    rather than reusing a lane-run-specific message variant.

Stated 2026-08-07. Once a ticket's PR is up for review, its bots
(`review_bots`, e.g. `cursor[bot]`) take anywhere from seconds to tens of
minutes to post their findings. Today noticing that they've finished — and
cleaning up whatever they found — is manual: someone has to remember to run
`tm pr status` again later. This stream makes the board notice for you: arm
a watcher for a ticket's PR, get a badge when the bots are done, and launch
(or attach to) a cleanup session in one keypress. Together with streams 5-6
this makes the board a full lifecycle control surface: groom (audit) →
execute (lane run) → observe (run overlay) → land (bot cleanup).

Two decisions are locked by the roadmap and not revisited here: the watcher
is a plain `tm` process polling `gh`, never a Claude session, and it never
runs on the board's own 2s tick (the board only ever reads local SQLite);
and spending tokens (a cleanup session) only happens *after* bots are done,
gated by `on_bots_done = "notify" | "launch"` (default `notify`).

## Ground truth

- **`kind` is unconstrained free text.** `runs.kind` is `TEXT NOT NULL
  DEFAULT 'lane'` with no `CHECK` (`src/runs/mod.rs` migration 4; a test
  round-trips `kind = "audit"` and a raw INSERT with no `kind` column at
  all defaults to `'lane'`). A new value (`review-watch` for the watcher,
  `bugbot-cleanup` for the cleanup session) needs **no migration** — this
  supersedes the roadmap's tentative "does the CHECK constraint need a
  migration?" framing; there is no constraint to touch.
- **`count_bot_findings`/`ReviewThread` stay untouched.** Per the assigned
  gotcha, `ReviewThread.author_login` comes from GraphQL without the
  `[bot]` suffix; `count_bot_findings` already handles the suffix-and-case
  matching correctly and is reused as-is by `tm pr status` and by this
  stream's finished-finding count. Nothing here "fixes" it.
- **`pr_review_threads` only tells you what threads exist, not whether a
  bot has run.** A bot that reviews and finds nothing posts a review with
  zero comments — no thread, so no signal in `ReviewThread` data at all.
  "Bots finished" therefore cannot be derived from thread data; it needs
  the PR's *review submissions* (one entry per bot that has posted a
  review, regardless of whether it left comments). `GhCli` has no method
  for that today — new plumbing, not reuse.
- **`pr_list()` returns only `number, title`.** `PrSummary { number,
  title }` is unused outside its own tests. `find_issue_key_with_source`
  (`src/github/pr.rs`) needs `title`, `body`, and `head_ref_name` to find a
  ticket key reliably (title prefix → title token → body token → branch
  name, in that order) and takes a full `PrInfo`, not a `PrSummary`.
  Resolving ticket → PR needs `pr_list` widened to fetch the same fields
  `pr_view` already does.
- **`pr_view()` is current-branch-only** (`gh pr view` with no PR number
  reads the checked-out branch). The watcher is a detached background
  process; it has no reason to run from the ticket's worktree (the ticket
  may not even have an active lane run), so `pr_view` cannot resolve "the
  PR for ticket KEY" — this is exactly why `pr_list` is the right existing
  tool, per the roadmap's hint.
- **`register_session` is already kind-generic**, not audit-specific
  (`src/runs/session.rs`, stream 2): `register_session(store, kind,
  ticket, env)` adopts a pre-registered run via `TSKMSTR_SESSION_RUN_ID`
  for whatever `kind` is passed. Nothing about it is audit-specific except
  that today only `tm ticket audit`/`tm ticket create` call it. The
  cleanup session can reuse it directly if something calls it — see
  "Adoption" below.
- **`tm-session-state.sh` (await/resume) and `tm-session-end.sh`
  (abandonment finish) are already kind-agnostic** (stream 4/2): they key
  off the session marker file and `TSKMSTR_RUN_ID`'s *absence*, not off
  `kind`. A `bugbot-cleanup` run adopted the same way as an audit run gets
  waiting-state and abandonment-finish telemetry for free — **no new hook
  scripts**.
- **`AuditIndicator`/`audit_indicator` are already generic** over
  "tmux session exists" + "latest run of this kind for this ticket" — the
  function and type don't hardcode audits anywhere except the name. The
  cleanup session (also an interactive tmux `claude` session, launched and
  adopted exactly like an audit) can reuse the type and precedence
  function outright rather than growing a parallel `CleanupIndicator`.
- **`launch_audit` (`src/work/audit.rs`) is the template for launching an
  interactive session**: refuse if the tmux session already exists,
  pre-register a run row, `tmux.new_session_with_command` with
  `TSKMSTR_SESSION_RUN_ID` in the environment. The cleanup launch reuses
  this shape almost verbatim.
- **The `LaneLauncher`/`LaunchHandle` seam (stream 5, `src/tui/launcher.rs`
  + `Cmd::LaunchLaneRun` interception in `src/tui/event.rs`) is the
  template for launching a detached child from the board and reporting
  spawn failure/success back through `Msg` without blocking the event
  loop.** Arming a watcher from the board needs the identical shape (spawn
  `tm pr watch <key>`, watch the child, not the polling loop itself).
- **Board keys `f`, `p`, `a`, `w` are taken** (`src/tui/keymap.rs`); `b`
  ("bots") is free.
- **`RunStatus` stays a 6-variant enum** (`Queued/Running/Blocked/Review/
  Done/Failed`, ADR-0001's "keep the status vocabulary small"). `Review`
  ("finished and awaiting human review") is an exact semantic fit for "the
  watcher is done and there are findings to look at" — no new variant
  needed.

## Design

### CLI surface

New `tm pr watch <KEY>` subcommand (`PrCmd::Watch { key: String,
#[arg(long)] foreground: bool }`), alongside `create`/`status` — it's PR
lifecycle, same as those two, and reuses the same `TicketingContext`
(`jira`, `gh`, `config`) plus a `RunStore`.

- Resolves KEY → open PR via `find_pr_for_ticket` (below). No PR found:
  print `no open pull request found for KEY. Run tm pr create first.` and
  exit 1 (mirrors `PrCliError::NoPr`'s wording/exit-code convention).
- Dedup: `store.latest_run_for_ticket_kind(key, "review-watch")` — if
  `Running`, print `already watching KEY (run <id>)` and exit 1. Same
  check-then-act race window `launch_audit` already accepts for
  `tmux.has_session` (single board process; two independent CLI
  invocations racing is an operator error, not designed against).
- Without `--foreground`: re-execs itself as `tm pr watch <key>
  --foreground` detached via `setsid` (the exact re-exec idiom
  `src/work/run.rs`'s detached lane path already uses), then exits 0 once
  the child is spawned. This is what the board's launcher watches (see
  "Board integration"): it's a quick, watchable step whose *own* exit code
  and stderr report "PR not found" / "already watching" synchronously,
  before the real long-lived poll loop detaches.
- `--foreground` runs the poll loop in this process (used by the
  re-exec'd child, and directly useful for manual debugging without a
  detach).
- Exit codes: `0` on clean loop exit (PR merged/closed, or bots-done
  handled), `1` on resolution/dedup failure, `2` if the loop gives up after
  `max_wait_mins` with the PR still open and bots not done (distinguishing
  "gave up" from "handled" in scripts/manual runs).

### Ticket → PR resolution

`PrSummary` is widened to carry the fields `find_issue_key_with_source`
needs and folded into `PrInfo` (it was already a strict subset with no
other callers):

```rust
fn pr_list(&self) -> Result<Vec<PrInfo>, GhError>;
```

fetching `--json number,url,title,body,headRefName` (identical to
`PR_VIEW_JSON_FIELDS`, so one JSON-shape parser serves both). New pure
helper in `src/github/pr.rs`:

```rust
pub fn find_pr_for_ticket(prs: &[PrInfo], key: &str) -> Option<&PrInfo>
```

— the first (by ascending PR number, for determinism) PR for which
`find_issue_key_with_source(pr) == Some((key.to_uppercase(), _))`. This
reuses the exact title/body/branch matching `tm pr create` already
produces (via `with_issue_key_prefix`) and needs no new regex or
GraphQL/REST call beyond the widened `pr_list`. Known gap, documented not
"fixed": a PR opened by hand with no key in title/body and a branch name
that doesn't match the `key-123` shape won't resolve — same limitation
`find_issue_key_with_source` already has everywhere else it's used.

### Poll loop: what "bots finished" means

New `GhCli` method, additive (does not touch `pr_review_threads` or
`ReviewThread`):

```rust
fn pr_reviews(&self, number: u64) -> Result<Vec<PrReview>, GhError>;
```

`PrReview { author_login: Option<String> }`, populated via `gh api
repos/{owner}/{repo}/pulls/{number}/reviews --jq '[.[] | {login:
(.user.login // null)}]'` (REST; per the documented gotcha this returns
bot logins *with* the `[bot]` suffix, unlike the GraphQL-only
`reviewThreads` connection). Owner/repo resolution factors the `gh repo
view --json owner,name` call `pr_review_threads` already makes into a
small shared private helper (`resolve_repo`) — a pure extraction, not a
behavior change to either caller.

The suffix-and-case matcher inside `count_bot_findings` is extracted from
`matches_bot_login` (currently private in `bot_findings.rs`) to
`pub(crate) fn bot_login_matches(login: &str, author: &str) -> bool`, used
by both `count_bot_findings` (unchanged behavior) and the new predicate:

```rust
/// True once every configured bot login has at least one review
/// submission on the PR. Vacuously true when `bot_logins` is empty.
pub fn bots_have_reviewed(reviews: &[PrReview], bot_logins: &[String]) -> bool
```

**Deterministic predicate:** `bots_have_reviewed(gh.pr_reviews(number)?,
&config.review_bots)`. Known limitation, documented not designed around:
this checks "has reviewed at least once," not "has reviewed the latest
push" — a bot that already reviewed once and the author pushes a fix
won't re-arm a *running* watcher (the watcher has already finished by
then); re-running `tm pr watch` covers that case if wanted later.

### Poll loop mechanics

`--foreground` loop, `poll_secs` cadence (config, default 45 — the
midpoint of the roadmap's 30-60s band):

1. Check PR lifecycle: `gh pr view <number> --json state,merged` (new
   `GhCli::pr_state(number) -> Result<PrLifecycle, GhError>`, `PrLifecycle
   { Open, Merged, Closed }`). `Merged`/`Closed` → emit event `pr_closed`
   (detail `{"reason": "merged"|"closed"}`), `finish_run` status `Done`,
   exit 0. This is the watcher's documented death condition.
2. Otherwise `bots_have_reviewed`. Not yet → `add_event(kind: "bot_poll",
   detail: None)` (a heartbeat-only tick; also gives `tm runs show` a
   timeline of polls) and sleep.
3. Bots done, first time seen this run → `gh.pr_review_threads(number)` +
   `count_bot_findings` for the final tally, `add_event(kind:
   "bots_done", detail: {"total": N, "unresolved": N})`.
   - `unresolved == 0` → `finish_run` status `Done`, exit 0. Roadmap's
     "zero findings → badge straight to done, no cleanup session."
   - `unresolved > 0` → write the findings file (see "Findings-to-prompt
     plumbing"), `finish_run` status `Review` (the "awaiting human
     review" fit), then if `on_bots_done == "launch"`, call the same
     cleanup-launch function the board's `b` key calls (see below) before
     exiting 0. `notify` mode exits 0 without launching anything — the
     board badge alone (via the `Review` status) is the notification.
4. `pid = Some(process::id())` was recorded at `start_run` time (the
   watcher is a real long-lived process, unlike an audit's pre-registered
   pid-`NULL` run), and step 2's `bot_poll` event bumps `heartbeat_at`
   every tick — so `RunStore::reap`'s existing pid-alive check already
   protects a healthy watcher, and a killed one (machine sleep, kill -9)
   goes stale and gets reaped `failed` by the existing mechanism with **no
   watcher-specific reap logic needed**.
5. `gh` failures (network blip, `gh` not authenticated) mid-loop: log via
   `add_event(kind: "poll_error", detail: {"message": ...})` and continue,
   up to a bounded consecutive-failure counter (10, ≈ 7-8 minutes of
   backoff at the default cadence) before giving up: `finish_run` status
   `Failed`, exit 1. A single blip must not kill the watcher; a truly
   broken `gh` must not spin forever.
6. Give-up timeout: if wall-clock since `started_at` exceeds
   `max_wait_mins` (config, default 1440 = 24h) with the PR still open and
   bots not done, `add_event(kind: "give_up", detail: None)`,
   `finish_run` status `Failed`, exit 2.

### Findings-to-prompt plumbing

New `GhCli` method, additive (separate from `pr_review_threads` — the
cleanup path needs comment bodies/locations that `tm pr status`'s counting
path never needed, so this doesn't touch the counting query or
`ReviewThread` at all):

```rust
fn pr_bot_finding_details(&self, number: u64) -> Result<Vec<FindingDetail>, GhError>;
```

`FindingDetail { author_login: Option<String>, is_resolved: bool, path:
Option<String>, line: Option<i64>, body: String, url: String }`, via a
GraphQL query extending `REVIEW_THREADS_QUERY`'s shape with the first
comment's `body`, `path`, `line`, and `url`. Filtered to unresolved +
bot-authored (via `bot_login_matches`) the same way `count_bot_findings`
filters, in a new `bot_finding_details(details: &[FindingDetail],
bot_logins: &[String]) -> Vec<FindingDetail>` pure function in
`bot_findings.rs`.

Serialized to `${XDG_DATA_HOME:-~/.local/share}/tskmstr/findings/<lowercased
key>.json` — a JSON array of `{file, line, body, url}` — following the
existing precedent of file-based data handoff (`work.ml`/`runner.rs`'s
`out_json`) rather than cramming multi-line comment bodies into a shell
command string (which `launch_audit`'s `shell_quote` already has to work
around for the far simpler `{key}` substitution alone).

`[work.review_watch].prompt` defaults to `/bugbot-triage {key}
{findings_file}` — a second placeholder beyond audit's `{key}`, both
substituted the same way. The `/bugbot-triage` skill (axiom-side, out of
this repo's scope) reads `{findings_file}` itself; this plan's job stops
at "the file exists at a known path with a known shape by the time the
session starts."

### Cleanup session: recommendation — tmux-hosted interactive, not headless

The roadmap leaves this open; recommendation is **interactive tmux
session, structurally identical to stream 4's audit launch**, not a
headless lane run:

1. **Findings need judgment.** Bots produce false positives; blindly
   auto-applying every finding in a headless run is worse than the manual
   status quo, and a headless run has no controlling terminal to escalate
   an ambiguous finding to (this is the exact reasoning stream 6 already
   used to rule out attach-to-lane-run: "headless `setsid` runs have no
   controlling terminal"). A human needs to be in the loop, at least to
   approve/reject each fix.
2. **The prompt already exists for this shape.** `/bugbot-triage` (axiom)
   is described as a skill, i.e. a conversational entry point, not a
   fire-and-forget batch prompt.
3. **It's overwhelmingly reuse, not new plumbing.** `launch_audit`'s
   refuse-if-live / pre-register / `tmux.new_session_with_command`
   sequence, `SessionEnv::session_run_id` adoption, `tm-session-state.sh`'s
   await/resume telemetry, and `AuditIndicator`'s precedence function all
   transfer with a different `kind` string and prompt — a headless lane
   run would require none of that and none of the "is it stuck or
   thinking" visibility audits already solved for.
4. **Ergonomics match the rest of the board.** `on_bots_done = "notify"`
   (default) plus a loud badge plus one keypress to launch-or-attach is
   the same shape `a` already gives ticket audits; a headless run would
   need a *different* one-keypress affordance (stream 6's run overlay) to
   get equivalent visibility, duplicating work instead of reusing it.

New module `src/work/bugbot.rs`, deliberately parallel to
`src/work/audit.rs`:

- `cleanup_session_name(key) -> String` = `tm-bugbot-<lowercased key>`.
- `launch_cleanup(store, tmux, cfg, key)`: refuse if
  `tmux.has_session(name)`; write the findings file (previous section);
  `store.start_run` with `kind = "bugbot-cleanup"`, `lane =
  "bugbot-cleanup"`; `tmux.new_session_with_command` with
  `TSKMSTR_SESSION_RUN_ID` set, same as `launch_audit`. `[work.review_watch]`
  reuses `[work.audit].dir` when its own `dir` is unset (same repo/skills
  directory almost always) — see Config below.

### Adoption: reusing `register_session` for a non-audit, non-create kind

`register_session(store, kind, ticket, env)` is already kind-generic
(stream 2), but every existing caller is a Rust command (`tm ticket
audit`/`create`) that the respective skill happens to invoke as its first
turn. `/bugbot-triage` has no reason to call `tm ticket audit`. New thin
CLI entry point, `tm runs register --kind <kind> <KEY>` (`RunsCmd::Register
{ kind: String, key: String }`), doing nothing but leniently opening the
store and calling `register_session(store, &kind, &key, &env)` — a
one-line wrapper, not new logic. `[work.review_watch].prompt`'s default
becomes `/bugbot-triage {key} {findings_file}`, and the *axiom-side*
`/bugbot-triage` skill's documented first step (operational, see below) is
`tm runs register --kind bugbot-cleanup {key}`. This closes the same
gap stream 4 closed for audits, generically, instead of adding a
bugbot-cleanup-specific adoption path.

With adoption wired this way, `tm-session-state.sh` (await/resume) and
`tm-session-end.sh` (abandonment finish) apply with **zero changes** —
both already key off the session marker + `TSKMSTR_RUN_ID`'s absence, not
off `kind`.

### Board integration

`TuiDeps` already carries `store`/`tmux` (stream 4). Two new poll
`Cmd`s on the same ~2s/8-tick cadence as audit/lane-run status:

- `Cmd::LoadBotWatchStatus` → `list_runs_filtered(Some("review-watch"))`,
  latest per ticket, mapped through a new pure fn:

  ```rust
  pub enum BotWatchIndicator { Watching, Ready, Clean, Failed }

  pub fn bot_watch_indicator(run: Option<RunStatus>) -> Option<BotWatchIndicator> {
      match run {
          Some(RunStatus::Running) => Some(BotWatchIndicator::Watching),
          Some(RunStatus::Review) => Some(BotWatchIndicator::Ready),
          Some(RunStatus::Done) => Some(BotWatchIndicator::Clean),
          Some(RunStatus::Failed) => Some(BotWatchIndicator::Failed),
          _ => None,
      }
  }
  ```

  No session/tmux input at all — the watcher is headless, so (like
  `RunIndicator`/lane runs) the run row is the only source of truth, and a
  terminal badge (`Ready`/`Clean`/`Failed`) persists until superseded by a
  newer watcher run, matching `RunIndicator`'s precedent exactly.
- `Cmd::LoadCleanupStatus` → reuses `list_runs_filtered(Some
  ("bugbot-cleanup"))` + `tmux.list_sessions()` filtered to
  `tm-bugbot-<key>`, through the **existing** `audit_indicator` function
  unchanged (it was already generic over "any tmux-hosted interactive
  session kind"), producing the existing `AuditStatusEntry`/`AuditIndicator`
  types. Rendered with a `clean:` badge prefix and its own theme accent
  rather than `audit:`'s, but no new indicator type.

Cards can show both a `bots:` badge and a `clean:` badge simultaneously
(once triage starts, the review-watch run has already finished `Review` —
showing both is mildly redundant, not wrong; stream 5 already accepted the
analogous "audit + lane-run badges can coexist" outcome rather than
merging kinds).

Keybinding `b` ("bots"), same ungated policy as `a`/`w`. Precedence,
mirroring `a`'s if/else exactly:

1. `tm-bugbot-<key>` tmux session exists → attach (identical mechanics to
   `Cmd::AttachAudit`, reusing the same terminal suspend/restore code with
   a session-name parameter rather than a second `Cmd` variant —
   `Cmd::AttachAudit { session_name }` already takes the name, not an
   audit-specific type).
2. No cleanup session, but the ticket's latest review-watch run is
   `Review` (`BotWatchIndicator::Ready`) → launch cleanup (`Cmd::
   LaunchCleanup { key }`, executed the same watched-child way as
   `Cmd::LaunchLaneRun`).
3. Review-watch run `Running` → status-line message (`watching PR for
   KEY — bots not done yet`), no action.
4. Otherwise (no watcher, or the last one is `Done`/`Failed`) → arm:
   `Cmd::LaunchBotWatch { key }`, spawning `tm pr watch <key>` as a
   watched child through the **same `LaneLauncher` trait**, widened from
   `spawn(lane, key)` to `spawn(argv: &[String])` (a strictly more general
   signature: `tm work run <lane> <key>` and `tm pr watch <key>` are both
   "spawn this tm subcommand as argv via `current_exe()`, watch it exit").
   One trait, one fake, two call sites — narrower than adding a parallel
   `BotWatchLauncher` trait for identical child-watching mechanics.

Step 1's board key polls line up with the loop's own exit-fast design
(`tm pr watch`'s foreground-detach split from "CLI surface" above): the
watched child here is the quick resolve-and-detach step, not the whole
poll loop, exactly like `LaunchLaneRun`'s watched child is
`prepare_run_lane`'s quick preflight, not the whole lane run.

### Config

```toml
[work.review_watch]
dir = "~/Projects/axiom"          # optional; falls back to [work.audit].dir
prompt = "/bugbot-triage {key} {findings_file}"  # optional, this is the default
poll_secs = 45                    # optional, default 45
max_wait_mins = 1440              # optional, default 1440 (24h)
on_bots_done = "notify"           # optional, "notify" | "launch"; default "notify"
```

`RawReviewWatchConfig { dir, prompt, poll_secs, max_wait_mins,
on_bots_done }`, all `Option`, merged field-by-field exactly like
`merge_audit` (global then repo, `.or()` per field) — the same "single
section, no whole-vs-field ambiguity" rationale documented on
`merge_audit`. Validated `ReviewWatchConfig`'s `dir` resolution is a
two-step `.or()`: `review_watch.dir` then `audit.dir`, applied once in
`merge_work` after both subsections are merged (not in `merge_audit`,
which stays audit-only). `on_bots_done` parses to a small enum
(`OnBotsDone { Notify, Launch }`, `parse`/`as_str` mirroring `RunStatus`'s
shape); an unrecognized value is a `ConfigError`, not a silent default —
same posture as other enum-shaped config values.

## Implementation steps

Each step is TDD'd and lands as one commit; tests + clippy green via `nix
develop -c cargo ...`.

1. **`bot_login_matches` extraction + `bots_have_reviewed`.** Pull the
   matcher out of `count_bot_findings` as `pub(crate) fn
   bot_login_matches`, re-verify `count_bot_findings`'s existing 8 tests
   unchanged; add `PrReview`, `bots_have_reviewed` + table tests (all
   bots reviewed, missing one, suffix/case variants, empty bot list
   vacuously true, empty reviews).
2. **`GhCli::pr_reviews` + `pr_state`.** `resolve_repo` extraction shared
   with `pr_review_threads` (existing `pr_review_threads` tests must still
   pass unchanged); new `ShellGhCli`/`FakeGhCli` impls + parse-layer tests
   (REST reviews JSON shape, `state`/`merged` JSON shape, command-failure
   mapping) following `pr_review_threads`'s own test pattern.
3. **`pr_list` widened to `PrInfo` + `find_pr_for_ticket`.** Change the
   trait signature and both impls' JSON fields; update `pr_list`'s
   existing tests for the new shape; new `find_pr_for_ticket` table tests
   (title/body/branch key match, no match, multiple PRs picks lowest
   number).
4. **`pr_bot_finding_details` + `bot_finding_details` filter.** New
   GraphQL query + parse layer (mirroring `pr_review_threads`'s test
   shape), pure filter function tests (resolved excluded, non-bot
   excluded, ordering).
5. **Store**: `latest_run_for_ticket_kind` already exists (stream 2) — no
   store changes needed for dedup/lookup. Confirm with a test that a
   `review-watch`/`bugbot-cleanup` kind round-trips with no migration
   (mirrors the existing `kind_column_defaults_to_lane_for_rows_inserted_
   without_it` style test).
6. **Config**: `RawReviewWatchConfig`/`ReviewWatchConfig`,
   `merge_review_watch`, the `dir` fallback-to-audit step, `OnBotsDone`
   enum + parse, TOML load tests (global/repo precedence, dir fallback,
   defaults, bad `on_bots_done` value is a `ConfigError`).
7. **`tm runs register --kind` CLI.** Thin `RunsCmd::Register` wrapping
   `register_session`, tests against a fake store/env (no-op when
   `CLAUDE_CODE_SESSION_ID` unset, adopts when set — same test shape
   `register_session` itself already has).
8. **Poll loop core.** `src/github/... ` (or a new `src/work/review_watch.rs`)
   housing the pure step logic: lifecycle check → bots-done check →
   findings tally/write → event emission → give-up/error-counter — built
   and tested against `FakeGhCli`/`RunStore` with an injectable clock/sleep
   seam (no real `sleep`/`gh` in tests), covering: merged/closed exit,
   not-done tick, bots-done zero-findings, bots-done with findings +
   notify, bots-done with findings + launch (asserts the cleanup-launch
   call happens), `gh` error backoff and give-up, wall-clock timeout
   give-up.
9. **`tm pr watch` CLI + detach.** `PrCmd::Watch`, ticket→PR resolution,
   dedup check, `--foreground` flag, setsid re-exec (mirroring
   `src/work/run.rs`'s detached path), exit codes. Detach mechanics
   untestable in-process (matching `RealDetachSpawner`'s and stream 5's
   precedent) — manual verification note, not a unit test gap.
10. **`src/work/bugbot.rs` launch_cleanup.** `cleanup_session_name`,
    `launch_cleanup` against fakes (refuse-if-live, pre-register, tmux
    command construction including `{findings_file}` substitution),
    following `launch_audit`'s existing test shape.
11. **Board wiring.** `LaneLauncher` trait widened to `spawn(argv: &[String])`
    (update the stream-5 fake + its call site to pass `["work", "run",
    lane, key]`), `Cmd::LaunchBotWatch`/`LaunchCleanup`/`LoadBotWatchStatus`/
    `LoadCleanupStatus`, `BotWatchIndicator` + `bot_watch_indicator` (pure,
    table-tested), reuse of `AuditIndicator`/`audit_indicator` for cleanup,
    keymap `b` (Board-only, mirrors `a`/`w` tests), reducer precedence
    tests for all four `b` branches, card badge render tests (`ui.rs`
    `cell_at`), theme entries for the two new badge families (`bots:` /
    `clean:` accents, following the existing style-contract test shape:
    distinct fg per variant, `bg == None`).
12. **Docs.** README (config section, board key, `tm pr watch`), ROADMAP
    stream 7 → done, this file gains a Status section.

### Operational (axiom-side, out of this repo)

13. Add the `/bugbot-triage` skill to the axiom repo (or confirm it
    already exists per the roadmap's mention) with a documented first
    step of `tm runs register --kind bugbot-cleanup {key}`, reading
    `{findings_file}` for the finding list. Same "personal config, not
    tm's job" boundary ADR-0002 draws for `/ticket-audit`.
14. Sync the axiom repo's hook copies / `settings.json` if step 8's poll
    loop or step 10's launch path end up needing anything beyond what
    streams 2/4 already require synced (expected: nothing new, since
    adoption/await/resume/session-end are all reused unchanged) — verify
    during manual end-to-end testing rather than assuming.

## Out of scope

- Re-arming a running watcher when the PR receives a new push after bots
  already reviewed once (documented limitation in "Poll loop: what bots
  finished means").
- A `Notification`-based OS-level alert when bots finish — the board
  badge is the alert, same stance stream 4 took for audits.
- Multiple concurrent cleanup sessions per ticket — one `tm-bugbot-<key>`
  session at a time, same as one audit at a time.
- Auto-resolving GitHub review threads after the cleanup session pushes a
  fix — `/bugbot-triage`'s business, not tm's.
