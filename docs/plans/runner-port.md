# Plan: Port the lane runner (`j work`) to `tm work`

Governing decisions: `docs/decisions/0002-runner-absorption.md` (port, don't
vendor; the seam is "tskmstr owns what it needs to do its job") and
`docs/decisions/0001-run-state.md` (SQLite run state, hooks telemetry, no
Jira mirroring — still governs everything below the process-spawning line).
Destination: ROADMAP stream 4 (board-launched sessions), so the
session-launching core must be callable from a TUI, not just a CLI verb.

## 1. Inventory: what happens to each `j work` behavior

| OCaml behavior | Classification | Notes |
|---|---|---|
| `run <name> [ticket] ...` (provision + `claude -p` + `tm runs start/finish`) | **Ports** | The core; everything else is scaffolding around it |
| `deploy_tm_hooks` (copy hooks, generate `--settings` JSON) | **Ports** | Hooks are tm's telemetry contract, not personal config |
| `new <name> [branch] [--from base]` (worktree provision + attach) | **Ports** | Same provisioning path `run` uses when the worktree is missing |
| `remove <name>` | **Ports** | Symmetric with `new` |
| `list` (tmux sessions + worktree/session kind) | **Ports** | Feeds stream 4's "what's running" view |
| `restore` (recreate sessions after reboot) | **Ports** | Operational necessity, not personal taste |
| `branch_owner` (git config → `gh api user` → git config → `"claude"`) | **Ports** | Generic resolution logic; keep the fallback chain, keep reading `git config j.branchOwner` for compatibility |
| `--no-track` branch creation from a remote base | **Ports (verbatim)** | Load-bearing fix for a real incident (2026-08-05); see Risks |
| Billing-safety env stripping (`env -u ANTHROPIC_API_KEY ...`) | **Ports (verbatim)** | Silent-failure mode if dropped |
| `start <dir>` (attach/create tmux session for **any** directory) | **Ports** as `tm work start [<dir>]` | Decision reversed by Joe 2026-08-06: this is his main tmux entry point (tmux from the repo dir → session manager → pick a session), and an interface for attaching to sessions is in line with tm's task-managing ethos even as an ad hoc command. Defaults to cwd when no dir is given; same has-session/new-session/attach mechanism `run`/`new` use |
| Session window set `["code", "fish", "claude", "server"]` | **Stays personal (configurable)** | Jowi's shell layout, not tm's job. tm defines *that* a session gets created and attached; the window list is a config knob with a minimal default |
| `worktrees_root = ~/Worktrees` | **Stays personal, but as config not a hardcoded path** | Becomes `work.worktree_root` in `~/.config/tskmstr/config.toml` |
| `prompt_file = ~/.claude/prompts/<lane>.md` | **Stays personal, as a per-lane config field** | The convention is fine as a *default*; the path must be configurable since another tm user's prompts live elsewhere |
| Default model `"fable"` | **Stays personal, as a per-lane config field** | Hardcoding a specific model in the binary is exactly the kind of personal config the seam excludes |
| `log_dir = ~/.local/state/j-work`, hook deploy dir `~/.local/share/j-work` | **Ports, renamed** | Not personal — becomes tm's own XDG paths (`~/.local/state/tskmstr/work`, `~/.local/share/tskmstr/hooks`) |
| `Common.repo_root` / `DEVTOOLS_ROOT` (hook source location) | **Dies** | Hooks move into the tskmstr binary; there is no more "devtools repo" source of truth for them |
| `j.ml` dispatch / help text for `work` | **Dies**, replaced by `tm work --help` | See §5 for the transition |

### The tmux question

**tmux orchestration belongs to tm, not personal config.** ROADMAP stream 4
requires the board to launch a session and let the user attach/detach —
that's tmux's job (or an equivalent detachable-session primitive), and it's
exactly "the telemetry hooks it deploys, run supervision" language from
ADR-0002. If tmux stayed in devtools, the board could launch a `claude -p`
process but couldn't hand the user a way to attach to it — the roadmap item
would be undoable. So: **`tm` owns creating/attaching/listing/killing
sessions.** What's personal is *what's in the session* — which extra windows
get created (`fish`, `server`, ...) and their names — because that's Jowi's
shell habits, not a task-master concern. That list is a config knob with a
minimal default (e.g. a single window), not a removed feature.

