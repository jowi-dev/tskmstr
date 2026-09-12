//! `tm check`: a read-only drift report for a repo already onboarded by `tm
//! init` (GitHub issue #38). It answers "is this repo up to date with the
//! tskmstr binary running it?" two ways:
//!
//! - **The schema_version stamp** ([`crate::config::manifest`]): does the
//!   repo's `.tskmstr.toml` carry the revision this binary expects, an older
//!   one, or a newer one?
//! - **The same structural presence checks `tm init` runs and discards**:
//!   does every configured lane have a prompt file where a run would look
//!   for it, and does every configured session's leading `/skill` exist
//!   somewhere this binary would find it?
//!
//! Both checks are presence-only. `tm check` never opens a scaffolded
//! asset's *contents* — lane prompts and skills are meant to be edited after
//! `tm init` writes them (GitHub issue #30's agent-assisted setup edits them
//! on purpose), so a content diff would flag normal customization as drift.
//! See [`crate::config::manifest::CURRENT_SCHEMA_VERSION`]'s doc comment for
//! the same point made from the stamp's side.
//!
//! This module performs no filesystem writes: it is safe to run at any time,
//! including in CI, without risking the repo-local config or any scaffolded
//! asset.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;
use toml_edit::{DocumentMut, Item};

use crate::agent::AgentRunner;
use crate::config::ConfigPaths;
use crate::config::manifest::{self, StampStatus};

use super::init::{existing_lane_prompt_path, resolve_repo_relative, str_at};

/// Errors surfaced by `tm check`. An un-onboarded repo (no `.tskmstr.toml`
/// at all) is one of these, not a drift finding: there is nothing to report
/// drift against.
#[derive(Debug, Error)]
pub enum CheckCliError {
    /// `.tskmstr.toml` doesn't exist. Distinct from
    /// [`CheckCliError::ParseRepoConfig`]: a missing file means the repo was
    /// never onboarded (or the command is running outside one), not that an
    /// existing file has gone bad.
    #[error("no {path} found; this repo hasn't been onboarded — run `tm init` first")]
    MissingRepoConfig { path: PathBuf },
    /// `ctx.paths.repo` itself is `None` (no working directory to resolve a
    /// repo-local config against), mirroring [`super::init::InitCliError::NoRepoDir`].
    #[error("cannot determine where .tskmstr.toml would live (no working directory)")]
    NoRepoDir,
    /// The repo config exists but isn't valid TOML. Mirrors
    /// [`super::init::InitCliError::ParseRepoConfig`]'s shape so the two
    /// commands report the same failure the same way.
    #[error("failed to parse {path}: {source}")]
    ParseRepoConfig {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Dependencies for [`run_check`], mirroring `InitContext` but trimmed to
/// what a read-only report needs: no keychain, no ticket provider, no
/// prompter (there is nothing to ask).
pub struct CheckContext<'a> {
    /// Where the repo-local `.tskmstr.toml` lives.
    pub paths: &'a ConfigPaths,
    /// Home directory, for resolving `~/`-rooted paths and user-level
    /// skills, matching `tm init`'s resolution rules.
    pub home: &'a Path,
    /// The AI coding agent this repo is configured to use (or the default,
    /// when unconfigured) — resolves default session prompts and skill
    /// directories the same way `tm init` does.
    pub runner: &'a dyn AgentRunner,
}

/// One line of drift between this repo's `.tskmstr.toml` and what the
/// running `tm init` would expect of it today. Each variant renders as
/// exactly one human-readable, actionable line via [`std::fmt::Display`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftFinding {
    /// No `schema_version` key was found (or it wasn't an integer — treated
    /// identically to absent, since a hand-edited non-integer value carries
    /// no usable version information either way; see [`stamp_finding`]'s
    /// doc comment for the reasoning).
    StampMissing,
    /// The stamp is older than [`manifest::CURRENT_SCHEMA_VERSION`]: an
    /// older tskmstr onboarded this repo.
    StampStale { found: i64 },
    /// The stamp is newer than [`manifest::CURRENT_SCHEMA_VERSION`]: a
    /// newer tskmstr onboarded this repo than is running this check.
    StampNewer { found: i64 },
    /// A configured lane's prompt file doesn't exist where a lane run would
    /// look for it — the same check `tm init`'s
    /// `audit_existing_lane_prompts` runs on every re-run, but reported
    /// instead of offered a scaffold.
    MissingLanePrompt { lane: String, path: PathBuf },
    /// A configured session's prompt leads with a `/skill` invocation that
    /// exists neither repo-locally nor at the user level. `session` names
    /// which `[work.*]` section it came from (e.g. `"work.audit"`).
    MissingSkill {
        name: String,
        session: String,
        repo_path: PathBuf,
        home_path: PathBuf,
    },
}

