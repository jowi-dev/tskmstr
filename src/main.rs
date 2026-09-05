//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::agent::AgentRunner;
use tskmstr::agent::claude::ClaudeRunner;
use tskmstr::cli::work::Dispatch;
use tskmstr::cli::{
    AuthCmd, BackendCmd, Cli, Command, PrCmd, RealPrompter, ReviewCmd, RunsCmd, TicketCmd, WorkCmd,
};
use tskmstr::config::{self, AgentKind, BackendKind, Config, ConfigPaths};
use tskmstr::github::gh_cli::{GhCli, ShellGhCli};
use tskmstr::jira::client::{HttpJiraClient, JiraClientContext};
use tskmstr::keychain::{KeychainStore, MacosKeychain, resolve_token};
use tskmstr::ticketing::github_provider::GithubProvider;
use tskmstr::ticketing::provider::{JiraProvider, TicketProvider};
use tskmstr::ticketing::{CreateTicketContext, TicketingContext};
use tskmstr::tui::event::{TuiDeps, run};
use tskmstr::work::git::ShellGitOps;
use tskmstr::work::tmux::ShellTmuxOps;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Board);

    // `tm pr watch` is special-cased ahead of `dispatch`/`Command`'s usual
    // `Result<(), _>` plumbing because it alone needs a three-way exit code
    // (0 handled/detached, 1 failed, 2 gave up) rather than the uniform
    // 0/1 every other command produces — see
    // `docs/plans/bugbot-watch.md`'s "CLI surface".
    if let Command::Pr {
        cmd: PrCmd::Watch { key, foreground },
    } = command
    {
        return run_pr_watch(key, foreground);
    }

    // `tm ready <KEY>` is special-cased the same way, ahead of `dispatch`'s
    // uniform `Result<(), _>` plumbing, because it now needs a three-way
    // exit code (0 ready, 3 stackable, 1 blocked/error) rather than the
    // uniform 0/1 every other command produces — see
    // `READY_EXIT_STACKABLE`'s doc comment. `tm ready` with no
    // key (the list form) never errors and stays on the uniform 0/1 path
    // via `dispatch`/`run_ready`.
    if let Command::Ready { key: Some(key) } = command {
        return run_ready_check(key);
    }

    // `tm review fix <KEY>` is special-cased the same way: it needs a
    // three-way exit code (0 dispatched, 1 error or a failed `--fg` run, 3
    // no comments captured) rather than dispatch's uniform 0/1 — see
    // `REVIEW_FIX_EXIT_NO_COMMENTS`'s doc comment. `ReviewCmd::Fix` is
    // `tm review`'s only subcommand today, so every `Command::Review` ends
    // up here.
    if let Command::Review {
        cmd: ReviewCmd::Fix { key, headless, fg },
    } = command
    {
        return run_review_fix(key, Dispatch::from_flags(headless, fg));
    }

    match dispatch(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// `tm pr watch <KEY> [--foreground]`: build real dependencies and run
/// [`tskmstr::cli::pr::watch`], mapping its [`tskmstr::cli::pr::WatchOutcome`]
/// to an exit code (`0` detached/handled, `1` failed, `2` gave up).
fn run_pr_watch(key: String, foreground: bool) -> ExitCode {
    let paths = default_config_paths();
    let env_token = std::env::var("JIRA_API_TOKEN").ok();
    let keychain = MacosKeychain::new();

    let (config, jira, gh) = match build_ticketing_deps(&paths, &keychain, env_token) {
        Ok(deps) => deps,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let ctx = TicketingContext {
        jira: jira.as_ref(),
        gh: &gh,
        config: &config,
    };

    let run_db_path = run_db_path_from_config(&config);
    let run_store = match tskmstr::runs::RunStore::open(&run_db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);

    let detach = tskmstr::work::detach::RealDetachSpawner;
    let clock = tskmstr::work::review_watch::SystemClock;
    let sleeper = tskmstr::work::review_watch::RealSleeper;
    let tmux = ShellTmuxOps::new();
    let git = ShellGitOps::new();
    let backend_identity = tskmstr::config::BackendIdentity::from_config(&config);
    let cleanup_launcher = tskmstr::work::bugbot::RealCleanupLauncher {
        store: &run_store,
        tmux: &tmux,
        cfg: &config.work.review_watch,
        home: &home,
        xdg_data_home: xdg_data_home.as_deref(),
        identity: &backend_identity,
        runner: agent_runner_for(&config),
    };
    let deps = tskmstr::cli::pr::PrWatchDeps {
        run_store: &run_store,
        detach: &detach,
        current_exe: &current_exe,
        clock: &clock,
        sleeper: &sleeper,
        cleanup_launcher: &cleanup_launcher,
        home: &home,
        xdg_data_home: xdg_data_home.as_deref(),
        git: &git,
        cwd: &cwd,
    };
    let mut stdout = std::io::stdout();

    match tskmstr::cli::pr::watch(&ctx, &deps, &key, foreground, &mut stdout) {
        Ok(tskmstr::cli::pr::WatchOutcome::Detached | tskmstr::cli::pr::WatchOutcome::Handled) => {
            ExitCode::SUCCESS
        }
        Ok(tskmstr::cli::pr::WatchOutcome::Failed) => ExitCode::FAILURE,
        Ok(tskmstr::cli::pr::WatchOutcome::GaveUp) => ExitCode::from(2),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Build real dependencies for `command` and run it.
fn dispatch(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let paths = default_config_paths();
    let env_token = std::env::var("JIRA_API_TOKEN").ok();
    let keychain = MacosKeychain::new();

    match command {
        Command::Auth { cmd } => run_auth(cmd, &paths, &keychain, env_token),
        Command::Ticket { key, cmd } => run_ticket(key, cmd, &paths, &keychain, env_token),
        Command::Pr { cmd } => run_pr(cmd, &paths, &keychain, env_token),
        Command::Ready { key } => run_ready(key, &paths, &keychain, env_token),
        Command::Board => run_board(&paths, &keychain, env_token),
        Command::Runs {
            kind,
            by_outcome,
            by_retro,
            cmd,
        } => run_runs(kind, by_outcome, by_retro, cmd),
        Command::Work { cmd } => run_work(cmd, &paths, &keychain, env_token),
        Command::Review { cmd } => run_review(cmd),
        Command::Backend { cmd } => run_backend(cmd, &paths),
        Command::Init { yes } => run_init(yes, &paths, &keychain, env_token),
    }
}

/// Dispatch `tm backend init-labels`.
///
/// Needs no Jira token (or keychain access at all): `gh` handles its own
/// authentication, so this only loads config to learn `backend` and
/// `github_repo`.
fn run_backend(cmd: BackendCmd, paths: &ConfigPaths) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    match cmd {
        BackendCmd::InitLabels => {
            let gh = ShellGhCli::new();
            let repo = config.github_repo.clone().unwrap_or_default();
            let mut stdout = std::io::stdout();
            tskmstr::cli::backend::init_labels(config.backend, &repo, &gh, &mut stdout)?;
        }
    }
    Ok(())
}

/// Build a [`TicketProvider`] appropriate for `config.backend`: a real Jira
/// client (resolving a token from the keychain/environment) under
/// [`BackendKind::Jira`], or a [`GithubProvider`] under [`BackendKind::Github`].
///
/// The [`GithubProvider`] case leaks a freshly constructed [`ShellGhCli`] to
/// get a `&'static dyn GhCli` — [`GithubProvider`] borrows its [`GhCli`]
/// rather than owning a boxed one (see that type's doc comment for why:
/// tests need to inspect a `FakeGhCli`'s recorded calls by reference after
/// the fact), but `Box<dyn TicketProvider>` requires `'static`. `tm` is a
/// short-lived CLI process, and `ShellGhCli` is a zero-sized unit struct, so
/// leaking one costs nothing.
///
/// It also opens (and leaks, same rationale) a [`tskmstr::runs::RunStore`]
/// at `config`'s configured run database path, attached via
/// [`GithubProvider::with_rank_store`] — this is the one place production
/// wiring backs `TicketProvider::rank`/the `Ranked`/`ReadyCandidates`
/// queries with the local `ticket_rank` table (phase 6,
/// `docs/plans/github-issues-backend.md`). A `RunStore` (unlike `ShellGhCli`)
/// isn't zero-sized, but it's still a single per-invocation allocation for a
/// short-lived CLI process, the same trade this function already makes for
/// `GhCli`.
fn ticket_provider_for(
    config: &Config,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<Box<dyn TicketProvider>, Box<dyn std::error::Error>> {
    match config.backend {
        BackendKind::Jira => {
            let token = resolve_token(keychain, env_token)?;
            Ok(jira_client_for(config, &token))
        }
        BackendKind::Github => {
            let repo = config
                .github_repo
                .clone()
                .ok_or("github backend selected but no [backend.github].repo is configured")?;
            let gh: &'static dyn GhCli = Box::leak(Box::new(ShellGhCli::new()));
            let run_store: &'static tskmstr::runs::RunStore = Box::leak(Box::new(
                tskmstr::runs::RunStore::open(&run_db_path_from_config(config))?,
            ));
            Ok(Box::new(
                GithubProvider::new(gh, repo).with_rank_store(run_store),
            ))
        }
    }
}

/// Build an [`AgentRunner`] for `config.agent` (see [`AgentKind`]), mirroring
/// [`ticket_provider_for`]: the one factory that turns [`AgentKind`] into a
/// live implementation, so nothing else outside config parsing needs to
/// `match` on it (see `docs/plans/agent-runner.md` and GitHub issue #17).
///
/// The [`AgentKind::Claude`] arm leaks a freshly constructed [`ClaudeRunner`]
/// to get a `&'static dyn AgentRunner` — [`ClaudeRunner`] is a zero-sized
/// unit struct and `tm` is a short-lived CLI process, so leaking one costs
/// nothing, the same trade [`ticket_provider_for`] makes for `ShellGhCli`.
fn agent_runner_for(config: &Config) -> &'static dyn AgentRunner {
    match config.agent {
        AgentKind::Claude => Box::leak(Box::new(ClaudeRunner)),
    }
}

/// Best-effort ticket provider for `tm work run`'s branch-name-slug lookup
/// (see `run_work`'s doc comment): the same provider [`ticket_provider_for`]
/// builds for `config.backend`, or `None` on any construction/auth failure
/// — never a hard error, since `tm work run` has always worked without
/// ticket-backend access and this feature must not change that.
fn run_ticket_provider(
    config: &Config,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Option<Box<dyn TicketProvider>> {
    ticket_provider_for(config, keychain, env_token).ok()
}

/// `tm review fix <KEY>` is always special-cased in `main` as
/// [`run_review_fix`] instead (see the comment above that early-return),
/// since `ReviewCmd::Fix` is `tm review`'s only subcommand and always needs
/// the non-uniform exit code `run_review_fix` provides. This arm exists only
/// so `dispatch`'s match stays exhaustive over [`Command`]; it is never
/// actually reached.
fn run_review(cmd: ReviewCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ReviewCmd::Fix { .. } => unreachable!("tm review fix is handled by run_review_fix"),
    }
}

/// Dispatch `tm work new/remove/list/restore/start/run`.
///
/// Loads config leniently (like `tm runs`, unlike every other command):
/// `tm work` should still work with an absent/invalid ticket-backend config,
/// since most of its subcommands don't touch a ticket provider at all. A
/// missing `[work]` section just means no lanes and every default falls back
/// to `tskmstr::cli::work`'s own hardcoded fallbacks (e.g. `~/Worktrees`).
///
/// `WorkCmd::Run` is the one exception that *can* use a ticket provider (for
/// the human-readable branch-name slug — see
/// `tskmstr::work::run::resolve_ticket_slug`), selected by `config.backend`
/// via [`ticket_provider_for`] (the same construction `tm ticket`/`tm board`
/// use), but only opportunistically: it's still built from this same
/// leniently-loaded `full_config`, and a missing/invalid config or a
/// construction/auth failure just means no ticket provider is wired in, not
/// a hard error. See that match arm for the fallback.
fn run_work(
    cmd: WorkCmd,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let full_config = config::load(paths).ok();
    let work_config = full_config
        .as_ref()
        .map(|cfg| cfg.work.clone())
        .unwrap_or_default();
    let git = ShellGitOps::new();
    let tmux = ShellTmuxOps::new();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let cwd = std::env::current_dir()?;
    let ctx = tskmstr::cli::work::WorkContext {
        git: &git,
        tmux: &tmux,
        config: &work_config,
        home: &home,
    };
    let mut stdout = std::io::stdout();

    match cmd {
        WorkCmd::New { name, branch, from } => {
            tskmstr::cli::work::new(
                &ctx,
                &name,
                branch.as_deref(),
                from.as_deref(),
                &cwd,
                &mut stdout,
            )?;
        }
        WorkCmd::Remove { name } => {
            tskmstr::cli::work::remove(&ctx, &name, &cwd, &mut stdout)?;
        }
        WorkCmd::List => {
            tskmstr::cli::work::list(&ctx, &mut stdout)?;
        }
        WorkCmd::Restore => {
            tskmstr::cli::work::restore(&ctx, &mut stdout)?;
        }
        WorkCmd::Session { key } => {
            let run_store = tskmstr::runs::RunStore::open(&resolve_run_db_path())?;
            let current_exe = std::env::current_exe()?;
            let identity = backend_identity_or_placeholder(full_config.as_ref());
            tskmstr::cli::work::session(
                &ctx,
                &run_store,
                &identity,
                &current_exe,
                &key,
                agent_runner_or_default(full_config.as_ref()),
                &mut stdout,
            )?;
        }
        WorkCmd::Clean { key } => {
            let run_store = tskmstr::runs::RunStore::open(&resolve_run_db_path())?;
            let identity = backend_identity_or_placeholder(full_config.as_ref());
            tskmstr::cli::work::clean(&ctx, &run_store, &identity, &key, &mut stdout)?;
        }
        WorkCmd::Start { dir } => {
            let dir_path = dir.map(PathBuf::from);
            tskmstr::cli::work::start(&ctx, dir_path.as_deref(), &cwd, &mut stdout)?;
        }
        WorkCmd::Run {
            lane,
            ticket,
            from,
            model,
            max_turns,
            permission_mode,
            prompt,
            headless,
            fg,
        } => {
            let gh = ShellGhCli::new();
            let spawner = tskmstr::work::runner::StdProcessSpawner;
            let run_db_path = resolve_run_db_path();
            let run_store = tskmstr::runs::RunStore::open(&run_db_path)?;
            let clock = tskmstr::work::run::SystemClock;
            let detach = tskmstr::work::detach::RealDetachSpawner;
            let current_exe = std::env::current_exe()?;
            // Best-effort ticket provider for the branch-name slug: absent
            // config or a construction/auth failure silently means no
            // provider, never a hard error — see this function's doc
            // comment.
            let ticket_provider: Option<Box<dyn TicketProvider>> = full_config
                .as_ref()
                .and_then(|cfg| run_ticket_provider(cfg, keychain, env_token.clone()));
            // The invoking repo's own backend identity, for the
            // lane/backend-compatibility preflight (GitHub issue #5 phase
            // 2). See `backend_identity_or_placeholder` on why the
            // missing-config placeholder is safe here.
            let current_backend_identity = backend_identity_or_placeholder(full_config.as_ref());
            let backend_identity_resolver =
                tskmstr::config::FsBackendIdentityResolver { home: home.clone() };
            let run_deps = tskmstr::cli::work::RunDeps {
                gh: &gh,
                spawner: &spawner,
                run_store: &run_store,
                clock: &clock,
                detach: &detach,
                current_exe: &current_exe,
                run_db_path: &run_db_path,
                ticket_provider: ticket_provider.as_deref(),
                current_repo_dir: &cwd,
                current_backend_identity: &current_backend_identity,
                backend_identity_resolver: &backend_identity_resolver,
                runner: agent_runner_or_default(full_config.as_ref()),
            };
            let request = tskmstr::work::run::RunLaneRequest {
                ticket,
                from_base: from,
                model,
                max_turns,
                permission_mode,
                prompt_override: prompt,
                // `run` sets `mode` from the resolved dispatch below.
                ..Default::default()
            };
            let succeeded = tskmstr::cli::work::run(
                &ctx,
                &run_deps,
                &lane,
                request,
                Dispatch::from_flags(headless, fg),
                &mut stdout,
            )?;
            if !succeeded {
                return Err("lane run failed".into());
            }
        }
        WorkCmd::Supervise { state_file } => {
            let raw = std::fs::read_to_string(&state_file)?;
            let state: tskmstr::work::detach::SupervisorState = serde_json::from_str(&raw)?;
            let run_store = tskmstr::runs::RunStore::open(&state.run_db_path)?;
            let spawner = tskmstr::work::runner::StdProcessSpawner;
            let gh = ShellGhCli::new();
            let succeeded = tskmstr::cli::work::supervise(
                &spawner,
                &gh,
                &run_store,
                &state,
                agent_runner_or_default(full_config.as_ref()),
                &mut stdout,
            )?;
            // The state file has served its one-shot handoff purpose; a
            // failed removal isn't worth failing the (already recorded) run
            // over. On a supervisor crash it survives for debugging.
            let _ = std::fs::remove_file(&state_file);
            if !succeeded {
                return Err("lane run failed".into());
            }
        }
        WorkCmd::Hooks { cmd } => match cmd {
            tskmstr::cli::HooksCmd::Install { user, dry_run } => {
                if !user {
                    return Err("tm work hooks install currently only supports --user".into());
                }
                let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
                let clock = tskmstr::work::run::SystemClock;
                let (year, month, day, hour, min, sec) =
                    tskmstr::work::run::Clock::now_parts(&clock);
                let backup_suffix =
                    tskmstr::work::naming::format_timestamp(year, month, day, hour, min, sec);
                let runner = agent_runner_or_default(full_config.as_ref());
                match runner.install_user_hooks(
                    &home,
                    xdg_data_home.as_deref(),
                    &backup_suffix,
                    dry_run,
                )? {
                    Some(report) => report.write_summary(&mut stdout)?,
                    None => writeln!(
                        stdout,
                        "{} has no user-level telemetry to install.",
                        runner.name()
                    )?,
                }
            }
        },
    }
    Ok(())
}

/// The interactive terminal board.
///
/// Opens the run-state database leniently (like [`run_ticket_audit`]'s read
/// mode, via [`tskmstr::cli::ticket::AuditStoreStatus`]'s stance): a broken
/// runs DB must never block the Jira board itself, only degrade the audit
/// status badge/launch to unavailable.
fn run_board(
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let runner = agent_runner_for(&config);
    let jira = ticket_provider_for(&config, keychain, env_token)?;
    let store = tskmstr::runs::RunStore::open(&run_db_path_from_config(&config)).ok();
    let tmux = ShellTmuxOps::new();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let cwd = std::env::current_dir()?;
    let lanes = config.work.lanes.clone();

    // Backend-compatibility filtering (GitHub issue #5 phase 2): resolved
    // once here, eagerly, rather than per keypress or via a `Cmd` — there
    // are typically only 1-3 lanes, and this is the same I/O `config::load`
    // already does for the process's own cwd. See
    // `docs/plans/issue-5-lane-backend-routing.md`.
    let current_backend_identity = tskmstr::config::BackendIdentity::from_config(&config);
    let backend_identity_resolver =
        tskmstr::config::FsBackendIdentityResolver { home: home.clone() };
    let (lane_names, hidden_lane_count) = tskmstr::config::compatible_lane_names(
        &current_backend_identity,
        &lanes,
        &backend_identity_resolver,
    );

    // Audit-dir fallback (GitHub issue #5 phase 2): fall back to the
    // current repo, rather than refuse, when the configured audit dir's
    // backend is incompatible -- see `resolve_audit_host_dir`'s doc
    // comment.
    let mut audit = config.work.audit;
    let mut audit_dir_fallback = false;
    if let Some(raw_dir) = audit.dir.clone() {
        let expanded = tskmstr::work::naming::expand_tilde(&raw_dir, &home);
        let (effective_dir, fell_back) = tskmstr::config::resolve_audit_host_dir(
            &expanded,
            &current_backend_identity,
            &cwd,
            &backend_identity_resolver,
        );
        audit.dir = Some(effective_dir.to_string_lossy().into_owned());
        audit_dir_fallback = fell_back;
    }

    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    run(TuiDeps {
        jira,
        base_url: config.jira_base_url,
        project_key: config.default_project_key,
        board_column_order: config.board_column_order,
        store,
        tmux: Box::new(tmux),
        audit,
        review_watch: config.work.review_watch,
        xdg_data_home,
        home,
        launcher: Box::new(tskmstr::tui::launcher::RealLaneLauncher),
        lane_names,
        hidden_lane_count,
        audit_dir_fallback,
        gh: Box::new(ShellGhCli::new()),
        git: Box::new(ShellGitOps::new()),
        cwd,
        lanes,
        backend_identity: current_backend_identity,
        runner,
    })?;
    Ok(())
}

/// The invoking repo's own [`tskmstr::config::BackendIdentity`], or a
/// placeholder when no config could be loaded. The placeholder's empty
/// Jira identity matches no real lane/scope: commands that reach it either
/// fail earlier for the missing config (e.g. `prepare_run_lane`'s
/// `UnknownLane` check, since a missing config also means zero lanes) or
/// degrade to unscoped behavior, which is the pre-issue-#10 status quo.
fn backend_identity_or_placeholder(config: Option<&Config>) -> tskmstr::config::BackendIdentity {
    config
        .map(tskmstr::config::BackendIdentity::from_config)
        .unwrap_or(tskmstr::config::BackendIdentity::Jira {
            base_url: String::new(),
            project_key: String::new(),
        })
}

/// [`agent_runner_for`] for an optionally-loaded config, mirroring
/// [`backend_identity_or_placeholder`]: callers that load config leniently
/// (`tm work run`, `tm review fix` — see their doc comments) still need a
/// runner, defaulting to [`AgentKind::Claude`] (the same default an absent
/// `[agent]` table resolves to) when no config loaded at all.
fn agent_runner_or_default(config: Option<&Config>) -> &'static dyn AgentRunner {
    config
        .map(agent_runner_for)
        .unwrap_or_else(|| Box::leak(Box::new(ClaudeRunner)))
}

