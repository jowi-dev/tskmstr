//! `tm work run --fg`: the foreground lane-run core, ported from devtools'
//! `~/devtools/work.ml`'s `run_lane` (the `opts.fg` branch, lines ~519-566).
//!
//! [`run_lane_fg`] is the single entry point: it takes every dependency as
//! an injected trait object ([`GitOps`], [`GhCli`], [`ProcessSpawner`],
//! [`RunStore`], [`Clock`]) plus a [`WorkConfig`] and a `Write` sink, so it's
//! callable from `tm work run` (see `src/cli/work.rs`) and, per
//! `docs/plans/runner-port.md`'s destination note, later from a TUI —
//! neither layer needs to duplicate this sequencing.
//!
//! Unlike `tm work new`/`start`, this never touches tmux: `work.ml`'s
//! `run_lane` doesn't create or attach any tmux session in either its `--fg`
//! or detached branch — sessions are `new`/`start`'s job, not `run`'s.
//!
//! # The ported `--fg` sequence
//!
//! 1. Resolve the lane's config entry (`repo`, `prompt_file`, `base_branch`,
//!    `model`, `max_turns`, `permission_mode`), falling back to
//!    `work`-level defaults. Unlike `new`/`remove` (which fall back to
//!    resolving the repo from `cwd`), a lane run always requires a
//!    configured lane — `run_lane` has no cwd-based repo-resolution path,
//!    and there is no `cwd` to fall back to (this is the specific case
//!    `docs/plans/runner-port.md` §2 requires the `repo` field for).
//! 2. Resolve the prompt file: `--prompt` override, else the lane's
//!    `prompt_file`, else `~/.claude/prompts/<lane>.md` (`work.ml`'s
//!    default). Error before spawning anything if it doesn't exist.
//! 3. Derive `wt_name` (worktree/branch-prefix name): the lowercased ticket
//!    if one was given, else the lane name (`run_lane`'s `wt_name`).
//! 4. Provision the worktree if `wt_path` doesn't exist yet, cutting
//!    `wt_name` as the initial branch from the resolved base. If
//!    `<repo_root>/.env.local` exists, [`GitOps::provision_worktree`]
//!    symlinks it into the new worktree and this prints `work.ml`'s
//!    "Linked .env.local from main repo" message.
//! 5. Fetch `origin` in the worktree ([`GitOps::fetch_origin`]), so the base
//!    ref this run's branch is about to be cut from (step 6) is current —
//!    `work.ml` does this immediately after provisioning-if-missing, before
//!    the dirty check. `work.ml` ignores this call's exit status entirely
//!    (`let _ = Sys.command ...`); this port surfaces the error but only
//!    warns and continues, since an offline fetch failure shouldn't sink an
//!    otherwise-viable run. Then error if the worktree is dirty (a previous
//!    run may have left work behind) — `work.ml`'s `git status --porcelain`
//!    guard.
//! 6. Resolve the branch owner (see [`resolve_branch_owner`]) and cut this
//!    run's fresh timestamped branch off the resolved base
//!    (`GitOps::switch_new_branch`, always `--no-track`).
//! 7. Read the prompt file and append `"\n\nWork ticket: <ticket>."` when a
//!    ticket was given.
//! 8. Deploy the hooks and write the generated `--settings` JSON to
//!    `hooks_deploy_dir/settings.json`.
//! 9. [`RunStore::start_run`] — ticket (falling back to the lane name when
//!    untracked by ticket), lane, worktree, branch, and the current
//!    process's pid (this *is* the driver process for `--fg`, unlike the
//!    detached path's separate supervisor).
//! 10. Build the `claude` invocation ([`crate::work::claude`]) with the new
//!     run id wired in as `TSKMSTR_RUN_ID`, and spawn it
//!     ([`crate::work::runner`]), stdout redirected to
//!     `state_dir/<wt_name>-<timestamp>.json`.
//! 11. Parse the result JSON ([`crate::work::runner::parse_run_outcome`]).
//!     A non-zero exit status forces a failed outcome regardless of what
//!     the JSON says (mirrors `work.ml`'s `if status <> 0 then ... exit
//!     status`, which never gets as far as inspecting `is_error`).
//! 12. [`RunStore::finish_run`] with `Done`/`Failed`, the parsed
//!     session id/cost/turns, and the model-usage map re-serialized to JSON.
//! 13. Print the run summary (`work.ml`'s final `printf` block).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::WorkConfig;
use crate::github::gh_cli::{GhCli, GhError};
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError, StartRun};
use crate::work::claude::{ClaudeInvocationInputs, build_claude_invocation};
use crate::work::git::{GitError, GitOps};
use crate::work::hooks::{self, HooksError};
use crate::work::naming::{self, expand_tilde};
use crate::work::runner::{ProcessSpawner, SpawnError, SpawnRequest, parse_run_outcome};

/// Errors that can occur while running [`run_lane_fg`].
#[derive(Debug, Error)]
pub enum RunLaneError {
    /// `name` doesn't match any lane in `config.work.lanes`. A lane run,
    /// unlike `tm work new`/`remove`, has no cwd-based fallback (see the
    /// module doc's step 1).
    #[error("no configured lane named `{0}` (see [work.lanes] in config)")]
    UnknownLane(String),

    /// The resolved prompt file doesn't exist. Checked before any
    /// provisioning or spawning, mirroring `work.ml`'s up-front check.
    #[error("no prompt file at {}", .0.display())]
    PromptFileMissing(PathBuf),

