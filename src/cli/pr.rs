//! `tm pr create`, `tm pr status`, and `tm pr watch`.

use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::github::bot_findings::count_bot_findings;
use crate::github::gh_cli::{GhError, PrCreateRequest};
use crate::github::pr::find_pr_for_ticket;
use crate::runs::{RunStatus, RunStore, RunStoreError, StartRun};
use crate::ticketing::{
    TicketingContext, TicketingError, associate_existing_ticket_for_pr_create,
    auto_create_and_associate, resolve_existing_key,
};
use crate::work::detach::{DetachError, DetachSpawner};
use crate::work::git::{GitError, GitOps};
use crate::work::review_watch::{self, CleanupLauncher, Clock, PollDeps, PollRequest, Sleeper};

/// Errors surfaced by `tm pr create` and `tm pr status`.
#[derive(Debug, Error)]
pub enum PrCliError {
    /// Neither `--title` nor an interactive prompt produced a non-empty
    /// title.
    #[error("PR title is required; pass --title or answer the prompt")]
    TitleRequired,

    /// The current branch has no open pull request.
    #[error("no pull request found for branch `{branch}`. Run `tm pr create` first.")]
    NoPr {
        /// The branch that has no open pull request.
        branch: String,
    },

    /// A `gh`/`git` shell-out failed.
    #[error(transparent)]
    Gh(#[from] GhError),

    /// Ticket association or creation failed.
    #[error(transparent)]
    Ticketing(#[from] TicketingError),

    /// A prompt or output write failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `tm pr watch <KEY>` found no open pull request resolving to `key` via
    /// [`find_pr_for_ticket`].
    #[error("no open pull request found for {key}. Run `tm pr create` first.")]
    NoPrForTicket {
        /// The ticket key that has no resolvable open pull request.
        key: String,
    },

    /// `tm pr watch <KEY>` found a `review-watch` run for `key` already
    /// `Running`.
    #[error("already watching {key} (run {run_id})")]
    AlreadyWatching {
        /// The ticket key already being watched.
        key: String,
        /// The id of the already-running watch run.
        run_id: i64,
    },

    /// A run-state store operation failed.
    #[error(transparent)]
    RunStore(#[from] RunStoreError),

    /// Spawning the detached watcher failed.
    #[error(transparent)]
    Detach(#[from] DetachError),

    /// A `git` shell-out failed while resolving the ticket's repo root (see
    /// [`resolve_watch_repo_root`]).
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Options for `tm pr create`, mirroring its CLI flags.
#[derive(Debug, Clone, Default)]
pub struct PrCreateOptions {
    /// Pull request title; prompted for interactively if `None`.
    pub title: Option<String>,
    /// Pull request body; empty if `None`.
    pub body: Option<String>,
    /// Base branch to open the PR against.
    pub base: Option<String>,
}

/// Options for `tm pr status`, mirroring its CLI flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrStatusOptions {
    /// Auto-create a ticket if none is associated, without prompting.
    pub auto_ticket: bool,
}

/// `tm pr create`: open a pull request for the current branch, then
/// associate a ticket with it (an existing key if the title/body already
/// carries one, otherwise a freshly created one), applying
/// [`crate::config::Config::status_on_pr`] to it either way — see
/// [`associate_existing_ticket_for_pr_create`] and
/// [`auto_create_and_associate`].
pub fn create(
    ctx: &TicketingContext,
    opts: &PrCreateOptions,
    prompter: &mut dyn super::Prompter,
    out: &mut dyn Write,
) -> Result<(), PrCliError> {
    let title = match &opts.title {
        Some(title) => title.clone(),
        None => prompter.prompt_line("PR title", "")?,
    };
    if title.trim().is_empty() {
        return Err(PrCliError::TitleRequired);
    }

    let req = PrCreateRequest {
        title,
        body: opts.body.clone().unwrap_or_default(),
        base: opts.base.clone(),
    };
    let pr = ctx.gh.pr_create(&req)?;
    writeln!(out, "Created PR #{}: {}", pr.number, pr.url)?;

    let outcome = match resolve_existing_key(ctx.jira, &pr)? {
        Some(key) => associate_existing_ticket_for_pr_create(ctx, &key)?,
        None => auto_create_and_associate(ctx, &pr)?,
    };
    writeln!(out, "Ticket {}: {}", outcome.issue_key, outcome.issue_url)?;
    super::print_status_transition(&outcome.issue_key, &outcome.status_transition, out)?;

    Ok(())
}

/// `tm pr status`: report the pull request open for the current branch,
/// which ticket (if any) is associated with it, and a summary of any bot
/// review findings (see [`crate::github::bot_findings::count_bot_findings`])
/// on the pull request.
///
/// The bot-findings check is best-effort: if
/// [`crate::github::gh_cli::GhCli::pr_review_threads`] fails, a warning line
/// is printed and the rest of `tm pr status` (ticket resolution, prompts)
/// still runs, returning `Ok` on the happy path as usual.
pub fn status(
    ctx: &TicketingContext,
    opts: &PrStatusOptions,
    prompter: &mut dyn super::Prompter,
    out: &mut dyn Write,
) -> Result<(), PrCliError> {
    let pr = match ctx.gh.pr_view()? {
        Some(pr) => pr,
        None => {
            let branch = ctx.gh.current_branch()?;
            writeln!(
                out,
                "no pull request found for branch `{branch}`. Run `tm pr create` first."
            )?;
            return Err(PrCliError::NoPr { branch });
        }
    };

    writeln!(out, "PR #{}: {}", pr.number, pr.url)?;
    writeln!(out, "Title: {}", pr.title)?;

    // `tm pr status` is run from inside the repo, same as `pr_view`/
    // `current_branch` above (both ambient-cwd trait methods); `dir` here is
    // just that same ambient cwd made explicit for `pr_review_threads`,
    // which (unlike those two) also needs to resolve owner/repo for a REST
    // path (see `GhCli::pr_review_threads`'s doc comment).
    let cwd = std::env::current_dir()?;
    match ctx.gh.pr_review_threads(&cwd, pr.number) {
        Ok(threads) => {
            let counts = count_bot_findings(&threads, &ctx.config.review_bots);
            if counts.total > 0 {
                writeln!(
                    out,
                    "Bot findings: {} unresolved (of {})",
                    counts.unresolved, counts.total
                )?;
            } else {
                writeln!(out, "Bot findings: none")?;
            }
        }
        Err(err) => {
            writeln!(out, "warning: could not check bot findings: {err}")?;
        }
    }

    match resolve_existing_key(ctx.jira, &pr)? {
        Some(key) => {
            let issue_url = format!("{}/browse/{key}", ctx.config.jira_base_url);
            writeln!(out, "Ticket {key}: {issue_url}")?;
        }
        None => {
            writeln!(out, "no ticket associated")?;
            let should_create = if opts.auto_ticket {
                true
            } else {
                prompter.confirm(&format!(
                    "Create a ticket in {}?",
                    ctx.config.default_project_key
                ))?
            };
            if should_create {
                let outcome = auto_create_and_associate(ctx, &pr)?;
                writeln!(
                    out,
                    "Created ticket {}: {}",
                    outcome.issue_key, outcome.issue_url
                )?;
                super::print_status_transition(
                    &outcome.issue_key,
                    &outcome.status_transition,
                    out,
                )?;
            }
        }
    }

    Ok(())
}

/// Dependencies `tm pr watch` needs beyond [`TicketingContext`] (`jira`
/// isn't used by this command at all, but the same context is threaded
/// through for consistency with `create`/`status`): the run-state store, the
/// detached-spawn seam, and the poll loop's own seams
/// ([`Clock`]/[`Sleeper`]/[`CleanupLauncher`]).
pub struct PrWatchDeps<'a> {
    /// The run-state store `start_run`/dedup-lookup is called against.
    pub run_store: &'a RunStore,
    /// Detached-child process spawning (real or fake). Only used without
    /// `--foreground`.
    pub detach: &'a dyn DetachSpawner,
    /// This process's own executable path, re-exec'd as the detached
    /// `--foreground` child. Only used without `--foreground`.
    pub current_exe: &'a Path,
    /// "Now" source for the poll loop's give-up timeout. Only used with
    /// `--foreground`.
    pub clock: &'a dyn Clock,
    /// Sleep between poll ticks (real or fake). Only used with
    /// `--foreground`.
    pub sleeper: &'a dyn Sleeper,
    /// Cleanup-session launch seam. Only used with `--foreground`.
    pub cleanup_launcher: &'a dyn CleanupLauncher,
    /// The invoking user's home directory, for the detached child's log
    /// file location and the findings-file path fallback.
    pub home: &'a Path,
    /// `$XDG_DATA_HOME`, if set, for the findings-file path.
    pub xdg_data_home: Option<&'a Path>,
    /// Git operations (real or fake), used by [`resolve_watch_repo_root`]'s
    /// cwd-repo-root fallback when `key` has no known lane run.
    pub git: &'a dyn GitOps,
    /// This process's own cwd at invocation time, used by
    /// [`resolve_watch_repo_root`]'s fallback — distinct from `home`, which
    /// is never the right answer for a repo-scoped `gh` call.
    pub cwd: &'a Path,
}

/// The outcome of `tm pr watch`, mapped by the CLI layer to exit codes
/// `0`/`0`/`1`/`2` respectively (see `docs/plans/bugbot-watch.md`'s "CLI
/// surface").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    /// Re-exec'd a detached `--foreground` child and returned without
    /// waiting for it.
    Detached,
    /// The foreground poll loop reached a terminal, handled state (PR
    /// closed/merged, or bots finished).
    Handled,
    /// The foreground poll loop gave up after repeated `gh` failures.
    Failed,
    /// The foreground poll loop gave up after `max_wait_mins` elapsed.
    GaveUp,
}

/// Resolve the repository `tm pr watch <key>` should run every `gh` shell-out
/// against — the same "lane config repo, falling back to cwd's repo root"
/// shape [`crate::cli::work::resolve_repo_root`] already uses for `tm work`,
/// applied here because `tm pr watch` has no lane argument of its own.
///
/// Prefers the `repo` of the most recent `kind: "lane"` run for `key` (see
/// [`RunStore::latest_run_for_ticket_kind`]) — the ticket's lane, if it's
/// ever been run, is a far more reliable source of truth than whatever
/// directory this process happens to be standing in, especially since this
/// is called both from the board (ambient cwd: wherever `tm board` was
/// launched from) and from the re-exec'd `--foreground` detached child,
/// whose own cwd is whatever [`watch`] resolved for the *parent* invocation
/// and passed as [`crate::work::detach::DetachSpawner::spawn_detached`]'s
/// `working_dir` — see that call site's comment.
///
/// Falls back to `cwd`'s git repo root when `key` has no `lane`-kind run, or
/// its lane is no longer configured (docs/plans/bugbot-watch.md's ground
/// truth: "the watcher... has no reason to run from the ticket's worktree —
/// the ticket may not even have an active lane run"). This fallback is only
/// correct when the caller's `cwd` really is the ticket's repo (e.g. a
/// developer running `tm pr watch KEY` by hand from inside it); it cannot be
/// correct for a lane-less ticket watched from the board or spawned
/// elsewhere, which is a known gap, not engineered around here.
fn resolve_watch_repo_root(
    lanes: &std::collections::BTreeMap<String, crate::config::LaneConfig>,
    run_store: &RunStore,
    git: &dyn GitOps,
    cwd: &Path,
    key: &str,
) -> Result<PathBuf, PrCliError> {
    if let Some(run) = run_store.latest_run_for_ticket_kind(key, Some("lane"))?
        && let Some(lane) = lanes.get(&run.lane)
    {
        return Ok(PathBuf::from(&lane.repo));
    }
    Ok(git.repo_root(cwd)?)
}

/// Directory the detached watcher's log file lives in:
/// `<home>/.local/state/tskmstr/review-watch`.
pub(crate) fn watch_log_dir(home: &Path) -> PathBuf {
    home.join(".local")
        .join("state")
        .join("tskmstr")
        .join("review-watch")
}

/// `tm pr watch <KEY> [--foreground]`: resolve `KEY`'s open pull request,
/// refuse to double-watch, then either re-exec a detached `--foreground`
/// child or run the poll loop synchronously.
///
/// **Dedup race, accepted, not engineered around**: this same dedup check
/// (`store.latest_run_for_ticket_kind(key, "review-watch")` still
/// `Running`) runs every time this function is called — once in the
/// un-detached parent before it spawns the child, and again in the re-exec'd
/// `--foreground` child itself, since both invocations go through this one
/// function. The parent's `start_run` doesn't happen until the
/// `--foreground` branch runs (in whichever process reaches it first: the
/// detached child, or a direct `--foreground` caller), so there is a
/// sub-second window between the parent process exiting and the child's own
/// `start_run` call during which a second `tm pr watch KEY` invocation would
/// see nothing running yet and also proceed. This is the identical
/// check-then-act race `docs/plans/board-audits.md`'s `launch_audit` already
/// accepts for `tmux.has_session` (single board process; two independent
/// CLI invocations racing is an operator error, not designed against) — not
/// engineered around here either.
///
/// **Detach mechanics are not unit-tested**: the actual re-exec/`setsid`
/// path (`--foreground: false`) is exercised here only insofar as
/// [`DetachSpawner::spawn_detached`] is called with the expected argv/log
/// path; the real spawn-and-detach mechanics are `RealDetachSpawner`'s job
/// and, per `src/work/detach.rs`'s own doc comment and stream 5's
/// precedent, are manually verified rather than unit-tested (they spawn a
/// real detached process outliving the test's own process tree).
pub fn watch(
    ctx: &TicketingContext,
    deps: &PrWatchDeps<'_>,
    key: &str,
    foreground: bool,
    out: &mut dyn Write,
) -> Result<WatchOutcome, PrCliError> {
    let repo_root = resolve_watch_repo_root(
        &ctx.config.work.lanes,
        deps.run_store,
        deps.git,
        deps.cwd,
        key,
    )?;

    let prs = ctx.gh.pr_list(&repo_root)?;
    let pr = find_pr_for_ticket(&prs, key).ok_or_else(|| PrCliError::NoPrForTicket {
        key: key.to_string(),
    })?;
    let pr_number = pr.number;

    if let Some(existing) = deps
        .run_store
        .latest_run_for_ticket_kind(key, Some("review-watch"))?
        && existing.status == RunStatus::Running
    {
        return Err(PrCliError::AlreadyWatching {
            key: key.to_string(),
            run_id: existing.id,
        });
    }

    if !foreground {
        let log_dir = watch_log_dir(deps.home);
        std::fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join(format!("{}.log", key.to_lowercase()));
        let argv = vec![
            "pr".to_string(),
            "watch".to_string(),
            key.to_string(),
            "--foreground".to_string(),
        ];
        // The detached `--foreground` child's cwd is `repo_root`, not
        // `deps.home`: it re-derives its own `repo_root` via
        // `resolve_watch_repo_root` the same way this invocation just did,
        // and that function's cwd-fallback branch only produces a correct
        // answer if the process's cwd is already inside the target repo.
        // Spawning into `deps.home` (as a plain "somewhere stable to run
        // from" choice) would make that fallback branch resolve nothing
        // useful whenever `key` has no known lane run.
        deps.detach
            .spawn_detached(deps.current_exe, &argv, &repo_root, &log_path)?;
        writeln!(
            out,
            "watching {key} (detached; log: {})",
            log_path.display()
        )?;
        return Ok(WatchOutcome::Detached);
    }

    // Recomputed rather than threaded through from the `!foreground` branch
    // above: that branch runs in a *different process* than this one when
    // detached (the re-exec'd `--foreground` child never sees the parent's
    // locals), and a direct `tm pr watch KEY --foreground` invocation never
    // runs that branch at all. `watch_log_dir`/the filename convention are
    // the single source of truth both branches derive from.
    let log_path = watch_log_dir(deps.home).join(format!("{}.log", key.to_lowercase()));

    let run_id = deps.run_store.start_run(&StartRun {
        ticket: key.to_string(),
        lane: "review-watch".to_string(),
        worktree: repo_root.to_string_lossy().into_owned(),
        branch: None,
        pid: Some(std::process::id()),
        kind: "review-watch".to_string(),
        log_path: Some(log_path.to_string_lossy().into_owned()),
    })?;

    let started_at_unix = deps.clock.now_unix_secs();
    let poll_deps = PollDeps {
        gh: ctx.gh,
        store: deps.run_store,
        clock: deps.clock,
        sleeper: deps.sleeper,
        cleanup_launcher: deps.cleanup_launcher,
    };
    let poll_req = PollRequest {
        run_id,
        ticket: key,
        pr_number,
        repo_root: &repo_root,
        bot_logins: &ctx.config.review_bots,
        config: &ctx.config.work.review_watch,
        started_at_unix,
        home: deps.home,
        xdg_data_home: deps.xdg_data_home,
    };

    Ok(
        match review_watch::run_poll_loop(&poll_deps, &poll_req, out) {
            review_watch::PollOutcome::Handled => WatchOutcome::Handled,
            review_watch::PollOutcome::Failed => WatchOutcome::Failed,
            review_watch::PollOutcome::GaveUp => WatchOutcome::GaveUp,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakePrompter;
    use crate::config::Config;
    use crate::github::bot_findings::ReviewThread;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::github::pr::PrInfo;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::{Issue, IssueFields, Status, StatusCategory, Transition};

    fn issue(key: &str) -> Issue {
        Issue {
            key: key.to_string(),
            fields: IssueFields {
                summary: "Fix the thing".to_string(),
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

    fn pr_with_title(title: &str) -> PrInfo {
        PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            body: String::new(),
            head_ref_name: "some-branch".to_string(),
        }
    }

    fn config() -> Config {
        Config {
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            default_assignee_account_id: Some("acct-1".to_string()),
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: vec!["cursor[bot]".to_string()],
            board_column_order: Vec::new(),
            work: crate::config::WorkConfig::default(),
        }
    }

    #[test]
    fn create_with_flag_title_auto_creates_ticket_when_none_associated() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_create_result(Ok(pr_with_title("Add the widget")));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("Add the widget".to_string()),
            body: Some("Details".to_string()),
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Created PR #42: https://github.com/example/repo/pull/42"));
        assert!(output.contains("Ticket PROJ-9: https://example.atlassian.net/browse/PROJ-9"));
        assert_eq!(jira.create_issue_calls().len(), 1);
    }

    #[test]
    fn create_prints_moved_line_when_status_on_pr_transition_applies() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Send to review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new().with_pr_create_result(Ok(pr_with_title("Add the widget")));
        let cfg = Config {
            status_on_pr: Some("In Review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Moved PROJ-9 to In Review"));
    }

    #[test]
    fn create_prints_warning_line_when_no_matching_transition() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_create_result(Ok(pr_with_title("Add the widget")));
        let cfg = Config {
            status_on_pr: Some("In Review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning:"));
    }

    #[test]
    fn create_prints_nothing_extra_when_status_on_pr_unset() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_create_result(Ok(pr_with_title("Add the widget")));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("Add the widget".to_string()),
            body: None,
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Moved"));
        assert!(!output.contains("warning:"));
    }

    #[test]
    fn status_auto_create_prints_moved_line_when_transition_applies() {
        let jira = FakeJiraClient::new()
            .with_create_issue_result(issue("PROJ-9"))
            .with_transitions(
                "PROJ-9",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Send to review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("Fix the thing"))));
        let cfg = Config {
            status_on_pr: Some("In Review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions { auto_ticket: true };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Moved PROJ-9 to In Review"));
    }