/// The default global/repo config paths for this machine and working
/// directory.
fn default_config_paths() -> ConfigPaths {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let repo_root = std::env::current_dir().ok();
    config::default_paths(&home, repo_root.as_deref())
}

/// Build a [`TicketProvider`] for the given config and token.
/// Read all of stdin as `tm ticket comment`'s piped-body source, but only
/// when stdin isn't a TTY — an interactive terminal has nothing piped in,
/// and blocking on `read_to_string` there would hang waiting for EOF that
/// never comes. Returns `Ok(None)` for an interactive terminal (so
/// [`tskmstr::cli::ticket::resolve_comment_body`] falls through to
/// `$EDITOR`), `Ok(Some(content))` when something was piped in, regardless
/// of content (an empty pipe is still "stdin was given"; `comment`'s
/// empty-body check catches a truly empty result).
fn read_piped_stdin() -> std::io::Result<Option<String>> {
    use std::io::{IsTerminal, Read};

    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(Some(buf))
}

fn jira_client_for(config: &Config, token: &str) -> Box<dyn TicketProvider> {
    Box::new(JiraProvider::new(HttpJiraClient::new(JiraClientContext {
        base_url: config.jira_base_url.clone(),
        email: config.jira_email.clone(),
        token: token.to_string(),
    })))
}

