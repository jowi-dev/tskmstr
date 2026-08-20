//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::cli::work::Dispatch;
use tskmstr::cli::{
    AuthCmd, Cli, Command, PrCmd, RealPrompter, ReviewCmd, RunsCmd, TicketCmd, WorkCmd,
};
use tskmstr::config::{self, Config, ConfigPaths};
use tskmstr::github::gh_cli::ShellGhCli;
use tskmstr::jira::client::{HttpJiraClient, JiraClient, JiraClientContext};
use tskmstr::keychain::{KeychainStore, MacosKeychain, resolve_token};
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
        jira: &jira,
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
    let cleanup_launcher = tskmstr::work::bugbot::RealCleanupLauncher {
        store: &run_store,
        tmux: &tmux,
        cfg: &config.work.review_watch,
        home: &home,
        xdg_data_home: xdg_data_home.as_deref(),
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
    }
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
/// `tm work` should still work with an absent/invalid Jira config, since
/// most of its subcommands don't touch Jira at all. A missing `[work]`
/// section just means no lanes and every default falls back to
/// `tskmstr::cli::work`'s own hardcoded fallbacks (e.g. `~/Worktrees`).
///
/// `WorkCmd::Run` is the one exception that *can* use Jira (for the
/// human-readable branch-name slug — see
/// `tskmstr::work::run::resolve_ticket_slug`), but only opportunistically:
/// it's still built from this same leniently-loaded `full_config`, and a
/// missing/invalid config or unresolvable token just means no Jira client
/// is wired in, not a hard error. See that match arm for the fallback.
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
            tskmstr::cli::work::session(&ctx, &run_store, &current_exe, &key, &mut stdout)?;
        }
        WorkCmd::Clean { key } => {
            let run_store = tskmstr::runs::RunStore::open(&resolve_run_db_path())?;
            tskmstr::cli::work::clean(&ctx, &run_store, &key, &mut stdout)?;
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
            // Best-effort Jira client for the branch-name slug: absent
            // config or an unresolvable token silently means no client,
            // never a hard error — see this function's doc comment.
            let jira: Option<Box<dyn JiraClient>> = full_config.as_ref().and_then(|cfg| {
                resolve_token(keychain, env_token.clone())
                    .ok()
                    .map(|token| jira_client_for(cfg, &token))
            });
            let run_deps = tskmstr::cli::work::RunDeps {
                gh: &gh,
                spawner: &spawner,
                run_store: &run_store,
                clock: &clock,
                detach: &detach,
                current_exe: &current_exe,
                run_db_path: &run_db_path,
                jira: jira.as_deref(),
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
            let succeeded =
                tskmstr::cli::work::supervise(&spawner, &gh, &run_store, &state, &mut stdout)?;
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
                let hooks_dir =
                    tskmstr::work::hooks_install::user_hooks_dir(xdg_data_home.as_deref(), &home);
                let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
                let settings_path = tskmstr::work::hooks_install::user_settings_path(
                    claude_config_dir.as_deref(),
                    &home,
                );
                let clock = tskmstr::work::run::SystemClock;
                let (year, month, day, hour, min, sec) =
                    tskmstr::work::run::Clock::now_parts(&clock);
                let backup_suffix =
                    tskmstr::work::naming::format_timestamp(year, month, day, hour, min, sec);
                let report = tskmstr::work::hooks_install::install_user_hooks(
                    &hooks_dir,
                    &settings_path,
                    &backup_suffix,
                    dry_run,
                )?;
                report.write_summary(&mut stdout)?;
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
    let token = resolve_token(keychain, env_token)?;
    let jira = jira_client_for(&config, &token);
    let store = tskmstr::runs::RunStore::open(&run_db_path_from_config(&config)).ok();
    let tmux = ShellTmuxOps::new();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let lanes = config.work.lanes.clone();
    let lane_names: Vec<String> = lanes.keys().cloned().collect();
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let cwd = std::env::current_dir()?;
    run(TuiDeps {
        jira,
        base_url: config.jira_base_url,
        project_key: config.default_project_key,
        board_column_order: config.board_column_order,
        store,
        tmux: Box::new(tmux),
        audit: config.work.audit,
        review_watch: config.work.review_watch,
        xdg_data_home,
        home,
        launcher: Box::new(tskmstr::tui::launcher::RealLaneLauncher),
        lane_names,
        gh: Box::new(ShellGhCli::new()),
        git: Box::new(ShellGitOps::new()),
        cwd,
        lanes,
    })?;
    Ok(())
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

/// Build a [`JiraClient`] for the given config and token.
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

fn jira_client_for(config: &Config, token: &str) -> Box<dyn JiraClient> {
    Box::new(HttpJiraClient::new(JiraClientContext {
        base_url: config.jira_base_url.clone(),
        email: config.jira_email.clone(),
        token: token.to_string(),
    }))
}

fn run_auth(
    cmd: AuthCmd,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
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

/// Load config and build a real Jira client + `gh` wrapper, shared by
/// `tm ticket` and `tm pr`.
fn build_ticketing_deps(
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(Config, HttpJiraClient, ShellGhCli), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let token = resolve_token(keychain, env_token)?;
    let jira = HttpJiraClient::new(JiraClientContext {
        base_url: config.jira_base_url.clone(),
        email: config.jira_email.clone(),
        token,
    });
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
                jira: &jira,
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::transition(jira.as_ref(), &key, status.as_deref(), &mut stdout)?;
            Ok(())
        }
        (None, Some(TicketCmd::Update { key, body })) => {
            let config = config::load(paths)?;
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::update(jira.as_ref(), &key, &body, &mut stdout)?;
            Ok(())
        }
        (None, Some(TicketCmd::Comment { key, body, pr })) => {
            let (config, jira, gh) = build_ticketing_deps(paths, keychain, env_token)?;
            let ctx = TicketingContext {
                jira: &jira,
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
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
    let mut stdout = std::io::stdout();
    let session_env = tskmstr::runs::session::SessionEnv::from_process_env();
    let sessions_dir = tskmstr::runs::session::sessions_dir_from_process_env();

    match record {
        Some(verdict) => {
            let store = tskmstr::runs::RunStore::open(&db_path)?;
            tskmstr::cli::ticket::audit_record(
                &store,
                &key,
                verdict.as_str(),
                notes.as_deref(),
                &session_env,
                &sessions_dir,
                &mut stdout,
            )?;
        }
        None => {
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
            // `AuditStoreStatus::Open` borrows `store`, so the store itself
            // has to live in this match's success arm rather than being
            // built into a `status` variable up front and dropped early.
            match tskmstr::runs::RunStore::open(&db_path) {
                Ok(store) => {
                    let status = tskmstr::cli::ticket::AuditStoreStatus::Open(&store);
                    tskmstr::cli::ticket::audit_read(
                        jira.as_ref(),
                        &status,
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
    let token = resolve_token(keychain, env_token)?;
    let jira = jira_client_for(&config, &token);
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
    let token = match resolve_token(&keychain, env_token) {
        Ok(token) => token,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let jira = jira_client_for(&config, &token);
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
            tskmstr::cli::runs::show(&store, &ticket, kind.as_deref(), json, &mut stdout)?;
        }
        Some(RunsCmd::Resume { ticket }) => {
            let mut stderr = std::io::stderr();
            tskmstr::cli::runs::resume(&store, &ticket, &mut stdout, &mut stderr)?;
        }
        Some(RunsCmd::Reopen {
            ticket_or_id,
            kind,
            to,
        }) => {
            tskmstr::cli::runs::reopen(
                &store,
                &ticket_or_id,
                kind.as_deref(),
                to.into(),
                &mut stdout,
            )?;
        }
        Some(RunsCmd::Register { kind, key }) => {
            let session_env = tskmstr::runs::session::SessionEnv::from_process_env();
            let sessions_dir = tskmstr::runs::session::sessions_dir_from_process_env();
            tskmstr::cli::runs::register(&store, &sessions_dir, &session_env, &kind, &key);
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
        jira: &jira,
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
