//! `tm work new/remove/list/restore/start`: CLI wiring for the lane runner's
//! worktree/tmux-session provisioning, ported from devtools'
//! `~/devtools/work.ml` (see `docs/plans/runner-port.md` step 5).
//!
//! Every function here takes `&dyn GitOps`/`&dyn TmuxOps`/`&Config` plus an
//! explicit `home`/`cwd` and a `Write` sink, mirroring the existing
//! `cli::pr`/`cli::ticket` pattern: no env reads, no real process/terminal
//! interaction inside the logic itself, so tests exercise it with
//! `FakeGitOps`/`FakeTmuxOps` and a `Vec<u8>` sink. `src/main.rs` is the
//! only place that wires up the real `ShellGitOps`/`ShellTmuxOps` and reads
//! `$HOME`/`cwd`.
//!
//! # Repo resolution (`new`/`remove`)
//!
//! `work.ml` always resolves the target repo from the current working
//! directory (`git_repo_root()`), which assumes the invoker is standing
//! inside the repo they want to operate on. Per `docs/plans/runner-port.md`
//! §2, `tm work` widens that: [`resolve_repo_root`] first checks whether
//! `name` matches a configured lane in `config.work.lanes`, and if so uses
//! that lane's `repo` path directly (no `cwd` involved at all — this is
//! what lets `tm work new <lane>` work from a board TUI or a cron job that
//! isn't "in" any repo). Only when `name` doesn't match a configured lane
//! does it fall back to `work.ml`'s original behavior: resolve the repo
//! root from `cwd` via [`crate::work::git::GitOps::repo_root`], so the
//! command still works stand-alone inside any repo, lane-configured or not.
//!
//! # `start`
//!
//! `tm work start [<dir>]` is Joe's main tmux entry point (see the plan's
//! amended inventory): `dir` defaults to `cwd` when omitted. It ports
//! `work.ml`'s `start`/`tmux_new_session`/`tmux_attach` verbatim other than
//! reading window names from config instead of a hardcoded list (see
//! `crate::work::tmux` module docs).
//!
//! # `list`/`restore`
//!
//! `list` renders every current tmux session with a `worktree`/`session`
//! kind column (`work.ml`'s `worktree_list`). `restore` walks
//! `config.work.worktree_root`'s subdirectories two levels deep
//! (`<repo>/<lane>`) and recreates a session for every linked worktree that
//! doesn't already have one running (`work.ml`'s `worktree_restore`) —
//! this is the after-a-reboot operation, and never attaches.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::WorkConfig;
use crate::github::gh_cli::GhCli;
use crate::jira::client::JiraClient;
use crate::runs::{RunStore, RunStoreError};
use crate::work::detach::{DetachError, DetachSpawner};
use crate::work::git::{GitError, GitOps};
use crate::work::naming::{self, expand_tilde};
use crate::work::run::{Clock, RunLaneError, RunLaneRequest};
use crate::work::runner::ProcessSpawner;
use crate::work::tmux::{TmuxError, TmuxOps};

/// Errors surfaced by `tm work new/remove/list/restore/start`.
#[derive(Debug, Error)]
pub enum WorkCliError {
    /// A `git` shell-out failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A `tmux` shell-out failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),

    /// A filesystem or output-write operation failed.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// `tm work start <dir>` was given a directory that doesn't exist.
    /// Mirrors `work.ml`'s `start`, which checks `Sys.file_exists` up
    /// front.
    #[error("directory does not exist: {}", .0.display())]
    DirectoryNotFound(PathBuf),

    /// `tm work remove <name>` named a worktree that hasn't been
    /// provisioned. Mirrors `work.ml`'s `worktree_remove` existence check.
    #[error("worktree does not exist: {}", .0.display())]
    WorktreeNotFound(PathBuf),

    /// The resolved repo root has no final path component to use as a
    /// worktree-root subdirectory name (e.g. `/`). `work.ml` has no
    /// equivalent guard — `Filename.basename` never fails — but
    /// `Path::file_name` can return `None`, so this is a deliberate,
    /// explicit error rather than a silent empty-string fallback.
    #[error("cannot derive a repo name from `{}`", .0.display())]
    UnresolvableRepoName(PathBuf),

    /// The lane-run core (shared by `--fg` and the detached path's
    /// foreground provisioning half) failed. See
    /// [`crate::work::run::RunLaneError`] for the specific cause.
    #[error(transparent)]
    Run(#[from] RunLaneError),

    /// Spawning the detached run's supervisor process failed.
    #[error(transparent)]
    Detach(#[from] DetachError),

    /// The supervisor's `--state-file` could not be serialized/deserialized
    /// as JSON.
    #[error("failed to (de)serialize supervisor state: {0}")]
    SupervisorState(#[from] serde_json::Error),

    /// A run-state store operation failed (the supervisor path's
    /// `update_pid` call, outside [`RunLaneError`]'s scope).
    #[error(transparent)]
    RunStore(#[from] RunStoreError),

    /// `tm work new`/`remove` was given an empty (or all-whitespace) name.
    /// See [`worktree_path_for`]'s doc comment for why this is rejected
    /// before ever calling `naming::worktree_path`.
    #[error("worktree name cannot be empty")]
    EmptyWorktreeName,

    /// Belt-and-suspenders: the worktree path computed by
    /// `naming::worktree_path` did not land exactly one level below the
    /// project's worktree directory, as
    /// [`naming::worktree_path_has_expected_parent`] expects. Should be
    /// unreachable given [`WorkCliError::EmptyWorktreeName`]'s check, but
    /// checked independently right before any `git worktree add`/`remove`.
    #[error(
        "refusing to touch computed worktree path {} — it does not sit one level below the expected worktree directory",
        .0.display()
    )]
    WorktreePathMismatch(PathBuf),
}

/// Dependencies `tm work` subcommands need, gathered so callers don't have
/// to thread four separate parameters through every function. Follows the
/// same "trait objects + config, no env reads" shape as
/// [`crate::ticketing::TicketingContext`].
pub struct WorkContext<'a> {
    /// Git operations (real or fake).
    pub git: &'a dyn GitOps,
    /// tmux operations (real or fake).
    pub tmux: &'a dyn TmuxOps,
    /// Validated `[work]` config.
    pub config: &'a WorkConfig,
    /// The invoking user's home directory, for `~`-expanding
    /// `config.worktree_root`. Passed explicitly rather than read from
    /// `$HOME` here, so tests never depend on the real environment.
    pub home: &'a Path,
}

/// Resolve the repository a lane/worktree name operates against: the
/// configured lane's `repo` if `name` matches one, otherwise `cwd`'s repo
/// root. See the module docs' "Repo resolution" section.
fn resolve_repo_root(
    ctx: &WorkContext<'_>,
    name: &str,
    cwd: &Path,
) -> Result<PathBuf, WorkCliError> {
    match ctx.config.lanes.get(name) {
        Some(lane) => Ok(PathBuf::from(&lane.repo)),
        None => Ok(ctx.git.repo_root(cwd)?),
    }
}

/// The configured worktree root, `~`-expanded against `ctx.home`, falling
/// back to `work.ml`'s hardcoded default (`~/Worktrees`) when unset.
fn resolve_worktree_root(ctx: &WorkContext<'_>) -> PathBuf {
    let raw = ctx.config.worktree_root.as_deref().unwrap_or("~/Worktrees");
    expand_tilde(raw, ctx.home)
}

/// The final path component of `repo_root`, used as the worktree root's
/// per-repo subdirectory, mirroring `work.ml`'s `repo_name`
/// (`Filename.basename (git_repo_root ())`).
fn repo_name(repo_root: &Path) -> Result<String, WorkCliError> {
    repo_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| WorkCliError::UnresolvableRepoName(repo_root.to_path_buf()))
}