fn run_auth(
    cmd: AuthCmd,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Under the GitHub backend there's no Jira token to manage at all --
    // `gh`'s own `gh auth login`/`gh auth status` cover it. `tm auth`'s full
    // GitHub-aware UX (e.g. reporting `gh auth status` here) is deferred to
    // phase 7 of docs/plans/github-issues-backend.md; for now this just
    // avoids running the Jira-specific login/status flow, which would fail
    // outright with no Jira config to bootstrap. A config that fails to load
    // at all (e.g. no config file yet) falls through to the existing flow
    // unchanged, since `tm auth login`'s bootstrap only ever creates a Jira
    // config and that's still the right default for a fresh install.
    if let Ok(config) = config::load(paths)
        && config.backend == BackendKind::Github
    {
        println!(
            "tm auth is not applicable to the github backend; \
             this repo authenticates via `gh auth login`/`gh auth status` instead."
        );
        return Ok(());
    }

    let ctx = tskmstr::cli::auth::AuthContext {
        paths,
        keychain,
        env_token,
        jira_client_factory: &jira_client_for,
    };
    let mut prompter = RealPrompter;
    let mut stdout = std::io::stdout();

    match cmd {
        AuthCmd::Login => tskmstr::cli::auth::login(&ctx, &mut prompter, &mut stdout)?,
        AuthCmd::Status => tskmstr::cli::auth::status(&ctx, &mut stdout)?,
    }
    Ok(())
}