    #[test]
    fn create_missing_title_prompts_and_fails_if_still_empty() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new();
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions::default();
        let mut prompter = FakePrompter::new().with_line("");
        let mut out = Vec::new();

        let err = create(&ctx, &opts, &mut prompter, &mut out).expect_err("should fail");
        assert!(matches!(err, PrCliError::TitleRequired));
    }

    #[test]
    fn status_with_no_pr_reports_message_and_fails() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(None))
            .with_current_branch(Ok("some-branch".to_string()));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        let err = status(&ctx, &opts, &mut prompter, &mut out).expect_err("should fail");
        match err {
            PrCliError::NoPr { branch } => assert_eq!(branch, "some-branch"),
            other => panic!("expected NoPr, got {other:?}"),
        }
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("no pull request found for branch `some-branch`"));
    }

    #[test]
    fn create_with_existing_key_applies_status_on_pr_transition() {
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", issue("PROJ-372"))
            .with_transitions(
                "PROJ-372",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Send to review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new()
            .with_pr_create_result(Ok(pr_with_title("[PROJ-372] Fix the thing")))
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))));
        let cfg = Config {
            status_on_pr: Some("In Review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("[PROJ-372] Fix the thing".to_string()),
            body: None,
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Moved PROJ-372 to In Review"));
        assert_eq!(
            jira.transition_calls(),
            vec![("PROJ-372".to_string(), "21".to_string())]
        );
    }

    #[test]
    fn create_with_existing_key_already_in_target_status_skips_transition() {
        let mut already_in_review = issue("PROJ-372");
        already_in_review.fields.status.name = "In Review".to_string();
        let jira = FakeJiraClient::new()
            .with_issue("PROJ-372", already_in_review)
            .with_transitions(
                "PROJ-372",
                vec![Transition {
                    id: "21".to_string(),
                    name: "Send to review".to_string(),
                    to: Status {
                        name: "In Review".to_string(),
                        status_category: StatusCategory {
                            key: "indeterminate".to_string(),
                        },
                    },
                }],
            );
        let gh = FakeGhCli::new()
            .with_pr_create_result(Ok(pr_with_title("[PROJ-372] Fix the thing")))
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))));
        let cfg = Config {
            status_on_pr: Some("in review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrCreateOptions {
            title: Some("[PROJ-372] Fix the thing".to_string()),
            body: None,
            base: None,
        };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        create(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(
            !output.contains("Moved"),
            "should not print a Moved line when already in the target status: {output}"
        );
        assert!(!output.contains("warning:"));
        assert!(
            jira.transition_calls().is_empty(),
            "should not call transition when the issue is already in the target status"
        );
    }

    #[test]
    fn status_with_existing_key_does_not_call_jira_create() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Ticket PROJ-372: https://example.atlassian.net/browse/PROJ-372"));
        assert_eq!(jira.create_issue_calls().len(), 0);
    }

    #[test]
    fn status_with_existing_key_never_transitions_even_when_status_on_pr_configured() {
        let jira = FakeJiraClient::new().with_transitions(
            "PROJ-372",
            vec![Transition {
                id: "21".to_string(),
                name: "Send to review".to_string(),
                to: Status {
                    name: "In Review".to_string(),
                    status_category: StatusCategory {
                        key: "indeterminate".to_string(),
                    },
                },
            }],
        );
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))));
        let cfg = Config {
            status_on_pr: Some("In Review".to_string()),
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert!(
            jira.transition_calls().is_empty(),
            "tm pr status must never transition an already-associated ticket"
        );
    }

    #[test]
    fn status_with_auto_ticket_flag_creates_ticket_without_prompting() {
        let jira = FakeJiraClient::new().with_create_issue_result(issue("PROJ-9"));
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions { auto_ticket: true };
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(jira.create_issue_calls().len(), 1);
        assert!(
            prompter.messages.is_empty(),
            "should not prompt when --auto-ticket is set"
        );
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Created ticket PROJ-9"));
    }

    #[test]
    fn status_prompt_declined_does_not_create_ticket() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_view(Ok(Some(pr_with_title("Fix the thing"))));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new().with_confirm(false);
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        assert_eq!(jira.create_issue_calls().len(), 0);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("no ticket associated"));
        assert!(!output.contains("Created ticket"));
    }

    fn review_thread(is_resolved: bool, author_login: &str) -> ReviewThread {
        ReviewThread {
            is_resolved,
            author_login: Some(author_login.to_string()),
        }
    }

    #[test]
    fn status_prints_bot_findings_counts_for_mixed_threads() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))))
            .with_review_threads(
                42,
                Ok(vec![
                    review_thread(true, "cursor"),
                    review_thread(false, "cursor"),
                    review_thread(false, "some-human"),
                ]),
            );
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Bot findings: 1 unresolved (of 2)"));
    }

    #[test]
    fn status_prints_bot_findings_none_when_no_bot_threads() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))))
            .with_review_threads(42, Ok(vec![review_thread(false, "some-human")]));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Bot findings: none"));
    }

    #[test]
    fn status_prints_warning_and_continues_when_review_threads_fails() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))))
            .with_review_threads(
                42,
                Err(GhError::Command {
                    command: "gh api graphql".to_string(),
                    exit_code: Some(1),
                    stderr: "boom".to_string(),
                }),
            );
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should still succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("warning: could not check bot findings:"));
        assert!(output.contains("Ticket PROJ-372: https://example.atlassian.net/browse/PROJ-372"));
    }

    #[test]
    fn status_respects_custom_review_bots_config() {
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_view(Ok(Some(pr_with_title("[PROJ-372] Fix the thing"))))
            .with_review_threads(42, Ok(vec![review_thread(false, "my-custom-bot")]));
        let cfg = Config {
            review_bots: vec!["my-custom-bot".to_string()],
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let opts = PrStatusOptions::default();
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();

        status(&ctx, &opts, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Bot findings: 1 unresolved (of 1)"));
    }

    // --- watch: resolution / dedup / detach / foreground outcome mapping ---

    use crate::github::gh_cli::PrLifecycle;
    use crate::work::detach::FakeDetachSpawner;
    use crate::work::git::FakeGitOps;
    use crate::work::review_watch::{FakeCleanupLauncher, FakeClock, FakeSleeper};
    use tempfile::tempdir;

    fn open_run_store(dir: &Path) -> RunStore {
        RunStore::open(&dir.join("runs.db")).expect("open should succeed")
    }

    #[allow(clippy::too_many_arguments)]
    fn watch_deps<'a>(
        run_store: &'a RunStore,
        detach: &'a FakeDetachSpawner,
        current_exe: &'a Path,
        clock: &'a FakeClock,
        sleeper: &'a FakeSleeper,
        cleanup: &'a FakeCleanupLauncher,
        home: &'a Path,
        git: &'a FakeGitOps,
        cwd: &'a Path,
    ) -> PrWatchDeps<'a> {
        PrWatchDeps {
            run_store,
            detach,
            current_exe,
            clock,
            sleeper,
            cleanup_launcher: cleanup,
            home,
            xdg_data_home: None,
            git,
            cwd,
        }
    }

    #[test]
    fn watch_errors_when_no_open_pr_resolves_to_the_ticket() {
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![]));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home_dir = tempdir().unwrap();
        let home = home_dir.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let err = watch(&ctx, &deps, "PROJ-1", false, &mut out).expect_err("should fail");
        match err {
            PrCliError::NoPrForTicket { key } => assert_eq!(key, "PROJ-1"),
            other => panic!("expected NoPrForTicket, got {other:?}"),
        }
        assert!(detach.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn watch_errors_when_already_watching() {
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let existing_id = run_store
            .start_run(&StartRun {
                ticket: "PROJ-372".to_string(),
                lane: "review-watch".to_string(),
                worktree: "/irrelevant".to_string(),
                branch: None,
                pid: Some(1),
                kind: "review-watch".to_string(),
                log_path: None,
            })
            .unwrap();

        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_with_title("[PROJ-372] Fix the thing")]));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home_dir = tempdir().unwrap();
        let home = home_dir.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let err = watch(&ctx, &deps, "PROJ-372", false, &mut out).expect_err("should fail");
        match err {
            PrCliError::AlreadyWatching { key, run_id } => {
                assert_eq!(key, "PROJ-372");
                assert_eq!(run_id, existing_id);
            }
            other => panic!("expected AlreadyWatching, got {other:?}"),
        }
        assert!(detach.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn watch_resolves_pr_list_against_the_ticket_lane_repo_not_the_process_cwd() {
        // Regression test for the reported bug: `tm pr watch` used to call
        // `gh.pr_list()` with no explicit dir, so it listed PRs for whatever
        // repository the ambient process cwd happened to be — never the
        // ticket's actual repo when launched from the board or the detached
        // `--foreground` child. `resolve_watch_repo_root` must find AX-408's
        // lane run and use its lane's configured repo, ignoring both `cwd`
        // and `git.repo_root`'s fallback answer entirely.
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        run_store
            .start_run(&StartRun {
                ticket: "AX-408".to_string(),
                lane: "axiom".to_string(),
                worktree: "/worktrees/axiom-ax-408".to_string(),
                branch: Some("jowi-dev/ax-408".to_string()),
                pid: None,
                kind: "lane".to_string(),
                log_path: None,
            })
            .unwrap();

        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_with_title("[AX-408] Fix the thing")]));
        let mut lanes = std::collections::BTreeMap::new();
        lanes.insert(
            "axiom".to_string(),
            crate::config::LaneConfig {
                repo: "/repos/axiom".to_string(),
                prompt_file: None,
                base_branch: None,
                model: None,
                max_turns: None,
                permission_mode: None,
            },
        );
        let cfg = Config {
            work: crate::config::WorkConfig {
                lanes,
                ..crate::config::WorkConfig::default()
            },
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home_dir = tempdir().unwrap();
        let home = home_dir.path().to_path_buf();
        // The board's own cwd and the git fallback's answer are both
        // deliberately something *other* than the lane's repo, so this
        // test fails if the lane-run lookup is skipped in favor of either.
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/wrong-cwd-repo")));
        let cwd = PathBuf::from("/home/jowi/wherever-the-board-was-launched-from");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        watch(&ctx, &deps, "AX-408", false, &mut out).expect("should resolve and detach");

        assert_eq!(
            gh.pr_list_calls(),
            vec![PathBuf::from("/repos/axiom")],
            "gh pr list must be shelled out against the ticket's lane repo, not cwd"
        );
        let recorded = detach.recorded.lock().unwrap();
        assert_eq!(recorded[0].working_dir, PathBuf::from("/repos/axiom"));
    }

    #[test]
    fn watch_falls_back_to_git_repo_root_of_cwd_when_ticket_has_no_lane_run() {
        // Ground truth from docs/plans/bugbot-watch.md: a watched ticket may
        // have no active lane run at all. In that case the only reasonable
        // source of truth left is the invoking process's own cwd.
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_with_title("[PROJ-9] Fix it")]));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home_dir = tempdir().unwrap();
        let home = home_dir.path().to_path_buf();
        let git = FakeGitOps::new().with_repo_root(Ok(PathBuf::from("/repos/fallback")));
        let cwd = PathBuf::from("/repos/fallback/subdir");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        watch(&ctx, &deps, "PROJ-9", false, &mut out).expect("should resolve and detach");

        assert_eq!(gh.pr_list_calls(), vec![PathBuf::from("/repos/fallback")]);
    }

    #[test]
    fn watch_without_foreground_spawns_a_detached_reexec_and_creates_no_run() {
        let tmp = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_with_title("[PROJ-372] Fix the thing")]));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home = tmp.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let outcome = watch(&ctx, &deps, "PROJ-372", false, &mut out).unwrap();

        assert_eq!(outcome, WatchOutcome::Detached);
        assert!(
            run_store.list_runs().unwrap().is_empty(),
            "the parent must not start_run"
        );

        let recorded = detach.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].program, current_exe);
        assert_eq!(
            recorded[0].argv,
            vec![
                "pr".to_string(),
                "watch".to_string(),
                "PROJ-372".to_string(),
                "--foreground".to_string(),
            ]
        );
        // The detached child's cwd is the resolved repo root (here, the
        // `FakeGitOps` fallback repo root, since no lane run exists for
        // PROJ-372), never `home` — see `resolve_watch_repo_root`'s doc
        // comment on why `home` would leave the fallback branch unable to
        // resolve anything.
        assert_eq!(recorded[0].working_dir, PathBuf::from("/repo"));

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("watching PROJ-372"));
    }

    #[test]
    fn watch_foreground_merged_pr_starts_a_run_and_returns_handled() {
        let tmp = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_with_title("[PROJ-372] Fix the thing")]))
            .with_pr_state(42, Ok(PrLifecycle::Merged));
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home = tmp.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let outcome = watch(&ctx, &deps, "PROJ-372", true, &mut out).unwrap();

        assert_eq!(outcome, WatchOutcome::Handled);
        assert!(detach.recorded.lock().unwrap().is_empty());
        let runs = run_store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].kind, "review-watch");
        assert_eq!(runs[0].status, RunStatus::Done);
        let run = run_store.run_by_id(runs[0].id).unwrap().unwrap();
        let expected_log_path = watch_log_dir(&home).join("proj-372.log");
        assert_eq!(
            run.log_path.as_deref(),
            Some(expected_log_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn watch_foreground_gh_failure_backoff_returns_failed() {
        let tmp = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new()
            .with_pr_list(Ok(vec![pr_with_title("[PROJ-372] Fix the thing")]))
            .with_pr_state(
                42,
                Err(GhError::Command {
                    command: "gh pr view".to_string(),
                    exit_code: Some(1),
                    stderr: "boom".to_string(),
                }),
            );
        let cfg = config();
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        let clock = FakeClock::at(0);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home = tmp.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let outcome = watch(&ctx, &deps, "PROJ-372", true, &mut out).unwrap();

        assert_eq!(outcome, WatchOutcome::Failed);
        assert_eq!(sleeper.calls().len(), 9);
    }

    #[test]
    fn watch_foreground_wall_clock_timeout_returns_gave_up() {
        let tmp = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let run_store = open_run_store(db_dir.path());
        let jira = FakeJiraClient::new();
        let gh = FakeGhCli::new().with_pr_list(Ok(vec![pr_with_title("[PROJ-372] Fix the thing")]));
        let cfg = Config {
            work: crate::config::WorkConfig {
                review_watch: crate::config::ReviewWatchConfig {
                    max_wait_mins: 10,
                    ..crate::config::ReviewWatchConfig::default()
                },
                ..crate::config::WorkConfig::default()
            },
            ..config()
        };
        let ctx = TicketingContext {
            jira: &jira,
            gh: &gh,
            config: &cfg,
        };
        let detach = FakeDetachSpawner::new(9999);
        let current_exe = PathBuf::from("/usr/local/bin/tm");
        // First call (capturing `started_at_unix`) reports 0; every call
        // after that (the loop's own elapsed-time check) reports well past
        // the 10-minute deadline.
        let clock = FakeClock::advancing(0, 10 * 60 + 2);
        let sleeper = FakeSleeper::default();
        let cleanup = FakeCleanupLauncher::default();
        let home = tmp.path().to_path_buf();
        let git = FakeGitOps::new();
        let cwd = PathBuf::from("/repo");
        let deps = watch_deps(
            &run_store,
            &detach,
            &current_exe,
            &clock,
            &sleeper,
            &cleanup,
            &home,
            &git,
            &cwd,
        );
        let mut out = Vec::new();

        let outcome = watch(&ctx, &deps, "PROJ-372", true, &mut out).unwrap();

        assert_eq!(outcome, WatchOutcome::GaveUp);
        assert!(
            gh.pr_state_calls().is_empty(),
            "must give up before calling gh"
        );
    }
}