impl std::fmt::Display for DriftFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriftFinding::StampMissing => write!(
                f,
                "no schema_version stamp (onboarded before stamping existed); run `tm init` to stamp it"
            ),
            DriftFinding::StampStale { found } => write!(
                f,
                "schema_version stamp is {found}, older than this tskmstr's {} \
                 (onboarded by an older tskmstr); re-run `tm init` to catch it up",
                manifest::CURRENT_SCHEMA_VERSION
            ),
            DriftFinding::StampNewer { found } => write!(
                f,
                "schema_version stamp is {found}, newer than this tskmstr's {} \
                 (onboarded by a newer tskmstr); update tskmstr",
                manifest::CURRENT_SCHEMA_VERSION
            ),
            DriftFinding::MissingLanePrompt { lane, path } => write!(
                f,
                "lane `{lane}` has no prompt file at {} (a lane run fails preflight without it)",
                path.display()
            ),
            DriftFinding::MissingSkill {
                name,
                session,
                repo_path,
                home_path,
            } => write!(
                f,
                "[{session}]'s /{name} skill exists neither at {} nor {}",
                repo_path.display(),
                home_path.display()
            ),
        }
    }
}

/// `tm check`: read `.tskmstr.toml` and report every [`DriftFinding`]
/// between it and what the running `tm init` would expect, writing a
/// human-readable report to `out`. Performs no filesystem writes.
///
/// `quiet` collapses the report to exactly one line — a summary suitable for
/// scripting — instead of one line per finding.
///
/// A missing or unparseable `.tskmstr.toml` is a [`CheckCliError`], not
/// drift: this command only makes sense for a repo `tm init` already
/// touched.
pub fn run_check(
    ctx: &CheckContext,
    quiet: bool,
    out: &mut dyn Write,
) -> Result<Vec<DriftFinding>, CheckCliError> {
    let repo_config_path = ctx.paths.repo.clone().ok_or(CheckCliError::NoRepoDir)?;
    if !repo_config_path.exists() {
        return Err(CheckCliError::MissingRepoConfig {
            path: repo_config_path,
        });
    }
    let text = std::fs::read_to_string(&repo_config_path)?;
    let doc: DocumentMut = text
        .parse()
        .map_err(|source| CheckCliError::ParseRepoConfig {
            path: repo_config_path.clone(),
            source,
        })?;

    let repo_dir = repo_config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut findings = Vec::new();
    findings.extend(stamp_finding(&doc));
    findings.extend(lane_findings(ctx, &doc, &repo_dir));
    findings.extend(session_findings(ctx, &doc, &repo_dir));

    render(&findings, quiet, out)?;
    Ok(findings)
}

/// The stamp half of the report: read the top-level `schema_version` key
/// and compare it against [`manifest::CURRENT_SCHEMA_VERSION`] via
/// [`manifest::stamp_status`].
///
/// A key present but not an integer (e.g. hand-edited to a string) is read
/// as `None` by `Item::as_integer`, which folds it into
/// [`StampStatus::Missing`] the same as a wholly absent key — there is no
/// usable version to compare in either case, and reporting it identically
/// to "never stamped" is the more actionable message (it tells the user to
/// run `tm init`, which fixes both).
fn stamp_finding(doc: &DocumentMut) -> Option<DriftFinding> {
    let found = doc.get("schema_version").and_then(Item::as_integer);
    match manifest::stamp_status(found) {
        StampStatus::Current => None,
        StampStatus::Missing => Some(DriftFinding::StampMissing),
        StampStatus::Stale(found) => Some(DriftFinding::StampStale { found }),
        StampStatus::Newer(found) => Some(DriftFinding::StampNewer { found }),
    }
}