/// Dispatch `tm init`: detect the repo's origin remote, default branch, and
/// hook-install state, then run the onboarding wizard against real deps.
fn run_init(
    yes: bool,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let repo_dir = paths
        .repo
        .as_deref()
        .and_then(|repo_config| repo_config.parent().map(PathBuf::from));

    let gh = ShellGhCli::new();
    let origin_slug = repo_dir
        .as_deref()
        .and_then(tskmstr::cli::init::detect_origin_slug);
    let origin_default_branch = repo_dir
        .as_deref()
        .and_then(tskmstr::cli::init::detect_origin_default_branch);

    // `tm init` runs before any config necessarily exists, so this probes
    // the default runner ([`AgentKind::Claude`]) rather than one selected by
    // config — mirrors `agent_runner_or_default`'s other no-config callers.
    let init_runner = agent_runner_or_default(None);
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let hooks_installed = init_runner.user_hooks_installed(&home, xdg_data_home.as_deref());
    let hook_installer = |out: &mut dyn std::io::Write| -> Result<(), String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let clock = tskmstr::work::run::SystemClock;
        let (year, month, day, hour, min, sec) = tskmstr::work::run::Clock::now_parts(&clock);
        let backup_suffix =
            tskmstr::work::naming::format_timestamp(year, month, day, hour, min, sec);
        let runner = agent_runner_or_default(None);
        match runner
            .install_user_hooks(&home, xdg_data_home.as_deref(), &backup_suffix, false)
            .map_err(|err| err.to_string())?
        {
            Some(report) => report.write_summary(out).map_err(|err| err.to_string()),
            None => writeln!(
                out,
                "{} has no user-level telemetry to install.",
                runner.name()
            )
            .map_err(|err| err.to_string()),
        }
    };

    let ctx = tskmstr::cli::init::InitContext {
        paths,
        home: &home,
        keychain,
        env_token,
        jira_client_factory: &jira_client_for,
        gh: &gh,
        origin_slug,
        origin_default_branch,
        hooks_installed,
        hook_installer: &hook_installer,
    };
    let mut prompter = RealPrompter;
    let mut stdout = std::io::stdout();
    tskmstr::cli::init::run_init(&ctx, yes, &mut prompter, &mut stdout)?;
    Ok(())
}

/// Load config and build a real ticket provider (Jira or GitHub, per
/// `config.backend` — see [`ticket_provider_for`]) + `gh` wrapper, shared by
/// `tm ticket` and `tm pr`.
type TicketingDeps = (Config, Box<dyn TicketProvider>, ShellGhCli);