/// Build a lane/worktree's full path, given an already-resolved repo root.
///
/// Rejects an empty (or all-whitespace) `name` up front: `naming::worktree_path`'s
/// `PathBuf::join("")` no-op would otherwise silently collapse the result
/// onto `<worktree_root>/<repo>` — the project's per-repo worktree
/// directory — rather than a real worktree path one level below it. As a
/// second, independent guard (a bug in the check above must not leave this
/// uncaught), the computed path is also re-checked against
/// [`naming::worktree_path_has_expected_parent`] before being returned to
/// `new`/`remove`, both of which use it to provision/remove a worktree.
fn worktree_path_for(
    ctx: &WorkContext<'_>,
    repo_root: &Path,
    name: &str,
) -> Result<PathBuf, WorkCliError> {
    if name.trim().is_empty() {
        return Err(WorkCliError::EmptyWorktreeName);
    }

    let root = resolve_worktree_root(ctx);
    let repo = repo_name(repo_root)?;
    let wt_path = naming::worktree_path(&root.to_string_lossy(), &repo, name);

    if !naming::worktree_path_has_expected_parent(&root.to_string_lossy(), &repo, &wt_path) {
        return Err(WorkCliError::WorktreePathMismatch(wt_path));
    }

    Ok(wt_path)
}

/// Create (or attach to an already-provisioned) session in `dir`, and
/// finally attach to it. Shared by `tm work new` (once the worktree exists)
/// and `tm work start`, mirroring `work.ml`'s `start`.
fn start_in_dir(
    ctx: &WorkContext<'_>,
    dir: &Path,
    out: &mut dyn Write,
) -> Result<(), WorkCliError> {
    if !dir.exists() {
        return Err(WorkCliError::DirectoryNotFound(dir.to_path_buf()));
    }

    let name = naming::session_name_from_dir(&dir.to_string_lossy());
    let dir_str = dir.to_string_lossy();

    if ctx.tmux.has_session(&name)? {
        writeln!(out, "Attaching to existing session: {name}")?;
    } else {
        writeln!(out, "Creating session: {name} ({dir_str})")?;
        create_session(ctx, &name, &dir_str)?;
    }
    ctx.tmux.attach(&name)?;
    Ok(())
}

/// Create a new tmux session named `name` rooted at `dir`, building every
/// window from [`crate::work::tmux::window_creation_sequence`] and
/// selecting the primary window at the end, mirroring `work.ml`'s
/// `tmux_new_session`.
fn create_session(ctx: &WorkContext<'_>, name: &str, dir: &str) -> Result<(), WorkCliError> {
    let sequence = crate::work::tmux::window_creation_sequence(
        &ctx.config.tmux_windows,
        ctx.config.tmux_primary_window.as_deref(),
    );
    let (primary, extras) = sequence
        .split_first()
        .expect("window_creation_sequence always returns at least the primary window");

    ctx.tmux.new_session(name, dir, primary)?;
    for window in extras {
        ctx.tmux.new_window(name, window, dir)?;
    }
    ctx.tmux.select_window(name, primary)?;
    Ok(())
}

/// Dependencies `tm work run` needs beyond [`WorkContext`]'s
/// `git`/`config`/`home` (which it reuses): the extra seams
/// [`crate::work::run::run_lane_fg`] requires. Kept separate from
/// [`WorkContext`] rather than folded into it so the four existing
/// `new`/`remove`/`list`/`restore`/`start` commands (and every existing test
/// constructing a bare `WorkContext`) are unaffected by `run`'s wider
/// dependency set.
pub struct RunDeps<'a> {
    /// `gh` CLI operations (real or fake), for branch-owner resolution.
    pub gh: &'a dyn GhCli,
    /// Process spawning (real or fake).
    pub spawner: &'a dyn ProcessSpawner,
    /// The run-state store `start_run`/`finish_run` are called against.
    pub run_store: &'a RunStore,
    /// "Now" source for the run's timestamp.
    pub clock: &'a dyn Clock,
    /// Detached-supervisor process spawning (real or fake). Only used when
    /// `run`'s `fg` argument is `false`.
    pub detach: &'a dyn DetachSpawner,
    /// This process's own executable path (`std::env::current_exe()` in
    /// production), re-exec'd as the detached supervisor. Only used when
    /// `fg` is `false`.
    pub current_exe: &'a Path,
    /// The run-state database path, threaded through to the state file so
    /// the re-exec'd supervisor (a separate process, sharing no memory with
    /// this one) knows which `RunStore` to open. Only used when `fg` is
    /// `false`.
    pub run_db_path: &'a Path,
    /// Jira client used to look up a run's ticket summary for the
    /// human-readable branch-name slug (see
    /// [`crate::work::run::RunLaneDeps::jira`]), or `None` when Jira isn't
    /// configured/authenticated. Absence — like any lookup failure through
    /// it — silently falls back to the timestamp-based branch name; it is
    /// never a hard error for `tm work run` to have no Jira access.
    pub jira: Option<&'a dyn JiraClient>,
}

