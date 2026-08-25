//! `tm init`: interactive wizard that onboards the current repo — backend
//! choice, repo-local `.tskmstr.toml`, work-lane scaffolding, status labels,
//! and session assets — so `tm board` works immediately after (GitHub
//! issue #8).
//!
//! The repo-local file is edited with `toml_edit` rather than re-serialized
//! from a struct so a re-run preserves the user's comments and formatting;
//! values the wizard leaves unchanged are never rewritten.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{self, BackendKind, Config, ConfigError, ConfigPaths};
use crate::github::gh_cli::GhCli;
use crate::keychain::{KeychainError, KeychainStore};
use crate::ticketing::provider::TicketProvider;

use super::Prompter;

/// Errors surfaced by `tm init`.
#[derive(Debug, Error)]
pub enum InitCliError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] super::auth::AuthCliError),
    #[error(transparent)]
    Keychain(#[from] KeychainError),
    #[error("failed to parse {path}: {source}")]
    ParseRepoConfig {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    #[error("`{key}` in {path} is not a table; fix it by hand and re-run `tm init`")]
    NotATable { key: String, path: PathBuf },
    #[error("cannot determine where .tskmstr.toml would live (no working directory)")]
    NoRepoDir,
    #[error("--yes could not resolve a default for {field}; run `tm init` without --yes")]
    MissingDefault { field: &'static str },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Dependencies for [`run_init`], injected so the wizard is testable with
/// canned prompt answers and fake clients (mirrors
/// [`super::auth::AuthContext`]).
pub struct InitContext<'a> {
    /// Where the global config and the repo-local `.tskmstr.toml` live.
    pub paths: &'a ConfigPaths,
    /// Home directory, for locating user-level skills and prompt files.
    pub home: &'a Path,
    /// Keychain a Jira token may already be stored in.
    pub keychain: &'a dyn KeychainStore,
    /// The `JIRA_API_TOKEN` environment variable, if set.
    pub env_token: Option<String>,
    /// Builds a [`TicketProvider`] for the Jira token handoff.
    pub jira_client_factory: &'a dyn Fn(&Config, &str) -> Box<dyn TicketProvider>,
    /// GitHub CLI, for creating the `tm:status/*` labels.
    pub gh: &'a dyn GhCli,
    /// `owner/name` detected from the `origin` remote, if any.
    pub origin_slug: Option<String>,
    /// Default branch detected from `origin/HEAD`, if any.
    pub origin_default_branch: Option<String>,
    /// Whether the user-level session hooks are already installed.
    pub hooks_installed: bool,
    /// Performs `tm work hooks install --user`; `Err` is a display message.
    pub hook_installer: &'a dyn Fn(&mut dyn Write) -> Result<(), String>,
}

/// `tm init`: inspect the repo, ask the questions the config files would
/// otherwise force the user to remember, and write everything `tm board`
/// needs. With `yes`, accept the default answer for every question.
pub fn run_init(
    ctx: &InitContext,
    yes: bool,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<(), InitCliError> {
    let repo_config_path = ctx.paths.repo.clone().ok_or(InitCliError::NoRepoDir)?;
    let file_existed = repo_config_path.exists();
    let original = if file_existed {
        std::fs::read_to_string(&repo_config_path)?
    } else {
        String::new()
    };
    let mut doc: DocumentMut =
        original
            .parse()
            .map_err(|source| InitCliError::ParseRepoConfig {
                path: repo_config_path.clone(),
                source,
            })?;

    // --- Questions ---
    let backend = ask_backend(ctx, yes, &doc, prompter, out)?;
    let mut create_labels_in: Option<String> = None;

    match backend {
        BackendKind::Github => {
            let default = str_at(&doc, &["backend", "github", "repo"])
                .map(str::to_string)
                .or_else(|| ctx.origin_slug.clone())
                .unwrap_or_default();
            let slug = ask_required(
                yes,
                prompter,
                out,
                "GitHub repo (owner/name)",
                &default,
                "the GitHub repo slug",
            )?;
            set_str(
                &mut doc,
                &["backend"],
                "provider",
                "github",
                &repo_config_path,
            )?;
            set_str(
                &mut doc,
                &["backend", "github"],
                "repo",
                &slug,
                &repo_config_path,
            )?;
            if ask_confirm(
                yes,
                prompter,
                &format!("Create the tm:status/* labels in {slug} now?"),
                true,
            )? {
                create_labels_in = Some(slug);
            }
        }
        BackendKind::Jira => todo!("jira path"),
    }

    // --- Writes ---
    ensure_global_exists(ctx, out)?;
    if !write_repo_file(
        yes,
        prompter,
        out,
        &repo_config_path,
        file_existed,
        &original,
        &doc,
    )? {
        writeln!(out, "Nothing else to do.")?;
        return Ok(());
    }

    // Prove the board can start from what was written.
    config::load(ctx.paths)?;

    // --- Side effects ---
    if let Some(slug) = create_labels_in
        && let Err(err) = super::backend::init_labels(BackendKind::Github, &slug, ctx.gh, out)
    {
        writeln!(
            out,
            "warning: creating labels failed: {err}. Run `tm backend init-labels` once resolved."
        )?;
    }

    writeln!(out)?;
    writeln!(out, "Done. Run `tm board` to open the board.")?;
    Ok(())
}

/// Ask which ticket backend the repo uses, re-prompting until the answer
/// parses. Defaults to the configured provider, else `github` when an origin
/// remote was detected, else `jira`.
fn ask_backend(
    ctx: &InitContext,
    yes: bool,
    doc: &DocumentMut,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<BackendKind, InitCliError> {
    let default = str_at(doc, &["backend", "provider"])
        .map(str::to_string)
        .unwrap_or_else(|| {
            if ctx.origin_slug.is_some() {
                "github"
            } else {
                "jira"
            }
            .to_string()
        });
    if yes {
        return BackendKind::parse(&default)
            .ok_or(ConfigError::InvalidProvider { value: default }.into());
    }
    loop {
        let answer = prompter.prompt_line("Ticket backend (jira or github)", &default)?;
        match BackendKind::parse(answer.trim()) {
            Some(kind) => return Ok(kind),
            None => writeln!(out, "expected \"jira\" or \"github\"")?,
        }
    }
}

/// Prompt for a required value, re-prompting while the answer is empty. With
/// `yes`, take `default` — or fail if there is no default to take.
fn ask_required(
    yes: bool,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
    message: &str,
    default: &str,
    field: &'static str,
) -> Result<String, InitCliError> {
    if yes {
        if default.is_empty() {
            return Err(InitCliError::MissingDefault { field });
        }
        return Ok(default.to_string());
    }
    loop {
        let answer = prompter.prompt_line(message, default)?;
        let trimmed = answer.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        writeln!(out, "This field is required.")?;
    }
}

/// Confirm a yes/no question; with `yes`, take `default` silently.
fn ask_confirm(
    yes: bool,
    prompter: &mut dyn Prompter,
    message: &str,
    default: bool,
) -> io::Result<bool> {
    if yes {
        return Ok(default);
    }
    prompter.confirm_with_default(message, default)
}

/// Read the string at a dotted `path` in `doc`, if present.
fn str_at<'a>(doc: &'a DocumentMut, path: &[&str]) -> Option<&'a str> {
    let (first, rest) = path.split_first()?;
    let mut item: &Item = doc.get(first)?;
    for key in rest {
        item = item.as_table_like()?.get(key)?;
    }
    item.as_str()
}

/// Set `key = val` in the table at `path`, creating intermediate tables as
/// needed. Leaves the document untouched when the value is already `val`, so
/// unchanged keys keep their comments and a no-op wizard run writes nothing.
fn set_str(
    doc: &mut DocumentMut,
    path: &[&str],
    key: &str,
    val: &str,
    file: &Path,
) -> Result<(), InitCliError> {
    let table = table_at(doc, path, file)?;
    if table.get(key).and_then(Item::as_str) != Some(val) {
        table.insert(key, value(val));
    }
    Ok(())
}

/// Descend to (creating as needed) the table at `path`. Created intermediate
/// tables stay implicit so `[work.lanes.name]`-style headers render without a
/// bare `[work]` above them; the final table gets an explicit header.
fn table_at<'a>(
    doc: &'a mut DocumentMut,
    path: &[&str],
    file: &Path,
) -> Result<&'a mut Table, InitCliError> {
    let mut table = doc.as_table_mut();
    for (i, key) in path.iter().enumerate() {
        let item = table.entry(key).or_insert_with(|| {
            let mut created = Table::new();
            created.set_implicit(true);
            Item::Table(created)
        });
        table = item.as_table_mut().ok_or_else(|| InitCliError::NotATable {
            key: path[..=i].join("."),
            path: file.to_path_buf(),
        })?;
    }
    table.set_implicit(false);
    Ok(table)
}