fn build_ticketing_deps(
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<TicketingDeps, Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let jira = ticket_provider_for(&config, keychain, env_token)?;
    let gh = ShellGhCli::new();
    Ok((config, jira, gh))
}

/// Dispatch `tm ticket <KEY>`, `tm ticket create`, `tm ticket transition`,
/// `tm ticket assign`, `tm ticket rank`, `tm ticket link`, `tm ticket
/// unlink`, `tm ticket update`, `tm ticket comment`, and `tm ticket search`.
///
/// The forms need different dependencies: associating a key needs the full
/// [`TicketingContext`] (Jira + `gh` + config) to find the current branch's
/// PR, while creating a ticket has nothing to do with a PR and only needs
/// Jira + config (see [`CreateTicketContext`]). `tm ticket transition`, `tm
/// ticket assign`, `tm ticket rank`, `tm ticket link`, `tm ticket unlink`,
/// `tm ticket update`, and `tm ticket search` need only a Jira client (and,
/// for `assign`/`search`, config) — no `gh`/`git` at all, since none of them
/// reads or writes anything about a pull request; `assign` additionally
/// needs `config` itself (not just to build the Jira client) for
/// [`Config::default_assignee_account_id`], and `search` needs it for
/// [`Config::default_project_key`]. `tm ticket comment` needs the full
/// [`TicketingContext`] like `tm ticket <KEY>` does (a key can be inferred
/// from the current branch's PR, and `--pr` posts to it too), plus
/// [`read_piped_stdin`] and a [`tskmstr::cli::RealEditorPrompter`] to resolve
/// its body before [`tskmstr::cli::ticket::comment`] ever runs — that
/// resolution is real I/O, kept out of the testable `comment`/`comment_ticket`
/// functions themselves. `key` and `cmd` are both `Option` at the clap layer
/// so `tm ticket create`/`tm ticket transition`/`tm ticket assign`/`tm ticket
/// rank`/`tm ticket link`/`tm ticket unlink`/`tm ticket update`/`tm ticket
/// comment`/`tm ticket search` don't also require a positional key; exactly
/// one of them is expected to be `Some`, which this function enforces since
/// clap itself doesn't.
fn run_ticket(
    key: Option<String>,
    cmd: Option<TicketCmd>,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match (key, cmd) {
        (Some(key), None) => {
            let (config, jira, gh) = build_ticketing_deps(paths, keychain, env_token)?;
            let ctx = TicketingContext {
                jira: jira.as_ref(),
                gh: &gh,
                config: &config,
            };
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::run(&ctx, &key, &mut stdout)?;
            Ok(())
        }
        (
            None,
            Some(TicketCmd::Create {
                title,
                body,
                status,
                no_transition,
            }),
        ) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let ctx = CreateTicketContext {
                jira: jira.as_ref(),
                config: &config,
            };
            let opts = tskmstr::cli::ticket::CreateOptions {
                title,
                body,
                status,
                no_transition,
            };
            let mut prompter = RealPrompter;
            let mut stdout = std::io::stdout();
            // Opened leniently (`.ok()`), unlike `run_ticket_audit`'s hard
            // error on a missing config: a broken/absent runs DB must never
            // block ticket creation, since session registration is pure
            // telemetry (see `docs/plans/session-usage.md`).
            let session_store =
                tskmstr::runs::RunStore::open(&run_db_path_from_config(&config)).ok();
            let session_env = tskmstr::runs::session::SessionEnv::from_process_env();
            let sessions_dir = tskmstr::runs::session::sessions_dir_from_process_env();
            tskmstr::cli::ticket::create(
                &ctx,
                &opts,
                &mut prompter,
                session_store.as_ref(),
                &session_env,
                &sessions_dir,
                &mut stdout,
            )?;
            Ok(())
        }
        (None, Some(TicketCmd::Transition { key, status })) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::transition(jira.as_ref(), &key, status.as_deref(), &mut stdout)?;
            Ok(())
        }
        (None, Some(TicketCmd::Update { key, body })) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::update(jira.as_ref(), &key, &body, &mut stdout)?;
            Ok(())
        }
        (None, Some(TicketCmd::Comment { key, body, pr })) => {
            let (config, jira, gh) = build_ticketing_deps(paths, keychain, env_token)?;
            let ctx = TicketingContext {
                jira: jira.as_ref(),
                gh: &gh,
                config: &config,
            };
            let piped_stdin = read_piped_stdin()?;
            let mut editor = tskmstr::cli::RealEditorPrompter;
            let body_markdown =
                tskmstr::cli::ticket::resolve_comment_body(body, piped_stdin, &mut editor)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::comment(&ctx, key.as_deref(), &body_markdown, pr, &mut stdout)?;
            Ok(())
        }
        (
            None,
            Some(TicketCmd::Assign {
                key,
                name,
                me,
                unassign,
            }),
        ) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::assign(
                jira.as_ref(),
                &config,
                &key,
                name.as_deref(),
                me,
                unassign,
                &mut stdout,
            )?;
            Ok(())
        }
        (None, Some(TicketCmd::Rank { key, above, below })) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::rank(
                jira.as_ref(),
                &key,
                above.as_deref(),
                below.as_deref(),
                &mut stdout,
            )?;
            Ok(())
        }
        (
            None,
            Some(TicketCmd::Link {
                key,
                blocks,
                blocked_by,
            }),
        ) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::link(
                jira.as_ref(),
                &key,
                blocks.as_deref(),
                blocked_by.as_deref(),
                &mut stdout,
            )?;
            Ok(())
        }
        (None, Some(TicketCmd::Unlink { key, other })) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::unlink(jira.as_ref(), &key, &other, &mut stdout)?;
            Ok(())
        }
        (None, Some(TicketCmd::Audit { key, record, notes })) => {
            run_ticket_audit(key, record, notes, paths, keychain, env_token)
        }
        (
            None,
            Some(TicketCmd::Retro {
                key,
                clean,
                defect: _,
                severity,
                note,
            }),
        ) => run_ticket_retro(key, clean, severity, note, paths),
        (None, Some(TicketCmd::Search { text })) => {
            let config = config::load(paths)?;
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::search(jira.as_ref(), &config, &text, &mut stdout)?;
            Ok(())
        }
        (None, None) => Err(Box::new(
            tskmstr::cli::ticket::TicketCliError::KeyOrCreateRequired,
        )),
        (Some(_), Some(_)) => {
            unreachable!("clap's args_conflicts_with_subcommands rejects key and cmd together")
        }
    }
}