/// The lane half of the report: for every `[work.lanes.<name>]`, resolve its
/// prompt path exactly as `tm init`'s `existing_lane_prompt_path` does and
/// flag any that's missing. Lanes are visited in the document's own order.
fn lane_findings(ctx: &CheckContext, doc: &DocumentMut, repo_dir: &Path) -> Vec<DriftFinding> {
    let Some(lanes) = doc
        .get("work")
        .and_then(Item::as_table_like)
        .and_then(|work| work.get("lanes"))
        .and_then(Item::as_table_like)
    else {
        return Vec::new();
    };

    lanes
        .iter()
        .filter_map(|(lane, _)| {
            let resolved = existing_lane_prompt_path(ctx.runner, ctx.home, doc, lane, repo_dir);
            if resolved.exists() {
                None
            } else {
                Some(DriftFinding::MissingLanePrompt {
                    lane: lane.to_string(),
                    path: resolved,
                })
            }
        })
        .collect()
}

/// One optional `[work.*]` session section this command probes for a
/// missing skill, mirroring `tm init`'s `SessionSection`.
struct Session {
    /// Table name under `[work]`.
    table: &'static str,
    /// The prompt template used when the section sets no `prompt` key.
    default_prompt: &'static str,
}

/// The session half of the report: for `[work.audit]` and
/// `[work.review_watch]`, when present, resolve the configured (or default)
/// session prompt's leading `/skill` and flag it when it exists nowhere
/// this binary would look.
fn session_findings(ctx: &CheckContext, doc: &DocumentMut, repo_dir: &Path) -> Vec<DriftFinding> {
    let sessions = [
        Session {
            table: "audit",
            default_prompt: ctx.runner.default_audit_prompt_template(),
        },
        Session {
            table: "review_watch",
            default_prompt: ctx.runner.default_cleanup_prompt_template(),
        },
    ];

    sessions
        .iter()
        .filter_map(|section| session_finding(ctx, doc, repo_dir, section))
        .collect()
}

/// Check a single session section, returning `None` when the section is
/// absent, its prompt names no `/skill`, or the skill is found.
fn session_finding(
    ctx: &CheckContext,
    doc: &DocumentMut,
    repo_dir: &Path,
    section: &Session,
) -> Option<DriftFinding> {
    let present = doc
        .get("work")
        .and_then(Item::as_table_like)
        .and_then(|work| work.get(section.table))
        .is_some();
    if !present {
        return None;
    }

    let prompt = str_at(doc, &["work", section.table, "prompt"]).unwrap_or(section.default_prompt);
    let skill = prompt
        .split_whitespace()
        .next()
        .and_then(|first| first.strip_prefix('/'))?;

    // review_watch's `dir` falls back to audit's at load time (mirroring
    // `config::merge`'s resolution); a bare "." is the ultimate fallback
    // when neither section sets one.
    let dir = str_at(doc, &["work", section.table, "dir"])
        .or_else(|| str_at(doc, &["work", "audit", "dir"]))
        .unwrap_or(".");
    let resolved_dir = resolve_repo_relative(dir, repo_dir, ctx.home);

    let repo_path = ctx.runner.skills_dir(&resolved_dir).join(skill);
    let home_path = ctx.runner.skills_dir(ctx.home).join(skill);
    if repo_path.exists() || home_path.exists() {
        return None;
    }

    Some(DriftFinding::MissingSkill {
        name: skill.to_string(),
        session: format!("work.{}", section.table),
        repo_path,
        home_path,
    })
}