## 2. Config design

New `[work]` section in `~/.config/tskmstr/config.toml` (global) with
per-lane subsections. Repo-local `.tskmstr.toml` override follows the
existing merge precedence (`config/mod.rs`).

```toml
[work]
worktree_root = "~/Worktrees"        # personal path, was hardcoded
default_model = "fable"              # personal driver-model pin
default_max_turns = 200
default_permission_mode = "acceptEdits"
tmux_windows = ["shell"]             # extra windows beyond the primary one
tmux_primary_window = "code"

[work.lanes.partner-integrations]
repo = "/Users/jowi/Projects/axiom"  # REQUIRED, see below — not optional
prompt_file = "~/.claude/prompts/partner-integrations.md"  # defaults to this convention if omitted
base_branch = "staging"              # overrides `origin/HEAD` default
model = "sonnet"                     # overrides default_model
max_turns = 300
```

**Design change from the OCaml version, not just a restatement:** `work.ml`
resolves the target repo via `git_repo_root()` on the *current working
directory* — it assumes you're standing inside the repo you want to run a
lane against. That assumption breaks for `tm work run` invoked from tm's own
directory, a cron job, or (the whole point of stream 4) a board TUI that
isn't "in" any repo at all. Each lane must declare its `repo` path
explicitly in config. `tm work run <lane>` then `cd`s (or sets the git
working directory) there itself rather than trusting `cwd`.

## 3. Hook migration

Recommendation: **embed the six scripts via `include_str!` in the tm binary,
write them to `~/.local/share/tskmstr/hooks/` at deploy time**, identical in
spirit to `deploy_tm_hooks` — copy-on-every-run is cheap and idempotent, so
there's no install-time step to forget. Concretely:

- Move `claude-hooks/*.sh` from devtools into `tskmstr/hooks/*.sh` (tracked
  in this repo).
- A `src/work/hooks.rs` module with `const HOOK_SCRIPTS: &[(&str, &str)]`
  built from `include_str!("../../hooks/tm-event.sh")` etc.
- `deploy_hooks()` writes each to the deploy dir, `chmod 0o755`, and
  generates the `--settings` JSON via `serde_json` (not hand-built string
  formatting — the OCaml version's `sprintf`-based JSON construction is a
  latent injection/escaping risk that serde_json removes for free).
- The scripts themselves **stay bash+jq**. Claude Code's hook protocol
  invokes an arbitrary executable with JSON on stdin; rewriting them as a
  `tm hooks run <name>` Rust subcommand is possible and would drop the `jq`
  dependency, but it's not required by the seam (a hook is "what tm
  deploys," not "must be Rust") and is extra risk for this port. Note it as
  a good follow-up, not part of this plan.
- `guard-delegate.sh`'s lane policy (no direct edits from the main loop
  during a tracked run) ports unchanged — it's a tm run-telemetry/policy
  concern per ADR-0001's hook table.

## 4. Architecture change worth making during the port

The OCaml version duplicates result-parsing logic between the `--fg` path
(reads `out_json` via `jq` inline) and the detached path (a generated shell
wrapper that re-reads the same file via `jq`). In Rust this duplication has
no reason to exist: write one function that spawns `claude`, waits, and
parses its JSON result with `serde_json`, and have both `--fg` and detached
modes call it. This also deletes the wrapper-script-generation step
entirely (no more writing a `.sh` file to disk and `nohup`-ing it) — the
detached path becomes "spawn a detached child that runs the same in-process
logic," not "generate and shell out to a bespoke script." It also deletes
the version-skew guard in the OCaml wrapper (`tm runs finish --help | grep
-q -- --model-usage`) — that existed only because the wrapper shelled out to
whatever `tm` happened to be on `PATH`; calling `RunStore` directly removes
the concept of "the installed tm might not support this flag." Use the
existing `crate::github::gh_cli` module for the PR-URL lookup rather than
shelling to `gh` ad hoc, matching existing patterns.

## 5. Migration / compatibility path