    /// The worktree has uncommitted changes left over from a previous run.
    #[error("worktree {} is dirty — a previous run may have left work behind", .0.display())]
    WorktreeDirty(PathBuf),

    /// A `git` shell-out failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A `gh` shell-out failed. Only surfaced for calls other than
    /// [`GhCli::current_user_login`], whose failures are tolerated (see
    /// [`resolve_branch_owner`]).
    #[error(transparent)]
    Gh(#[from] GhError),

    /// Hook deployment failed.
    #[error(transparent)]
    Hooks(#[from] HooksError),

    /// A run-state store operation failed.
    #[error(transparent)]
    RunStore(#[from] RunStoreError),

    /// Spawning or waiting on `claude` failed.
    #[error(transparent)]
    Spawn(#[from] SpawnError),

    /// A filesystem/output-write operation failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// Serializing the generated hooks `--settings` JSON to disk failed.
    #[error("failed to serialize settings JSON: {0}")]
    SettingsJson(#[from] serde_json::Error),

    /// The repo's default base branch could not be derived and no
    /// `base_branch`/`--from` override was given.
    #[error(
        "could not resolve a base branch for `{0}` (no --from, lane base_branch, or origin/HEAD)"
    )]
    NoBaseBranch(String),
}

/// A clock abstraction supplying "now" as already-broken-down local time
/// components, matching [`naming::format_timestamp`]'s inputs. No trait like
/// this existed before this module — [`naming`]'s doc comments anticipated
/// exactly this seam ("the caller resolves 'now' ... and supplies the
/// already-broken-down components"), so this follows the same trait+fake
/// pattern as [`GitOps`]/[`TmuxOps`](crate::work::tmux::TmuxOps)/
/// [`ProcessSpawner`] rather than reading the clock inline.
pub trait Clock {
    /// `(year, month, day, hour, min, sec)`, `month` 1-12, in local time.
    fn now_parts(&self) -> (i32, u32, u32, u32, u32, u32);
}

/// Production [`Clock`] backed by `libc::time`/`libc::localtime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_parts(&self) -> (i32, u32, u32, u32, u32, u32) {
        // SAFETY: `time(NULL)` and `localtime` on a valid `time_t` pointer
        // are both side-effect-free with respect to Rust's aliasing rules;
        // `localtime` returns a pointer to a `libc`-owned static `tm`, which
        // is copied out (by value) before this function returns, so no
        // reference escapes.
        unsafe {
            let mut t: libc::time_t = 0;
            libc::time(&mut t);
            let tm_ptr = libc::localtime(&t);
            let tm = *tm_ptr;
            (
                tm.tm_year + 1900,
                (tm.tm_mon + 1) as u32,
                tm.tm_mday as u32,
                tm.tm_hour as u32,
                tm.tm_min as u32,
                tm.tm_sec as u32,
            )
        }
    }
}

/// A [`Clock`] test double returning a fixed, caller-supplied time.
#[derive(Debug, Clone, Copy)]
pub struct FakeClock(pub (i32, u32, u32, u32, u32, u32));

impl Clock for FakeClock {
    fn now_parts(&self) -> (i32, u32, u32, u32, u32, u32) {
        self.0
    }
}

/// Dependencies [`run_lane_fg`] needs, gathered the same way
/// [`crate::cli::work::WorkContext`] gathers `GitOps`/`TmuxOps`: trait
/// objects the caller wires up once (real or fake), threaded through by
/// reference.
pub struct RunLaneDeps<'a> {
    /// Git operations (real or fake).
    pub git: &'a dyn GitOps,
    /// `gh` CLI operations (real or fake), used for branch-owner resolution.
    pub gh: &'a dyn GhCli,
    /// Process spawning (real or fake).
    pub spawner: &'a dyn ProcessSpawner,
    /// The run-state store `start_run`/`finish_run` are called against.
    pub run_store: &'a RunStore,
    /// "Now" source for the run's timestamp.
    pub clock: &'a dyn Clock,
}

/// Already-resolved filesystem locations [`run_lane_fg`] needs, per
/// `docs/plans/runner-port.md` §1's renaming of `work.ml`'s
/// `~/.local/{state,share}/j-work` to tm's own XDG paths.
pub struct RunLanePaths {
    /// The invoking user's home directory, for `~`-expanding
    /// `config.worktree_root` and the default prompt-file convention.
    pub home: PathBuf,
    /// Where per-run output JSON is written
    /// (`~/.local/state/tskmstr/work`).
    pub state_dir: PathBuf,
    /// Where hook scripts and the generated `--settings` JSON are deployed
    /// (`~/.local/share/tskmstr/hooks`).
    pub hooks_deploy_dir: PathBuf,
}

/// `tm work run <lane>`'s CLI-level options, already parsed but not yet
/// resolved against lane config.
#[derive(Debug, Clone, Default)]
pub struct RunLaneRequest {
    /// `--ticket`/positional ticket argument, if given. Scopes the
    /// worktree/branch name and is appended to the prompt.
    pub ticket: Option<String>,
    /// `--from`: base branch override, taking precedence over the lane's
    /// `base_branch` and the repo's `origin/HEAD` default.
    pub from_base: Option<String>,
    /// `--model` override, taking precedence over the lane's `model` and
    /// `work.default_model`.
    pub model: Option<String>,
    /// `--max-turns` override, taking precedence over the lane's
    /// `max_turns` and `work.default_max_turns`.
    pub max_turns: Option<String>,
    /// `--permission-mode` override, taking precedence over the lane's
    /// `permission_mode` and `work.default_permission_mode`.
    pub permission_mode: Option<String>,
    /// `--prompt`: prompt file path override, taking precedence over the
    /// lane's `prompt_file` and the `~/.claude/prompts/<lane>.md` default.
    pub prompt_override: Option<String>,
}

