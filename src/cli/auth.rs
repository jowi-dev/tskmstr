//! `tm auth login` and `tm auth status`.

use std::io::Write;
use std::path::Path;

use thiserror::Error;

use crate::config::{self, Config, ConfigError, ConfigPaths, GlobalConfigSeed};
use crate::jira::client::{JiraClient, JiraError};
use crate::keychain::{KeychainError, KeychainStore};

use super::Prompter;

/// Where to point users who need to create a Jira API token.
const TOKEN_HELP_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";

/// Errors surfaced by `tm auth` subcommands.
#[derive(Debug, Error)]
pub enum AuthCliError {
    /// Config could not be loaded, bootstrapped, or updated.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The keychain could not be read or written.
    #[error(transparent)]
    Keychain(#[from] KeychainError),

    /// A Jira API call failed.
    #[error(transparent)]
    Jira(#[from] JiraError),

    /// A prompt or output write failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// No Jira API token is available from either the environment or the
    /// keychain.
    #[error(
        "no Jira API token found; run `tm auth login` to store one, or set \
         the JIRA_API_TOKEN environment variable"
    )]
    NoToken,
}

/// Dependencies shared by [`login`] and [`status`].
pub struct AuthContext<'a> {
    /// Where to read/write the global (and optional repo) config file.
    pub paths: &'a ConfigPaths,
    /// Keychain the validated token is stored in (`login`) or read from
    /// (`status`).
    pub keychain: &'a dyn KeychainStore,
    /// The `JIRA_API_TOKEN` environment variable, if set; takes precedence
    /// over the keychain.
    pub env_token: Option<String>,
    /// Builds a [`JiraClient`] for a given config and token. Injected so
    /// tests can supply a fake client without a real Jira instance.
    pub jira_client_factory: &'a dyn Fn(&Config, &str) -> Box<dyn JiraClient>,
}