/// `tm ticket audit <KEY> [--record <VERDICT> [--notes <TEXT>]]`.
///
/// Both modes load config strictly via [`config::load`] (unlike `tm runs`,
/// which loads leniently so it works with no Jira config at all): read mode
/// already needs a full Jira client, and record mode reaching the same
/// `run_db_path` override without a second, differently-lenient config-load
/// path is simpler than branching the loading strategy per mode. The
/// tradeoff is that `tm ticket audit --record` (which itself never touches
/// Jira) still requires valid Jira config to run.
///
/// Read mode degrades a runs-DB open failure to `Last audit: unavailable
/// (...)` rather than failing the command (see
/// [`tskmstr::cli::ticket::AuditStoreStatus`]); record mode propagates the
/// same failure as a hard error, since persisting the verdict is the whole
/// point of that mode.
fn run_ticket_audit(
    key: String,
    record: Option<tskmstr::cli::AuditVerdict>,
    notes: Option<String>,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let db_path = run_db_path_from_config(&config);
    let scope = tskmstr::config::BackendIdentity::from_config(&config).scope();
    let mut stdout = std::io::stdout();
    let session_env = tskmstr::runs::session::SessionEnv::from_process_env();
    let sessions_dir = tskmstr::runs::session::sessions_dir_from_process_env();

    match record {
        Some(verdict) => {
            let store = tskmstr::runs::RunStore::open(&db_path)?;
            tskmstr::cli::ticket::audit_record(
                &store,
                &scope,
                &key,
                verdict.as_str(),
                notes.as_deref(),
                &session_env,
                &sessions_dir,
                &mut stdout,
            )?;
        }
        None => {
            let jira = ticket_provider_for(&config, keychain, env_token)?;
            // `AuditStoreStatus::Open` borrows `store`, so the store itself
            // has to live in this match's success arm rather than being
            // built into a `status` variable up front and dropped early.
            match tskmstr::runs::RunStore::open(&db_path) {
                Ok(store) => {
                    let status = tskmstr::cli::ticket::AuditStoreStatus::Open(&store);
                    tskmstr::cli::ticket::audit_read(
                        jira.as_ref(),
                        &status,
                        &scope,
                        &key,
                        &session_env,
                        &sessions_dir,
                        &mut stdout,
                    )?;
                }
                Err(err) => {
                    let status =
                        tskmstr::cli::ticket::AuditStoreStatus::Unavailable(err.to_string());
                    tskmstr::cli::ticket::audit_read(
                        jira.as_ref(),
                        &status,
                        &scope,
                        &key,
                        &session_env,
                        &sessions_dir,
                        &mut stdout,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// `tm ticket retro <KEY> (--clean|--defect --severity <SEVERITY>) [--note
/// <TEXT>]`: persist a shipped-ticket retro verdict.
///
/// `clap`'s `retro_verdict` `ArgGroup` on [`TicketCmd::Retro`] guarantees
/// `clean` is `true` xor `--defect` was given, so `clean` alone is enough to
/// pick the verdict here. Loads config strictly via [`config::load`] purely
/// to resolve `run_db_path` (same tradeoff [`run_ticket_audit`] documents:
/// this command never touches Jira, but still needs a valid config file to
/// run), then opens the runs DB and delegates to
/// [`tskmstr::cli::ticket::retro`], which does the actual validation and
/// persistence.
fn run_ticket_retro(
    key: String,
    clean: bool,
    severity: Option<tskmstr::cli::RetroSeverityArg>,
    note: Option<String>,
    paths: &ConfigPaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let db_path = run_db_path_from_config(&config);
    let scope = tskmstr::config::BackendIdentity::from_config(&config).scope();
    let store = tskmstr::runs::RunStore::open(&db_path)?;

    let verdict = if clean {
        tskmstr::runs::RetroVerdict::Clean
    } else {
        tskmstr::runs::RetroVerdict::Defect
    };
    let severity = severity.map(tskmstr::runs::RetroSeverity::from);

    let mut stdout = std::io::stdout();
    tskmstr::cli::ticket::retro(
        &store,
        &scope,
        &key,
        verdict,
        severity,
        note.as_deref(),
        &mut stdout,
    )?;
    Ok(())
}

/// Resolve the run database path from an already-loaded [`Config`]: the
/// configured `run_db_path` if set, otherwise the XDG default. Shares the
/// XDG-fallback logic with [`resolve_run_db_path`], which additionally
/// tolerates a missing/invalid config file.
fn run_db_path_from_config(config: &Config) -> PathBuf {
    match &config.run_db_path {
        Some(path) => PathBuf::from(path),
        None => default_xdg_run_db_path(),
    }
}

/// The XDG-derived default run database path: `$XDG_DATA_HOME/tskmstr/runs.db`
/// when set, otherwise `~/.local/share/tskmstr/runs.db`.
fn default_xdg_run_db_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    tskmstr::runs::default_db_path(&home, xdg_data_home.as_deref())
}

/// `tm ready` (no key): needs a Jira client + config, same as the
/// `TicketCmd::Transition` arm above, plus a `gh` client and the configured
/// `review_bots` for the best-effort bot-findings/stackability annotations
/// both forms now carry (see [`tskmstr::cli::ready::ReadyContext`]). `tm
/// ready <KEY>` is special-cased in `main` as [`run_ready_check`] instead —
/// see its doc comment for why.
fn run_ready(
    key: Option<String>,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    debug_assert!(
        key.is_none(),
        "tm ready <KEY> is handled by run_ready_check"
    );
    let config = config::load(paths)?;
    let jira = ticket_provider_for(&config, keychain, env_token)?;
    let gh = ShellGhCli::new();
    let ctx = tskmstr::cli::ready::ReadyContext {
        jira: jira.as_ref(),
        gh: &gh,
        review_bots: &config.review_bots,
    };
    let mut stdout = std::io::stdout();
    tskmstr::cli::ready::list(&ctx, &mut stdout)?;
    Ok(())
}

/// Exit code for `tm ready <KEY>` reporting a ticket **stackable**: exactly
/// one unmerged direct blocker, with an open PR to build on (see
/// [`tskmstr::cli::ready::ReadyOutcome::Stackable`]) — distinct from `0`
/// (ready) and `1` (blocked, or any other error) so an autonomous agent can
/// branch on "safe to proceed by stacking" without parsing stdout. Chosen to
/// avoid `2`, already used by `tm pr watch`'s `GaveUp` outcome, even though
/// exit codes are scoped per-command — picking a different value keeps the
/// two commands' schemes easy to tell apart at a glance. Documented in
/// README.md's `tm ready` section.
const READY_EXIT_STACKABLE: u8 = 3;

/// `tm ready <KEY>`: build real dependencies and run
/// [`tskmstr::cli::ready::check`], mapping its
/// [`tskmstr::cli::ready::ReadyOutcome`] to an exit code (`0` ready, `3`
/// stackable, `1` blocked or any other error) — see
/// [`READY_EXIT_STACKABLE`]'s doc comment for why this needs its own
/// three-way scheme rather than `dispatch`'s uniform 0/1.
fn run_ready_check(key: String) -> ExitCode {
    let paths = default_config_paths();
    let env_token = std::env::var("JIRA_API_TOKEN").ok();
    let keychain = MacosKeychain::new();

    let config = match config::load(&paths) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let jira = match ticket_provider_for(&config, &keychain, env_token) {
        Ok(jira) => jira,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let gh = ShellGhCli::new();
    let ctx = tskmstr::cli::ready::ReadyContext {
        jira: jira.as_ref(),
        gh: &gh,
        review_bots: &config.review_bots,
    };
    let mut stdout = std::io::stdout();

    match tskmstr::cli::ready::check(&ctx, &key, &mut stdout) {
        Ok(tskmstr::cli::ready::ReadyOutcome::Ready) => ExitCode::SUCCESS,
        Ok(tskmstr::cli::ready::ReadyOutcome::Stackable) => ExitCode::from(READY_EXIT_STACKABLE),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Exit code for `tm review fix <KEY>` reporting that `vdiff` captured no
/// review comments for the ticket's worktree (see
/// [`tskmstr::cli::review::FixOutcome::NoComments`]) — distinct from `0`
/// (dispatched) and `1` (any error, or a `--fg` run that finished failed) so
/// an autonomous agent can branch on "nothing to fix yet" without parsing
/// stdout. Exit codes are scoped per-command, so reusing
/// [`READY_EXIT_STACKABLE`]'s value here (rather than picking a fresh one)
/// is fine — the two commands share nothing that would make the overlap
/// confusing.
const REVIEW_FIX_EXIT_NO_COMMENTS: u8 = 3;

/// `tm review fix <KEY> [--fg]`: build real dependencies and run
/// [`tskmstr::cli::review::fix`], mapping its
/// [`tskmstr::cli::review::FixOutcome`] to an exit code (`0` dispatched/
/// completed successfully, `3` no comments captured, `1` any other error or
/// a `--fg` run that finished failed) — see
/// [`REVIEW_FIX_EXIT_NO_COMMENTS`]'s doc comment for why this needs its own
/// three-way scheme rather than `dispatch`'s uniform 0/1.
fn run_review_fix(key: String, dispatch: Dispatch) -> ExitCode {
    let run_db_path = resolve_run_db_path();
    let run_store = match tskmstr::runs::RunStore::open(&run_db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));

    let git = ShellGitOps::new();
    let gh = ShellGhCli::new();
    let spawner = tskmstr::work::runner::StdProcessSpawner;
    let vdiff = tskmstr::work::vdiff::ShellVdiffOps::new();
    let tmux = ShellTmuxOps::new();
    let detach = tskmstr::work::detach::RealDetachSpawner;
    let clock = tskmstr::work::run::SystemClock;

    let run_paths = tskmstr::work::run::RunLanePaths {
        home: home.clone(),
        state_dir: home.join(".local/state/tskmstr/work"),
        hooks_deploy_dir: home.join(".local/share/tskmstr/hooks"),
    };
    // Lenient, like `resolve_run_db_path`: a fix pass must still dispatch on
    // a machine whose ticket config can't load; the placeholder identity
    // just degrades the session name to an unscoped slug.
    let full_config = config::load(&default_config_paths()).ok();
    let backend_identity = backend_identity_or_placeholder(full_config.as_ref());
    let deps = tskmstr::cli::review::ReviewFixDeps {
        git: &git,
        gh: &gh,
        spawner: &spawner,
        run_store: &run_store,
        clock: &clock,
        vdiff: &vdiff,
        detach: &detach,
        current_exe: &current_exe,
        run_db_path: &run_db_path,
        tmux: &tmux,
        backend_identity: &backend_identity,
        runner: agent_runner_or_default(full_config.as_ref()),
    };
    let mut stdout = std::io::stdout();

    match tskmstr::cli::review::fix(&deps, &run_paths, &key, dispatch, &mut stdout) {
        Ok(tskmstr::cli::review::FixOutcome::Dispatched { succeeded: true }) => ExitCode::SUCCESS,
        Ok(tskmstr::cli::review::FixOutcome::Dispatched { succeeded: false }) => ExitCode::FAILURE,
        Ok(tskmstr::cli::review::FixOutcome::NoComments) => {
            eprintln!("No comments captured for {key}'s worktree — nothing to fix yet.");
            ExitCode::from(REVIEW_FIX_EXIT_NO_COMMENTS)
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch `tm runs`, `tm runs start`, and `tm runs finish`.
///
/// Deliberately doesn't go through [`build_ticketing_deps`]/[`config::load`]
/// in the strict (error-if-missing) sense: `tm runs` must work on a machine
/// with no Jira config at all, so config loading here is best-effort (see
/// [`resolve_run_db_path`]).
fn run_runs(
    kind: Option<String>,
    by_outcome: bool,
    by_retro: bool,
    cmd: Option<RunsCmd>,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = tskmstr::runs::RunStore::open(&resolve_run_db_path())?;
    let mut stdout = std::io::stdout();
    // Same lenient stance as `resolve_run_db_path`: ticket-keyed lookups
    // scope to the invoking repo when its config loads (GitHub issue #10),
    // and fall back to unscoped — the pre-#10 behavior — when it doesn't,
    // so `tm runs` still works on a machine with no config at all.
    let full_config = config::load(&default_config_paths()).ok();
    let scope = full_config
        .as_ref()
        .map(|cfg| tskmstr::config::BackendIdentity::from_config(cfg).scope());
    let scope = scope.as_deref();

    match cmd {
        None if by_outcome => {
            tskmstr::cli::runs::list_by_outcome(&store, kind.as_deref(), &mut stdout)?
        }
        None if by_retro => {
            tskmstr::cli::runs::list_by_retro(&store, kind.as_deref(), &mut stdout)?
        }
        None => tskmstr::cli::runs::list(&store, kind.as_deref(), &mut stdout)?,
        Some(RunsCmd::Start {
            ticket,
            lane,
            worktree,
            branch,
            pid,
            kind,
        }) => {
            let params = tskmstr::runs::StartRun {
                ticket,
                scope: scope.unwrap_or_default().to_string(),
                lane,
                worktree,
                branch,
                pid,
                kind,
                log_path: None,
            };
            tskmstr::cli::runs::start(&store, &params, &mut stdout)?;
        }
        Some(RunsCmd::Finish {
            run_id,
            status,
            exit_code,
            session_id,
            cost_usd,
            num_turns,
            blocker,
            pr_url,
            transcript,
            model_usage,
            findings_count,
        }) => {
            let outcome = tskmstr::runs::FinishRun {
                status: status.into(),
                exit_code,
                session_id,
                cost_usd,
                num_turns,
                blocker,
                pr_url,
                transcript,
                model_usage,
                findings_count,
            };
            tskmstr::cli::runs::finish(&store, run_id, &outcome, &mut stdout)?;
        }
        Some(RunsCmd::Event {
            run_id,
            kind,
            detail,
        }) => {
            tskmstr::cli::runs::event(&store, run_id, &kind, detail.as_deref(), &mut stdout)?;
        }
        Some(RunsCmd::Reap { stale_after }) => {
            tskmstr::cli::runs::reap(
                &store,
                stale_after,
                &tskmstr::runs::pid::pid_alive,
                &mut stdout,
            )?;
        }
        Some(RunsCmd::Show { ticket, json, kind }) => {
            tskmstr::cli::runs::show(&store, scope, &ticket, kind.as_deref(), json, &mut stdout)?;
        }
        Some(RunsCmd::Resume { ticket }) => {
            let mut stderr = std::io::stderr();
            tskmstr::cli::runs::resume(
                &store,
                scope,
                &ticket,
                agent_runner_or_default(full_config.as_ref()),
                &mut stdout,
                &mut stderr,
            )?;
        }
        Some(RunsCmd::Reopen {
            ticket_or_id,
            kind,
            to,
        }) => {
            tskmstr::cli::runs::reopen(
                &store,
                scope,
                &ticket_or_id,
                kind.as_deref(),
                to.into(),
                &mut stdout,
            )?;
        }
        Some(RunsCmd::Register { kind, key }) => {
            let session_env = tskmstr::runs::session::SessionEnv::from_process_env();
            let sessions_dir = tskmstr::runs::session::sessions_dir_from_process_env();
            tskmstr::cli::runs::register(&store, scope, &sessions_dir, &session_env, &kind, &key);
        }
        Some(RunsCmd::Watch) => {
            tskmstr::tui::event::run_watch(tskmstr::tui::event::WatchDeps { store })?;
        }
        Some(RunsCmd::Logs {
            ticket_or_id,
            kind,
            tail,
            follow,
        }) => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"));
            let sleeper = tskmstr::work::review_watch::RealSleeper;
            tskmstr::cli::runs::logs(
                &store,
                scope,
                &home,
                &ticket_or_id,
                kind.as_deref(),
                tail,
                follow,
                &sleeper,
                &mut stdout,
            )?;
        }
    }
    Ok(())
}

/// Resolve the run database path: the configured `run_db_path` if config
/// loads and sets it, otherwise the XDG default.
///
/// Lenient by design: `tm runs` needs to work on machines with no Jira
/// config at all, so a missing or invalid config file is silently ignored
/// here rather than surfaced as an error (unlike every other command, which
/// requires config to load via [`config::load`]).
fn resolve_run_db_path() -> PathBuf {
    let paths = default_config_paths();
    let configured = config::load(&paths).ok().and_then(|cfg| cfg.run_db_path);

    match configured {
        Some(path) => PathBuf::from(path),
        None => default_xdg_run_db_path(),
    }
}

fn run_pr(
    cmd: PrCmd,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (config, jira, gh) = build_ticketing_deps(paths, keychain, env_token)?;
    let ctx = TicketingContext {
        jira: jira.as_ref(),
        gh: &gh,
        config: &config,
    };
    let mut prompter = RealPrompter;
    let mut stdout = std::io::stdout();

    match cmd {
        PrCmd::Create { title, body, base } => {
            let opts = tskmstr::cli::pr::PrCreateOptions { title, body, base };
            tskmstr::cli::pr::create(&ctx, &opts, &mut prompter, &mut stdout)?;
        }
        PrCmd::Status { auto_ticket } => {
            let opts = tskmstr::cli::pr::PrStatusOptions { auto_ticket };
            tskmstr::cli::pr::status(&ctx, &opts, &mut prompter, &mut stdout)?;
        }
        PrCmd::Watch { .. } => {
            // Handled entirely by `main`'s pre-`dispatch` special case (see
            // its doc comment) so `tm pr watch` can produce its own 0/1/2
            // exit codes instead of `dispatch`'s uniform 0/1. Unreachable in
            // practice; kept only so this match stays exhaustive.
            unreachable!("tm pr watch is dispatched before reaching run_pr");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tskmstr::keychain::InMemoryKeychain;

    fn github_config(repo: &str) -> Config {
        Config {
            backend: BackendKind::Github,
            jira_base_url: String::new(),
            jira_email: String::new(),
            default_project_key: String::new(),
            github_repo: Some(repo.to_string()),
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: Vec::new(),
            board_column_order: Vec::new(),
            work: tskmstr::config::WorkConfig::default(),
            agent: tskmstr::config::AgentKind::Claude,
        }
    }

    fn jira_config() -> Config {
        Config {
            backend: BackendKind::Jira,
            jira_base_url: "https://example.atlassian.net".to_string(),
            jira_email: "dev@example.com".to_string(),
            default_project_key: "PROJ".to_string(),
            github_repo: None,
            default_assignee_account_id: None,
            status_on_pr: None,
            status_on_create: None,
            run_db_path: None,
            review_bots: Vec::new(),
            board_column_order: Vec::new(),
            work: tskmstr::config::WorkConfig::default(),
            agent: tskmstr::config::AgentKind::Claude,
        }
    }

    // `tm work run`'s branch-name-slug provider must be selected by
    // `config.backend` (via `ticket_provider_for`, the same construction
    // `tm ticket`/`tm board` use), not always Jira — see issue #5's root
    // cause 3. A github-backend config with no jira credentials at all
    // must still get a usable provider: `run_ticket_provider` must not
    // fall through to a Jira-token lookup that has nothing to resolve.
    #[test]
    fn agent_runner_for_claude_returns_the_claude_runner() {
        let config = jira_config();

        let runner = agent_runner_for(&config);

        assert_eq!(runner.name(), "claude");
    }

    #[test]
    fn run_ticket_provider_github_backend_does_not_need_a_jira_token() {
        // `ticket_provider_for`'s github arm opens a `RunStore` at
        // `config.run_db_path`, defaulting to `$HOME/.local/share/tskmstr/
        // runs.db` (`run_db_path_from_config`) when unset. Leaving it unset
        // here made this test depend on the real process `$HOME` being
        // writable -- true in an ordinary dev shell, false in `nix build`'s
        // sandboxed `$HOME` (verified: this test fails there with "github
        // backend must not require a Jira token to produce a ticket
        // provider", because `RunStore::open` fails and `run_ticket_provider`
        // swallows it into `None` per its documented opportunistic
        // contract). Point it at a temp dir instead, so the test is
        // hermetic regardless of the ambient `$HOME`.
        let tmp = tempfile::tempdir().unwrap();
        let mut config = github_config("jowi-dev/tskmstr");
        config.run_db_path = Some(tmp.path().join("runs.db").to_string_lossy().into_owned());
        let keychain = InMemoryKeychain::empty();

        let provider = run_ticket_provider(&config, &keychain, None);

        assert!(
            provider.is_some(),
            "github backend must not require a Jira token to produce a ticket provider"
        );
    }

    #[test]
    fn run_ticket_provider_jira_backend_is_none_without_a_token() {
        let config = jira_config();
        let keychain = InMemoryKeychain::empty();

        let provider = run_ticket_provider(&config, &keychain, None);

        assert!(provider.is_none());
    }

    #[test]
    fn run_ticket_provider_jira_backend_is_some_with_env_token() {
        let config = jira_config();
        let keychain = InMemoryKeychain::empty();

        let provider = run_ticket_provider(&config, &keychain, Some("tok".to_string()));

        assert!(provider.is_some());
    }
}