/// `tm work run <lane> [ticket] [--from base] [--model m] [--max-turns n]
/// [--permission-mode m] [--prompt path] [--fg]`: run one lane run. Thin CLI
/// wiring over [`crate::work::run`], which owns the actual sequencing (see
/// that module's doc comment) so it stays callable from a future TUI
/// without going through this CLI layer at all.
///
/// `fg = true` runs synchronously in this process
/// ([`crate::work::run::run_lane_fg`]) and returns once `claude` has
/// finished. `fg = false` (the default) does provisioning/preflight/
/// `start_run` in this process — so a bad lane, missing prompt, or dirty
/// worktree still errors out immediately with no run row left behind — then
/// re-execs `deps.current_exe` as a detached supervisor (see
/// `crate::work::detach`) to run `claude` and finish the tracked run, and
/// returns without waiting for it.
///
/// Returns `Ok(true)` when the run completed successfully (`fg`) or was
/// successfully handed off to a supervisor (detached); `Ok(false)` only when
/// an `fg` run completed but was recorded as failed (a non-zero `claude`
/// exit or `is_error: true`) — mirroring `work.ml`'s `if is_err then exit 1
/// else exit 0`, which is not itself an error condition worth an `Err`. The
/// detached path has no equivalent "did it fail" signal at hand-off time —
/// `tm runs watch`/`tm runs show` are how that outcome is observed later —
/// so it always returns `Ok(true)`.
pub fn run(
    ctx: &WorkContext<'_>,
    deps: &RunDeps<'_>,
    lane: &str,
    request: RunLaneRequest,
    fg: bool,
    out: &mut dyn Write,
) -> Result<bool, WorkCliError> {
    let paths = crate::work::run::RunLanePaths {
        home: ctx.home.to_path_buf(),
        state_dir: ctx.home.join(".local/state/tskmstr/work"),
        hooks_deploy_dir: ctx.home.join(".local/share/tskmstr/hooks"),
    };
    let run_deps = crate::work::run::RunLaneDeps {
        git: ctx.git,
        gh: deps.gh,
        spawner: deps.spawner,
        run_store: deps.run_store,
        clock: deps.clock,
        jira: deps.jira,
    };

    if fg {
        let outcome =
            crate::work::run::run_lane_fg(&run_deps, ctx.config, &paths, lane, request, out)?;
        return Ok(!outcome.is_error);
    }

    // Detached: provisioning/preflight/start_run happen here, in the
    // foreground, with pid = None (the supervisor records its own pid on
    // startup — see prepare_run_lane's doc comment on why).
    let prepared = crate::work::run::prepare_run_lane(
        &run_deps, ctx.config, &paths, lane, request, None, out,
    )?;

    std::fs::create_dir_all(&paths.state_dir)?;
    let log_path = paths
        .state_dir
        .join(format!("{}-{}.log", prepared.wt_name, prepared.timestamp));
    let state_path = paths.state_dir.join(format!(
        "{}-{}.supervisor.json",
        prepared.wt_name, prepared.timestamp
    ));

    let ticket = prepared.ticket.clone();
    let branch = prepared.branch.clone();
    let worktree = prepared.worktree.clone();

    let state = crate::work::detach::SupervisorState {
        prepared,
        run_db_path: deps.run_db_path.to_path_buf(),
    };
    std::fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;

    let argv = crate::work::detach::supervisor_argv(&state_path);
    deps.detach
        .spawn_detached(deps.current_exe, &argv, &worktree, &log_path)?;

    writeln!(
        out,
        "started   {lane} {} on {branch}",
        ticket.as_deref().unwrap_or("-")
    )?;
    writeln!(out, "worktree  {}", worktree.display())?;
    writeln!(out, "log       {}", log_path.display())?;
    writeln!(out, "watch:    tm runs watch")?;
    writeln!(out, "follow:   tail -f {}", log_path.display())?;
    if let Some(ticket) = &ticket {
        writeln!(out, "resume:   tm runs resume {ticket}")?;
    }

    Ok(true)
}

/// `tm work __supervise --state-file <path>`: the detached run's re-exec'd
/// supervisor entry point. Reads back the [`crate::work::detach::SupervisorState`]
/// `run` (above) wrote, opens the run store it points at, and runs
/// [`crate::work::run::supervise_run`] — record this process's own pid, then
/// spawn `claude`, wait, parse, and finish the tracked run.
///
/// Split from the thin `main.rs`-level dispatch (which owns reading the
/// state file, opening the real [`RunStore`], and picking
/// [`crate::work::runner::StdProcessSpawner`]) so the actual supervision
/// logic here is exercised with injected fakes in tests, exactly like
/// [`run`] above.
pub fn supervise(
    spawner: &dyn ProcessSpawner,
    gh: &dyn GhCli,
    run_store: &RunStore,
    state: &crate::work::detach::SupervisorState,
    out: &mut dyn Write,
) -> Result<bool, WorkCliError> {
    let outcome = crate::work::run::supervise_run(
        spawner,
        gh,
        run_store,
        &state.prepared,
        std::process::id(),
        out,
    )?;
    Ok(!outcome.is_error)
}