/// `tm auth login`: bootstrap config if needed, validate a Jira API token
/// against `GET /myself`, and store it in the keychain.
pub fn login(
    ctx: &AuthContext,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<(), AuthCliError> {
    let config = if ctx.paths.global.exists() {
        config::load(ctx.paths)?
    } else {
        bootstrap_config(&ctx.paths.global, prompter, out)?
    };

    let token = prompter.prompt_password(&format!(
        "Jira API token (create one at {TOKEN_HELP_URL}): "
    ))?;

    let jira = (ctx.jira_client_factory)(&config, &token);
    let myself = jira.myself()?;

    ctx.keychain.set_token(&token)?;
    writeln!(
        out,
        "Authenticated as {} (accountId {})",
        myself.display_name, myself.account_id
    )?;
    writeln!(out, "Jira API token stored in keychain.")?;

    if config.default_assignee_account_id.is_none() {
        config::set_assignee_account_id(&ctx.paths.global, &myself.account_id)?;
        writeln!(
            out,
            "Set default assignee to {} in config.",
            myself.account_id
        )?;
    }

    Ok(())
}

/// Prompt for base URL, email, and default project key, write a new global
/// config file, and return the resulting [`Config`].
fn bootstrap_config(
    path: &Path,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<Config, AuthCliError> {
    writeln!(
        out,
        "No config found at {}; let's set one up.",
        path.display()
    )?;

    let seed = GlobalConfigSeed {
        jira_base_url: prompt_required(
            prompter,
            out,
            "Jira base URL (e.g. https://your-site.atlassian.net)",
        )?,
        jira_email: prompt_required(prompter, out, "Jira email")?,
        default_project_key: prompt_required(prompter, out, "Default Jira project key")?
            .to_uppercase(),
    };
    config::write_global(path, &seed)?;
    writeln!(out, "Wrote config to {}", path.display())?;

    Ok(Config {
        jira_base_url: seed.jira_base_url,
        jira_email: seed.jira_email,
        default_project_key: seed.default_project_key,
        default_assignee_account_id: None,
        status_on_pr: None,
        status_on_create: None,
        run_db_path: None,
        review_bots: vec!["cursor[bot]".to_string()],
    })
}

/// Prompt for a line of input, re-prompting (with a short reminder) as long
/// as the answer is empty. Used for bootstrap fields that have no sensible
/// default and must be supplied explicitly.
fn prompt_required(
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
    message: &str,
) -> Result<String, AuthCliError> {
    loop {
        let answer = prompter.prompt_line(message, "")?;
        let trimmed = answer.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        writeln!(out, "This field is required.")?;
    }
}

/// `tm auth status`: report configuration, whether a token resolves (never
/// printing it), and whether it actually authenticates and can see the
/// default project.
pub fn status(ctx: &AuthContext, out: &mut dyn Write) -> Result<(), AuthCliError> {
    let config = config::load(ctx.paths)?;

    writeln!(out, "Jira base URL: {}", config.jira_base_url)?;
    writeln!(out, "Jira email: {}", config.jira_email)?;
    writeln!(out, "Default project: {}", config.default_project_key)?;

    let Some((token, source)) = resolve_token_source(ctx)? else {
        writeln!(out, "Jira API token: not found")?;
        return Err(AuthCliError::NoToken);
    };
    writeln!(out, "Jira API token: found ({source})")?;

    let jira = (ctx.jira_client_factory)(&config, &token);
    let myself = jira.myself()?;
    writeln!(out, "Jira auth: OK ({})", myself.display_name)?;

    jira.get_project(&config.default_project_key)?;
    writeln!(out, "Project {}: visible", config.default_project_key)?;

    Ok(())
}

/// Resolve the token to use along with where it came from, without ever
/// exposing the token value itself in a printable form beyond this function's
/// return value.
///
/// Mirrors [`crate::keychain::resolve_token`]'s env-over-keychain precedence,
/// but also reports the source so `status` can display it.
fn resolve_token_source(ctx: &AuthContext) -> Result<Option<(String, &'static str)>, AuthCliError> {
    if let Some(token) = ctx.env_token.clone().filter(|t| !t.is_empty()) {
        return Ok(Some((token, "env")));
    }
    match ctx.keychain.get_token()? {
        Some(token) => Ok(Some((token, "keychain"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakePrompter;
    use crate::jira::fake::FakeJiraClient;
    use crate::jira::types::Myself;
    use crate::keychain::InMemoryKeychain;
    use tempfile::tempdir;

    fn myself() -> Myself {
        Myself {
            account_id: "acct-1".to_string(),
            display_name: "Jane Doe".to_string(),
            email_address: Some("dev@example.com".to_string()),
        }
    }

    fn factory_returning(jira: FakeJiraClient) -> impl Fn(&Config, &str) -> Box<dyn JiraClient> {
        let jira = std::rc::Rc::new(jira);
        move |_cfg, _token| Box::new(FakeJiraClientHandle(jira.clone()))
    }

    /// A cheaply cloneable handle wrapping a shared [`FakeJiraClient`] so it
    /// can be captured by a `Fn` closure (which may be called more than
    /// once) and still recorded calls/config observed by the test.
    struct FakeJiraClientHandle(std::rc::Rc<FakeJiraClient>);

    impl JiraClient for FakeJiraClientHandle {
        fn myself(&self) -> Result<Myself, JiraError> {
            self.0.myself()
        }
        fn get_issue(&self, key: &str) -> Result<crate::jira::types::Issue, JiraError> {
            self.0.get_issue(key)
        }
        fn create_issue(
            &self,
            req: &crate::jira::types::CreateIssueRequest,
        ) -> Result<crate::jira::types::Issue, JiraError> {
            self.0.create_issue(req)
        }
        fn add_remote_link(
            &self,
            key: &str,
            link: &crate::jira::types::RemoteLinkRequest,
        ) -> Result<(), JiraError> {
            self.0.add_remote_link(key, link)
        }
        fn transitions(&self, key: &str) -> Result<Vec<crate::jira::types::Transition>, JiraError> {
            self.0.transitions(key)
        }
        fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
            self.0.transition(key, transition_id)
        }
        fn search(&self, jql: &str) -> Result<crate::jira::types::SearchResult, JiraError> {
            self.0.search(jql)
        }
        fn get_project(&self, key: &str) -> Result<(), JiraError> {
            self.0.get_project(key)
        }
        fn assignable_users(
            &self,
            project: &str,
        ) -> Result<Vec<crate::jira::types::JiraUser>, JiraError> {
            self.0.assignable_users(project)
        }
        fn assign(&self, key: &str, account_id: Option<&str>) -> Result<(), JiraError> {
            self.0.assign(key, account_id)
        }
        fn rank(
            &self,
            keys: &[String],
            anchor: crate::jira::client::RankAnchor,
        ) -> Result<(), JiraError> {
            self.0.rank(keys, anchor)
        }
        fn create_link(
            &self,
            req: &crate::jira::types::CreateLinkRequest,
        ) -> Result<(), JiraError> {
            self.0.create_link(req)
        }
        fn delete_link(&self, link_id: &str) -> Result<(), JiraError> {
            self.0.delete_link(link_id)
        }
        fn update_description(
            &self,
            key: &str,
            description: &serde_json::Value,
        ) -> Result<(), JiraError> {
            self.0.update_description(key, description)
        }
    }

    fn write_test_config(path: &Path) {
        config::write_global(
            path,
            &GlobalConfigSeed {
                jira_base_url: "https://example.atlassian.net".to_string(),
                jira_email: "dev@example.com".to_string(),
                default_project_key: "PROJ".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn login_bootstraps_config_when_missing_then_validates_and_stores_token() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("nested/config.toml");
        let paths = ConfigPaths {
            global: global_path.clone(),
            repo: None,
        };
        let keychain = InMemoryKeychain::empty();
        let jira = FakeJiraClient::new().with_myself(myself());
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut prompter = FakePrompter::new()
            .with_line("https://example.atlassian.net")
            .with_line("dev@example.com")
            .with_line("proj")
            .with_password("super-secret-token");
        let mut out = Vec::new();

        login(&ctx, &mut prompter, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Authenticated as Jane Doe (accountId acct-1)"));
        assert!(output.contains("Jira API token stored in keychain."));
        assert!(output.contains("Set default assignee to acct-1 in config."));

        assert_eq!(
            keychain.get_token().unwrap(),
            Some("super-secret-token".to_string())
        );

        let cfg = config::load(&paths).expect("config should be loadable");
        assert_eq!(cfg.default_assignee_account_id, Some("acct-1".to_string()));
        // Project key answer is normalized to uppercase.
        assert_eq!(cfg.default_project_key, "PROJ");
    }

    #[test]
    fn login_bootstrap_reprompts_on_empty_answers() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("nested/config.toml");
        let paths = ConfigPaths {
            global: global_path.clone(),
            repo: None,
        };
        let keychain = InMemoryKeychain::empty();
        let jira = FakeJiraClient::new().with_myself(myself());
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut prompter = FakePrompter::new()
            .with_line("") // base URL: empty, should reprompt
            .with_line("https://example.atlassian.net")
            .with_line("") // email: empty, should reprompt
            .with_line("dev@example.com")
            .with_line("") // project key: empty, should reprompt
            .with_line("proj")
            .with_password("super-secret-token");
        let mut out = Vec::new();

        login(&ctx, &mut prompter, &mut out).expect("should succeed");

        let cfg = config::load(&paths).expect("config should be loadable");
        assert_eq!(cfg.jira_base_url, "https://example.atlassian.net");
        assert_eq!(cfg.jira_email, "dev@example.com");
        assert_eq!(cfg.default_project_key, "PROJ");
    }

    #[test]
    fn login_uses_existing_config_without_prompting_for_bootstrap_fields() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::empty();
        let jira = FakeJiraClient::new().with_myself(myself());
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut prompter = FakePrompter::new().with_password("super-secret-token");
        let mut out = Vec::new();

        login(&ctx, &mut prompter, &mut out).expect("should succeed");

        // Only the token was prompted for; no bootstrap questions asked.
        assert_eq!(prompter.messages.len(), 1);
    }

    #[test]
    fn login_invalid_token_does_not_store_it() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::empty();
        let jira = FakeJiraClient::new().with_myself_unauthorized();
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut prompter = FakePrompter::new().with_password("bad-token");
        let mut out = Vec::new();

        let err = login(&ctx, &mut prompter, &mut out).expect_err("should fail");
        assert!(matches!(err, AuthCliError::Jira(JiraError::Unauthorized)));
        assert_eq!(keychain.get_token().unwrap(), None);
    }

    #[test]
    fn status_reports_ok_when_token_and_project_resolve() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::with_token("stored-token");
        let jira = FakeJiraClient::new().with_myself(myself());
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut out = Vec::new();

        status(&ctx, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Jira API token: found (keychain)"));
        assert!(output.contains("Jira auth: OK (Jane Doe)"));
        assert!(output.contains("Project PROJ: visible"));
        assert!(!output.contains("stored-token"));
    }

    #[test]
    fn status_prefers_env_token_and_reports_its_source() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::with_token("stored-token");
        let jira = FakeJiraClient::new().with_myself(myself());
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: Some("env-token".to_string()),
            jira_client_factory: &factory,
        };
        let mut out = Vec::new();

        status(&ctx, &mut out).expect("should succeed");

        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Jira API token: found (env)"));
    }

    #[test]
    fn status_no_token_reports_not_found_and_fails() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::empty();
        let jira = FakeJiraClient::new();
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut out = Vec::new();

        let err = status(&ctx, &mut out).expect_err("should fail");
        assert!(matches!(err, AuthCliError::NoToken));
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Jira API token: not found"));
    }

    #[test]
    fn status_invalid_token_reports_error_and_fails() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::with_token("stale-token");
        let jira = FakeJiraClient::new().with_myself_unauthorized();
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut out = Vec::new();

        let err = status(&ctx, &mut out).expect_err("should fail");
        assert!(matches!(err, AuthCliError::Jira(JiraError::Unauthorized)));
    }

    #[test]
    fn status_project_not_visible_fails() {
        let dir = tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        write_test_config(&global_path);
        let paths = ConfigPaths {
            global: global_path,
            repo: None,
        };
        let keychain = InMemoryKeychain::with_token("stored-token");
        let jira = FakeJiraClient::new()
            .with_myself(myself())
            .with_get_project_not_found("PROJ");
        let factory = factory_returning(jira);
        let ctx = AuthContext {
            paths: &paths,
            keychain: &keychain,
            env_token: None,
            jira_client_factory: &factory,
        };
        let mut out = Vec::new();

        let err = status(&ctx, &mut out).expect_err("should fail");
        assert!(matches!(
            err,
            AuthCliError::Jira(JiraError::NotFound { .. })
        ));
    }
}
