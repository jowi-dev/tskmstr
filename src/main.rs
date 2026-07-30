//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::cli::{AuthCmd, Cli, Command, PrCmd, RealPrompter};
use tskmstr::config::{self, Config, ConfigPaths};
use tskmstr::github::gh_cli::ShellGhCli;
use tskmstr::jira::client::{HttpJiraClient, JiraClient, JiraClientContext};
use tskmstr::keychain::{KeychainStore, MacosKeychain, resolve_token};
use tskmstr::ticketing::TicketingContext;
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
        Command::Ticket { key } => run_ticket(&key, &paths, &keychain, env_token),
        Command::Pr { cmd } => run_pr(cmd, &paths, &keychain, env_token),
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

fn run_ticket(
    key: &str,
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
    let mut stdout = std::io::stdout();
    tskmstr::cli::ticket::run(&ctx, key, &mut stdout)?;
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