/// `tm work start [<dir>]`: attach to (or create) the tmux session for
/// `dir`, defaulting to `cwd` when `dir` is `None`. Joe's main tmux entry
/// point; see the module docs.
pub fn start(
    ctx: &WorkContext<'_>,
    dir: Option<&Path>,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<(), WorkCliError> {
    let target = dir.unwrap_or(cwd);
    start_in_dir(ctx, target, out)
}

/// `tm work new <name> [branch] [--from base]`: provision the worktree if
/// missing, then start/attach its session. Mirrors `work.ml`'s
/// `worktree_new`/`provision_worktree`, including the `.env.local` symlink:
/// [`GitOps::provision_worktree`] performs the link (it's the provisioning
/// path shared with [`crate::work::run::run_lane_fg`]) and reports back
/// whether one was created, so this prints `work.ml`'s "Linked .env.local
/// from main repo" message at the same point `work.ml` does.
pub fn new(
    ctx: &WorkContext<'_>,
    name: &str,
    branch: Option<&str>,
    from_base: Option<&str>,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<(), WorkCliError> {
    let repo_root = resolve_repo_root(ctx, name, cwd)?;
    let wt_path = worktree_path_for(ctx, &repo_root, name)?;

    if wt_path.exists() {
        writeln!(out, "Worktree already exists: {}", wt_path.display())?;
        writeln!(out, "Attaching to session...")?;
    } else {
        let branch_name = branch.unwrap_or(name);
        writeln!(
            out,
            "Creating worktree: {} (branch: {branch_name})",
            wt_path.display()
        )?;
        let linked = ctx
            .git
            .provision_worktree(&repo_root, &wt_path, branch_name, from_base)?;
        if linked {
            writeln!(out, "Linked .env.local from main repo")?;
        }
    }

    start_in_dir(ctx, &wt_path, out)
}

/// `tm work remove <name>`: kill the worktree's tmux session (if any) and
/// remove the worktree. Mirrors `work.ml`'s `worktree_remove`.
pub fn remove(
    ctx: &WorkContext<'_>,
    name: &str,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<(), WorkCliError> {
    let repo_root = resolve_repo_root(ctx, name, cwd)?;
    let wt_path = worktree_path_for(ctx, &repo_root, name)?;

    if !wt_path.exists() {
        return Err(WorkCliError::WorktreeNotFound(wt_path));
    }

    let session = naming::session_name_from_dir(&wt_path.to_string_lossy());
    if ctx.tmux.has_session(&session)? {
        writeln!(out, "Killing tmux session: {session}")?;
        ctx.tmux.kill_session(&session)?;
    }

    writeln!(out, "Removing worktree: {}", wt_path.display())?;
    ctx.git.remove_worktree(&repo_root, &wt_path)?;
    writeln!(out, "Done.")?;
    Ok(())
}

/// `tm work list`: render every current tmux session with a
/// `worktree`/`session` kind column. Mirrors `work.ml`'s `worktree_list`.
///
/// A session whose path isn't a git repository at all makes
/// [`GitOps::is_worktree`] return an `Err` (it shells out to `git
/// rev-parse`, which fails outside a repo) rather than `work.ml`'s tolerant
/// `false` (its `is_worktree` only ever inspects a file's existence after a
/// `Sys.command`, never surfacing an error). This function restores that
/// tolerance at the call site: any `is_worktree` error is treated as "not a
/// worktree" (kind `session`), matching `work.ml`'s observable behavior
/// without changing `GitOps`'s stricter error-propagating contract.
pub fn list(ctx: &WorkContext<'_>, out: &mut dyn Write) -> Result<(), WorkCliError> {
    let sessions = ctx.tmux.list_sessions()?;

    writeln!(out, "{:<20} {:<50} TYPE", "SESSION", "DIR")?;
    writeln!(out, "{:<20} {:<50} ----", "-------", "---")?;
    for session in sessions {
        let kind = match ctx.git.is_worktree(Path::new(&session.path)) {
            Ok(true) => "worktree",
            _ => "session",
        };
        writeln!(out, "{:<20} {:<50} {kind}", session.name, session.path)?;
    }
    Ok(())
}

/// `tm work restore`: recreate tmux sessions for every existing worktree
/// under the configured worktree root that doesn't already have one
/// running. Mirrors `work.ml`'s `worktree_restore`. Never attaches.
///
/// Iterates `<worktree_root>/<repo>/<name>` two levels deep, exactly the
/// shape [`resolve_worktree_root`]/[`naming::worktree_path`] produce.
/// Directory entries are sorted at each level for deterministic output —
/// `work.ml`'s `Sys.readdir` order is OS-dependent and never a documented
/// guarantee, so sorting is a strict improvement, not a semantics change.
pub fn restore(ctx: &WorkContext<'_>, out: &mut dyn Write) -> Result<(), WorkCliError> {
    let root = resolve_worktree_root(ctx);

    if !root.exists() {
        writeln!(out, "No worktrees directory found at {}", root.display())?;
        return Ok(());
    }

    let mut restored = 0u32;
    let mut skipped = 0u32;
    let mut misplaced = 0u32;

    for repo_dir in sorted_subdirs(&root)? {
        // A `repo_dir` (`<worktree_root>/<repo>`) that is ITSELF a worktree
        // root means a worktree got provisioned directly onto the project's
        // worktree directory — e.g. the `naming::worktree_path` empty-name
        // collapse this module's `worktree_path_for` now rejects, or a
        // manually-run `git worktree add` outside `tm`. Drilling into it as
        // if it were a normal `<repo>` directory is exactly what made a
        // real incident spin up a tmux session per ordinary subdirectory
        // (`lib/`, `test/`, ...) of a misplaced worktree. Surface it as a
        // warning naming the offending path and skip it entirely, rather
        // than walking its contents. Counted separately from `skipped`
        // (already-active sessions) so the summary line doesn't conflate
        // the two.
        if ctx.git.is_worktree(&repo_dir).unwrap_or(false) {
            writeln!(
                out,
                "Warning: {} is itself a worktree, not a repo directory — skipping (this usually means a worktree was accidentally created without a name; see `tm work new`)",
                repo_dir.display()
            )?;
            misplaced += 1;
            continue;
        }

        for wt_dir in sorted_subdirs(&repo_dir)? {
            if !ctx.git.is_worktree(&wt_dir).unwrap_or(false) {
                continue;
            }

            let session = naming::session_name_from_dir(&wt_dir.to_string_lossy());
            if ctx.tmux.has_session(&session)? {
                writeln!(out, "Already active: {session}")?;
                skipped += 1;
            } else {
                writeln!(out, "Restoring: {session} ({})", wt_dir.display())?;
                create_session(ctx, &session, &wt_dir.to_string_lossy())?;
                restored += 1;
            }
        }
    }

    writeln!(out)?;
    write!(out, "{restored} restored, {skipped} already active")?;
    if misplaced > 0 {
        write!(out, ", {misplaced} misplaced worktree(s) skipped")?;
    }
    writeln!(out, ".")?;
    Ok(())
}

/// Sorted list of `dir`'s direct subdirectories (non-directory entries
/// skipped), for deterministic `restore` iteration.
fn sorted_subdirs(dir: &Path) -> Result<Vec<PathBuf>, WorkCliError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    Ok(entries)
}

impl fmt::Debug for WorkContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkContext")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LaneConfig;
    use crate::work::git::FakeGitOps;
    use crate::work::tmux::{FakeTmuxOps, TmuxCall};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn config_with_lanes(lanes: BTreeMap<String, LaneConfig>) -> WorkConfig {
        WorkConfig {
            worktree_root: Some("/Worktrees".to_string()),
            default_model: None,
            default_max_turns: None,
            default_permission_mode: None,
            tmux_windows: vec!["fish".to_string()],
            tmux_primary_window: Some("code".to_string()),
            lanes,
            audit: crate::config::AuditConfig::default(),
            review_watch: crate::config::ReviewWatchConfig::default(),
        }
    }

    fn default_config() -> WorkConfig {
        config_with_lanes(BTreeMap::new())
    }

    fn out_string(buf: &[u8]) -> String {
        String::from_utf8(buf.to_vec()).unwrap()
    }

    // --- resolve_repo_root ---

    #[test]
    fn resolve_repo_root_uses_lane_repo_when_name_matches_a_lane() {
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "partner-integrations".to_string(),
            LaneConfig {
                repo: "/Users/jowi/Projects/axiom".to_string(),
                prompt_file: None,
                base_branch: None,
                model: None,
                max_turns: None,
                permission_mode: None,
            },
        );
        let config = config_with_lanes(lanes);
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };

        let repo =
            resolve_repo_root(&ctx, "partner-integrations", Path::new("/somewhere/else")).unwrap();
        assert_eq!(repo, PathBuf::from("/Users/jowi/Projects/axiom"));
        // No cwd-based git lookup should happen when a lane matched.
        assert!(git.branch_exists_local_calls().is_empty());
    }

    #[test]
    fn resolve_repo_root_falls_back_to_cwd_repo_root_when_no_lane_matches() {
        let config = default_config();
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo")));
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };

        let repo = resolve_repo_root(&ctx, "not-a-lane", Path::new("/somewhere")).unwrap();
        assert_eq!(repo, PathBuf::from("/repo"));
    }

    // --- new: provisioning ---

    #[test]
    fn new_provisions_worktree_when_missing_then_creates_and_attaches_session() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();
        // FakeGitOps::provision_worktree creates `wt_path` on disk on
        // success (mirroring what a real `git worktree add` would do), so
        // this doesn't need to be created here.
        let wt_path = worktree_root.join("axiom").join("my-lane");

        new(
            &ctx,
            "my-lane",
            None,
            Some("origin/main"),
            Path::new("/cwd"),
            &mut out,
        )
        .unwrap();

        assert_eq!(
            git.provision_worktree_calls(),
            vec![crate::work::git::ProvisionWorktreeCall {
                repo_dir: PathBuf::from("/repo/axiom"),
                wt_path: wt_path.clone(),
                branch: "my-lane".to_string(),
                from_base: Some("origin/main".to_string()),
            }]
        );

        let session_name = naming::session_name_from_dir(&wt_path.to_string_lossy());
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::HasSession(session_name.clone()),
                TmuxCall::NewSession {
                    name: session_name.clone(),
                    dir: wt_path.to_string_lossy().into_owned(),
                    primary_window: "code".to_string(),
                },
                TmuxCall::NewWindow {
                    name: session_name.clone(),
                    window_name: "fish".to_string(),
                    dir: wt_path.to_string_lossy().into_owned(),
                },
                TmuxCall::SelectWindow {
                    name: session_name.clone(),
                    window: "code".to_string(),
                },
                TmuxCall::Attach(session_name),
            ]
        );

        let printed = out_string(&out);
        assert!(printed.contains("Creating worktree:"));
        assert!(printed.contains("branch: my-lane"));
        assert!(printed.contains("Creating session:"));
    }

    #[test]
    fn new_rejects_an_empty_name_instead_of_collapsing_the_worktree_path() {
        // Regression test for Defect A: an empty name used to make
        // `naming::worktree_path`'s trailing `.join("")` a no-op, landing
        // `git worktree add` directly on `<worktree_root>/<repo>` — the
        // project's per-repo worktree directory itself.
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        let err = new(&ctx, "", None, None, Path::new("/cwd"), &mut out).unwrap_err();

        assert!(matches!(err, WorkCliError::EmptyWorktreeName));
        assert!(git.provision_worktree_calls().is_empty());
    }

    #[test]
    fn new_prints_linked_env_local_message_when_main_repo_has_one() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let repo_root = tmp.path().join("repo").join("axiom");
        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(repo_root.join(".env.local"), "DATABASE_URL=postgres://\n").unwrap();
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(repo_root.clone()));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        new(
            &ctx,
            "my-lane",
            None,
            Some("origin/main"),
            Path::new("/cwd"),
            &mut out,
        )
        .unwrap();

        let wt_path = worktree_root.join("axiom").join("my-lane");
        assert!(wt_path.join(".env.local").is_symlink());

        let printed = out_string(&out);
        assert!(printed.contains("Linked .env.local from main repo"));
    }

    #[test]
    fn new_does_not_print_linked_env_local_message_when_main_repo_has_none() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let repo_root = tmp.path().join("repo").join("axiom");
        std::fs::create_dir_all(&repo_root).unwrap();
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(repo_root.clone()));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        new(
            &ctx,
            "my-lane",
            None,
            Some("origin/main"),
            Path::new("/cwd"),
            &mut out,
        )
        .unwrap();

        let wt_path = worktree_root.join("axiom").join("my-lane");
        assert!(!wt_path.join(".env.local").exists());

        let printed = out_string(&out);
        assert!(!printed.contains("Linked .env.local"));
    }

    #[test]
    fn new_uses_given_branch_name_instead_of_lane_name() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        new(
            &ctx,
            "my-lane",
            Some("custom-branch"),
            None,
            Path::new("/cwd"),
            &mut out,
        )
        .unwrap();

        let calls = git.provision_worktree_calls();
        assert_eq!(calls[0].branch, "custom-branch");
        assert_eq!(calls[0].from_base, None);
    }

    #[test]
    fn new_skips_provisioning_when_worktree_already_exists() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let wt_path = worktree_root.join("axiom").join("my-lane");
        std::fs::create_dir_all(&wt_path).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        new(&ctx, "my-lane", None, None, Path::new("/cwd"), &mut out).unwrap();

        assert!(git.provision_worktree_calls().is_empty());
        let session_name = naming::session_name_from_dir(&wt_path.to_string_lossy());
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::HasSession(session_name.clone()),
                TmuxCall::Attach(session_name),
            ]
        );

        let printed = out_string(&out);
        assert!(printed.contains("Worktree already exists:"));
        assert!(printed.contains("Attaching to session..."));
        assert!(printed.contains("Attaching to existing session:"));
    }

    // --- remove ---

    #[test]
    fn remove_kills_session_and_removes_worktree() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let wt_path = worktree_root.join("axiom").join("my-lane");
        std::fs::create_dir_all(&wt_path).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        remove(&ctx, "my-lane", Path::new("/cwd"), &mut out).unwrap();

        let session_name = naming::session_name_from_dir(&wt_path.to_string_lossy());
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::HasSession(session_name.clone()),
                TmuxCall::KillSession(session_name),
            ]
        );
        assert_eq!(
            git.remove_worktree_calls(),
            vec![(PathBuf::from("/repo/axiom"), wt_path.clone())]
        );

        let printed = out_string(&out);
        assert!(printed.contains("Killing tmux session:"));
        assert!(printed.contains("Removing worktree:"));
        assert!(printed.contains("Done."));
    }

    #[test]
    fn remove_skips_kill_session_when_no_session_running() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let wt_path = worktree_root.join("axiom").join("my-lane");
        std::fs::create_dir_all(&wt_path).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new().with_has_session(Ok(false));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        remove(&ctx, "my-lane", Path::new("/cwd"), &mut out).unwrap();

        assert_eq!(
            tmux.calls(),
            vec![TmuxCall::HasSession(naming::session_name_from_dir(
                &wt_path.to_string_lossy()
            ))]
        );
    }

    #[test]
    fn remove_errors_when_worktree_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repo/axiom")));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        let err = remove(&ctx, "ghost-lane", Path::new("/cwd"), &mut out).unwrap_err();
        assert!(matches!(err, WorkCliError::WorktreeNotFound(_)));
    }

    // --- list ---

    #[test]
    fn list_renders_header_and_rows_with_kind() {
        let config = default_config();
        let git = FakeGitOps::new().with_is_worktree(Ok(true));
        let tmux =
            FakeTmuxOps::new().with_list_sessions(Ok(vec![crate::work::tmux::TmuxSession {
                name: "axiom-lane".to_string(),
                path: "/Worktrees/axiom/axiom-lane".to_string(),
            }]));
        let home = PathBuf::from("/Users/jowi");
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        list(&ctx, &mut out).unwrap();

        let printed = out_string(&out);
        assert!(printed.contains("SESSION"));
        assert!(printed.contains("axiom-lane"));
        assert!(printed.contains("worktree"));
    }

    #[test]
    fn list_treats_is_worktree_error_as_a_plain_session() {
        let config = default_config();
        let git = FakeGitOps::new().with_is_worktree(Err(GitError::Command {
            command: "git rev-parse".to_string(),
            exit_code: Some(128),
            stderr: "not a git repository".to_string(),
        }));
        let tmux =
            FakeTmuxOps::new().with_list_sessions(Ok(vec![crate::work::tmux::TmuxSession {
                name: "main".to_string(),
                path: "/Users/jowi/Projects/axiom".to_string(),
            }]));
        let home = PathBuf::from("/Users/jowi");
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        list(&ctx, &mut out).unwrap();

        let printed = out_string(&out);
        assert!(printed.contains("main"));
        assert!(printed.contains("session"));
        assert!(!printed.contains("worktree\n") && !printed.trim_end().ends_with("worktree"));
    }

    // --- restore ---

    #[test]
    fn restore_reports_no_worktrees_directory_when_root_missing() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        restore(&ctx, &mut out).unwrap();

        assert!(out_string(&out).contains("No worktrees directory found"));
    }

    #[test]
    fn restore_recreates_missing_sessions_and_skips_active_ones() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let lane_a = worktree_root.join("axiom").join("lane-a");
        let lane_b = worktree_root.join("axiom").join("lane-b");
        std::fs::create_dir_all(&lane_a).unwrap();
        std::fs::create_dir_all(&lane_b).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new()
            .with_is_worktree(Ok(false))
            .with_is_worktree_for_path(lane_a.clone(), Ok(true))
            .with_is_worktree_for_path(lane_b.clone(), Ok(true));
        // lane-a already has an active session; lane-b does not.
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        // Configure has_session to answer true for lane-a's session name and
        // false otherwise isn't directly supported by FakeTmuxOps (single
        // canned answer for all calls), so exercise the "none active" path
        // here and the "all active" path in a second test below.
        restore(&ctx, &mut out).unwrap();

        let calls = tmux.calls();
        let restore_calls: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c, TmuxCall::NewSession { .. }))
            .collect();
        assert_eq!(restore_calls.len(), 2);

        let printed = out_string(&out);
        assert!(printed.contains("Restoring:"));
        assert!(printed.contains("2 restored, 0 already active."));
    }

    #[test]
    fn restore_skips_worktrees_with_an_already_active_session() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let lane_a = worktree_root.join("axiom").join("lane-a");
        std::fs::create_dir_all(&lane_a).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new()
            .with_is_worktree(Ok(false))
            .with_is_worktree_for_path(lane_a.clone(), Ok(true));
        let tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        restore(&ctx, &mut out).unwrap();

        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSession { .. }))
        );
        let printed = out_string(&out);
        assert!(printed.contains("Already active:"));
        assert!(printed.contains("0 restored, 1 already active."));
    }

    #[test]
    fn restore_ignores_non_worktree_directories() {
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let not_a_worktree = worktree_root.join("axiom").join("scratch");
        std::fs::create_dir_all(&not_a_worktree).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        let git = FakeGitOps::new().with_is_worktree(Ok(false));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        restore(&ctx, &mut out).unwrap();

        assert!(tmux.calls().is_empty());
        assert!(out_string(&out).contains("0 restored, 0 already active."));
    }

    #[test]
    fn restore_skips_a_repo_dir_that_is_itself_a_misplaced_worktree() {
        // Regression test for the reported incident: a worktree got
        // provisioned directly at `<worktree_root>/<repo>` (e.g. via the
        // `naming::worktree_path` empty-name collapse), so that directory
        // now holds a checkout's ordinary subdirectories (`lib/`, `test/`)
        // rather than per-lane worktree directories. Before the fix,
        // `is_worktree` answered `true` for those subdirectories too
        // (`git rev-parse` walks upward), so `restore` spun up a tmux
        // session for each of them. The fix must skip the whole misplaced
        // `repo_dir` — zero sessions for its subdirectories — and warn
        // about it by name instead of silently ignoring it.
        let tmp = TempDir::new().unwrap();
        let worktree_root = tmp.path().join("Worktrees");
        let misplaced = worktree_root.join("axiom");
        let lib_subdir = misplaced.join("lib");
        let test_subdir = misplaced.join("test");
        std::fs::create_dir_all(&lib_subdir).unwrap();
        std::fs::create_dir_all(&test_subdir).unwrap();

        let config = WorkConfig {
            worktree_root: Some(worktree_root.to_string_lossy().into_owned()),
            ..config_with_lanes(BTreeMap::new())
        };
        // Model the real upward-walking bug this guards against
        // independently of: the misplaced worktree ROOT, and both of its
        // ordinary subdirectories, all answer `true` — exactly what a
        // pre-fix (or hypothetically regressed) `is_worktree` would report.
        let git = FakeGitOps::new()
            .with_is_worktree(Ok(false))
            .with_is_worktree_for_path(misplaced.clone(), Ok(true))
            .with_is_worktree_for_path(lib_subdir.clone(), Ok(true))
            .with_is_worktree_for_path(test_subdir.clone(), Ok(true));
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        restore(&ctx, &mut out).unwrap();

        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSession { .. })),
            "must not create a session for either subdirectory of the misplaced worktree"
        );
        let printed = out_string(&out);
        assert!(printed.contains("Warning:"));
        assert!(printed.contains(&misplaced.display().to_string()));
        assert!(printed.contains("0 restored, 0 already active"));
        assert!(printed.contains("1 misplaced worktree(s) skipped"));
    }

    // --- start ---

    #[test]
    fn start_defaults_to_cwd_when_no_dir_given() {
        let tmp = TempDir::new().unwrap();
        let config = default_config();
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        start(&ctx, None, tmp.path(), &mut out).unwrap();

        let session_name = naming::session_name_from_dir(&tmp.path().to_string_lossy());
        assert!(tmux.calls().contains(&TmuxCall::Attach(session_name)));
    }

    #[test]
    fn start_attaches_to_existing_session_without_creating_one() {
        let tmp = TempDir::new().unwrap();
        let config = default_config();
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new().with_has_session(Ok(true));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        start(&ctx, Some(tmp.path()), Path::new("/irrelevant"), &mut out).unwrap();

        assert!(
            !tmux
                .calls()
                .iter()
                .any(|c| matches!(c, TmuxCall::NewSession { .. }))
        );
        assert!(out_string(&out).contains("Attaching to existing session:"));
    }

    #[test]
    fn start_creates_session_with_configured_windows_when_none_exists() {
        let tmp = TempDir::new().unwrap();
        let config = default_config();
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new().with_has_session(Ok(false));
        let home = tmp.path().to_path_buf();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        start(&ctx, Some(tmp.path()), Path::new("/irrelevant"), &mut out).unwrap();

        let session_name = naming::session_name_from_dir(&tmp.path().to_string_lossy());
        assert_eq!(
            tmux.calls(),
            vec![
                TmuxCall::HasSession(session_name.clone()),
                TmuxCall::NewSession {
                    name: session_name.clone(),
                    dir: tmp.path().to_string_lossy().into_owned(),
                    primary_window: "code".to_string(),
                },
                TmuxCall::NewWindow {
                    name: session_name.clone(),
                    window_name: "fish".to_string(),
                    dir: tmp.path().to_string_lossy().into_owned(),
                },
                TmuxCall::SelectWindow {
                    name: session_name.clone(),
                    window: "code".to_string(),
                },
                TmuxCall::Attach(session_name),
            ]
        );
    }

    #[test]
    fn start_errors_when_dir_does_not_exist() {
        let config = default_config();
        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let home = PathBuf::from("/Users/jowi");
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };
        let mut out = Vec::new();

        let err = start(
            &ctx,
            Some(Path::new("/does/not/exist")),
            Path::new("/cwd"),
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, WorkCliError::DirectoryNotFound(_)));
    }

    // --- run (detached) / supervise ---

    use crate::github::gh_cli::FakeGhCli;
    use crate::runs::RunStatus;
    use crate::work::detach::{FakeDetachSpawner, SupervisorState};
    use crate::work::run::FakeClock;
    use crate::work::runner::FakeProcessSpawner;

    fn lane_config_for_run(repo: &str) -> LaneConfig {
        LaneConfig {
            repo: repo.to_string(),
            prompt_file: None,
            base_branch: None,
            model: None,
            max_turns: None,
            permission_mode: None,
        }
    }

    fn run_setup() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let prompt_dir = home.join(".claude/prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        let prompt_path = prompt_dir.join("mylane.md");
        std::fs::write(&prompt_path, "Do the lane thing.").unwrap();
        (tmp, home, repo_root, prompt_path)
    }

    fn canned_claude_json() -> String {
        r#"{"session_id":"sess-1","total_cost_usd":0.5,"num_turns":3,"is_error":false,"result":"done"}"#.to_string()
    }

    #[test]
    fn run_detached_spawns_a_supervisor_and_returns_without_running_claude() {
        let (tmp, home, repo_root, _prompt_path) = run_setup();
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "mylane".to_string(),
            lane_config_for_run(&repo_root.to_string_lossy()),
        );
        let config = WorkConfig {
            worktree_root: Some(tmp.path().join("Worktrees").to_string_lossy().into_owned()),
            ..config_with_lanes(lanes)
        };

        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };

        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_claude_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = RunDeps {
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            jira: None,
        };
        let mut out = Vec::new();

        let succeeded = run(
            &ctx,
            &deps,
            "mylane",
            RunLaneRequest::default(),
            false,
            &mut out,
        )
        .unwrap();

        assert!(succeeded);

        // The supervisor path never spawns `claude` itself — that's the
        // re-exec'd supervisor's job.
        assert!(spawner.recorded.lock().unwrap().is_empty());

        let recorded = detach.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].program, current_exe);
        assert_eq!(recorded[0].argv[0], "work");
        assert_eq!(recorded[0].argv[1], "__supervise");
        assert_eq!(recorded[0].argv[2], "--state-file");
        let state_path = PathBuf::from(&recorded[0].argv[3]);
        assert!(state_path.exists());

        // A run row was started (with no pid yet — the supervisor records
        // its own).
        let runs = run_store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        let run_row = run_store.run_by_id(runs[0].id).unwrap().unwrap();
        assert_eq!(run_row.pid, None);
        assert_eq!(run_row.status, RunStatus::Running);

        let printed = out_string(&out);
        assert!(printed.contains("started   mylane"));
        assert!(printed.contains("worktree "));
        assert!(printed.contains("log       "));
        assert!(printed.contains("watch:    tm runs watch"));
        assert!(printed.contains("follow:   tail -f"));

        // The written state file round-trips into a valid SupervisorState
        // pointing at the same run.
        let raw = std::fs::read_to_string(&state_path).unwrap();
        let state: SupervisorState = serde_json::from_str(&raw).unwrap();
        assert_eq!(state.prepared.run_id, run_row.id);
        assert_eq!(state.run_db_path, run_db_path);
    }

    #[test]
    fn run_detached_prints_resume_line_only_when_a_ticket_was_given() {
        let (tmp, home, repo_root, _prompt_path) = run_setup();
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "mylane".to_string(),
            lane_config_for_run(&repo_root.to_string_lossy()),
        );
        let config = WorkConfig {
            worktree_root: Some(tmp.path().join("Worktrees").to_string_lossy().into_owned()),
            ..config_with_lanes(lanes)
        };

        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };

        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_claude_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = RunDeps {
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            jira: None,
        };
        let mut out = Vec::new();

        let request = RunLaneRequest {
            ticket: Some("ABC-123".to_string()),
            ..Default::default()
        };

        run(&ctx, &deps, "mylane", request, false, &mut out).unwrap();

        let printed = out_string(&out);
        assert!(printed.contains("resume:   tm runs resume ABC-123"));
    }

    #[test]
    fn run_detached_errors_before_any_supervisor_spawn_when_prompt_file_missing() {
        let (tmp, home, repo_root, prompt_path) = run_setup();
        std::fs::remove_file(&prompt_path).unwrap();
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "mylane".to_string(),
            lane_config_for_run(&repo_root.to_string_lossy()),
        );
        let config = WorkConfig {
            worktree_root: Some(tmp.path().join("Worktrees").to_string_lossy().into_owned()),
            ..config_with_lanes(lanes)
        };

        let git = FakeGitOps::new();
        let tmux = FakeTmuxOps::new();
        let ctx = WorkContext {
            git: &git,
            tmux: &tmux,
            config: &config,
            home: &home,
        };

        let gh = FakeGhCli::new();
        let spawner = FakeProcessSpawner::success(canned_claude_json());
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let run_db_path = tmp.path().join("runs.db");
        let deps = RunDeps {
            gh: &gh,
            spawner: &spawner,
            run_store: &run_store,
            clock: &clock,
            detach: &detach,
            current_exe: &current_exe,
            run_db_path: &run_db_path,
            jira: None,
        };
        let mut out = Vec::new();

        let err = run(
            &ctx,
            &deps,
            "mylane",
            RunLaneRequest::default(),
            false,
            &mut out,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            WorkCliError::Run(RunLaneError::PromptFileMissing(_))
        ));
        assert!(detach.recorded.lock().unwrap().is_empty());
        assert_eq!(run_store.list_runs().unwrap().len(), 0);
    }

    #[test]
    fn supervise_records_its_own_pid_and_completes_the_run() {
        let (tmp, home, repo_root, _prompt_path) = run_setup();
        let mut lanes = BTreeMap::new();
        lanes.insert(
            "mylane".to_string(),
            lane_config_for_run(&repo_root.to_string_lossy()),
        );
        let config = WorkConfig {
            worktree_root: Some(tmp.path().join("Worktrees").to_string_lossy().into_owned()),
            ..config_with_lanes(lanes)
        };

        // Prepare a run row exactly the way the detached `run` path would,
        // with no pid yet.
        let git = FakeGitOps::new();
        let gh = FakeGhCli::new();
        let run_store = RunStore::open(&tmp.path().join("runs.db")).unwrap();
        let clock = FakeClock((2026, 8, 6, 9, 5, 3));
        let prepare_spawner = FakeProcessSpawner::success(canned_claude_json());
        let run_deps = crate::work::run::RunLaneDeps {
            git: &git,
            gh: &gh,
            spawner: &prepare_spawner,
            run_store: &run_store,
            clock: &clock,
            jira: None,
        };
        let paths = crate::work::run::RunLanePaths {
            home: home.clone(),
            state_dir: tmp.path().join("state"),
            hooks_deploy_dir: tmp.path().join("hooks"),
        };
        let mut prepare_out = Vec::new();
        let prepared = crate::work::run::prepare_run_lane(
            &run_deps,
            &config,
            &paths,
            "mylane",
            RunLaneRequest::default(),
            None,
            &mut prepare_out,
        )
        .unwrap();

        let state = SupervisorState {
            prepared,
            run_db_path: tmp.path().join("runs.db"),
        };

        let supervisor_spawner = FakeProcessSpawner::success(canned_claude_json());
        let mut out = Vec::new();

        let succeeded = supervise(&supervisor_spawner, &gh, &run_store, &state, &mut out).unwrap();

        assert!(succeeded);
        let run_row = run_store.run_by_id(state.prepared.run_id).unwrap().unwrap();
        assert_eq!(run_row.pid, Some(std::process::id()));
        assert_eq!(run_row.status, RunStatus::Done);

        let printed = out_string(&out);
        assert!(printed.contains("session   sess-1"));
    }
}