Cheapest safe path: **don't touch OCaml until `tm work` has parity and has
been dogfooded.** Build `tm work` fully in this repo; devtools' `j work`
keeps working entirely unchanged in parallel (different worktree/session
names won't collide for real lanes). Once `tm work run` has run a handful of
real lanes successfully, replace `j.ml`'s `work` dispatch with a one-line
shim (`exec tm work "$@"`) in devtools — that's a devtools-repo change, out
of scope for this repo's commits, but it's the cheapest way to keep `j work`
muscle memory working without maintaining two implementations. Do not build
a compatibility shim *in* tskmstr; there's nothing here for it to call back
into.

## 6. Ordered, individually-committable steps

Each sized for one Sonnet subagent + TDD checkpoint commit. Pure logic gets
plain unit tests; anything touching processes, tmux, or the filesystem gets
a trait + fake, mirroring `Prompter`/`GhCli`-style seams already in this
codebase.

1. **Config: `[work]` section.** Extend `config/mod.rs` `RawConfig`/`Config`
   with `worktree_root`, `default_model`, `default_max_turns`,
   `default_permission_mode`, `tmux_windows`, `tmux_primary_window`, and a
   `lanes: BTreeMap<String, LaneConfig>` (with `repo` required, everything
   else optional and falling back to the `work`-level defaults). Pure
   TOML parsing + merge; tests mirror the existing `merge_*`/`load_*` style.
2. **Pure naming/path helpers.** `src/work/naming.rs`: worktree path
   construction, session name derivation (dots→dashes), branch name
   (`owner/lane-timestamp`), timestamp formatting. All pure functions of
   their inputs — no fakes needed, straightforward TDD.
3. **Git operations trait.** `src/work/git.rs`: a `GitOps` trait
   (`repo_root`, `is_worktree`, `branch_exists_local`, `branch_exists_remote`,
   `provision_worktree`, `status_is_clean`, `switch_new_branch`,
   `default_base`) with a real impl (shells to `git`) and a `FakeGitOps` for
   tests. Port the `--no-track` behavior verbatim (see Risks) with a test
   asserting the exact flag is present when cutting a branch from a remote
   base.
4. **tmux operations trait.** `src/work/tmux.rs`: `TmuxOps` trait
   (`has_session`, `new_session`, `new_window`, `select_window`, `attach`,
   `kill_session`, `list_sessions`) + fake. Session/window creation reads
   `tmux_windows`/`tmux_primary_window` from config rather than the
   hardcoded `["fish"; "claude"; "server"]`.
5. **`tm work new/remove/list/restore/start`.** Wire `GitOps`+`TmuxOps` into
   CLI subcommands under `Command::Work`. `start [<dir>]` attaches to (or
   creates) the tmux session for a directory, defaulting to cwd — Joe's
   main tmux entry point, ported per the amended inventory. Tests use the
   fakes; assert the same provisioning/attach/removal sequencing as
   `work.ml`.
6. **Hook scripts into this repo.** Move the six `.sh` files from devtools
   into `tskmstr/hooks/`, add `src/work/hooks.rs` with `include_str!`-backed
   constants, `deploy_hooks()`, and settings-JSON generation via
   `serde_json::json!`. Test: deployed file contents match the embedded
   source; generated settings JSON matches the expected hook-matcher shape
   (parse it back and assert on structure, not string equality).
7. **Claude invocation argv builder (pure).** A function taking lane config
   + run options (`ticket`, `model`, `max_turns`, `permission_mode`,
   `prompt`) and returning the exact `Command` args/env for `claude -p`,
   including the billing-safety env removals and `--settings` wiring. Pure
   and highly testable without spawning anything.
8. **Process spawn + result parsing.** `src/work/runner.rs`: a
   `ProcessSpawner` trait (`spawn`, wrapping `std::process::Command`) +
   fake, and a single function that spawns, waits, reads the output-JSON
   file, and returns a typed `RunOutcome` (session_id, cost, turns,
   is_error, result text) via `serde_json` — the one parsing path §4 calls
   for. Test with the fake spawner writing a canned JSON file.
9. **`tm work run --fg`.** Wire steps 2/3/7/8 plus `RunStore::start_run`/
   `finish_run` for the foreground path. Integration test: fake
   `GitOps`+`ProcessSpawner`, real temp-file `RunStore`, assert the run row
   ends up `done`/`failed` correctly and the printed summary matches.
10. **Detached mode.** Design and implement process detachment (see Risks —
    this is the step most likely to need iteration). Isolate OS-specific
    bits (`setsid`, stdio redirection) behind a small seam so the
    argument-building/logging-path-construction is still unit-testable even
    though the actual detach can only be verified manually/by integration
    test. Manual test plan: spawn detached, close the terminal, confirm the
    run row still reaches `done`.
11. **`tm work run` CLI wiring end-to-end.** Full clap surface matching
    `work.ml`'s options (`--from`, `--model`, `--max-turns`,
    `--permission-mode`, `--prompt`, `--fg`); dispatch to steps 9/10.
