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

use crate::config::{self, BackendKind, Config, ConfigError, ConfigPaths, GlobalConfigSeed};
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
    let mut global_seed: Option<GlobalConfigSeed> = None;
    let mut needs_login = false;

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
        BackendKind::Jira => {
            let global_doc = read_global_doc(ctx);
            let global_field = |key: &str| -> Option<String> {
                global_doc
                    .as_ref()
                    .and_then(|d| jira_field(d, key))
                    .map(str::to_string)
            };
            // Defaults prefer a repo-local override, then the global config —
            // the same precedence `config::merge` resolves at load time.
            let default_for = |key: &str| -> String {
                jira_field(&doc, key)
                    .map(str::to_string)
                    .or_else(|| global_field(key))
                    .unwrap_or_default()
            };

            let base_url = ask_required(
                yes,
                prompter,
                out,
                "Jira base URL (e.g. https://your-site.atlassian.net)",
                &default_for("jira_base_url"),
                "the Jira base URL",
            )?;
            let email = ask_required(
                yes,
                prompter,
                out,
                "Jira email",
                &default_for("jira_email"),
                "the Jira email",
            )?;
            let project_key = ask_required(
                yes,
                prompter,
                out,
                "Default Jira project key",
                &default_for("default_project_key"),
                "the default Jira project key",
            )?
            .to_uppercase();

            set_str(
                &mut doc,
                &["backend"],
                "provider",
                "jira",
                &repo_config_path,
            )?;
            if ctx.paths.global.exists() {
                // The global config stays authoritative (restructuring it is
                // out of scope); only answers that differ from it land as
                // repo-local `[backend.jira]` overrides.
                for (key, answer) in [
                    ("jira_base_url", &base_url),
                    ("jira_email", &email),
                    ("default_project_key", &project_key),
                ] {
                    if global_field(key).as_deref() != Some(answer.as_str()) {
                        set_str(
                            &mut doc,
                            &["backend", "jira"],
                            key,
                            answer,
                            &repo_config_path,
                        )?;
                    }
                }
            } else {
                global_seed = Some(GlobalConfigSeed {
                    jira_base_url: base_url,
                    jira_email: email,
                    default_project_key: project_key,
                });
            }

            let token = match &ctx.env_token {
                Some(token) => Some(token.clone()),
                None => ctx.keychain.get_token()?,
            };
            needs_login = token.is_none();
        }
    }

    let scaffold = lane_step(ctx, yes, &mut doc, &repo_config_path, prompter, out)?;
    session_step(
        ctx,
        yes,
        &mut doc,
        &repo_config_path,
        prompter,
        out,
        &SessionSection {
            table: "audit",
            question: "Configure ticket audit sessions ([work.audit], the board's `a` key)?",
            default_prompt: "/ticket-audit {key}",
        },
    )?;
    session_step(
        ctx,
        yes,
        &mut doc,
        &repo_config_path,
        prompter,
        out,
        &SessionSection {
            table: "review_watch",
            question: "Configure review-watch sessions ([work.review_watch])?",
            default_prompt: "/bugbot-triage {key} {findings_file}",
        },
    )?;
    let install_hooks = !ctx.hooks_installed
        && ask_confirm(
            yes,
            prompter,
            "Install the Claude Code session hooks (tm work hooks install --user)?",
            false,
        )?;

    // --- Writes ---
    ensure_global_exists(ctx, global_seed.as_ref(), out)?;
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

    if let Some((path, contents)) = scaffold {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        writeln!(out, "Wrote {}", path.display())?;
    }

    // Prove the board can start from what was written.
    config::load(ctx.paths)?;

    // --- Side effects ---
    if needs_login {
        if yes {
            writeln!(
                out,
                "No Jira API token found. Run `tm auth login` to store one."
            )?;
        } else {
            writeln!(out, "No Jira API token found; starting `tm auth login`.")?;
            let auth_ctx = super::auth::AuthContext {
                paths: ctx.paths,
                keychain: ctx.keychain,
                env_token: ctx.env_token.clone(),
                jira_client_factory: ctx.jira_client_factory,
            };
            super::auth::login(&auth_ctx, prompter, out)?;
        }
    }

    if let Some(slug) = create_labels_in
        && let Err(err) = super::backend::init_labels(BackendKind::Github, &slug, ctx.gh, out)
    {
        writeln!(
            out,
            "warning: creating labels failed: {err}. Run `tm backend init-labels` once resolved."
        )?;
    }

    if install_hooks {
        if let Err(message) = (ctx.hook_installer)(out) {
            writeln!(out, "warning: hook install failed: {message}")?;
        }
    } else if !ctx.hooks_installed {
        writeln!(
            out,
            "hint: run `tm work hooks install --user` to enable session telemetry."
        )?;
    }

    writeln!(out)?;
    writeln!(out, "Done. Run `tm board` to open the board.")?;
    Ok(())
}

