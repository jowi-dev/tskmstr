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
//! This module itself never touches tmux, in any mode. Issue #2 phase 3
//! makes an interactive run tmux-hosted, but the window is
//! `crate::work::interactive`'s job and the CLI layer's to sequence —
//! everything here stays a pure resolve-provision-prepare pipeline over
//! injected seams. What this module contributes to a tmux-hosted run is the
//! `PreparedRun`, including a `RunMode::Interactive` invocation.
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
//!    `prompt_file` (relative to the lane's repo root), else
//!    `~/.claude/prompts/<lane>.md` (`work.ml`'s default). Error before
//!    spawning anything if it doesn't exist.
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
//!    run's fresh branch off the resolved base (`GitOps::switch_new_branch`,
//!    always `--no-track`). The branch name is `<owner>/<wt_name>-<slug>`
//!    when a ticket summary is available from the configured backend's
//!    ticket provider (see [`resolve_ticket_slug`] and
//!    [`naming::branch_name_with_slug`]), falling back to the original
//!    `<owner>/<wt_name>-<timestamp>` (`naming::branch_name`]) whenever it
//!    isn't — no ticket, no ticket-provider dependency, a failed lookup, or
//!    an empty summary all land on the fallback, silently, since a
//!    ticket-provider hiccup must never fail a run. Either
//!    way, the candidate branch name is resolved against
//!    [`naming::resolve_branch_collision`] first, since (unlike the
//!    timestamp) a slug does not guarantee uniqueness across re-runs.
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

use crate::blocker_stacking::{self, StackDecision};
use crate::config::{BackendIdentity, BackendIdentityResolver, ConfigError, WorkConfig};
use crate::github::gh_cli::{GhCli, GhError};
use crate::runs::{FinishRun, RunStatus, RunStore, RunStoreError, StartRun};
use crate::ticketing::provider::TicketProvider;
use crate::work::claude::{ClaudeInvocationInputs, RunMode, build_claude_invocation};
use crate::work::git::{GitError, GitOps};
use crate::work::hooks::{self, HooksError};
use crate::work::interactive::interactive_prompt;
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

    /// The run's ticket has two or more direct `Blocks` blockers (per the
    /// configured ticket provider) that are not yet merged (see
    /// [`resolve_blocker_stacking`]'s decision
    /// table). A run branch can only be stacked on one unmerged dependency
    /// at a time, so this refuses before any run row is created rather than
    /// guessing which blocker to build on.
    #[error(
        "{ticket} has {} unmerged blockers, can't stack a single run branch on more than one: {}",
        blockers.len(),
        blockers.join(", ")
    )]
    MultipleUnmergedBlockers {
        /// The ticket the run was requested for.
        ticket: String,
        /// One entry per unmerged blocker, formatted as `<KEY> (PR #N open)`
        /// or `<KEY> (no PR)`.
        blockers: Vec<String>,
    },

    /// `request.ticket` was given as an empty (or all-whitespace) string.
    /// Lowercased, this becomes the worktree/branch-name component
    /// (`wt_name`), and `naming::worktree_path` silently collapses an empty
    /// component via `PathBuf::join`'s no-op-on-empty-string behavior,
    /// landing the "worktree" on the project's per-repo worktree directory
    /// itself rather than a real worktree path. Rejected here, before
    /// `naming::worktree_path` is ever called.
    #[error("--ticket cannot be empty (an empty ticket collapses the worktree path)")]
    EmptyWorktreeName,

    /// Belt-and-suspenders: the worktree path computed by
    /// `naming::worktree_path` did not land exactly one level below the
    /// project's worktree directory as
    /// [`naming::worktree_path_has_expected_parent`] expects. This should
    /// be unreachable given the [`RunLaneError::EmptyWorktreeName`] check
    /// above, but is checked independently right before any
    /// `git worktree add` — a bug in that earlier check must not leave this
    /// hazard uncaught.
    #[error(
        "refusing to provision worktree at {} — it does not sit one level below the expected worktree directory",
        .0.display()
    )]
    WorktreePathMismatch(PathBuf),

    /// A config error occurred while resolving a directory's backend
    /// identity (see [`crate::config::BackendIdentityResolver`]) — most
    /// likely the lane repo's own `.tskmstr.toml`/global config failed to
    /// merge.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The lane's repo resolves to a different backend identity than the
    /// invoking repo's — launching it would run a session whose cwd-driven
    /// backend resolution talks to the wrong ticket provider (or the right
    /// provider, wrong project). Checked before any prompt/worktree/run-row
    /// work, exactly like [`RunLaneError::UnknownLane`]. See GitHub issue #5
    /// phase 2: `docs/plans/issue-5-lane-backend-routing.md`.
    ///
    /// Boxed (see [`BackendMismatchInfo`]): this is by far the largest
    /// variant of `RunLaneError` (two [`BackendIdentity`]s and two
    /// [`PathBuf`]s), and clippy's `result_large_err` flags every `Result<_,
    /// RunLaneError>`-returning function in this module for the size every
    /// other, much smaller, variant would otherwise pay for.
    #[error("{0}")]
    BackendMismatch(Box<BackendMismatchInfo>),
}

/// Detail carried by [`RunLaneError::BackendMismatch`]; see that variant's
/// doc comment for why it's boxed.
#[derive(Debug)]
pub struct BackendMismatchInfo {
    /// Name of the lane that was refused.
    pub lane: String,
    /// The lane's configured (already `~`/relative-resolved) repo path.
    pub lane_repo: PathBuf,
    /// The invoking repo's own directory.
    pub current_repo: PathBuf,
    /// The lane repo's resolved backend identity.
    pub lane_backend: BackendIdentity,
    /// The invoking repo's resolved backend identity.
    pub current_backend: BackendIdentity,
}

impl std::fmt::Display for BackendMismatchInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lane `{}` (repo {}) is backend-incompatible with the current repo ({}): \
             lane resolves to {}, current repo resolves to {}",
            self.lane,
            self.lane_repo.display(),
            self.current_repo.display(),
            self.lane_backend,
            self.current_backend,
        )
    }
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
    /// The ticket provider for `config.backend` (Jira or GitHub — see
    /// [`crate::ticketing::provider::TicketProvider`]), used to look up a
    /// run's ticket summary for the human-readable branch-name slug (step 6
    /// of this module's doc comment). `None` when no provider could be
    /// constructed/authenticated for this invocation — `tm work run` still
    /// works without ticket-backend access, it just falls back to the
    /// original timestamp-based branch name (see [`resolve_ticket_slug`]).
    /// Never treated as a hard requirement: `tm work run` has always worked
    /// without it, and this feature must not change that.
    pub ticket_provider: Option<&'a dyn TicketProvider>,
    /// The invoking repo's own directory (typically `cwd`), named in
    /// [`RunLaneError::BackendMismatch`]'s error message alongside the
    /// lane's repo.
    pub current_repo_dir: &'a Path,
    /// The invoking repo's resolved backend identity, compared against each
    /// launched lane's own resolved identity (see
    /// [`RunLaneError::BackendMismatch`]). See GitHub issue #5 phase 2:
    /// `docs/plans/issue-5-lane-backend-routing.md`.
    pub current_backend_identity: &'a BackendIdentity,
    /// Resolves a directory's backend identity — used to resolve the lane
    /// repo's own identity for the compatibility preflight. Behind a trait
    /// so tests can stay hermetic; see
    /// [`crate::config::FsBackendIdentityResolver`] for the real
    /// filesystem-backed implementation.
    pub backend_identity_resolver: &'a dyn BackendIdentityResolver,
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
    /// How this run hosts `claude`: [`RunMode::Interactive`] for a
    /// tmux-hosted session (`tm work run`'s default), [`RunMode::Headless`]
    /// for `--headless`'s supervisor-driven `claude -p`.
    ///
    /// Defaults to [`RunMode::Headless`] — the shape that predates issue #2
    /// phase 3 and the only one that needs no tmux server — even though the
    /// CLI's own default is interactive. The CLI always sets this
    /// explicitly; the conservative default is for programmatic callers that
    /// have not thought about window hosting.
    pub mode: RunMode,
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
    /// Whether the run did *not* end in [`RunStatus::Done`] — a non-zero
    /// `claude` exit status, an explicit `is_error: true` in its result
    /// JSON, or an ambiguous/unparseable result classified as
    /// [`RunStatus::Interrupted`]. Callers (the CLI layer) use this to
    /// decide the process's exit code; it collapses `Failed` and
    /// `Interrupted` into one bit because both mean "don't report success",
    /// even though the run row itself keeps the finer-grained status.
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