12. **PR URL + docs.** Reuse `github::gh_cli` for the PR lookup (replacing
    the ad hoc `gh pr list`/regex-scrape fallback with the existing typed
    module, keeping both the direct-lookup and result-text-scrape fallback
    since the OCaml version's belt-and-suspenders approach is deliberate).
    Update `tm work --help` and this repo's docs; note in ROADMAP that
    stream 1 is complete.

Step 10 is the one to budget extra review time for; everything else follows
established patterns already in this codebase (trait + fake, pure-function
extraction, clap subcommand wiring).

## 7. Risks

- **`--no-track` on branch creation.** Cutting a branch from a remote base
  without `--no-track` makes `push.default=tracking` push straight to the
  base branch — this caused a real incident (2026-08-05, called out twice in
  `work.ml`'s comments). Must be ported exactly, with a regression test
  asserting the flag is always present when `from_opt` is `Some`.
- **Billing-safety env stripping.** `env -u ANTHROPIC_API_KEY -u
  ANTHROPIC_AUTH_TOKEN -u CLAUDECODE` prevents silently billing the API
  instead of the subscription. Undocumented anywhere except a code comment;
  losing it fails silently (no error, just an unexpected bill).
- **Jowi-specific paths baked into the binary today**, all needing to become
  config per §2/§1: `~/Worktrees`, `~/.claude/prompts/<lane>.md`, the
  `"fable"` model default, the `["code","fish","claude","server"]` window
  set, `~/.local/state/j-work` / `~/.local/share/j-work`. Missing any one of
  these in the config design leaves a hardcoded personal assumption in a
  supposedly general tool.
- **Signal / child-process handling, Rust vs. OCaml.** OCaml's detached mode
  is just `nohup sh '<wrapper>' >>log 2>&1 </dev/null &` via `Sys.command`
  — shell semantics handle detachment, and the *wrapper script* (not the `j`
  process) does the waiting and `tm runs finish` call, so `j` can exit
  immediately without needing to survive its own detached child. Once that
  logic moves in-process (§4), tm loses the free detachment shell gave it:
  something has to both (a) let the initiating `tm work run` invocation
  return the terminal immediately, and (b) still be alive later to call
  `RunStore::finish_run` when `claude` exits. Plausible approaches: a
  self-re-exec (`tm` spawns itself with a hidden supervisor flag, detaches
  via `setsid`, redirects stdio to the log file, and exits) or a genuine
  double-fork. Get this wrong and either runs never reach a terminal status
  (row stuck `running` forever, relying on `reap` to paper over it) or
  zombie processes accumulate. This needs deliberate design in step 10, not
  a quick port.
- **`branch_owner` network call at run start.** `gh api user -q .login` hits
  the network on every run start (mitigated today by being one call per
  invocation) — verify this doesn't become a latency problem once wrapped
  in whatever detachment mechanism step 10 lands on.
- **Undocumented tolerance behaviors.** Several hook scripts have specific
  fallback/tolerance logic that's easy to drop by accident during a rewrite:
  `tm-tasklist.sh`'s fallback to parsing `"Task #N"` out of response text
  when the structured `tool_response.task.id` field is absent (older Claude
  Code versions); the graphify-nudge rate limit (5 nudges/session via a
  counter file); the PR-URL fallback chain (`gh pr list` → regex-scrape the
  result text). None of these are load-bearing for correctness, but losing
  them silently degrades UX with no test to catch it — worth an explicit
  parity checklist when hooks move (step 6), even though the scripts
  themselves aren't being rewritten.