/// One of the optional `[work.audit]` / `[work.review_watch]` sections the
/// wizard can fill in.
struct SessionSection {
    /// Table name under `[work]`.
    table: &'static str,
    /// The confirm question enabling the section.
    question: &'static str,
    /// The prompt the section runs when none is configured, whose leading
    /// `/skill` is checked for existence.
    default_prompt: &'static str,
}

/// Ask about an optional session section and write its `dir`. The prompt's
/// skill is user-supplied, not shipped with tm, so a missing one warns with
/// the expected paths instead of failing.
#[allow(clippy::too_many_arguments)]
fn session_step(
    ctx: &InitContext,
    yes: bool,
    doc: &mut DocumentMut,
    repo_config_path: &Path,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
    section: &SessionSection,
) -> Result<(), InitCliError> {
    let repo_dir = repo_config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let present = doc
        .get("work")
        .and_then(Item::as_table_like)
        .and_then(|work| work.get(section.table))
        .is_some();
    if !ask_confirm(yes, prompter, section.question, present)? {
        return Ok(());
    }

    // review_watch's dir falls back to audit's at load time; offer the same
    // fallback as the default here.
    let dir_default = str_at(doc, &["work", section.table, "dir"])
        .or_else(|| str_at(doc, &["work", "audit", "dir"]))
        .unwrap_or(".")
        .to_string();
    let dir = ask_required(
        yes,
        prompter,
        out,
        &format!(
            "Session directory for [work.{}] (\".\" = this repo)",
            section.table
        ),
        &dir_default,
        "the session directory",
    )?;
    set_str(doc, &["work", section.table], "dir", &dir, repo_config_path)?;

    let prompt = str_at(doc, &["work", section.table, "prompt"])
        .unwrap_or(section.default_prompt)
        .to_string();
    let resolved_dir = resolve_repo_relative(&dir, &repo_dir, ctx.home);
    warn_if_skill_missing(out, &prompt, &resolved_dir, ctx.home)?;
    Ok(())
}