/// Render the report. Non-quiet: one line per finding, then a blank line and
/// a closing hint when drift exists, or a single "up to date" line when
/// clean. Quiet: exactly one summary line either way.
fn render(findings: &[DriftFinding], quiet: bool, out: &mut dyn Write) -> io::Result<()> {
    if quiet {
        if findings.is_empty() {
            writeln!(
                out,
                "up to date (schema_version {})",
                manifest::CURRENT_SCHEMA_VERSION
            )?;
        } else {
            writeln!(
                out,
                "{} drift finding(s); run `tm check` for details",
                findings.len()
            )?;
        }
        return Ok(());
    }

    if findings.is_empty() {
        writeln!(
            out,
            "up to date (schema_version {})",
            manifest::CURRENT_SCHEMA_VERSION
        )?;
        return Ok(());
    }

    for finding in findings {
        writeln!(out, "{finding}")?;
    }
    writeln!(out)?;
    writeln!(out, "Run `tm init` to address the drift above.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::config::ConfigPaths;
    use tempfile::{TempDir, tempdir};

    /// A tempdir laid out like a home directory plus a repo checkout, with
    /// `ConfigPaths` pointing at both — mirrors `init`'s test harness.
    struct TestEnv {
        _root: TempDir,
        home: PathBuf,
        repo_dir: PathBuf,
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
            repo_dir,
            paths,
        }
    }

    fn ctx<'a>(env: &'a TestEnv, runner: &'a dyn AgentRunner) -> CheckContext<'a> {
        CheckContext {
            paths: &env.paths,
            home: &env.home,
            runner,
        }
    }

    fn write_repo_config(env: &TestEnv, contents: &str) {
        std::fs::write(env.paths.repo.as_ref().unwrap(), contents).expect("write repo config");
    }

    #[test]
    fn clean_repo_reports_no_findings() {
        let env = test_env();
        let lane_prompt = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        std::fs::create_dir_all(lane_prompt.parent().unwrap()).expect("mkdir");
        std::fs::write(&lane_prompt, "lane prompt").expect("write lane prompt");
        let skill_dir = env.repo_dir.join(".claude/skills/ticket-audit");
        std::fs::create_dir_all(&skill_dir).expect("mkdir skill");

        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n\
             [work.audit]\n\
             dir = \".\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert!(findings.is_empty(), "expected no findings: {findings:?}");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("up to date (schema_version 1)"),
            "clean report in: {rendered}"
        );
    }

    #[test]
    fn missing_stamp_is_reported() {
        let env = test_env();
        write_repo_config(&env, "");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert_eq!(findings, vec![DriftFinding::StampMissing]);
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("no schema_version stamp"),
            "missing-stamp line in: {rendered}"
        );
        assert!(rendered.contains("tm init"), "closing hint in: {rendered}");
    }

    #[test]
    fn non_integer_stamp_is_treated_as_missing() {
        let env = test_env();
        write_repo_config(&env, "schema_version = \"one\"\n");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert_eq!(findings, vec![DriftFinding::StampMissing]);
    }

    #[test]
    fn stale_stamp_is_reported() {
        let env = test_env();
        write_repo_config(&env, "schema_version = 0\n");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert_eq!(findings, vec![DriftFinding::StampStale { found: 0 }]);
    }

    #[test]
    fn newer_stamp_is_reported() {
        let env = test_env();
        write_repo_config(&env, "schema_version = 999\n");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert_eq!(findings, vec![DriftFinding::StampNewer { found: 999 }]);
    }

    #[test]
    fn lane_with_missing_prompt_file_is_reported() {
        let env = test_env();
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        let expected_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        assert_eq!(
            findings,
            vec![DriftFinding::MissingLanePrompt {
                lane: "widget".to_string(),
                path: expected_path.clone(),
            }]
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("widget") && rendered.contains(&expected_path.display().to_string()),
            "lane finding in: {rendered}"
        );
    }

    #[test]
    fn lane_without_prompt_file_key_resolves_to_runner_default() {
        let env = test_env();
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.lanes.widget]\n\
             repo = \".\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        let expected_path = env.home.join(".claude/prompts/widget.md");
        assert_eq!(
            findings,
            vec![DriftFinding::MissingLanePrompt {
                lane: "widget".to_string(),
                path: expected_path,
            }]
        );
    }

    #[test]
    fn audit_session_with_default_prompt_and_no_skill_anywhere_is_reported() {
        let env = test_env();
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.audit]\n\
             dir = \".\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert_eq!(
            findings,
            vec![DriftFinding::MissingSkill {
                name: "ticket-audit".to_string(),
                session: "work.audit".to_string(),
                repo_path: env.repo_dir.join(".claude/skills/ticket-audit"),
                home_path: env.home.join(".claude/skills/ticket-audit"),
            }]
        );
    }

    #[test]
    fn audit_session_with_skill_present_under_home_is_clean() {
        let env = test_env();
        std::fs::create_dir_all(env.home.join(".claude/skills/ticket-audit"))
            .expect("mkdir home skill");
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.audit]\n\
             dir = \".\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    #[test]
    fn review_watch_dir_falls_back_to_audit_dir() {
        let env = test_env();
        let sub_repo = env.repo_dir.join("sub");
        std::fs::create_dir_all(sub_repo.join(".claude/skills/bugbot-triage"))
            .expect("mkdir sub skill");
        std::fs::create_dir_all(sub_repo.join(".claude/skills/ticket-audit"))
            .expect("mkdir sub audit skill");
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.audit]\n\
             dir = \"sub\"\n\
             [work.review_watch]\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert!(
            findings.is_empty(),
            "review_watch should resolve its skill under audit's dir: {findings:?}"
        );
    }

    #[test]
    fn no_work_table_and_current_stamp_is_clean() {
        let env = test_env();
        write_repo_config(&env, "schema_version = 1\n");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        let findings = run_check(&ctx, false, &mut out).expect("check should succeed");

        assert!(findings.is_empty());
    }

    #[test]
    fn missing_repo_config_is_an_error() {
        let env = test_env();
        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();

        let err = run_check(&ctx, false, &mut out).expect_err("missing config should error");
        assert!(matches!(err, CheckCliError::MissingRepoConfig { .. }));
    }

    #[test]
    fn unparseable_repo_config_is_an_error() {
        let env = test_env();
        write_repo_config(&env, "this is not [ valid toml");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();

        let err = run_check(&ctx, false, &mut out).expect_err("bad toml should error");
        assert!(matches!(err, CheckCliError::ParseRepoConfig { .. }));
    }

    #[test]
    fn quiet_output_is_one_line_when_clean() {
        let env = test_env();
        write_repo_config(&env, "schema_version = 1\n");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        run_check(&ctx, true, &mut out).expect("check should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert_eq!(rendered.lines().count(), 1, "exactly one line: {rendered}");
        assert!(rendered.contains("up to date"));
    }

    #[test]
    fn quiet_output_is_one_line_when_drifted() {
        let env = test_env();
        write_repo_config(&env, "");

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        run_check(&ctx, true, &mut out).expect("check should succeed");

        let rendered = String::from_utf8(out).expect("utf8");
        assert_eq!(rendered.lines().count(), 1, "exactly one line: {rendered}");
        assert!(rendered.contains("drift finding"));
    }

    #[test]
    fn run_check_writes_no_files() {
        let env = test_env();
        write_repo_config(&env, "");

        let snapshot = |dir: &Path| -> Vec<PathBuf> {
            let mut entries: Vec<PathBuf> = walk(dir);
            entries.sort();
            entries
        };
        fn walk(dir: &Path) -> Vec<PathBuf> {
            let mut out = Vec::new();
            let Ok(read) = std::fs::read_dir(dir) else {
                return out;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path);
                }
            }
            out
        }

        let before = snapshot(env.repo_dir.parent().unwrap());

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner);
        let mut out = Vec::new();
        run_check(&ctx, false, &mut out).expect("check should succeed");

        let after = snapshot(env.repo_dir.parent().unwrap());
        assert_eq!(before, after, "run_check must write no files");
    }
}