/// Create the global config file when missing: `config::load` hard-errors
/// without one, so `tm init` must be reachable before it exists. The GitHub
/// backend needs no global fields, so a placeholder is enough.
fn ensure_global_exists(ctx: &InitContext, out: &mut dyn Write) -> Result<(), InitCliError> {
    if ctx.paths.global.exists() {
        return Ok(());
    }
    if let Some(parent) = ctx.paths.global.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &ctx.paths.global,
        "# tskmstr global config (created by `tm init`)\n",
    )?;
    writeln!(out, "Wrote {}", ctx.paths.global.display())?;
    Ok(())
}

/// Write the repo-local `.tskmstr.toml`. An existing file is shown as it
/// would become and only overwritten on confirmation; returns whether the
/// on-disk file matches `doc` afterwards.
fn write_repo_file(
    yes: bool,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
    path: &Path,
    file_existed: bool,
    original: &str,
    doc: &DocumentMut,
) -> Result<bool, InitCliError> {
    let new_text = doc.to_string();
    if file_existed && new_text == original {
        writeln!(out, "{} is already up to date.", path.display())?;
        return Ok(true);
    }
    if file_existed {
        writeln!(out)?;
        writeln!(out, "{} will become:", path.display())?;
        writeln!(out)?;
        write!(out, "{new_text}")?;
        writeln!(out)?;
        if !ask_confirm(yes, prompter, &format!("Write {}?", path.display()), true)? {
            writeln!(out, "Left {} unchanged.", path.display())?;
            return Ok(false);
        }
    }
    std::fs::write(path, new_text)?;
    writeln!(out, "Wrote {}", path.display())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakePrompter;
    use crate::github::gh_cli::{FakeGhCli, GhError};
    use crate::keychain::InMemoryKeychain;
    use tempfile::{TempDir, tempdir};

    /// A tempdir laid out like a home directory plus a repo checkout, with
    /// `ConfigPaths` pointing at both.
    struct TestEnv {
        _root: TempDir,
        home: PathBuf,
        paths: ConfigPaths,
    }

    fn test_env() -> TestEnv {
        let root = tempdir().expect("tempdir");
        let home = root.path().join("home");
        let repo_dir = root.path().join("repo");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&repo_dir).expect("create repo");
        let paths = ConfigPaths {
            global: home.join(".config/tskmstr/config.toml"),
            repo: Some(repo_dir.join(".tskmstr.toml")),
        };
        TestEnv {
            _root: root,
            home,
            paths,
        }
    }

    fn no_jira_factory(_cfg: &Config, _token: &str) -> Box<dyn TicketProvider> {
        panic!("jira client factory should not be called in this test");
    }

    fn no_hook_installer(_out: &mut dyn Write) -> Result<(), String> {
        panic!("hook installer should not be called in this test");
    }

    /// An `InitContext` for a GitHub-backed repo with an origin remote
    /// detected; hooks report installed so the hooks step stays quiet.
    fn github_ctx<'a>(
        env: &'a TestEnv,
        gh: &'a FakeGhCli,
        keychain: &'a InMemoryKeychain,
    ) -> InitContext<'a> {
        InitContext {
            paths: &env.paths,
            home: &env.home,
            keychain,
            env_token: None,
            jira_client_factory: &no_jira_factory,
            gh,
            origin_slug: Some("jowi-dev/widget".to_string()),
            origin_default_branch: Some("main".to_string()),
            hooks_installed: true,
            hook_installer: &no_hook_installer,
        }
    }

    #[test]
    fn github_flow_with_defaults_writes_config_and_creates_labels() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // No queued answers: every question takes its default.
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let config = config::load(&env.paths).expect("written config should load");
        assert_eq!(config.backend, BackendKind::Github);
        assert_eq!(config.github_repo.as_deref(), Some("jowi-dev/widget"));

        let labels = gh.label_create_calls();
        assert_eq!(labels.len(), 4, "all four tm:status/* labels");
        assert!(labels.iter().all(|(repo, ..)| repo == "jowi-dev/widget"));

        assert!(env.paths.global.exists(), "placeholder global config");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("tm board"), "board hint in: {rendered}");
    }

    #[test]
    fn github_flow_reprompts_on_invalid_backend_answer() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new().with_line("gitlab").with_line("github");
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("expected \"jira\" or \"github\""),
            "re-prompt notice in: {rendered}"
        );
        let config = config::load(&env.paths).expect("written config should load");
        assert_eq!(config.backend, BackendKind::Github);
    }

    #[test]
    fn github_flow_yes_accepts_all_defaults_without_prompting() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, true, &mut prompter, &mut out).expect("init should succeed");

        assert!(prompter.messages.is_empty(), "no prompts under --yes");
        let config = config::load(&env.paths).expect("written config should load");
        assert_eq!(config.github_repo.as_deref(), Some("jowi-dev/widget"));
        assert_eq!(gh.label_create_calls().len(), 4);
    }

    #[test]
    fn github_flow_yes_without_detected_origin_errors() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let mut ctx = github_ctx(&env, &gh, &keychain);
        ctx.origin_slug = None;

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        // Backend defaults to jira when no origin is detected, so pin github
        // via an existing repo file to hit the slug question.
        std::fs::write(
            env.paths.repo.as_ref().unwrap(),
            "[backend]\nprovider = \"github\"\n",
        )
        .expect("write repo config");
        let err = run_init(&ctx, true, &mut prompter, &mut out)
            .expect_err("--yes with no detectable slug should fail");
        assert!(matches!(err, InitCliError::MissingDefault { .. }));
    }

    #[test]
    fn github_flow_declining_labels_skips_label_creation() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new().with_confirm(false);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        assert!(gh.label_create_calls().is_empty());
        config::load(&env.paths).expect("config still written and loadable");
    }

    #[test]
    fn github_flow_label_failure_warns_but_completes() {
        let env = test_env();
        let gh = FakeGhCli::new().with_label_create_result(Err(GhError::Command {
            command: "gh label create".to_string(),
            exit_code: Some(1),
            stderr: "boom".to_string(),
        }));
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should still succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("warning") && rendered.contains("tm backend init-labels"),
            "label warning in: {rendered}"
        );
        config::load(&env.paths).expect("config written despite label failure");
    }

    #[test]
    fn github_flow_answered_slug_overrides_detected_default() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new()
            .with_line("github")
            .with_line("someone-else/other");
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let config = config::load(&env.paths).expect("written config should load");
        assert_eq!(config.github_repo.as_deref(), Some("someone-else/other"));
    }
}