/// Warn when the skill a session prompt invokes (its leading `/name` token)
/// exists neither in the session directory's repo-local skills nor in the
/// user-level ones.
fn warn_if_skill_missing(
    out: &mut dyn Write,
    prompt: &str,
    dir: &Path,
    home: &Path,
) -> io::Result<()> {
    let Some(skill) = prompt
        .split_whitespace()
        .next()
        .and_then(|first| first.strip_prefix('/'))
    else {
        return Ok(());
    };
    let repo_skill = dir.join(".claude/skills").join(skill);
    let home_skill = home.join(".claude/skills").join(skill);
    if repo_skill.exists() || home_skill.exists() {
        return Ok(());
    }
    writeln!(
        out,
        "warning: the /{skill} skill is user-supplied (tm does not ship it); expected at {} or {}.",
        repo_skill.display(),
        home_skill.display()
    )
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

/// Ask about a work lane and write it into `doc`: `repo = "."` by default,
/// an explicit `base_branch` (the `origin/HEAD` fallback fails on clones
/// where that ref was never set), and a `prompt_file`. Returns a starter
/// prompt to scaffold, if the user wants one.
fn lane_step(
    ctx: &InitContext,
    yes: bool,
    doc: &mut DocumentMut,
    repo_config_path: &Path,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<Option<(PathBuf, String)>, InitCliError> {
    let repo_dir = repo_config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let existing_lanes: Vec<String> = doc
        .get("work")
        .and_then(Item::as_table_like)
        .and_then(|work| work.get("lanes"))
        .and_then(Item::as_table_like)
        .map(|lanes| lanes.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();

    let wanted = if existing_lanes.is_empty() {
        ask_confirm(
            yes,
            prompter,
            "Configure a work lane for this repo (used by the board's `w` key)?",
            true,
        )?
    } else {
        writeln!(out, "Existing lanes: {}", existing_lanes.join(", "))?;
        ask_confirm(yes, prompter, "Add or update a lane?", false)?
    };
    if !wanted {
        return Ok(None);
    }

    let name_default = repo_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = ask_required(
        yes,
        prompter,
        out,
        "Lane name",
        &name_default,
        "the lane name",
    )?;

    let existing = |field: &str| -> Option<String> {
        str_at(doc, &["work", "lanes", name.as_str(), field]).map(str::to_string)
    };
    let repo = ask_required(
        yes,
        prompter,
        out,
        "Lane repo (\".\" = this repo)",
        &existing("repo").unwrap_or_else(|| ".".to_string()),
        "the lane repo",
    )?;
    let base_branch = ask_required(
        yes,
        prompter,
        out,
        "Lane base branch",
        &existing("base_branch")
            .or_else(|| ctx.origin_default_branch.clone())
            .unwrap_or_else(|| "main".to_string()),
        "the lane base branch",
    )?;
    let prompt_file = ask_required(
        yes,
        prompter,
        out,
        "Lane prompt file",
        &existing("prompt_file").unwrap_or_else(|| format!("prompts/{name}-lane.md")),
        "the lane prompt file",
    )?;

    let lane_path = ["work", "lanes", name.as_str()];
    set_str(doc, &lane_path, "repo", &repo, repo_config_path)?;
    set_str(
        doc,
        &lane_path,
        "base_branch",
        &base_branch,
        repo_config_path,
    )?;
    set_str(
        doc,
        &lane_path,
        "prompt_file",
        &prompt_file,
        repo_config_path,
    )?;

    let resolved = resolve_repo_relative(&prompt_file, &repo_dir, ctx.home);
    if resolved.exists() {
        return Ok(None);
    }
    if ask_confirm(
        yes,
        prompter,
        &format!("Scaffold a starter lane prompt at {}?", resolved.display()),
        true,
    )? {
        Ok(Some((resolved, lane_prompt_template(&name))))
    } else {
        writeln!(
            out,
            "warning: no prompt file at {}; `tm work run {name} <KEY>` will fail preflight until it exists.",
            resolved.display()
        )?;
        Ok(None)
    }
}

/// Where a config path value (lane `prompt_file`, session `dir`) points on
/// disk. Relative paths resolve against the repo root here (a lane
/// `prompt_file` resolves against `tm`'s invocation directory at run time,
/// which is normally the same place).
fn resolve_repo_relative(value: &str, repo_dir: &Path, home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_dir.join(path)
    }
}

/// Starter contents for a scaffolded lane prompt file.
fn lane_prompt_template(lane: &str) -> String {
    format!(
        "# {lane} work lane\n\
         \n\
         Autonomous work session for a single ticket in this repository. Do\n\
         not scope-creep beyond the named ticket.\n\
         \n\
         ## Start\n\
         \n\
         1. Run `tm ready <KEY>` and stop if it reports the ticket blocked.\n\
         2. Work only `<KEY>`. Note unrelated bugs or cleanup as follow-ups\n\
            instead of fixing them here.\n\
         \n\
         ## Workflow\n\
         \n\
         - Write a failing test before the implementation that makes it pass.\n\
         - Keep commits small and focused, one logical change per commit.\n\
         \n\
         ## Before finishing\n\
         \n\
         <!-- List the checks a run must leave green, e.g. your formatter,\n\
              linter, and test suite. -->\n"
    )
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
/// without one, so `tm init` must be reachable before it exists. The Jira
/// path seeds it with the answered fields; the GitHub backend needs no
/// global fields, so a placeholder is enough.
fn ensure_global_exists(
    ctx: &InitContext,
    seed: Option<&GlobalConfigSeed>,
    out: &mut dyn Write,
) -> Result<(), InitCliError> {
    if ctx.paths.global.exists() {
        return Ok(());
    }
    match seed {
        Some(seed) => config::write_global(&ctx.paths.global, seed)?,
        None => {
            if let Some(parent) = ctx.paths.global.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &ctx.paths.global,
                "# tskmstr global config (created by `tm init`)\n",
            )?;
        }
    }
    writeln!(out, "Wrote {}", ctx.paths.global.display())?;
    Ok(())
}

/// Parse the global config for offering its values as defaults; `None` when
/// it is missing or unparseable (`config::load` reports the latter later).
fn read_global_doc(ctx: &InitContext) -> Option<DocumentMut> {
    let text = std::fs::read_to_string(&ctx.paths.global).ok()?;
    text.parse().ok()
}

/// Read a Jira config field the way `config::merge` resolves it within one
/// file: the `[backend.jira]` table wins over the legacy flat key.
fn jira_field<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a str> {
    str_at(doc, &["backend", "jira", key]).or_else(|| str_at(doc, &[key]))
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

    fn ok_jira_factory(_cfg: &Config, _token: &str) -> Box<dyn TicketProvider> {
        Box::new(crate::ticketing::provider::JiraProvider::new(
            crate::jira::fake::FakeJiraClient::new().with_myself(crate::ticketing::types::Myself {
                account_id: "acct-1".to_string(),
                display_name: "Jane Doe".to_string(),
                email_address: None,
            }),
        ))
    }

    /// An `InitContext` with no origin remote, so the backend defaults to
    /// Jira.
    fn jira_ctx<'a>(
        env: &'a TestEnv,
        gh: &'a FakeGhCli,
        keychain: &'a InMemoryKeychain,
    ) -> InitContext<'a> {
        InitContext {
            paths: &env.paths,
            home: &env.home,
            keychain,
            env_token: None,
            jira_client_factory: &ok_jira_factory,
            gh,
            origin_slug: None,
            origin_default_branch: None,
            hooks_installed: true,
            hook_installer: &no_hook_installer,
        }
    }

    fn write_jira_global(path: &Path) {
        config::write_global(
            path,
            &config::GlobalConfigSeed {
                jira_base_url: "https://example.atlassian.net".to_string(),
                jira_email: "dev@example.com".to_string(),
                default_project_key: "PROJ".to_string(),
            },
        )
        .expect("write global config");
    }

    #[test]
    fn jira_flow_fresh_writes_global_and_hands_off_to_auth_login() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = jira_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new()
            .with_line("jira")
            .with_line("https://example.atlassian.net")
            .with_line("dev@example.com")
            .with_line("proj")
            .with_password("super-secret-token");
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let config = config::load(&env.paths).expect("written config should load");
        assert_eq!(config.backend, BackendKind::Jira);
        assert_eq!(config.jira_base_url, "https://example.atlassian.net");
        assert_eq!(config.default_project_key, "PROJ", "project key uppercased");

        assert_eq!(
            keychain.get_token().expect("keychain readable").as_deref(),
            Some("super-secret-token"),
            "login handoff stored the validated token"
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("Authenticated as Jane Doe"),
            "login output in: {rendered}"
        );
    }

    #[test]
    fn jira_flow_existing_global_keeps_defaults_out_of_repo_config() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::with_token("existing-token");
        let ctx = jira_ctx(&env, &gh, &keychain);
        write_jira_global(&env.paths.global);

        // Only the backend question is answered; the three Jira fields take
        // the defaults offered from the existing global config.
        let mut prompter = FakePrompter::new().with_line("jira");
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_text = std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read");
        assert!(
            !repo_text.contains("jira_base_url"),
            "unchanged values stay in the global config: {repo_text}"
        );
        let config = config::load(&env.paths).expect("config should load");
        assert_eq!(config.default_project_key, "PROJ");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            !rendered.contains("Authenticated"),
            "no login handoff when a token already resolves: {rendered}"
        );
    }

    #[test]
    fn jira_flow_differing_answer_lands_as_repo_local_override() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::with_token("existing-token");
        let ctx = jira_ctx(&env, &gh, &keychain);
        write_jira_global(&env.paths.global);

        // FakePrompter returns queued answers verbatim (no empty-line ->
        // default mapping), so "keeping" a value means answering with it.
        let mut prompter = FakePrompter::new()
            .with_line("jira")
            .with_line("https://example.atlassian.net")
            .with_line("dev@example.com")
            .with_line("other");
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_text = std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read");
        assert!(
            repo_text.contains("default_project_key = \"OTHER\""),
            "override in repo config: {repo_text}"
        );
        assert!(
            !repo_text.contains("jira_base_url"),
            "unchanged fields stay global: {repo_text}"
        );
        let config = config::load(&env.paths).expect("config should load");
        assert_eq!(config.default_project_key, "OTHER");
        assert_eq!(config.jira_base_url, "https://example.atlassian.net");
    }

    #[test]
    fn jira_flow_yes_with_existing_global_prints_login_hint() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let mut ctx = jira_ctx(&env, &gh, &keychain);
        ctx.jira_client_factory = &no_jira_factory;
        write_jira_global(&env.paths.global);

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, true, &mut prompter, &mut out).expect("init should succeed");

        assert!(prompter.messages.is_empty(), "no prompts under --yes");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("tm auth login"),
            "login hint in: {rendered}"
        );
    }

    #[test]
    fn jira_flow_yes_without_global_defaults_errors() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = jira_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        let err = run_init(&ctx, true, &mut prompter, &mut out)
            .expect_err("--yes with nothing to default from should fail");
        assert!(matches!(err, InitCliError::MissingDefault { .. }));
    }

    #[test]
    fn lane_step_scaffolds_lane_and_prompt_by_default() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let config = config::load(&env.paths).expect("written config should load");
        // The lane name defaults to the repo directory name ("repo" in the
        // test env); base_branch comes from the detected origin/HEAD.
        let lane = config
            .work
            .lanes
            .get("repo")
            .expect("lane scaffolded under the repo dir name");
        assert_eq!(lane.base_branch.as_deref(), Some("main"));
        assert_eq!(lane.prompt_file.as_deref(), Some("prompts/repo-lane.md"));

        let repo_dir = env.paths.repo.as_ref().unwrap().parent().unwrap();
        let prompt_path = repo_dir.join("prompts/repo-lane.md");
        let template = std::fs::read_to_string(&prompt_path).expect("starter prompt scaffolded");
        assert!(template.contains("work lane"), "template body: {template}");
    }

    #[test]
    fn lane_step_declined_writes_no_work_section() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // Confirms pop in order: labels yes, lane no.
        let mut prompter = FakePrompter::new().with_confirm(true).with_confirm(false);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_text = std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read");
        assert!(!repo_text.contains("[work"), "no work section: {repo_text}");
        let repo_dir = env.paths.repo.as_ref().unwrap().parent().unwrap();
        assert!(!repo_dir.join("prompts").exists(), "no prompt scaffolded");
    }

    #[test]
    fn lane_step_rerun_with_existing_lane_changes_nothing_by_default() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let original = "[backend]\nprovider = \"github\"\n\n[backend.github]\nrepo = \"jowi-dev/widget\"\n\n# keep me\n[work.lanes.mylane]\nrepo = \".\"\nbase_branch = \"develop\"\nprompt_file = \"prompts/custom.md\"\n";
        let repo_config = env.paths.repo.as_ref().unwrap();
        std::fs::write(repo_config, original).expect("write repo config");
        let repo_dir = repo_config.parent().unwrap();
        std::fs::create_dir_all(repo_dir.join("prompts")).expect("mkdir");
        std::fs::write(repo_dir.join("prompts/custom.md"), "# custom\n").expect("write prompt");

        // Accept every default: existing lanes mean the lane question
        // defaults to "no", so a plain re-run must change nothing.
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let after = std::fs::read_to_string(repo_config).expect("read");
        assert_eq!(after, original, "re-run must not rewrite the file");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("already up to date"),
            "no-op notice in: {rendered}"
        );
    }

    #[test]
    fn lane_step_updating_existing_lane_offers_current_values_as_defaults() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let original = "[backend]\nprovider = \"github\"\n\n[backend.github]\nrepo = \"jowi-dev/widget\"\n\n# keep me\n[work.lanes.mylane]\nrepo = \".\"\nbase_branch = \"develop\"\nprompt_file = \"prompts/custom.md\"\n";
        let repo_config = env.paths.repo.as_ref().unwrap();
        std::fs::write(repo_config, original).expect("write repo config");
        let repo_dir = repo_config.parent().unwrap();
        std::fs::create_dir_all(repo_dir.join("prompts")).expect("mkdir");
        std::fs::write(repo_dir.join("prompts/custom.md"), "# custom\n").expect("write prompt");

        // Update the lane but keep every value: lines pop in question order
        // (backend, slug, lane name), and only the lane name diverges from
        // its default; repo/base_branch/prompt_file fall back to the current
        // values as defaults.
        let mut prompter = FakePrompter::new()
            .with_line("github")
            .with_line("jowi-dev/widget")
            .with_line("mylane")
            .with_confirm(true) // labels
            .with_confirm(true); // update lane
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let after = std::fs::read_to_string(repo_config).expect("read");
        assert_eq!(
            after, original,
            "keeping the current values must not rewrite the file"
        );
        let config = config::load(&env.paths).expect("config should load");
        assert_eq!(
            config
                .work
                .lanes
                .get("mylane")
                .unwrap()
                .base_branch
                .as_deref(),
            Some("develop")
        );
    }

    #[test]
    fn lane_step_declining_prompt_scaffold_warns_with_expected_path() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // Confirms pop in order: labels yes, lane yes, scaffold no.
        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(true)
            .with_confirm(false);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_dir = env.paths.repo.as_ref().unwrap().parent().unwrap();
        assert!(!repo_dir.join("prompts/repo-lane.md").exists());
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("warning") && rendered.contains("prompts/repo-lane.md"),
            "missing-prompt warning in: {rendered}"
        );
        let config = config::load(&env.paths).expect("config should load");
        assert!(config.work.lanes.contains_key("repo"), "lane still written");
    }

    #[test]
    fn audit_step_configures_dir_and_warns_about_missing_skill() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // Confirms pop in order: labels yes, lane no, audit yes.
        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(true);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_text = std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read");
        assert!(repo_text.contains("[work.audit]"), "audit in: {repo_text}");
        let config = config::load(&env.paths).expect("config should load");
        assert!(config.work.audit.dir.is_some(), "audit dir parsed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("warning")
                && rendered.contains("ticket-audit")
                && rendered.contains(".claude/skills/ticket-audit"),
            "user-supplied skill warning with expected path in: {rendered}"
        );
    }

    #[test]
    fn audit_step_present_skill_produces_no_warning() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);
        std::fs::create_dir_all(env.home.join(".claude/skills/ticket-audit"))
            .expect("create skill dir");

        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(true);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            !rendered.contains("ticket-audit skill"),
            "no warning when the skill exists: {rendered}"
        );
    }

    #[test]
    fn review_watch_step_configures_dir_and_warns_about_bugbot_skill() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // Confirms: labels yes, lane no, audit no, review-watch yes.
        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(true);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let repo_text = std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read");
        assert!(
            repo_text.contains("[work.review_watch]"),
            "review_watch in: {repo_text}"
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("bugbot-triage"),
            "bugbot skill warning in: {rendered}"
        );
    }

    #[test]
    fn hooks_offer_runs_installer_on_confirm() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let mut ctx = github_ctx(&env, &gh, &keychain);
        ctx.hooks_installed = false;
        let installer = |out: &mut dyn Write| -> Result<(), String> {
            writeln!(out, "installer ran").map_err(|err| err.to_string())
        };
        ctx.hook_installer = &installer;

        // Confirms: labels yes, lane no, audit no, review-watch no, hooks yes.
        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(true);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("installer ran"), "in: {rendered}");
    }

    #[test]
    fn hooks_offer_declined_prints_install_hint() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let mut ctx = github_ctx(&env, &gh, &keychain);
        ctx.hooks_installed = false;

        // All confirm defaults: the hooks question defaults to "no".
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("tm work hooks install --user"),
            "hint in: {rendered}"
        );
    }

    #[test]
    fn hooks_installer_failure_warns_but_completes() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let mut ctx = github_ctx(&env, &gh, &keychain);
        ctx.hooks_installed = false;
        let installer = |_out: &mut dyn Write| -> Result<(), String> { Err("boom".to_string()) };
        ctx.hook_installer = &installer;

        let mut prompter = FakePrompter::new()
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(true);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should still succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("warning") && rendered.contains("boom"),
            "install failure warning in: {rendered}"
        );
    }

    #[test]
    fn rerun_declining_the_write_leaves_the_file_untouched() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        let original = "[backend]\nprovider = \"github\"\n\n[backend.github]\nrepo = \"jowi-dev/widget\"\n";
        let repo_config = env.paths.repo.as_ref().unwrap();
        std::fs::write(repo_config, original).expect("write repo config");

        // Change the slug, then decline the write. Confirms pop in order:
        // labels yes, lane no, audit no, review-watch no, write no.
        let mut prompter = FakePrompter::new()
            .with_line("github")
            .with_line("someone-else/other")
            .with_confirm(true)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(false)
            .with_confirm(false);
        let mut out = Vec::new();
        run_init(&ctx, false, &mut prompter, &mut out).expect("init should succeed");

        let after = std::fs::read_to_string(repo_config).expect("read");
        assert_eq!(after, original, "declined write must change nothing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("will become:") && rendered.contains("someone-else/other"),
            "preview shown before asking in: {rendered}"
        );
        assert!(
            rendered.contains("unchanged"),
            "decline notice in: {rendered}"
        );
    }

    #[test]
    fn rerun_yes_writes_additions_without_prompting() {
        let env = test_env();
        let gh = FakeGhCli::new();
        let keychain = InMemoryKeychain::empty();
        let ctx = github_ctx(&env, &gh, &keychain);

        // Backend configured, but no lane yet: --yes scaffolds the lane and
        // overwrites without asking.
        let original = "# my notes\n[backend]\nprovider = \"github\"\n\n[backend.github]\nrepo = \"jowi-dev/widget\"\n";
        let repo_config = env.paths.repo.as_ref().unwrap();
        std::fs::write(repo_config, original).expect("write repo config");

        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_init(&ctx, true, &mut prompter, &mut out).expect("init should succeed");

        assert!(prompter.messages.is_empty(), "no prompts under --yes");
        let after = std::fs::read_to_string(repo_config).expect("read");
        assert!(
            after.contains("[work.lanes.repo]"),
            "lane added under --yes: {after}"
        );
        assert!(
            after.contains("# my notes"),
            "existing comments preserved: {after}"
        );
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