/// Resolve the human-readable slug to fold into a lane run's branch name
/// (see [`naming::branch_name_with_slug`]), from the run's ticket's summary
/// as reported by whichever [`TicketProvider`] `config.backend` selects
/// (Jira or GitHub).
///
/// Returns `None` — meaning [`prepare_run_lane`]'s step 6 falls back to the
/// original timestamp-based [`naming::branch_name`] — whenever any of these
/// hold:
/// - `ticket` is `None` (the run isn't scoped to a ticket at all, e.g. a
///   bare lane run keyed by lane name).
/// - `ticket_provider` is `None` (no ticket backend is
///   configured/authenticated for this invocation).
/// - The `get_issue` call fails for any reason (network error, 404, auth
///   failure, ...).
/// - The issue's summary is empty or contains no alphanumeric characters,
///   per [`naming::slugify_summary`].
///
/// Every one of those is swallowed silently (no warning printed): a
/// ticket-backend hiccup must never fail a run, and the existing timestamp
/// naming is a perfectly good branch name on its own — this is a "nice to
/// have when available" enhancement, not a dependency `tm work run` should
/// ever block on or complain about losing.
fn resolve_ticket_slug(
    ticket_provider: Option<&dyn TicketProvider>,
    ticket: Option<&str>,
) -> Option<String> {
    let ticket_provider = ticket_provider?;
    let ticket = ticket?;
    let issue = ticket_provider.get_issue(ticket).ok()?;
    naming::slugify_summary(&issue.fields.summary)
}

/// The outcome of [`resolve_blocker_stacking`]: an optional base-branch
/// override plus any info/warning lines to print, in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockerResolution {
    /// `Some(base)` — always `origin/<head_ref_name>` — when exactly one
    /// direct blocker is unmerged and has an open PR to stack on. `None`
    /// means "use the normal base" (no blockers, a merged blocker, a
    /// no-PR/closed-PR blocker, or the ticket provider/gh unavailable/failing).
    pub stacked_base: Option<String>,
    /// Lines [`prepare_run_lane`] prints, in order — the stacking
    /// announcement, a no-PR warning, or a resolution-failure warning.
    pub messages: Vec<String>,
}

impl BlockerResolution {
    /// A resolution carrying a single warning line and no base override —
    /// every "fall back to normal base" outcome that still has something
    /// worth telling the caller (a no-PR blocker, or a ticket-provider/gh
    /// failure).
    fn warning(message: String) -> Self {
        Self {
            stacked_base: None,
            messages: vec![message],
        }
    }
}

/// Resolve whether a lane run's branch should be cut from a blocking
/// ticket's PR branch instead of the normal base, and what (if anything) to
/// print about it.
///
/// Only consulted when the run has a ticket, a ticket provider is configured,
/// and no `--from` override was given — [`prepare_run_lane`] skips calling
/// this at all in every other case, since `--from` is an explicit "use this
/// base" instruction that blocker logic must never second-guess.
///
/// The actual decision table (direct blockers only — the transitive chain is
/// never walked: if C is blocked by B which is blocked by A, B's own branch
/// already contains A's work by the time B's PR exists, so C only ever needs
/// to stack on B) lives in [`crate::blocker_stacking`], shared verbatim with
/// `tm ready`'s report of the same ticket so the two can never disagree — see
/// that module's doc comment for the incident this split fixes and the full
/// rule. This function's job is only to fetch the inputs
/// ([`TicketProvider::get_issue`], [`GhCli::pr_list_all`]) and act on the
/// resulting [`StackDecision`]:
/// - [`StackDecision::Ready`] → `stacked_base: None` (normal base).
/// - [`StackDecision::Stackable`] → `stacked_base:
///   Some("origin/<head_ref_name>")`, plus an announcement line.
/// - [`StackDecision::BlockedNoPr`] → `stacked_base: None`, plus a warning
///   line (nothing to stack on yet).
/// - [`StackDecision::BlockedMultiple`] →
///   `Err(RunLaneError::MultipleUnmergedBlockers)`, naming every unmerged
///   blocker and its PR state — two parallel unmerged dependencies can't
///   both be stacked on, and this is the one case where blocker resolution
///   refuses the run outright rather than falling back.
///
/// Any ticket-provider failure while resolving blockers (a bad `get_issue`), or a
/// **transient** `gh` failure from [`GhCli::pr_list_all`] (network error,
/// rate limit, a `5xx`, expired auth, `gh` missing, a timeout — see
/// [`GhError::is_permanent`]), short-circuits to `stacked_base: None` plus a
/// warning — a network hiccup must never fail a run; refusal is otherwise
/// reserved for the confirmed ≥2-unmerged case above, which only fires once
/// every blocker resolved cleanly.
///
/// A **permanent** `gh` failure from [`GhCli::pr_list_all`] — `gh` telling us
/// tm itself asked it something nonsensical, e.g. an invalid `--json` field —
/// is a different case entirely and is *not* swallowed: it propagates as
/// `Err`, refusing the run. A permanent error means blocker resolution never
/// actually ran and never will until the code is fixed, so we genuinely
/// cannot know whether the ticket's blocker is stackable; falling back to
/// the normal base here would silently dispatch an autonomous run against
/// the wrong base while pretending the feature worked — which is exactly
/// what happened before this distinction existed (`gh pr list --json` was
/// requesting an invalid `merged` field for months; every call failed
/// identically, six lane runs were dispatched against the wrong base as a
/// result, and nobody could tell because the "warning" went nowhere
/// visible). See [`GhError::is_permanent`]'s doc comment for the full
/// incident and the narrow stderr-based detection this relies on.
///
/// `repo_root` is passed through to [`GhCli::pr_list_all`] unchanged: a lane
/// run targets `lane_config.repo`, not necessarily the invoking process's
/// cwd, so `gh` must be told explicitly which repository's PRs to list (see
/// [`GhCli::pr_list_all`]'s doc comment for the wrong-repo failure mode this
/// avoids). All blockers are resolved against a single [`GhCli::pr_list_all`]
/// call — one `gh` invocation per run, not one per blocker (see
/// [`crate::blocker_stacking::find_blocker_pr`]'s doc comment).
pub fn resolve_blocker_stacking(
    ticket_provider: Option<&dyn TicketProvider>,
    gh: &dyn GhCli,
    repo_root: &Path,
    ticket: Option<&str>,
) -> Result<BlockerResolution, RunLaneError> {
    let Some(ticket_provider) = ticket_provider else {
        return Ok(BlockerResolution::default());
    };
    let Some(ticket) = ticket else {
        return Ok(BlockerResolution::default());
    };

    let issue = match ticket_provider.get_issue(ticket) {
        Ok(issue) => issue,
        Err(err) => {
            return Ok(BlockerResolution::warning(format!(
                "warning: could not resolve blockers for {ticket} ({err}) — using normal base"
            )));
        }
    };

    if blocker_stacking::direct_blockers(&issue).is_empty() {
        return Ok(BlockerResolution::default());
    }

    let prs = match gh.pr_list_all(repo_root) {
        Ok(prs) => prs,
        // A permanent gh error (see `GhError::is_permanent`'s doc comment —
        // this is the exact incident it exists for) means blocker
        // resolution did not run at all, and never will until the code is
        // fixed: we genuinely cannot know whether AX-2's blocker is
        // stackable. Silently falling back to the normal base here would
        // dispatch an autonomous run against the wrong base while
        // pretending the feature worked, which is exactly what happened
        // six times before this was caught — so this fails the run instead
        // of warning. Contrast the `Err(err)` transient arm below, which
        // preserves the original warn-and-fall-back-to-base behavior
        // exactly.
        Err(err) if err.is_permanent() => return Err(RunLaneError::Gh(err)),
        Err(err) => {
            return Ok(BlockerResolution::warning(format!(
                "warning: could not resolve PRs for blockers of {ticket} ({err}) — using normal base"
            )));
        }
    };

    let unmerged = blocker_stacking::unmerged_direct_blockers(&issue, &prs);
    match blocker_stacking::decide(unmerged) {
        StackDecision::Ready => Ok(BlockerResolution::default()),
        StackDecision::Stackable {
            blocker_key,
            pr_number,
            head_ref_name,
        } => {
            let base = format!("origin/{head_ref_name}");
            Ok(BlockerResolution {
                stacked_base: Some(base.clone()),
                messages: vec![format!(
                    "blocked by {blocker_key} (PR #{pr_number} open) — branching from {base}"
                )],
            })
        }
        StackDecision::BlockedNoPr { blocker } => Ok(BlockerResolution::warning(format!(
            "warning: blocked by {} but no PR found to stack on yet — using normal base",
            blocker.key
        ))),
        StackDecision::BlockedMultiple { blockers } => {
            Err(RunLaneError::MultipleUnmergedBlockers {
                ticket: ticket.to_string(),
                blockers: blockers
                    .iter()
                    .map(blocker_stacking::format_unmerged_blocker)
                    .collect(),
            })
        }
    }
}