/// Everything [`run_claude_and_finish`] needs to spawn `claude`, wait, parse
/// its result, and finish the tracked run — the output of
/// [`prepare_run_lane`] (steps 1-9 of the module doc's sequence), consumed by
/// steps 10-13.
///
/// Deliberately `Serialize`/`Deserialize`: this is also the exact state a
/// detached run's foreground half hands to its self-re-exec'd supervisor
/// (see `src/work/detach.rs`), written to a JSON file the supervisor process
/// reads back on startup since it shares no memory with its parent. Nothing
/// here is a trait object or a borrowed reference, so it round-trips cleanly.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedRun {
    /// The `RunStore` row id [`prepare_run_lane`] created via `start_run`.
    pub run_id: i64,
    /// The lane name this run was prepared for.
    pub lane: String,
    /// The ticket given for this run, if any (used for the detached path's
    /// printed `resume: tm runs resume <ticket>` line).
    pub ticket: Option<String>,
    /// The worktree/branch-prefix name (`run_lane`'s `wt_name`).
    pub wt_name: String,
    /// The timestamp this run's branch/log/state files are suffixed with.
    pub timestamp: String,
    /// The worktree path this run executes in.
    pub worktree: PathBuf,
    /// The fresh branch cut for this run.
    pub branch: String,
    /// The fully resolved `claude` invocation.
    pub invocation: crate::work::claude::ClaudeInvocation,
    /// Where the spawned `claude` process's stdout (its result JSON) is
    /// written, and later read back from.
    pub out_json_path: PathBuf,
}

/// The result of one completed `tm work run --fg` invocation.
#[derive(Debug, Clone)]
pub struct RunLaneOutcome {
    /// The `RunStore` row id created for this run.
    pub run_id: i64,
    /// Whether the run is considered failed — a non-zero `claude` exit
    /// status, or `.is_error: true` in its result JSON. Callers (the CLI
    /// layer) use this to decide the process's exit code.
    pub is_error: bool,
    /// The worktree path this run executed in.
    pub worktree: PathBuf,
    /// The fresh branch cut for this run.
    pub branch: String,
    /// `claude`'s reported session id, when the result JSON parsed
    /// successfully.
    pub session_id: Option<String>,
}

/// Sanitize a resolved branch-owner candidate for use as a branch-name
/// segment, mirroring `work.ml`'s `sanitize_branch_owner`: trim whitespace
/// and strip anything outside `[A-Za-z0-9-_.]` (case preserved).
pub fn sanitize_branch_owner(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

/// Resolve the branch-owner prefix used when cutting a lane run's branch,
/// mirroring `work.ml`'s `branch_owner` exactly: first hit wins, an empty
/// resolution (after sanitizing) falls through to the next source.
///
/// 1. `git config --get j.branchOwner` — explicit per-machine override.
/// 2. `gh api user -q .login` — the active `gh` session's GitHub handle.
/// 3. `git config --get github.user` — a common local convention.
/// 4. `"claude"` — fallback so runs never fail on naming.
///
/// Every source's failure (a `git`/`gh` error, not just "not found") is
/// tolerated the same as an empty result, matching `work.ml`'s
/// `2>/dev/null`-redirected shell-outs, which never distinguish "command
/// failed" from "command printed nothing".
pub fn resolve_branch_owner(git: &dyn GitOps, gh: &dyn GhCli, dir: &Path) -> String {
    let non_empty = |s: Option<String>| -> Option<String> {
        let sanitized = sanitize_branch_owner(&s?);
        if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        }
    };

    non_empty(git.config_get(dir, "j.branchOwner").ok().flatten())
        .or_else(|| non_empty(gh.current_user_login().ok().flatten()))
        .or_else(|| non_empty(git.config_get(dir, "github.user").ok().flatten()))
        .unwrap_or_else(|| "claude".to_string())
}

/// Resolve the prompt file path for a lane run: `--prompt` override, else
/// the lane's configured `prompt_file`, else `~/.claude/prompts/<lane>.md`
/// (`work.ml`'s default), `~`-expanded against `home`.
fn resolve_prompt_path(
    lane: &str,
    prompt_override: Option<&str>,
    lane_prompt_file: Option<&str>,
    home: &Path,
) -> PathBuf {
    let raw = prompt_override
        .or(lane_prompt_file)
        .map(str::to_string)
        .unwrap_or_else(|| format!("~/.claude/prompts/{lane}.md"));
    expand_tilde(&raw, home)
}

/// The configured worktree root, `~`-expanded against `home`, falling back
/// to `work.ml`'s hardcoded default (`~/Worktrees`) when unset. Mirrors
/// `crate::cli::work::resolve_worktree_root`, duplicated here (rather than
/// shared) because `cli::work` depends on `work::run`, not the reverse.
fn resolve_worktree_root(config: &WorkConfig, home: &Path) -> PathBuf {
    let raw = config.worktree_root.as_deref().unwrap_or("~/Worktrees");
    expand_tilde(raw, home)
}

