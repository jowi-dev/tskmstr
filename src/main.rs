//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::cli::{AuthCmd, Cli, Command, PrCmd, RealPrompter, TicketCmd};
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
        (None, None) => Err(Box::new(
            tskmstr::cli::ticket::TicketCliError::KeyOrCreateRequired,
        )),
        (Some(_), Some(_)) => {
            unreachable!("clap's args_conflicts_with_subcommands rejects key and cmd together")
        }
    }
}

/// `tm ready` / `tm ready <KEY>`: needs only a Jira client + config, the same
/// shape as the `TicketCmd::Transition` arm above, since neither form of
/// `tm ready` touches a pull request.
fn run_ready(
    key: Option<String>,
    paths: &ConfigPaths,
    keychain: &dyn KeychainStore,
    env_token: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = config::load(paths)?;
    let token = resolve_token(keychain, env_token)?;
    let jira = jira_client_for(&config, &token);
    let mut stdout = std::io::stdout();
    match key {
        Some(key) => tskmstr::cli::ready::check(jira.as_ref(), &key, &mut stdout)?,
        None => tskmstr::cli::ready::list(jira.as_ref(), &mut stdout)?,
    }
    Ok(())
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