/// Resolve the prompt file path for a lane run: `--prompt` override, else
/// the lane's configured `prompt_file`, else `~/.claude/prompts/<lane>.md`
/// (`work.ml`'s default). Every form is `~`-expanded against `home`.
///
/// A relative lane `prompt_file` resolves against `repo_root`, not the
/// process's cwd, so a lane prompt can live in the repo it instructs
/// (`prompt_file = "prompts/<lane>-lane.md"`, which is what `tm init`
/// scaffolds) and a run works regardless of where `tm` was invoked from. A
/// relative `--prompt` override stays cwd-relative: it is a path the caller
/// just typed at a shell, so shell-relative is the only reading that won't
/// surprise them.
fn resolve_prompt_path(
    lane: &str,
    prompt_override: Option<&str>,
    lane_prompt_file: Option<&str>,
    repo_root: &Path,
    home: &Path,
) -> PathBuf {
    if let Some(raw) = prompt_override {
        return expand_tilde(raw, home);
    }
    match lane_prompt_file {
        Some(raw) => {
            let path = expand_tilde(raw, home);
            if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            }
        }
        None => expand_tilde(&format!("~/.claude/prompts/{lane}.md"), home),
    }
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

/// The run's durable log file path: `<state_dir>/<wt_name>-<timestamp>.log`.
///
/// Pure and computed from `wt_name`/`timestamp` alone (not `PreparedRun`) so
/// [`prepare_run_lane`] can log to it *before* a run row exists — the
/// blocked-ticket branch-off resolution runs before `start_run`, and its
/// warnings/errors previously only ever reached `out` (this process's
/// stdout), which is invisible for a detached run once the launching
/// terminal closes and, worse, isn't the run's log file at all: that log
/// file's own path is computed independently by
/// `crate::cli::work::run` *after* `prepare_run_lane` returns, from the same
/// `wt_name`/`timestamp` pair. Sharing this one function is what makes both
/// computations agree, so anything appended here by `prepare_run_lane`
/// before the log file is otherwise touched is exactly the content
/// `crate::work::detach::DetachSpawner::spawn_detached`'s later
/// append-mode stdio redirection (and `tail -f`/`tm runs logs`) will see at
/// the top of the file.
pub fn run_log_path(state_dir: &Path, wt_name: &str, timestamp: &str) -> PathBuf {
    state_dir.join(format!("{wt_name}-{timestamp}.log"))
}

