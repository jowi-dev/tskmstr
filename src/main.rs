//! `tm`: thin binary entry point. Parses arguments, wires up real
//! dependencies (config files, the macOS keychain, `gh`/`git`, and the Jira
//! HTTP API), and dispatches to `tskmstr::cli`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use tskmstr::cli::{AuthCmd, Cli, Command, RealPrompter};
use tskmstr::config::{self, Config, ConfigPaths};
use tskmstr::jira::client::{HttpJiraClient, JiraClient, JiraClientContext};
use tskmstr::keychain::MacosKeychain;

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        return run_board();
    };
    if matches!(command, Command::Board) {
        return run_board();
    }

    match dispatch(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// The interactive terminal board. Not yet implemented; keeps the binary
/// honest about what it can actually do rather than silently no-op'ing.
fn run_board() -> ExitCode {
    println!("TUI not yet implemented");
    ExitCode::FAILURE
}

/// Prints a placeholder for a subcommand not yet wired up, and fails.
fn not_yet_implemented(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("`tm {name}` not yet implemented");
    Err(format!("`tm {name}` not yet implemented").into())
}

/// Build real dependencies for `command` and run it.
fn dispatch(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let paths = default_config_paths();
    let env_token = std::env::var("JIRA_API_TOKEN").ok();
    let keychain = MacosKeychain::new();

    match command {
        Command::Auth { cmd } => run_auth(cmd, &paths, &keychain, env_token),
        Command::Ticket { .. } => not_yet_implemented("ticket"),
        Command::Pr { .. } => not_yet_implemented("pr"),
        Command::Board => unreachable!("handled in main() before dispatch"),
    }
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
    keychain: &dyn tskmstr::keychain::KeychainStore,
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
