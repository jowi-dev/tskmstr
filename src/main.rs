//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::cli::{AuthCmd, Cli, Command, PrCmd, RealPrompter, RunsCmd, TicketCmd};
use tskmstr::config::{self, Config, ConfigPaths};
use tskmstr::github::gh_cli::ShellGhCli;
use tskmstr::jira::client::{HttpJiraClient, JiraClient, JiraClientContext};
use tskmstr::keychain::{KeychainStore, MacosKeychain, resolve_token};
use tskmstr::ticketing::{CreateTicketContext, TicketingContext};
use tskmstr::tui::event::{TuiDeps, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Board);

    match dispatch(command) {
        Ok(()) => ExitCode::SUCCESS,
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
        Command::Runs { cmd } => run_runs(cmd),
    }
}

/// The interactive terminal board.
fn run_board(
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let token = resolve_token(keychain, env_token)?;
    let jira = jira_client_for(&config, &token);
    run(TuiDeps {
        jira,
        base_url: config.jira_base_url,
        project_key: config.default_project_key,
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
/// `tm ticket assign`, `tm ticket rank`, `tm ticket link`, and `tm ticket
/// unlink`.
///
/// The forms need different dependencies: associating a key needs the full
/// [`TicketingContext`] (Jira + `gh` + config) to find the current branch's
/// PR, while creating a ticket has nothing to do with a PR and only needs
/// Jira + config (see [`CreateTicketContext`]). `tm ticket transition`, `tm
/// ticket assign`, `tm ticket rank`, `tm ticket link`, and `tm ticket
/// unlink` need only a Jira client (and, for `assign`, config) — no
/// `gh`/`git` at all, since none of them reads or writes anything about a
/// pull request; `assign` additionally needs `config` itself (not just to
/// build the Jira client) for [`Config::default_assignee_account_id`]. `key`
/// and `cmd` are both `Option` at the clap layer so `tm ticket create`/`tm
/// ticket transition`/`tm ticket assign`/`tm ticket rank`/`tm ticket link`/`tm
/// ticket unlink` don't also require a positional key; exactly one of them
/// is expected to be `Some`, which this function enforces since clap itself
/// doesn't.
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
        (None, Some(TicketCmd::Create { title, body })) => {
            let config = config::load(paths)?;
            let token = resolve_token(keychain, env_token)?;
            let jira = jira_client_for(&config, &token);
            let ctx = CreateTicketContext {
                jira: jira.as_ref(),
                config: &config,
            };
            let opts = tskmstr::cli::ticket::CreateOptions { title, body };
            let mut prompter = RealPrompter;
            let mut stdout = std::io::stdout();
            tskmstr::cli::ticket::create(&ctx, &opts, &mut prompter, &mut stdout)?;
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

    match record {
        Some(verdict) => {
            let store = tskmstr::runs::RunStore::open(&db_path)?;
            tskmstr::cli::ticket::audit_record(
                &store,
                &key,
                verdict.as_str(),
                notes.as_deref(),
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
                    tskmstr::cli::ticket::audit_read(jira.as_ref(), &status, &key, &mut stdout)?;
                }
                Err(err) => {
                    let status =
                        tskmstr::cli::ticket::AuditStoreStatus::Unavailable(err.to_string());
                    tskmstr::cli::ticket::audit_read(jira.as_ref(), &status, &key, &mut stdout)?;
                }
            }
        }
    }
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

/// `tm ready` / `tm ready <KEY>`: needs a Jira client + config, same as the
/// `TicketCmd::Transition` arm above, plus a `gh` client and the configured
/// `review_bots` for the best-effort bot-findings annotation both forms now
/// carry (see [`tskmstr::cli::ready::ReadyContext`]).
fn run_ready(
    key: Option<String>,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
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
    match key {
        Some(key) => tskmstr::cli::ready::check(&ctx, &key, &mut stdout)?,
        None => tskmstr::cli::ready::list(&ctx, &mut stdout)?,
    }
    Ok(())
}

/// Dispatch `tm runs`, `tm runs start`, and `tm runs finish`.
///
/// Deliberately doesn't go through [`build_ticketing_deps`]/[`config::load`]
/// in the strict (error-if-missing) sense: `tm runs` must work on a machine
/// with no Jira config at all, so config loading here is best-effort (see
/// [`resolve_run_db_path`]).
fn run_runs(cmd: Option<RunsCmd>) -> Result<(), Box<dyn std::error::Error>> {
    let store = tskmstr::runs::RunStore::open(&resolve_run_db_path())?;
    let mut stdout = std::io::stdout();

    match cmd {
        None => tskmstr::cli::runs::list(&store, &mut stdout)?,
        Some(RunsCmd::Start {
            ticket,
            lane,
            worktree,
            branch,
            pid,
        }) => {
            let params = tskmstr::runs::StartRun {
                ticket,
                lane,
                worktree,
                branch,
                pid,
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
        Some(RunsCmd::Show { ticket }) => {
            tskmstr::cli::runs::show(&store, &ticket, &mut stdout)?;
        }
        Some(RunsCmd::Resume { ticket }) => {
            tskmstr::cli::runs::resume(&store, &ticket, &mut stdout)?;
        }
        Some(RunsCmd::Watch) => {
            tskmstr::tui::event::run_watch(tskmstr::tui::event::WatchDeps { store })?;
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
    }
    Ok(())
}