/// Best-effort append of one line to the run's log file (see
/// [`run_log_path`]), creating it (and `state_dir`, transitively, since the
/// caller is expected to have already created it) if it doesn't exist yet.
///
/// Deliberately swallows the write failure rather than propagating it: this
/// exists to make a warning/error *more* visible, and a full disk or a
/// permissions problem on the log file must never be the reason a run itself
/// fails — `out` (and, for the hard-fail case, the propagated `Err`) is
/// still the authoritative channel; this is only a durable mirror of it,
/// the same stance `crate::work::review_watch::log_event` takes for its own
/// best-effort log line.
fn append_log_line(log_path: &Path, line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(file, "{line}");
    }
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

    // Backend-compatibility preflight (GitHub issue #5 phase 2): refuse a
    // lane whose repo resolves to a different backend identity than the
    // invoking repo's own, before any prompt/worktree/run-row work below —
    // see RunLaneError::BackendMismatch's doc comment for why.
    let lane_backend = deps.backend_identity_resolver.resolve(&repo_root)?;
    if lane_backend != *deps.current_backend_identity {
        return Err(RunLaneError::BackendMismatch(Box::new(
            BackendMismatchInfo {
                lane: lane.to_string(),
                lane_repo: repo_root,
                current_repo: deps.current_repo_dir.to_path_buf(),
                lane_backend,
                current_backend: deps.current_backend_identity.clone(),
            },
        )));
    }

    // Step 2: resolve and preflight the prompt file.
    let prompt_path = resolve_prompt_path(
        lane,
        request.prompt_override.as_deref(),
        lane_config.prompt_file.as_deref(),
        &repo_root,
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

    // An empty ticket (`--ticket ""`) lowercases to an empty `wt_name`,
    // which would make `naming::worktree_path`'s trailing `.join("")` a
    // no-op and silently collapse the worktree path onto the project's
    // worktree directory itself — see that function's doc comment. Reject
    // it here, before it ever reaches `naming::worktree_path`.
    if wt_name.trim().is_empty() {
        return Err(RunLaneError::EmptyWorktreeName);
    }

    let worktree_root = resolve_worktree_root(config, &paths.home);
    let repo = repo_name(&repo_root).unwrap_or_else(|| lane.to_string());
    let wt_path = naming::worktree_path(&worktree_root.to_string_lossy(), &repo, &wt_name);

    // Belt-and-suspenders: re-check the *computed* path independently of
    // the emptiness check above, right before anything below provisions a
    // worktree at it. See `naming::worktree_path_has_expected_parent`'s doc
    // comment for why this is a second, independent guard rather than
    // redundant with the check above.
    if !naming::worktree_path_has_expected_parent(&worktree_root.to_string_lossy(), &repo, &wt_path)
    {
        return Err(RunLaneError::WorktreePathMismatch(wt_path));
    }

    // The timestamp is computed here — earlier than strictly needed for
    // step 6's branch name below — specifically so `log_path` (and thus
    // `run_log_path`'s durable file) exists *before* the blocked-ticket
    // branch-off resolution below runs. That resolution can warn or
    // outright fail, and both used to be visible only on `out`: for a
    // detached run that's this short-lived parent process's stdout, gone
    // the instant its terminal closes, and it's also not the run's actual
    // log file (computed independently, from this same wt_name/timestamp
    // pair, by `crate::cli::work::run` — see `run_log_path`'s doc comment).
    // `paths.state_dir` is created now too so the log file can be written
    // immediately, rather than waiting for step 9's own `create_dir_all`.
    let (year, month, day, hour, min, sec) = deps.clock.now_parts();
    let timestamp = naming::format_timestamp(year, month, day, hour, min, sec);
    std::fs::create_dir_all(&paths.state_dir)?;
    let log_path = run_log_path(&paths.state_dir, &wt_name, &timestamp);

    // Blocked-ticket branch-off: resolved once, up front, before step 4's
    // worktree provisioning (which needs a base too, if the worktree is
    // new) and step 6's branch cut both consult it — resolving it inside
    // resolve_base itself would make the ticket-provider/gh calls twice per run.
    // Skipped entirely when `--from` was given: an explicit base override
    // must never be second-guessed by blocker logic. See
    // resolve_blocker_stacking's doc comment for the full decision table.
    //
    // No run row exists yet at this point (that's step 9, below), so there
    // is no `RunStore::add_event` to write to either a warning or a
    // permanent failure here — `log_path` above is the only durable channel
    // available. A permanent gh error is logged before propagating the
    // `Err`, since prepare_run_lane's caller has no other opportunity to
    // record it anywhere durable.
    let blocker_resolution = if request.from_base.is_none() {
        match resolve_blocker_stacking(
            deps.ticket_provider,
            deps.gh,
            &repo_root,
            request.ticket.as_deref(),
        ) {
            Ok(resolution) => resolution,
            Err(err) => {
                append_log_line(&log_path, &format!("error: {err}"));
                return Err(err);
            }
        }
    } else {
        BlockerResolution::default()
    };
    for message in &blocker_resolution.messages {
        writeln!(out, "{message}")?;
        append_log_line(&log_path, message);
    }

    // Step 4: provision the worktree if it doesn't exist yet.
    let resolve_base = |_git: &dyn GitOps| -> Result<String, RunLaneError> {
        if let Some(base) = blocker_resolution.stacked_base.clone() {
            return Ok(base);
        }
        if let Some(base) = request.from_base.clone() {
            return Ok(base);
        }
        if let Some(base) = lane_config.base_branch.clone() {
            return Ok(base);
        }
        deps.git
            .default_base(&repo_root)
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

    // Step 6: cut this run's fresh branch. When the run's ticket has a
    // summary available from the ticket provider, the branch is named from
    // a short slug of it
    // (`<owner>/<wt_name>-<slug>`, e.g. `jowi-dev/ax-414-delete-bid-
    // connector`) instead of the timestamp — see resolve_ticket_slug's doc
    // comment for the full list of fallback conditions, all of which land
    // back on the original `<owner>/<wt_name>-<timestamp>` naming. Unlike
    // the timestamp, a slug doesn't guarantee uniqueness across re-runs of
    // the same lane/ticket, so either way the candidate is run through
    // resolve_branch_collision against both a local and an
    // origin-remote-tracking ref before it's cut.
    let base = resolve_base(deps.git)?;
    let owner = resolve_branch_owner(deps.git, deps.gh, &repo_root);
    let candidate = match resolve_ticket_slug(deps.ticket_provider, request.ticket.as_deref()) {
        Some(slug) => naming::branch_name_with_slug(&owner, &wt_name, &slug),
        None => naming::branch_name(&owner, &wt_name, &timestamp),
    };
    let branch = naming::resolve_branch_collision(&candidate, |name| {
        Ok::<bool, RunLaneError>(
            deps.git.branch_exists_local(&wt_path, name)?
                || deps.git.branch_exists_remote(&wt_path, name)?,
        )
    })?;
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
        ticket: ticket_field.clone(),
        lane: lane.to_string(),
        worktree: wt_path.to_string_lossy().into_owned(),
        branch: Some(branch.clone()),
        pid,
        kind: "lane".to_string(),
        log_path: None,
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

    // An interactive session has to adopt the row `start_run` just created,
    // and only something running *inside* the session can do that (see
    // `interactive::registration_preamble`), so the instruction rides in
    // front of the prompt. The ticket it must register against is the run
    // row's own `ticket` column, lane name included.
    let prompt = match request.mode {
        RunMode::Headless => prompt,
        RunMode::Interactive => interactive_prompt("lane", &ticket_field, &prompt),
    };

    let invocation = build_claude_invocation(ClaudeInvocationInputs {
        prompt,
        model,
        max_turns,
        permission_mode,
        settings_path: settings_path.clone(),
        run_id: Some(run_id.to_string()),
        mode: request.mode,
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

/// Errors that can occur while running [`prepare_review_fix`].
#[derive(Debug, Error)]
pub enum ReviewFixError {
    /// The worktree has uncommitted changes. See [`prepare_review_fix`]'s
    /// doc comment for why this check is kept even though (unlike
    /// [`prepare_run_lane`]'s freshly-branched worktree) this branch
    /// legitimately carries the PR's already-committed history.
    #[error(
        "worktree {} has uncommitted changes — resolve or stash them before dispatching a fix pass",
        .0.display()
    )]
    WorktreeDirty(PathBuf),

    /// A `git` shell-out failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// Hook deployment failed.
    #[error(transparent)]
    Hooks(#[from] HooksError),

    /// A run-state store operation failed.
    #[error(transparent)]
    RunStore(#[from] RunStoreError),

    /// A filesystem/output-write operation failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// Serializing the generated hooks `--settings` JSON to disk failed.
    #[error("failed to serialize settings JSON: {0}")]
    SettingsJson(#[from] serde_json::Error),
}

/// Prepare a `tm review fix <KEY>` run (`crate::cli::review::fix`): the
/// `review-fix` analogue of [`prepare_run_lane`], for dispatching a Claude
/// fix pass over `vdiff`-captured review comments on a ticket's *existing*
/// lane-run worktree and branch — see
/// `docs/plans/board-vdiff-review-loop.md`.
///
/// Unlike [`prepare_run_lane`], this never provisions a worktree or cuts a
/// branch — both are already resolved by the caller (from the ticket's
/// latest `kind = "lane"` run row) and passed in as `worktree`/`branch`
/// unchanged. That collapses most of `prepare_run_lane`'s preflight, which
/// exists specifically to create a *new* branch off a *resolved* base:
///
/// - **Base-branch resolution, blocker stacking, worktree provisioning,
///   branch naming/cutting: dropped entirely.** None of them have anything
///   to resolve when there is no new branch to cut.
/// - **Prompt-file resolution: dropped.** The caller (`crate::cli::review::fix`)
///   builds `prompt` itself from the `vdiff --export-comments` markdown, not
///   from a lane's configured prompt file.
/// - **Dirty-worktree check: kept, but re-justified.** `prepare_run_lane`'s
///   version guards against a *previous run* leaving uncommitted work behind
///   on what is supposed to be a fresh branch. That framing does not apply
///   here — this branch legitimately has the PR's commits already on it —
///   but the underlying hazard is the same: uncommitted local changes (left
///   by an interrupted previous review-fix run, or picked up by hand while
///   reviewing in `vdiff`) would get silently folded into this fix pass's
///   session and attributed to it. So the check stays, just worded for this
///   case ([`ReviewFixError::WorktreeDirty`]) instead of reused verbatim.
/// - **Hook deployment, [`RunStore::start_run`], [`build_claude_invocation`]:
///   kept unchanged**, exactly as in `prepare_run_lane`.
///
/// Run rows are started with `kind = "review-fix"`, distinct from `"lane"`
/// so this run never shadows the ticket's lane run in
/// [`RunStore::latest_run_for_ticket_kind`] lookups — a second `tm review
/// fix` on the same ticket must resolve *its own* worktree/branch from the
/// lane run, not from a previous fix pass.
///
/// `pid` follows [`prepare_run_lane`]'s convention: `Some(current pid)` for
/// a foreground dispatch (this process is the driver and stays alive for
/// the whole run), `None` for a detached dispatch, whose caller re-execs the
/// same `tm work __supervise` supervisor [`prepare_run_lane`]'s detached path
/// uses (see `src/work/detach.rs`) — the supervisor only reads back a
/// [`PreparedRun`], so it has no idea (and no need to know) whether the run
/// it's supervising is a lane run or a review-fix run.
#[allow(clippy::too_many_arguments)]
pub fn prepare_review_fix(
    git: &dyn GitOps,
    run_store: &RunStore,
    clock: &dyn Clock,
    paths: &RunLanePaths,
    ticket: &str,
    lane: &str,
    worktree: &Path,
    branch: &str,
    prompt: String,
    pid: Option<u32>,
    mode: RunMode,
) -> Result<PreparedRun, ReviewFixError> {
    if !git.status_is_clean(worktree)? {
        return Err(ReviewFixError::WorktreeDirty(worktree.to_path_buf()));
    }

    let settings = hooks::deploy_hooks(&paths.hooks_deploy_dir)?;
    let settings_path = paths.hooks_deploy_dir.join("settings.json");
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    let run_id = run_store.start_run(&StartRun {
        ticket: ticket.to_string(),
        lane: lane.to_string(),
        worktree: worktree.to_string_lossy().into_owned(),
        branch: Some(branch.to_string()),
        pid,
        kind: "review-fix".to_string(),
        log_path: None,
    })?;

    // See `prepare_run_lane`'s equivalent: an interactive session adopts its
    // pre-registered row by running `tm runs register` itself.
    let prompt = match mode {
        RunMode::Headless => prompt,
        RunMode::Interactive => interactive_prompt("review-fix", ticket, &prompt),
    };

    let invocation = build_claude_invocation(ClaudeInvocationInputs {
        prompt,
        model: None,
        max_turns: None,
        permission_mode: None,
        settings_path: settings_path.clone(),
        run_id: Some(run_id.to_string()),
        mode,
    });

    let (year, month, day, hour, min, sec) = clock.now_parts();
    let timestamp = naming::format_timestamp(year, month, day, hour, min, sec);
    std::fs::create_dir_all(&paths.state_dir)?;
    let wt_name = format!("{}-review-fix", ticket.to_lowercase());
    let out_json_path = paths.state_dir.join(format!("{wt_name}-{timestamp}.json"));

    Ok(PreparedRun {
        run_id,
        lane: lane.to_string(),
        ticket: Some(ticket.to_string()),
        wt_name,
        timestamp,
        worktree: worktree.to_path_buf(),
        branch: branch.to_string(),
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

    // Parse the outcome and classify the run's terminal status. A non-zero
    // exit is always Failed, regardless of what (if anything) the JSON says
    // -- that's an unambiguous signal from the process itself. Otherwise:
    //
    // - JSON parsed and `is_error` was explicit -> Failed/Done, exactly as
    //   before.
    // - JSON failed to parse (or had no usable session_id) -> Interrupted:
    //   we genuinely don't know what happened, which is a different claim
    //   than "the agent failed".
    // - JSON parsed but `is_error` was entirely absent -> Interrupted, not
    //   Done. This is the fix for the bug this variant exists for: a mid-run
    //   event that ends the turn gracefully (exit 0) without ever writing an
    //   `is_error` field -- e.g. a usage-limit forced model switch -- used to
    //   default straight to `Done` via `unwrap_or(false)`. See
    //   `RunStatus::Interrupted`'s doc comment and
    //   `parse_run_outcome_leaves_is_error_none_when_absent`.
    let raw_json = std::fs::read_to_string(&prepared.out_json_path).unwrap_or_default();
    let parsed = parse_run_outcome(&raw_json).ok();
    let run_status = if !status.success() {
        RunStatus::Failed
    } else {
        match parsed.as_ref().map(|o| o.is_error) {
            Some(Some(true)) => RunStatus::Failed,
            Some(Some(false)) => RunStatus::Done,
            Some(None) | None => RunStatus::Interrupted,
        }
    };
    let is_error = run_status != RunStatus::Done;

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
    // Only a non-zero exit or an explicit `is_error: true` (both folded into
    // `run_status == Failed` above) may clobber a status the in-session
    // agent already set for itself via `tm runs finish` -- a crashed run is
    // never actually "blocked" or "done". The supervisor's own inferred
    // `Done`/`Interrupted` must defer to an already-terminal row instead.
    // See `RunStore::finish_run_from_supervisor`'s doc comment and the
    // regression this exists for: an agent ending its own run with
    // `--status blocked --blocker "..."` before `claude -p` exits 0, only
    // for this unconditional write to silently overwrite it back to `Done`.
    let force_status = run_status == RunStatus::Failed;
    run_store.finish_run_from_supervisor(
        prepared.run_id,
        &FinishRun {
            status: run_status,
            exit_code: status.code(),
            session_id: parsed.as_ref().map(|o| o.session_id.clone()),
            cost_usd: parsed.as_ref().and_then(|o| o.cost_usd),
            num_turns: parsed.as_ref().and_then(|o| o.num_turns).map(|t| t as i64),
            blocker: None,
            pr_url,
            transcript: Some(prepared.out_json_path.to_string_lossy().into_owned()),
            model_usage: model_usage_json,
            findings_count: None,
        },
        force_status,
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
    use crate::github::gh_cli::{PrLifecycle, PrSummary};
    use crate::jira::fake::FakeJiraClient;
    use crate::runs::RunStore;
    use crate::ticketing::types::{
        Issue, IssueFields, IssueLink, IssueLinkType, LinkedIssue, LinkedIssueFields, Status,
        StatusCategory,
    };
    use crate::work::git::FakeGitOps;
    use crate::work::runner::FakeProcessSpawner;
    use std::collections::BTreeMap;
    use std::sync::OnceLock;
    use tempfile::TempDir;

    /// A [`BackendIdentityResolver`] test double that resolves every
    /// directory to the same, fixed identity, regardless of what's asked.
    /// Used by the many `prepare_run_lane`/`run_lane_fg` tests below that
    /// aren't exercising the backend-compatibility preflight itself (see
    /// [`compatible_test_identity`]/[`compatible_test_resolver`]) — they
    /// don't need to care what directory the lane happens to be in, only
    /// that it's always reported compatible.
    struct AlwaysBackendIdentityResolver(BackendIdentity);

    impl BackendIdentityResolver for AlwaysBackendIdentityResolver {
        fn resolve(&self, _dir: &Path) -> Result<BackendIdentity, ConfigError> {
            Ok(self.0.clone())
        }
    }

    /// The fixed identity [`compatible_test_resolver`] always resolves to,
    /// shared with every test's `current_backend_identity` so the
    /// preflight's equality check always passes for tests that don't care
    /// about it.
    fn compatible_test_identity() -> &'static BackendIdentity {
        static IDENTITY: OnceLock<BackendIdentity> = OnceLock::new();
        IDENTITY.get_or_init(|| BackendIdentity::Jira {
            base_url: String::new(),
            project_key: String::new(),
        })
    }

    fn compatible_test_resolver() -> &'static dyn BackendIdentityResolver {
        static RESOLVER: OnceLock<AlwaysBackendIdentityResolver> = OnceLock::new();
        RESOLVER.get_or_init(|| AlwaysBackendIdentityResolver(compatible_test_identity().clone()))
    }

    /// A minimal Jira issue fixture with `summary` as its only field of
    /// interest to these tests — see [`resolve_ticket_slug`].
    fn issue(key: &str, summary: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: summary.to_string(),
                status: Status {
                    name: "To Do".to_string(),
                    status_category: StatusCategory {
                        key: "new".to_string(),
                    },
                },
                description: None,
                assignee: None,
                issue_links: vec![],
            },
        }
    }

    /// A `Blocks`-type inward link naming `blocker_key` as blocking the
    /// issue this is attached to (see [`IssueLink`]'s doc comment on
    /// direction) — the shape [`direct_blockers`]/[`resolve_blocker_stacking`]
    /// read. `blocker_status_category` lets tests seed a Jira status that
    /// disagrees with PR state, since PR state (not Jira status) is what
    /// resolve_blocker_stacking's decision table consults.
    fn blocks_link(blocker_key: &str, blocker_status_category: &str) -> IssueLink {
        IssueLink {
            id: format!("link-{blocker_key}"),
            link_type: IssueLinkType {
                name: "Blocks".to_string(),
                inward: "is blocked by".to_string(),
                outward: "blocks".to_string(),
            },
            inward_issue: Some(LinkedIssue {
                key: blocker_key.to_string(),
                fields: LinkedIssueFields {
                    summary: "blocker".to_string(),
                    status: Status {
                        name: "In Progress".to_string(),
                        status_category: StatusCategory {
                            key: blocker_status_category.to_string(),
                        },
                    },
                },
            }),
            outward_issue: None,
        }
    }

    fn pr_summary(number: u64, head_ref_name: &str, lifecycle: PrLifecycle) -> PrSummary {
        PrSummary {
            number,
            head_ref_name: head_ref_name.to_string(),
            lifecycle,
            updated_at: "2026-08-06T00:00:00Z".to_string(),
        }
    }

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
            audit: crate::config::AuditConfig::default(),
            review_watch: crate::config::ReviewWatchConfig::default(),
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

    #[test]
    fn resolve_prompt_path_resolves_relative_lane_prompt_against_repo_root() {
        assert_eq!(
            resolve_prompt_path(
                "mylane",
                None,
                Some("prompts/mylane-lane.md"),
                Path::new("/repo"),
                Path::new("/home/j"),
            ),
            PathBuf::from("/repo/prompts/mylane-lane.md")
        );
    }

    #[test]
    fn resolve_prompt_path_leaves_absolute_and_tilde_lane_prompts_alone() {
        assert_eq!(
            resolve_prompt_path(
                "mylane",
                None,
                Some("/elsewhere/lane.md"),
                Path::new("/repo"),
                Path::new("/home/j"),
            ),
            PathBuf::from("/elsewhere/lane.md")
        );
        assert_eq!(
            resolve_prompt_path(
                "mylane",
                None,
                Some("~/prompts/lane.md"),
                Path::new("/repo"),
                Path::new("/home/j"),
            ),
            PathBuf::from("/home/j/prompts/lane.md")
        );
    }

    #[test]
    fn resolve_prompt_path_keeps_cli_override_relative_to_cwd() {
        assert_eq!(
            resolve_prompt_path(
                "mylane",
                Some("scratch.md"),
                Some("prompts/mylane-lane.md"),
                Path::new("/repo"),
                Path::new("/home/j"),
            ),
            PathBuf::from("scratch.md")
        );
    }

    #[test]
    fn resolve_prompt_path_falls_back_to_home_claude_prompts() {
        assert_eq!(
            resolve_prompt_path(
                "mylane",
                None,
                None,
                Path::new("/repo"),
                Path::new("/home/j"),
            ),
            PathBuf::from("/home/j/.claude/prompts/mylane.md")
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
    fn run_lane_fg_absent_is_error_on_zero_exit_marks_run_interrupted() {
        // The usage-runout bug this variant exists to fix: `claude` exits 0
        // and writes valid result JSON, but the JSON never got an
        // `is_error` field at all (rather than an explicit `false`). That
        // must not be treated as a confirmed `Done` -- it's ambiguous.
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let json = r#"{"session_id":"sess-1","result":"partial work"}"#;
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
        assert_eq!(run.status, RunStatus::Interrupted);
    }

    #[test]
    fn run_lane_fg_unparseable_result_json_on_zero_exit_marks_run_interrupted() {
        // Malformed/missing-session-id JSON is ambiguous, not a confirmed
        // agent failure -- Interrupted, not Failed.
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success("not json".to_string());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
        assert_eq!(run.status, RunStatus::Interrupted);
    }

    #[test]
    fn run_lane_fg_nonzero_exit_with_absent_is_error_still_marks_failed() {
        // A non-zero exit is an unambiguous failure signal from the process
        // itself, regardless of what (if anything) the JSON says -- it must
        // win over the "absent is_error -> Interrupted" rule.
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let json = r#"{"session_id":"sess-1","result":"partial work"}"#;
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::with_exit_code(json.to_string(), 1);
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
    }

    /// The regression this suite exists for: the in-session agent
    /// deliberately finishes its own run (`tm runs finish <id> --status
    /// blocked --blocker "..."`) before `claude -p` exits 0. The
    /// supervisor's own `run_claude_and_finish` tail must not clobber that
    /// status back to `Done` -- but it should still fill in the telemetry
    /// (turns, cost, session id) that only it can observe, since the
    /// session-set outcome leaves those fields `None`.
    #[test]
    fn run_claude_and_finish_preserves_session_set_blocked_status_on_zero_exit() {
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        // The in-session agent finishes its own run before `claude -p`
        // exits.
        run_store
            .finish_run(
                prepared.run_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    blocker: Some("waiting on AX-408 / PR #490".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        // `claude -p` then exits 0 with an `is_error: false` result -- the
        // supervisor thinks this run is `Done`.
        let supervisor_spawner = FakeProcessSpawner::success(canned_json());
        let mut supervisor_out = Vec::new();

        run_claude_and_finish(
            &supervisor_spawner,
            &gh,
            &run_store,
            &prepared,
            &mut supervisor_out,
        )
        .unwrap();

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Blocked);
        assert_eq!(run.blocker.as_deref(), Some("waiting on AX-408 / PR #490"));
        // Telemetry only the supervisor can observe is still recorded.
        assert_eq!(run.session_id.as_deref(), Some("sess-1"));
        assert_eq!(run.num_turns, Some(3));
        assert_eq!(run.cost_usd, Some(0.5));
    }

    /// Precedence must still hold in the other direction: a non-zero exit
    /// is an unambiguous crash signal and must mark the run `Failed` even
    /// if the in-session agent already set some other status for itself.
    #[test]
    fn run_claude_and_finish_nonzero_exit_overrides_session_set_status_to_failed() {
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        run_store
            .finish_run(
                prepared.run_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    blocker: Some("waiting on something".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        let supervisor_spawner = FakeProcessSpawner::with_exit_code(canned_json(), 1);
        let mut supervisor_out = Vec::new();

        run_claude_and_finish(
            &supervisor_spawner,
            &gh,
            &run_store,
            &prepared,
            &mut supervisor_out,
        )
        .unwrap();

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failed);
    }

    /// Same precedence rule for the `Interrupted` classification added
    /// alongside the absent/unparseable `is_error` handling: it must not
    /// override a status the in-session agent already set, since an
    /// ambiguous exit-0-but-no-`is_error` result is a weaker signal than a
    /// deliberate `tm runs finish` call.
    #[test]
    fn run_claude_and_finish_does_not_interrupt_a_session_set_blocked_status() {
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        run_store
            .finish_run(
                prepared.run_id,
                &FinishRun {
                    status: RunStatus::Blocked,
                    blocker: Some("waiting on something".to_string()),
                    ..FinishRun::default()
                },
            )
            .unwrap();

        // Exit 0, but the JSON never got an `is_error` field at all -- the
        // supervisor would classify this as `Interrupted` on a fresh run.
        let json = r#"{"session_id":"sess-1","result":"partial work"}"#;
        let supervisor_spawner = FakeProcessSpawner::success(json.to_string());
        let mut supervisor_out = Vec::new();

        run_claude_and_finish(
            &supervisor_spawner,
            &gh,
            &run_store,
            &prepared,
            &mut supervisor_out,
        )
        .unwrap();

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Blocked);
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
    fn run_lane_fg_errors_before_any_run_row_when_lane_backend_is_incompatible() {
        // GitHub issue #5 phase 2: a lane whose repo resolves to a
        // different backend identity than the invoking repo's own must be
        // refused before any prompt/worktree/run-row work -- see
        // RunLaneError::BackendMismatch.
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

        let current_identity = BackendIdentity::Github {
            repo: "jowi-dev/tskmstr".to_string(),
        };
        let lane_identity = BackendIdentity::Jira {
            base_url: "https://axiom.atlassian.net".to_string(),
            project_key: "AX".to_string(),
        };
        let resolver = crate::config::FakeBackendIdentityResolver::new()
            .with_identity(repo_root.clone(), lane_identity.clone());
        let current_repo_dir = tmp.path().join("current-repo");

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            ticket_provider: None,
            current_repo_dir: &current_repo_dir,
            current_backend_identity: &current_identity,
            backend_identity_resolver: &resolver,
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

        match &err {
            RunLaneError::BackendMismatch(info) => {
                assert_eq!(info.lane, "mylane");
                assert_eq!(info.lane_repo, repo_root);
                assert_eq!(info.current_repo, current_repo_dir);
                assert_eq!(info.lane_backend, lane_identity);
                assert_eq!(info.current_backend, current_identity);
            }
            other => panic!("expected BackendMismatch, got {other:?}"),
        }
        assert!(
            run_store.list_runs().unwrap().is_empty(),
            "must not create a run row before the backend-compatibility preflight passes"
        );
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

    // Issue #5 fix 3: under the github backend, the ticket provider wired
    // into RunLaneDeps.ticket_provider is a GithubProvider, not a Jira
    // client — so GH-N slug resolution must actually resolve the issue's
    // summary via `gh`, not silently fail. Mirrors
    // `run_lane_fg_uses_jira_summary_slug_for_branch_name_when_available`
    // above, but with a real `GithubProvider` over a `FakeGhCli`.
    #[test]
    fn run_lane_fg_uses_github_summary_slug_for_branch_name_when_available() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let issue_gh = FakeGhCli::new().with_issue_view(
            3,
            Ok(crate::github::gh_cli::IssueInfo {
                number: 3,
                url: "https://github.com/jowi-dev/tskmstr/issues/3".to_string(),
                title: "Delete bid connector".to_string(),
                body: String::new(),
                state: crate::github::gh_cli::IssueState::Open,
                labels: Vec::new(),
                assignees: Vec::new(),
            }),
        );
        let github_provider = crate::ticketing::github_provider::GithubProvider::new(
            &issue_gh,
            "jowi-dev/tskmstr".to_string(),
        );
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&github_provider),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("GH-3".to_string()),
            ..Default::default()
        };

        let outcome = run_lane_fg(&deps, &config, &paths, "mylane", request, &mut out).unwrap();

        // Same shape as the Jira case: wt_name is the lowercased ticket,
        // the slug comes from the GitHub issue's title via GithubProvider's
        // get_issue mapping (title -> Issue.fields.summary).
        assert_eq!(outcome.branch, "claude/gh-3-delete-bid-connector");
    }

    // --- Jira-summary-slug branch naming and its fallbacks (module doc's
    // step 6): a ticket with a Jira summary available produces
    // `<owner>/<wt_name>-<slug>`; anything short of that (no ticket, no
    // Jira dependency, a failed/empty lookup) falls back to the original
    // `<owner>/<wt_name>-<timestamp>`, silently. See resolve_ticket_slug's
    // doc comment for the exhaustive fallback list. ---

    #[test]
    fn run_lane_fg_uses_jira_summary_slug_for_branch_name_when_available() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let jira =
            FakeJiraClient::new().with_issue("ABC-123", issue("ABC-123", "Delete bid connector"));
        let spawner = FakeProcessSpawner::success(canned_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        // No configured branch-owner source resolves ("claude" fallback);
        // wt_name is the lowercased ticket; the slug carries no timestamp.
        assert_eq!(outcome.branch, "claude/abc-123-delete-bid-connector");
    }

    #[test]
    fn prepare_run_lane_falls_back_to_timestamp_naming_when_jira_is_none() {
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

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert_eq!(prepared.branch, "claude/abc-123-20260806-090503");
    }

    #[test]
    fn prepare_run_lane_falls_back_to_timestamp_naming_when_no_ticket_given() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        // Seeded so a bug that ignores "no ticket" and looks up anyway
        // would still be caught (it has nothing to key a lookup on).
        let jira =
            FakeJiraClient::new().with_issue("ABC-123", issue("ABC-123", "Delete bid connector"));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        assert_eq!(prepared.branch, "claude/mylane-20260806-090503");
    }

    #[test]
    fn prepare_run_lane_falls_back_to_timestamp_naming_when_get_issue_fails() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let jira = FakeJiraClient::new().with_issue_not_found("ABC-123");
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        // A Jira lookup failure must not fail the run at all, let alone in
        // a way that surfaces as an Err here.
        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert_eq!(prepared.branch, "claude/abc-123-20260806-090503");
    }

    #[test]
    fn prepare_run_lane_falls_back_to_timestamp_naming_when_summary_is_empty() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let jira = FakeJiraClient::new().with_issue("ABC-123", issue("ABC-123", ""));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert_eq!(prepared.branch, "claude/abc-123-20260806-090503");
    }

    #[test]
    fn prepare_run_lane_appends_suffix_when_slug_branch_already_exists() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        // The first candidate and its "-2" retry are both already taken
        // (e.g. left over from earlier runs of the same ticket); only
        // "-3" is free.
        let git = FakeGitOps::new().with_existing_branches([
            "claude/abc-123-delete-bid-connector",
            "claude/abc-123-delete-bid-connector-2",
        ]);
        let gh = FakeGhCli::new();
        let jira =
            FakeJiraClient::new().with_issue("ABC-123", issue("ABC-123", "Delete bid connector"));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert_eq!(prepared.branch, "claude/abc-123-delete-bid-connector-3");
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
    fn prepare_run_lane_interactive_builds_a_tmux_hostable_invocation() {
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let request = RunLaneRequest {
            ticket: Some("PROJ-1".to_string()),
            mode: RunMode::Interactive,
            ..RunLaneRequest::default()
        };
        let mut out = Vec::new();

        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert_eq!(
            prepared.invocation.env_set,
            vec![(
                "TSKMSTR_SESSION_RUN_ID".to_string(),
                prepared.run_id.to_string()
            )],
            "an interactive run is finished by its SessionEnd hook, which \
             TSKMSTR_RUN_ID would gate off"
        );
        // The prompt is positional, and it opens by telling the session to
        // adopt the row `start_run` just created — the only way a session can
        // reach `register_session`, which needs an in-session env var.
        let prompt = &prepared.invocation.args[0];
        assert!(prompt.contains("tm runs register --kind lane PROJ-1"));
        assert!(prompt.contains("Work ticket: PROJ-1."));
        assert!(!prepared.invocation.args.iter().any(|arg| arg == "-p"));

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.pid, None, "no supervisor process to attribute a pid to");
    }

    #[test]
    fn prepare_run_lane_interactive_registers_a_ticketless_run_under_its_lane_name() {
        // With no ticket the run row's `ticket` column holds the lane name,
        // and that is what the session has to register against for adoption
        // to match.
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let request = RunLaneRequest {
            mode: RunMode::Interactive,
            ..RunLaneRequest::default()
        };
        let mut out = Vec::new();

        let prepared =
            prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert!(
            prepared.invocation.args[0].contains("tm runs register --kind lane mylane"),
            "got: {}",
            prepared.invocation.args[0]
        );
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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
            ticket_provider: None,
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
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

    // --- resolve_blocker_stacking: the blocked-ticket branch-off decision
    // table (see its doc comment for the full spec). ---

    #[test]
    fn resolve_blocker_stacking_no_ticket_never_calls_jira_or_gh() {
        let jira = FakeJiraClient::new().with_issue("AX-1", issue("AX-1", "Something"));
        let gh = FakeGhCli::new();

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), None).unwrap();

        assert_eq!(resolution, BlockerResolution::default());
        assert!(gh.pr_list_all_calls().is_empty());
    }

    #[test]
    fn resolve_blocker_stacking_no_jira_client_never_calls_gh() {
        let gh = FakeGhCli::new();

        let resolution =
            resolve_blocker_stacking(None, &gh, Path::new("/repo"), Some("AX-1")).unwrap();

        assert_eq!(resolution, BlockerResolution::default());
        assert!(gh.pr_list_all_calls().is_empty());
    }

    #[test]
    fn resolve_blocker_stacking_no_blockers_uses_normal_base() {
        let jira = FakeJiraClient::new().with_issue("AX-1", issue("AX-1", "Something"));
        let gh = FakeGhCli::new();

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-1")).unwrap();

        assert_eq!(resolution, BlockerResolution::default());
    }

    #[test]
    fn resolve_blocker_stacking_merged_blocker_pr_uses_normal_base() {
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_summary(
            10,
            "jowi-dev/ax-410-add-connector",
            PrLifecycle::Merged,
        )]));

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2")).unwrap();

        assert_eq!(resolution, BlockerResolution::default());
    }

    #[test]
    fn resolve_blocker_stacking_done_blocker_no_pr_uses_normal_base() {
        // A blocker that's Done in Jira with no discoverable PR at all (a
        // config change, a spike, docs, manual ops work) must clear just
        // like a merged-PR blocker does — this is the other half of
        // `unmerged_direct_blockers`'s satisfaction rule.
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "done")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![]));

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2")).unwrap();

        assert_eq!(resolution, BlockerResolution::default());
    }

    #[test]
    fn resolve_blocker_stacking_one_open_pr_blocker_stacks_on_its_head_ref() {
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_summary(
            123,
            "jowi-dev/ax-410-add-connector",
            PrLifecycle::Open,
        )]));

        let repo_root = Path::new("/repo");
        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, repo_root, Some("AX-2")).unwrap();
        assert_eq!(gh.pr_list_all_calls(), vec![repo_root.to_path_buf()]);

        assert_eq!(
            resolution.stacked_base,
            Some("origin/jowi-dev/ax-410-add-connector".to_string())
        );
        assert_eq!(resolution.messages.len(), 1);
        assert!(resolution.messages[0].contains("AX-410"));
        assert!(resolution.messages[0].contains("PR #123 open"));
        assert!(
            resolution.messages[0].contains("origin/jowi-dev/ax-410-add-connector"),
            "message was: {}",
            resolution.messages[0]
        );
    }

    #[test]
    fn resolve_blocker_stacking_one_blocker_with_no_pr_warns_and_uses_normal_base() {
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new(); // no PR configured -> pr_list_all returns []

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2")).unwrap();

        assert_eq!(resolution.stacked_base, None);
        assert_eq!(resolution.messages.len(), 1);
        assert!(resolution.messages[0].contains("warning"));
        assert!(resolution.messages[0].contains("AX-410"));
    }

    #[test]
    fn resolve_blocker_stacking_two_unmerged_blockers_refuses() {
        let mut blocked = issue("AX-2", "Depends on two things");
        blocked.fields.issue_links =
            vec![blocks_link("AX-410", "new"), blocks_link("AX-411", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_summary(
            123,
            "jowi-dev/ax-410-add-connector",
            PrLifecycle::Open,
        )]));
        // AX-411 has no matching PR at all; AX-410 has an open one — either
        // way both are unmerged, so this must refuse.

        let err = resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2"))
            .unwrap_err();

        match err {
            RunLaneError::MultipleUnmergedBlockers { ticket, blockers } => {
                assert_eq!(ticket, "AX-2");
                assert_eq!(blockers.len(), 2);
                assert!(blockers.iter().any(|b| b.contains("AX-410")));
                assert!(blockers.iter().any(|b| b.contains("AX-411")));
            }
            other => panic!("expected MultipleUnmergedBlockers, got {other:?}"),
        }
    }

    #[test]
    fn resolve_blocker_stacking_jira_error_warns_and_uses_normal_base() {
        let jira = FakeJiraClient::new().with_issue_not_found("AX-2");
        let gh = FakeGhCli::new();

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2")).unwrap();

        assert_eq!(resolution.stacked_base, None);
        assert_eq!(resolution.messages.len(), 1);
        assert!(resolution.messages[0].contains("warning"));
        assert!(gh.pr_list_all_calls().is_empty());
    }

    #[test]
    fn resolve_blocker_stacking_gh_error_warns_and_uses_normal_base() {
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "not authenticated".to_string(),
        }));

        let resolution =
            resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2")).unwrap();

        assert_eq!(resolution.stacked_base, None);
        assert_eq!(resolution.messages.len(), 1);
        assert!(resolution.messages[0].contains("warning"));
    }

    #[test]
    fn resolve_blocker_stacking_gh_permanent_error_fails_the_run_instead_of_warning() {
        // The motivating incident: `gh pr list --json ...` requesting an
        // invalid field is a permanent, code-level defect, not a network
        // hiccup. Falling back to "use the normal base" here would dispatch
        // an autonomous run against the wrong base without anyone knowing
        // blocker resolution never actually ran — this must fail loudly
        // instead, unlike the transient case pinned just above.
        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: r#"Unknown JSON field: "merged""#.to_string(),
        }));

        let err = resolve_blocker_stacking(Some(&jira), &gh, Path::new("/repo"), Some("AX-2"))
            .unwrap_err();

        match err {
            RunLaneError::Gh(gh_err) => assert!(gh_err.is_permanent()),
            other => panic!("expected a permanent GhError, got {other:?}"),
        }
    }

    // --- prepare_run_lane wiring: blocker stacking overrides the branch's
    // cut base, --from bypasses it entirely, and >=2 unmerged blockers
    // refuse before any run row exists. ---

    #[test]
    fn prepare_run_lane_cuts_branch_from_stacked_blocker_base() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_summary(
            123,
            "jowi-dev/ax-410-add-connector",
            PrLifecycle::Open,
        )]));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("AX-2".to_string()),
            ..Default::default()
        };

        prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        // gh must be asked about lane_config.repo (repo_root), not whatever
        // directory the test process happens to be running in — see
        // GhCli::pr_list_all's doc comment on the wrong-repo failure mode.
        assert_eq!(gh.pr_list_all_calls(), vec![repo_root.clone()]);

        let calls = git.switch_new_branch_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].base, "origin/jowi-dev/ax-410-add-connector");

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("blocked by AX-410"));
    }

    #[test]
    fn prepare_run_lane_writes_a_blocker_warning_to_the_run_log_file() {
        // The other half of the incident: the stacking warning printed to
        // `out` never showed up in any of the six lane logs inspected,
        // because it's computed before the run row (and, for the detached
        // path, the log file redirection) exists. `prepare_run_lane` must
        // also append its blocker-resolution messages to the run's durable
        // log file, at the same path `crate::cli::work::run` later points
        // `RunStore::update_log_path`/`DetachSpawner::spawn_detached` at —
        // see `run_log_path`.
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: "not authenticated".to_string(),
        }));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let state_dir = tmp.path().join("state");
        let paths = RunLanePaths {
            home,
            state_dir: state_dir.clone(),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("AX-2".to_string()),
            ..Default::default()
        };

        prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        let log_path = run_log_path(&state_dir, "ax-2", "20260806-090503");
        let log_contents = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|err| panic!("expected a log file at {log_path:?}: {err}"));
        assert!(
            log_contents.contains("warning: could not resolve PRs for blockers of AX-2"),
            "expected the blocker warning in the log file, got: {log_contents:?}"
        );

        // The run row doesn't exist yet at this point (blocker resolution
        // happens before `start_run`), so this is the only durable place
        // the warning can land — `out` alone (the previous behavior) isn't
        // enough, since a detached run's `out` is only the parent
        // process's stdout, gone the moment its terminal closes.
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("warning: could not resolve PRs for blockers of AX-2"));
    }

    #[test]
    fn prepare_run_lane_writes_a_permanent_gh_error_to_the_run_log_before_failing() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_pr_list_all(Err(GhError::Command {
            command: "gh pr list".to_string(),
            exit_code: Some(1),
            stderr: r#"Unknown JSON field: "merged""#.to_string(),
        }));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let state_dir = tmp.path().join("state");
        let paths = RunLanePaths {
            home,
            state_dir: state_dir.clone(),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("AX-2".to_string()),
            ..Default::default()
        };

        let err = prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out)
            .unwrap_err();
        assert!(matches!(err, RunLaneError::Gh(ref e) if e.is_permanent()));

        // No run row exists to leak (prepare_run_lane's whole design point,
        // per its doc comment), but the error must still land somewhere
        // durable rather than only on this process's stdout/stderr.
        assert_eq!(run_store.list_runs().unwrap().len(), 0);

        let log_path = run_log_path(&state_dir, "ax-2", "20260806-090503");
        let log_contents = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|err| panic!("expected a log file at {log_path:?}: {err}"));
        assert!(
            log_contents.contains("Unknown JSON field"),
            "expected the permanent gh error in the log file, got: {log_contents:?}"
        );
    }

    #[test]
    fn prepare_run_lane_with_explicit_from_skips_blocker_logic_entirely() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let mut blocked = issue("AX-2", "Depends on AX-410");
        blocked.fields.issue_links = vec![blocks_link("AX-410", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new().with_pr_list_all(Ok(vec![pr_summary(
            123,
            "jowi-dev/ax-410-add-connector",
            PrLifecycle::Open,
        )]));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));

        let deps = RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &FakeProcessSpawner::success(canned_json()),
            run_store: &run_store,
            clock: &clock,
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("AX-2".to_string()),
            from_base: Some("origin/staging".to_string()),
            ..Default::default()
        };

        prepare_run_lane(&deps, &config, &paths, "mylane", request, None, &mut out).unwrap();

        assert!(gh.pr_list_all_calls().is_empty());
        let calls = git.switch_new_branch_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].base, "origin/staging");
    }

    #[test]
    fn run_lane_fg_refuses_before_any_run_row_when_two_blockers_are_unmerged() {
        let (tmp, home, repo_root, worktree_root, _prompt_path) = setup();
        let config = config_with_lane(
            "mylane",
            lane_config(&repo_root.to_string_lossy()),
            &worktree_root,
        );

        let mut blocked = issue("AX-2", "Depends on two things");
        blocked.fields.issue_links =
            vec![blocks_link("AX-410", "new"), blocks_link("AX-411", "new")];
        let jira = FakeJiraClient::new().with_issue("AX-2", blocked);
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
            ticket_provider: Some(&jira),
            current_repo_dir: Path::new("/irrelevant-in-tests"),
            current_backend_identity: compatible_test_identity(),
            backend_identity_resolver: compatible_test_resolver(),
        };
        let paths = RunLanePaths {
            home,
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("AX-2".to_string()),
            ..Default::default()
        };

        let err = run_lane_fg(&deps, &config, &paths, "mylane", request, &mut out).unwrap_err();

        assert!(matches!(err, RunLaneError::MultipleUnmergedBlockers { .. }));
        assert!(spawner.recorded.lock().unwrap().is_empty());
        assert_eq!(run_store.list_runs().unwrap().len(), 0);
    }

    fn review_fix_paths(tmp: &TempDir) -> RunLanePaths {
        RunLanePaths {
            home: tmp.path().join("home"),
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        }
    }

    #[test]
    fn prepare_review_fix_starts_a_review_fix_run_on_the_existing_worktree_and_branch() {
        let tmp = TempDir::new().unwrap();
        let paths = review_fix_paths(&tmp);
        let git = FakeGitOps::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");
        std::fs::create_dir_all(&worktree).unwrap();

        let prepared = prepare_review_fix(
            &git,
            &run_store,
            &clock,
            &paths,
            "PROJ-1",
            "mylane",
            &worktree,
            "jowi-dev/proj-1-slug",
            "fix the review comments".to_string(),
            None,
            RunMode::Headless,
        )
        .unwrap();

        assert_eq!(prepared.worktree, worktree);
        assert_eq!(prepared.branch, "jowi-dev/proj-1-slug");
        assert_eq!(prepared.ticket, Some("PROJ-1".to_string()));
        assert!(
            prepared
                .invocation
                .args
                .contains(&"fix the review comments".to_string())
        );

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.kind, "review-fix");
        assert_eq!(run.ticket, "PROJ-1");
        assert_eq!(run.lane, "mylane");
        assert_eq!(run.worktree, worktree.to_string_lossy());
        assert_eq!(run.branch, Some("jowi-dev/proj-1-slug".to_string()));

        // No worktree provisioning and no branch cut -- this is the whole
        // point of prepare_review_fix over prepare_run_lane.
        assert!(git.provision_worktree_calls().is_empty());
        assert!(git.switch_new_branch_calls().is_empty());
    }

    #[test]
    fn prepare_review_fix_refuses_a_dirty_worktree_and_creates_no_run_row() {
        let tmp = TempDir::new().unwrap();
        let paths = review_fix_paths(&tmp);
        let git = FakeGitOps::new().with_status_is_clean(Ok(false));
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");

        let err = prepare_review_fix(
            &git,
            &run_store,
            &clock,
            &paths,
            "PROJ-1",
            "mylane",
            &worktree,
            "jowi-dev/proj-1-slug",
            "fix the review comments".to_string(),
            None,
            RunMode::Headless,
        )
        .unwrap_err();

        assert!(matches!(err, ReviewFixError::WorktreeDirty(path) if path == worktree));
        assert_eq!(run_store.list_runs().unwrap().len(), 0);
    }

    #[test]
    fn prepare_review_fix_interactive_builds_a_tmux_hostable_invocation() {
        let tmp = TempDir::new().unwrap();
        let paths = review_fix_paths(&tmp);
        let git = FakeGitOps::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");

        let prepared = prepare_review_fix(
            &git,
            &run_store,
            &clock,
            &paths,
            "PROJ-1",
            "mylane",
            &worktree,
            "jowi-dev/proj-1-slug",
            "fix the review comments".to_string(),
            None,
            RunMode::Interactive,
        )
        .unwrap();

        assert_eq!(
            prepared.invocation.env_set,
            vec![(
                "TSKMSTR_SESSION_RUN_ID".to_string(),
                prepared.run_id.to_string()
            )]
        );
        let prompt = &prepared.invocation.args[0];
        assert!(prompt.contains("tm runs register --kind review-fix PROJ-1"));
        assert!(prompt.ends_with("fix the review comments"));
    }

    #[test]
    fn prepare_review_fix_records_the_current_pid_when_given_one() {
        let tmp = TempDir::new().unwrap();
        let paths = review_fix_paths(&tmp);
        let git = FakeGitOps::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 18, 10, 0, 0));
        let worktree = tmp.path().join("Worktrees/axiom/proj-1");

        let prepared = prepare_review_fix(
            &git,
            &run_store,
            &clock,
            &paths,
            "PROJ-1",
            "mylane",
            &worktree,
            "jowi-dev/proj-1-slug",
            "fix the review comments".to_string(),
            Some(4242),
            RunMode::Headless,
        )
        .unwrap();

        let run = run_store.run_by_id(prepared.run_id).unwrap().unwrap();
        assert_eq!(run.pid, Some(4242));
    }
}