/// The final path component of `repo_root`, used as the worktree root's
/// per-repo subdirectory, mirroring `work.ml`'s `repo_name`.
fn repo_name(repo_root: &Path) -> Option<String> {
    repo_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

/// Run one foreground lane run: provision (if needed), cut this run's
/// branch, invoke `claude -p`, record the outcome in `run_store`, and print
/// the summary to `out`. See the module doc comment for the full ported
/// sequence.
///
/// Composed from [`prepare_run_lane`] (steps 1-9, provisioning through
/// `start_run`, recording *this* process's pid since `--fg` is itself the
/// driver) followed by [`run_claude_and_finish`] (steps 10-13, spawn through
/// the printed summary) — the same split the detached path
/// (`src/work/detach.rs`) uses, except the detached path hands
/// [`prepare_run_lane`]'s output to a re-exec'd supervisor instead of
/// calling [`run_claude_and_finish`] itself.
pub fn run_lane_fg(
    deps: &RunLaneDeps<'_>,
    config: &WorkConfig,
    paths: &RunLanePaths,
    lane: &str,
    request: RunLaneRequest,
    out: &mut dyn Write,
) -> Result<RunLaneOutcome, RunLaneError> {
    let prepared = prepare_run_lane(
        deps,
        config,
        paths,
        lane,
        request,
        Some(std::process::id()),
        out,
    )?;
    run_claude_and_finish(deps.spawner, deps.gh, deps.run_store, &prepared, out)
}

/// Steps 1-9 of the module doc's ported sequence: resolve the lane, preflight
/// the prompt file, provision the worktree if missing, cut this run's fresh
/// branch, deploy hooks, and [`RunStore::start_run`] — everything that must
/// happen in the foreground so errors (unknown lane, missing prompt, dirty
/// worktree, a failed `git`/`gh` call) surface to the invoking terminal
/// immediately, before any `claude` process is spawned and before any run
/// row exists to leak if something below fails.
///
/// `pid` is what gets recorded on the `start_run` row: `Some(current pid)`
/// for `--fg` (this process *is* the driver and stays alive for the whole
/// run), `None` for the detached path, whose caller re-execs a separate
/// supervisor process that records its own pid via
/// [`RunStore::update_pid`] immediately on startup — see
/// `src/work/detach.rs`'s module doc for why a two-step pid handoff, not the
/// parent's pid, is what keeps `tm runs reap` accurate.
pub fn prepare_run_lane(
    deps: &RunLaneDeps<'_>,
    config: &WorkConfig,
    paths: &RunLanePaths,
    lane: &str,
    request: RunLaneRequest,
    pid: Option<u32>,
    out: &mut dyn Write,
) -> Result<PreparedRun, RunLaneError> {
    let lane_config = config
        .lanes
        .get(lane)
        .ok_or_else(|| RunLaneError::UnknownLane(lane.to_string()))?;
    let repo_root = PathBuf::from(&lane_config.repo);

    // Step 2: resolve and preflight the prompt file.
    let prompt_path = resolve_prompt_path(
        lane,
        request.prompt_override.as_deref(),
        lane_config.prompt_file.as_deref(),
        &paths.home,
    );
    if !prompt_path.exists() {
        return Err(RunLaneError::PromptFileMissing(prompt_path));
    }

    // Step 3: derive the worktree/branch-prefix name.
    let wt_name = request
        .ticket
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_else(|| lane.to_string());

    let worktree_root = resolve_worktree_root(config, &paths.home);
    let repo = repo_name(&repo_root).unwrap_or_else(|| lane.to_string());
    let wt_path = naming::worktree_path(&worktree_root.to_string_lossy(), &repo, &wt_name);

    // Step 4: provision the worktree if it doesn't exist yet.
    let resolve_base = |git: &dyn GitOps| -> Result<String, RunLaneError> {
        if let Some(base) = request.from_base.clone() {
            return Ok(base);
        }
        if let Some(base) = lane_config.base_branch.clone() {
            return Ok(base);
        }
        git.default_base(&repo_root)
            .map_err(RunLaneError::from)
            .or(Err(RunLaneError::NoBaseBranch(lane.to_string())))
    };

    if !wt_path.exists() {
        let base = resolve_base(deps.git)?;
        let linked = deps
            .git
            .provision_worktree(&repo_root, &wt_path, &wt_name, Some(&base))?;
        if linked {
            writeln!(out, "Linked .env.local from main repo")?;
        }
    }

    // Step 5: fetch origin so the base ref this run's branch is about to be
    // cut from is current. work.ml ignores this call's exit status
    // entirely (`let _ = Sys.command ...`) — an offline fetch failure
    // shouldn't make an otherwise-viable run fail, so this warns and
    // continues rather than propagating the error.
    if let Err(err) = deps.git.fetch_origin(&wt_path) {
        writeln!(out, "warning: git fetch origin failed: {err}")?;
    }

    // Step 5b: refuse to run in a dirty worktree.
    if !deps.git.status_is_clean(&wt_path)? {
        return Err(RunLaneError::WorktreeDirty(wt_path));
    }

    // Step 6: cut this run's fresh branch.
    let base = resolve_base(deps.git)?;
    let owner = resolve_branch_owner(deps.git, deps.gh, &repo_root);
    let (year, month, day, hour, min, sec) = deps.clock.now_parts();
    let timestamp = naming::format_timestamp(year, month, day, hour, min, sec);
    let branch = naming::branch_name(&owner, &wt_name, &timestamp);
    deps.git.switch_new_branch(&wt_path, &branch, &base)?;

    writeln!(out, "worktree: {}  branch: {}", wt_path.display(), branch)?;

    // Step 7: build the prompt.
    let prompt_text = std::fs::read_to_string(&prompt_path)?;
    let prompt = match request.ticket.as_deref() {
        Some(ticket) => format!("{prompt_text}\n\nWork ticket: {ticket}."),
        None => prompt_text,
    };

    // Step 8: deploy hooks + settings.
    let settings = hooks::deploy_hooks(&paths.hooks_deploy_dir)?;
    let settings_path = paths.hooks_deploy_dir.join("settings.json");
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    // Step 9: start the tracked run. `pid` is `Some(current pid)` for --fg
    // (this process is the driver) and `None` for the detached path (the
    // caller re-execs a separate supervisor, which records its own pid via
    // `RunStore::update_pid` on startup — see this function's doc comment).
    let ticket_field = request.ticket.clone().unwrap_or_else(|| lane.to_string());
    let run_id = deps.run_store.start_run(&StartRun {
        ticket: ticket_field,
        lane: lane.to_string(),
        worktree: wt_path.to_string_lossy().into_owned(),
        branch: Some(branch.clone()),
        pid,
        kind: "lane".to_string(),
    })?;

    // Build the claude invocation (still part of step 9's "safe to do in the
    // foreground" work: pure argv construction, no spawning yet).
    let model = request
        .model
        .clone()
        .or_else(|| lane_config.model.clone())
        .or_else(|| config.default_model.clone());
    let max_turns = request
        .max_turns
        .clone()
        .or_else(|| lane_config.max_turns.map(|t| t.to_string()))
        .or_else(|| config.default_max_turns.map(|t| t.to_string()));
    let permission_mode = request
        .permission_mode
        .clone()
        .or_else(|| lane_config.permission_mode.clone())
        .or_else(|| config.default_permission_mode.clone());

    let invocation = build_claude_invocation(ClaudeInvocationInputs {
        prompt,
        model,
        max_turns,
        permission_mode,
        settings_path: settings_path.clone(),
        run_id: Some(run_id.to_string()),
    });

    std::fs::create_dir_all(&paths.state_dir)?;
    let out_json_path = paths.state_dir.join(format!("{wt_name}-{timestamp}.json"));

    Ok(PreparedRun {
        run_id,
        lane: lane.to_string(),
        ticket: request.ticket.clone(),
        wt_name,
        timestamp,
        worktree: wt_path,
        branch,
        invocation,
        out_json_path,
    })
}

/// Regex matching a GitHub pull-request URL in free-form text, mirroring
/// `work.ml`'s `grep -oE 'https://github[^ )]*/pull/[0-9]+'` fallback scrape
/// of the run's result text.
fn pr_url_regex() -> regex::Regex {
    regex::Regex::new(r"https://github[^\s)]*/pull/[0-9]+").expect("static regex is valid")
}

/// Resolve the PR URL for a finished run, mirroring `work.ml`'s
/// belt-and-suspenders order: prefer asking `gh` directly (accurate even if
/// the summary text never mentions the URL), then fall back to scraping the
/// first GitHub pull-request URL out of the run's result text. Both steps
/// degrade to `None` on any failure — a missing PR is the normal case for a
/// run that never opened one, never an error worth surfacing.
fn resolve_pr_url(gh: &dyn GhCli, branch: &str, result_text: Option<&str>) -> Option<String> {
    if let Ok(Some(url)) = gh.pr_url_for_branch(branch)
        && !url.is_empty()
    {
        return Some(url);
    }
    let text = result_text?;
    pr_url_regex().find(text).map(|m| m.as_str().to_string())
}

/// Steps 10-13 of the module doc's ported sequence: spawn `claude`, wait,
/// parse its result JSON, [`RunStore::finish_run`], and print the summary to
/// `out`. The one tail both `--fg` ([`run_lane_fg`]) and the detached
/// supervisor ([`supervise_run`]) call — per
/// `docs/plans/runner-port.md` §4, there is exactly one spawn-wait-parse
/// path, not one per run mode.
///
/// `gh` is used for the post-run PR-URL lookup (`gh pr list --head <branch>`,
/// falling back to scraping the result text — see [`resolve_pr_url`]),
/// ported from `work.ml`'s detached wrapper script so both `--fg` and
/// detached runs record `pr_url` the same way.
pub fn run_claude_and_finish(
    spawner: &dyn ProcessSpawner,
    gh: &dyn GhCli,
    run_store: &RunStore,
    prepared: &PreparedRun,
    out: &mut dyn Write,
) -> Result<RunLaneOutcome, RunLaneError> {
    let invocation = &prepared.invocation;

    let status = spawner.spawn(SpawnRequest {
        program: &invocation.program,
        args: &invocation.args,
        env_set: &invocation.env_set,
        env_remove: &invocation.env_remove,
        current_dir: &prepared.worktree,
        stdout_path: &prepared.out_json_path,
    })?;

    // Parse the outcome. A non-zero exit forces a failed outcome regardless
    // of what (if anything) the JSON says.
    let raw_json = std::fs::read_to_string(&prepared.out_json_path).unwrap_or_default();
    let parsed = parse_run_outcome(&raw_json).ok();
    let is_error = !status.success() || parsed.as_ref().map(|o| o.is_error).unwrap_or(true);

    // Finish the tracked run.
    let model_usage_json = parsed
        .as_ref()
        .and_then(|o| o.model_usage.as_ref())
        .and_then(|m| serde_json::to_string(m).ok());
    let pr_url = resolve_pr_url(
        gh,
        &prepared.branch,
        parsed.as_ref().and_then(|o| o.result.as_deref()),
    );
    run_store.finish_run(
        prepared.run_id,
        &FinishRun {
            status: if is_error {
                RunStatus::Failed
            } else {
                RunStatus::Done
            },
            exit_code: status.code(),
            session_id: parsed.as_ref().map(|o| o.session_id.clone()),
            cost_usd: parsed.as_ref().and_then(|o| o.cost_usd),
            num_turns: parsed.as_ref().and_then(|o| o.num_turns).map(|t| t as i64),
            blocker: None,
            pr_url,
            transcript: Some(prepared.out_json_path.to_string_lossy().into_owned()),
            model_usage: model_usage_json,
        },
    )?;

    // Print the summary, mirroring work.ml's final printf block.
    let session_id = parsed.as_ref().map(|o| o.session_id.clone());
    let turns = parsed
        .as_ref()
        .and_then(|o| o.num_turns)
        .map(|t| t.to_string())
        .unwrap_or_default();
    let cost = parsed
        .as_ref()
        .and_then(|o| o.cost_usd)
        .map(|c| c.to_string())
        .unwrap_or_default();
    let summary = parsed
        .as_ref()
        .and_then(|o| o.result.clone())
        .unwrap_or_default();

    writeln!(out)?;
    writeln!(out, "lane      {}", prepared.lane)?;
    writeln!(out, "worktree  {}", prepared.worktree.display())?;
    writeln!(out, "branch    {}", prepared.branch)?;
    writeln!(out, "session   {}", session_id.clone().unwrap_or_default())?;
    writeln!(out, "turns     {turns}")?;
    writeln!(out, "cost      ${cost}")?;
    writeln!(out, "error     {is_error}")?;
    writeln!(out)?;
    if let Some(session_id) = &session_id {
        writeln!(out, "resume:   claude --resume {session_id}")?;
    }
    writeln!(out, "summary:")?;
    write!(out, "{summary}")?;

    Ok(RunLaneOutcome {
        run_id: prepared.run_id,
        is_error,
        worktree: prepared.worktree.clone(),
        branch: prepared.branch.clone(),
        session_id,
    })
}

/// The detached supervisor's core: record this process's own pid on the
/// already-created run row (see [`prepare_run_lane`]'s doc comment on why
/// the pid recorded at `start_run` time is `None` for this path), then run
/// the same spawn-wait-parse-finish tail `--fg` uses.
///
/// This is everything `src/work/detach.rs`'s hidden `tm work __supervise`
/// subcommand does once it has deserialized its [`PreparedRun`] state file —
/// factored out here (rather than living in `detach.rs`) so it can be
/// exercised with fakes exactly like [`run_lane_fg`], with no process
/// re-exec, setsid, or file I/O involved in the test.
pub fn supervise_run(
    spawner: &dyn ProcessSpawner,
    gh: &dyn GhCli,
    run_store: &RunStore,
    prepared: &PreparedRun,
    supervisor_pid: u32,
    out: &mut dyn Write,
) -> Result<RunLaneOutcome, RunLaneError> {
    run_store.update_pid(prepared.run_id, supervisor_pid)?;
    run_claude_and_finish(spawner, gh, run_store, prepared, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LaneConfig;
    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::RunStore;
    use crate::work::git::FakeGitOps;
    use crate::work::runner::FakeProcessSpawner;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn lane_config(repo: &str) -> LaneConfig {
        LaneConfig {
            repo: repo.to_string(),
            prompt_file: None,
            base_branch: None,
            model: None,
            max_turns: None,
            permission_mode: None,
        }
    }

    fn config_with_lane(name: &str, lane: LaneConfig, worktree_root: &Path) -> WorkConfig {
        let mut lanes = BTreeMap::new();
        lanes.insert(name.to_string(), lane);
        WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            default_model: None,
            default_max_turns: None,
            default_permission_mode: None,
            tmux_windows: vec![],
            tmux_primary_window: None,
            lanes,
        }
    }

    fn canned_json() -> String {
        r#"{"session_id":"sess-1","total_cost_usd":0.5,"num_turns":3,"is_error":false,"result":"opened https://github.com/example/repo/pull/1"}"#.to_string()
    }

    #[test]
    fn sanitize_branch_owner_strips_disallowed_characters() {
        assert_eq!(sanitize_branch_owner("  jowi dev!!  "), "jowidev");
        assert_eq!(sanitize_branch_owner("jowi-dev_1.2"), "jowi-dev_1.2");
    }

    #[test]
    fn resolve_branch_owner_prefers_git_config_j_branch_owner() {
        let git = FakeGitOps::new().with_config_value("j.branchOwner", "from-git-config");
        let gh = FakeGhCli::new().with_current_user_login(Ok(Some("from-gh".to_string())));
        assert_eq!(
            resolve_branch_owner(&git, &gh, Path::new("/repo")),
            "from-git-config"
        );
    }

    #[test]
    fn resolve_branch_owner_falls_back_to_gh_login_when_git_config_unset() {
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_current_user_login(Ok(Some("from-gh".to_string())));
        assert_eq!(
            resolve_branch_owner(&git, &gh, Path::new("/repo")),
            "from-gh"
        );
    }

    #[test]
    fn resolve_branch_owner_falls_back_to_github_user_config() {
        let git = FakeGitOps::new().with_config_value("github.user", "from-github-user");
        let gh = FakeGhCli::new();
        assert_eq!(
            resolve_branch_owner(&git, &gh, Path::new("/repo")),
            "from-github-user"
        );
    }

    #[test]
    fn resolve_branch_owner_falls_back_to_claude_when_nothing_resolves() {
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        assert_eq!(
            resolve_branch_owner(&git, &gh, Path::new("/repo")),
            "claude"
        );
    }

    fn setup() -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let prompt_dir = home.join(".claude/prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        let prompt_path = prompt_dir.join("mylane.md");
        std::fs::write(&prompt_path, "Do the lane thing.").unwrap();
        (tmp, home, repo_root, worktree_root, prompt_path)
    }

    #[test]
    fn run_lane_fg_reaches_done_and_prints_summary_on_success() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home: home.clone(),
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert!(!outcome.is_error);
        assert_eq!(outcome.session_id, Some("sess-1".to_string()));

        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.lane, "mylane");
        assert_eq!(run.branch, Some(outcome.branch.clone()));

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("lane      mylane"));
        assert!(printed.contains("session   sess-1"));
        assert!(printed.contains("error     false"));
        assert!(printed.contains("resume:   claude --resume sess-1"));

        // Spawn argv/env/cwd/stdout path were exactly what run_lane_fg built.
        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let call = &recorded[0];
        assert_eq!(call.program, "claude");
        assert!(call.args.contains(&"-p".to_string()));
        assert_eq!(call.current_dir, outcome.worktree);
        assert!(call.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
    }

    #[test]
    fn run_lane_fg_nonzero_exit_marks_run_failed() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::with_exit_code(canned_json(), 1);
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert!(outcome.is_error);
        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("error     true"));
    }

    #[test]
    fn run_lane_fg_is_error_true_in_json_marks_run_failed_even_on_zero_exit() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let json = r#"{"session_id":"sess-1","is_error":true,"result":"blocked"}"#;
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(json.to_string());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert!(outcome.is_error);
    }

    #[test]
    fn run_lane_fg_errors_before_any_spawn_when_prompt_file_missing() {
        let (tmp, home, repo_root, worktree_root, prompt_path) = setup();
        std::fs::remove_file(&prompt_path).unwrap();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let err = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, RunLaneError::PromptFileMissing(_)));
        assert!(spawner.recorded.lock().unwrap().is_empty());
        assert_eq!(run_store.list_runs().unwrap().len(), 0);
    }

    #[test]
    fn run_lane_fg_errors_for_unknown_lane() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "other-lane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let err = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, RunLaneError::UnknownLane(_)));
    }

    #[test]
    fn run_lane_fg_errors_on_dirty_worktree() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new().with_status_is_clean(Ok(false));
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let err = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, RunLaneError::WorktreeDirty(_)));
        assert!(spawner.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn run_lane_fg_fetches_origin_before_cutting_the_run_branch() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        // fetch_origin must run before switch_new_branch cuts this run's
        // fresh branch, mirroring work.ml's run_lane ordering (fetch, then
        // cut) so the resolved base ref is current.
        assert_eq!(git.call_log(), vec!["fetch_origin", "switch_new_branch"]);
    }

    #[test]
    fn run_lane_fg_fetches_origin_even_when_worktree_already_exists() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );
        let wt_path = worktree_root
            .join(repo_root.file_name().unwrap())
            .join("mylane");
        std::fs::create_dir_all(&wt_path).unwrap();

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert_eq!(git.fetch_origin_calls(), vec![wt_path]);
    }

    #[test]
    fn run_lane_fg_warns_but_continues_when_fetch_origin_fails() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new().with_fetch_origin_result(Err(GitError::Command {
            command: "git fetch --quiet origin".to_string(),
            exit_code: Some(1),
            stderr: "could not resolve host".to_string(),
        }));
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert!(!outcome.is_error);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("warning: git fetch origin failed"));
        assert!(printed.contains("could not resolve host"));
    }

    #[test]
    fn run_lane_fg_prints_linked_env_local_message_when_present_in_repo_root() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        std::fs::write(repo_root.join(".env.local"), "DATABASE_URL=postgres://\n").unwrap();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        assert!(outcome.worktree.join(".env.local").is_symlink());
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("Linked .env.local from main repo"));
    }

    #[test]
    fn run_lane_fg_scopes_worktree_and_branch_to_lowercased_ticket() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("ABC-123".to_string()),
            ..Default::default()
        };

        let outcome = run_lane_fg(&deps, &config, &paths, "mylane", request, &mut out).unwrap();

        assert!(outcome.worktree.ends_with("abc-123"));
        assert!(outcome.branch.contains("abc-123-"));

        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.ticket, "ABC-123");
    }

    // --- pr_url resolution: gh lookup, falling back to a result-text scrape,
    // tolerant of neither resolving (see resolve_pr_url). ---

    #[test]
    fn run_claude_and_finish_records_pr_url_from_gh_lookup() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_pr_url_for_branch(Ok(Some(
            "https://github.com/example/repo/pull/7".to_string(),
        )));
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        // gh was asked about this run's own branch, not some other one.
        assert_eq!(gh.pr_url_for_branch_calls(), vec![outcome.branch.clone()]);

        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(
            run.pr_url,
            Some("https://github.com/example/repo/pull/7".to_string())
        );
    }

    #[test]
    fn run_claude_and_finish_falls_back_to_scraping_result_text_when_gh_finds_nothing() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let json = r#"{"session_id":"sess-1","total_cost_usd":0.5,"num_turns":3,"is_error":false,"result":"opened https://github.com/example/repo/pull/42 for review"}"#;
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new(); // no gh pr_url_for_branch configured -> None
        let spawner = FakeProcessSpawner::success(json.to_string());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(
            run.pr_url,
            Some("https://github.com/example/repo/pull/42".to_string())
        );
    }

    #[test]
    fn run_claude_and_finish_pr_url_is_none_when_neither_gh_nor_scrape_find_one() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let json = r#"{"session_id":"sess-1","total_cost_usd":0.5,"num_turns":3,"is_error":false,"result":"all done, no PR opened"}"#;
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(json.to_string());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let outcome = run_lane_fg(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            &mut out,
        )
        .unwrap();

        // No PR URL resolved, but the run still finishes normally.
        let run = run_store.run_by_id(outcome.run_id).unwrap().unwrap();
        assert_eq!(run.pr_url, None);
        assert_eq!(run.status, RunStatus::Done);
        assert!(!outcome.is_error);
    }

    // --- prepare_run_lane / run_claude_and_finish / supervise_run:
    // the detached path's split of run_lane_fg's sequence. ---

    #[test]
    fn prepare_run_lane_with_no_pid_starts_the_run_row_with_no_pid_recorded() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let spawner = FakeProcessSpawner::success(canned_json());

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let prepared = prepare_run_lane(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut out,
        )
        .unwrap();

        // Nothing was spawned yet — prepare_run_lane only gets as far as
        // start_run + building the invocation.
        assert!(spawner.recorded.lock().unwrap().is_empty());

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.pid, None);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn prepare_run_lane_with_a_pid_records_it_on_the_run_row() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let spawner = FakeProcessSpawner::success(canned_json());

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let prepared = prepare_run_lane(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            Some(4242),
            &mut out,
        )
        .unwrap();

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.pid, Some(4242));
    }

    #[test]
    fn prepare_run_lane_leaves_no_run_row_when_prompt_file_missing() {
        // Mirrors run_lane_fg_errors_before_any_spawn_when_prompt_file_missing,
        // but exercises prepare_run_lane directly — this is the guarantee the
        // detached CLI path depends on: a bad lane/prompt/dirty-worktree
        // fails before any run row or supervisor process exists.
        let (tmp, home, repo_root, worktree_root, prompt_path) = setup();
        std::fs::remove_file(&prompt_path).unwrap();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let spawner = FakeProcessSpawner::success(canned_json());

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let err = prepare_run_lane(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(err, RunLaneError::PromptFileMissing(_)));
        assert_eq!(run_store.list_runs().unwrap().len(), 0);
    }

    #[test]
    fn prepare_run_lane_state_round_trips_through_json() {
        // PreparedRun is handed to a re-exec'd supervisor process via a JSON
        // file (see src/work/detach.rs) — it must serialize and deserialize
        // back to an equal value.
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let spawner = FakeProcessSpawner::success(canned_json());

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let prepared = prepare_run_lane(
            &deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut out,
        )
        .unwrap();

        let json = serde_json::to_string(&prepared).unwrap();
        let round_tripped: PreparedRun = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.run_id, prepared.run_id);
        assert_eq!(round_tripped.branch, prepared.branch);
        assert_eq!(round_tripped.worktree, prepared.worktree);
        assert_eq!(round_tripped.invocation, prepared.invocation);
        assert_eq!(round_tripped.out_json_path, prepared.out_json_path);
    }

    #[test]
    fn supervise_run_records_its_own_pid_then_completes_the_run() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let prepare_spawner = FakeProcessSpawner::success(canned_json());

        let prepare_deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &prepare_spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut prepare_out = Vec::new();

        // As the detached CLI path does: prepare with no pid yet...
        let prepared = prepare_run_lane(
            &prepare_deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut prepare_out,
        )
        .unwrap();
        assert_eq!(
            run_store.run_by_id(prepared.run_id).unwrap().unwrap().pid,
            None
        );

        // ...then a separate "supervisor" (here, just a second spawner
        // standing in for the re-exec'd process) records its own pid and
        // runs the tail.
        let supervisor_spawner = FakeProcessSpawner::success(canned_json());
        let mut supervisor_out = Vec::new();

        let outcome = supervise_run(
            &supervisor_spawner,
            &gh,
            &run_store,
            &prepared,
            9999,
            &mut supervisor_out,
        )
        .unwrap();

        assert!(!outcome.is_error);
        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.pid, Some(9999));
        assert_eq!(run.status, RunStatus::Done);

        let printed = String::from_utf8(supervisor_out).unwrap();
        assert!(printed.contains("session   sess-1"));
    }

    #[test]
    fn supervise_run_marks_failed_on_nonzero_claude_exit() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let prepare_spawner = FakeProcessSpawner::success(canned_json());

        let prepare_deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &prepare_spawner,
            run_store: &run_store,
            clock: &clock,
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut prepare_out = Vec::new();

        let prepared = prepare_run_lane(
            &prepare_deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut prepare_out,
        )
        .unwrap();

        let supervisor_spawner = FakeProcessSpawner::with_exit_code(canned_json(), 1);
        let mut supervisor_out = Vec::new();

        let outcome = supervise_run(
            &supervisor_spawner,
            &gh,
            &run_store,
            &prepared,
            4321,
            &mut supervisor_out,
        )
        .unwrap();

        assert!(outcome.is_error);
        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.pid, Some(4321));
    }
}
