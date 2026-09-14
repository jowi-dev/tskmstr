//! `tm update`: apply the *additive* fixes for `tm check`'s drift findings
//! (GitHub issue #39). Where `tm check` (`src/cli/check.rs`) only reports,
//! this command catches an already-onboarded repo up to the running
//! tskmstr:
//!
//! - scaffold the asset kinds the running binary expects but the repo lacks
//!   (a configured lane's missing prompt file, using the same starter
//!   template `tm init` writes), and
//! - bump the `.tskmstr.toml` `schema_version` stamp to
//!   [`manifest::CURRENT_SCHEMA_VERSION`].
//!
//! Strictly additive: it only ever writes files that don't exist and the
//! stamp key. Existing user-customized assets (edited lane prompts, skill
//! bodies, config values, comments) are never touched — the same reasoning
//! that keeps `tm check` from content-diffing them (see
//! `docs/decisions/0007-asset-schema-version.md`). Two things it therefore
//! cannot fix are reported as remaining drift instead: a user-supplied
//! session skill that exists nowhere (tm doesn't ship skill content), and a
//! stamp *newer* than this binary (bumping it would be a downgrade; the
//! remedy is updating tskmstr itself).

use std::io::Write;
use std::path::Path;

use crate::agent::{AgentInvocation, AgentRunner};
use crate::config::ConfigPaths;
use crate::config::manifest;

use super::Prompter;
use super::check::{self, CheckCliError, CheckContext, DriftFinding};

/// Dependencies for [`run_update`]: `tm check`'s read-only trio, plus the
/// launcher for the optional agent-assisted setup session (the GitHub
/// issue #30 machinery `tm init` uses, offered here for only the assets
/// this run introduced).
pub struct UpdateContext<'a> {
    /// Where the repo-local `.tskmstr.toml` lives.
    pub paths: &'a ConfigPaths,
    /// Home directory, for resolving `~/`-rooted paths and user-level
    /// skills, matching `tm init`'s resolution rules.
    pub home: &'a Path,
    /// The AI coding agent this repo is configured to use (or the default,
    /// when unconfigured) — resolves default session prompts and skill
    /// directories the same way `tm init` does.
    pub runner: &'a dyn AgentRunner,
    /// Launches the agent-assisted setup session in the foreground (from
    /// the repo root) and waits for it to exit; `Err` is a display message.
    /// Injected so the command is testable without spawning a real agent.
    pub setup_launcher: &'a dyn Fn(&AgentInvocation) -> Result<(), String>,
}

/// `tm update`: compute `tm check`'s findings, apply every additive fix,
/// offer the agent-assisted setup session for only the assets this run
/// introduced (skipped under `yes`, like `tm init --yes`), and return the
/// drift that remains (empty when the repo is now up to date). Errors
/// mirror `tm check`'s: a repo that was never onboarded is a
/// [`CheckCliError::MissingRepoConfig`], not something to "update" into
/// existence — that's `tm init`'s job.
pub fn run_update(
    ctx: &UpdateContext,
    yes: bool,
    prompter: &mut dyn Prompter,
    out: &mut dyn Write,
) -> Result<Vec<DriftFinding>, CheckCliError> {
    let (mut doc, repo_config_path) = check::read_repo_doc(ctx.paths)?;
    let original = doc.to_string();
    let repo_dir = repo_config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let check_ctx = CheckContext {
        paths: ctx.paths,
        home: ctx.home,
        runner: ctx.runner,
    };

    let findings = check::collect_findings(&check_ctx, &doc, &repo_dir);
    if findings.is_empty() {
        writeln!(
            out,
            "up to date (schema_version {}); nothing to update",
            manifest::CURRENT_SCHEMA_VERSION
        )?;
        return Ok(Vec::new());
    }

    let mut newer_stamp = false;
    let mut tasks = super::init::SetupTasks::default();
    for finding in &findings {
        match finding {
            DriftFinding::MissingLanePrompt { lane, path } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, super::init::lane_prompt_template(lane))?;
                writeln!(out, "Wrote {}", path.display())?;
                tasks.lane_prompts.push(path.clone());
            }
            // Scaffold the runner's agent definition with a placeholder model
            // (GitHub issue #52): `tm update` has no model to declare — that's
            // `tm init`'s interactive question — so it writes the template and
            // hands it to the setup session to fill, never a guessed model.
            DriftFinding::MissingSubagentDef { agent, path, .. } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, ctx.runner.agent_definition_template(agent, None))?;
                writeln!(out, "Wrote {}", path.display())?;
                tasks.subagent_defs.push(path.clone());
            }
            // Bumping a newer stamp would be a downgrade: the repo already
            // has (or expects) assets this binary doesn't know about, so the
            // stale side here is the binary, not the repo.
            DriftFinding::StampNewer { .. } => newer_stamp = true,
            // Stamped below, outside the loop, so it happens exactly once.
            DriftFinding::StampMissing | DriftFinding::StampStale { .. } => {}
            // Skill content is user-supplied (tm never ships or authors it);
            // handed to the setup session, and otherwise reported as
            // remaining drift.
            DriftFinding::MissingSkill {
                name,
                repo_path,
                prompt,
                ..
            } => tasks.missing_skills.push(super::init::MissingSkill {
                name: name.clone(),
                path: repo_path.clone(),
                session_prompt: prompt.clone(),
            }),
        }
    }

    if !newer_stamp {
        super::init::stamp_schema_version(&mut doc);
    }
    if doc.to_string() != original {
        std::fs::write(&repo_config_path, doc.to_string())?;
        writeln!(
            out,
            "Stamped schema_version = {} in {}",
            manifest::CURRENT_SCHEMA_VERSION,
            repo_config_path.display()
        )?;
    }

    super::init::offer_agent_setup(
        ctx.runner,
        ctx.setup_launcher,
        "tm update",
        yes,
        &tasks,
        prompter,
        out,
    )?;

    // Re-check against what's on disk now: scaffolds cleared their lane
    // findings, a session may have authored the missing skills, the stamp
    // is current unless it was newer, and whatever is left is drift this
    // command cannot fix additively.
    let remaining = check::collect_findings(&check_ctx, &doc, &repo_dir);
    if remaining.is_empty() {
        writeln!(
            out,
            "up to date (schema_version {})",
            manifest::CURRENT_SCHEMA_VERSION
        )?;
    } else {
        writeln!(out)?;
        writeln!(out, "Drift `tm update` cannot fix:")?;
        for finding in &remaining {
            writeln!(out, "{finding}")?;
        }
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::ClaudeRunner;
    use crate::cli::FakePrompter;
    use crate::config::ConfigPaths;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    /// A tempdir laid out like a home directory plus a repo checkout, with
    /// `ConfigPaths` pointing at both — mirrors `check`'s test harness.
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

    fn ctx<'a>(
        env: &'a TestEnv,
        runner: &'a dyn AgentRunner,
        setup_launcher: &'a dyn Fn(&AgentInvocation) -> Result<(), String>,
    ) -> UpdateContext<'a> {
        UpdateContext {
            paths: &env.paths,
            home: &env.home,
            runner,
            setup_launcher,
        }
    }

    /// A launcher for tests whose path must never reach the setup session.
    fn no_launcher(invocation: &AgentInvocation) -> Result<(), String> {
        panic!("setup session must not launch: {invocation:?}");
    }

    fn write_repo_config(env: &TestEnv, contents: &str) {
        std::fs::write(env.paths.repo.as_ref().unwrap(), contents).expect("write repo config");
    }

    fn read_repo_config(env: &TestEnv) -> String {
        std::fs::read_to_string(env.paths.repo.as_ref().unwrap()).expect("read repo config")
    }

    #[test]
    fn scaffolds_missing_lane_prompt_and_stamps_the_config() {
        let env = test_env();
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert!(remaining.is_empty(), "all drift fixable: {remaining:?}");
        let prompt_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        let prompt = std::fs::read_to_string(&prompt_path).expect("scaffold written");
        assert!(
            prompt.contains("widget work lane"),
            "starter template: {prompt}"
        );
        assert!(
            read_repo_config(&env).contains("schema_version = 1"),
            "stamp bumped: {}",
            read_repo_config(&env)
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains(&prompt_path.display().to_string()),
            "scaffold named in: {rendered}"
        );
        assert!(
            rendered.contains("schema_version"),
            "stamp write named in: {rendered}"
        );
        assert!(
            rendered.contains("up to date"),
            "clean close in: {rendered}"
        );
    }

    #[test]
    fn scaffolds_missing_subagent_definition_additively() {
        let env = test_env();
        // Lane prompt already present, so the only fixable drift is the
        // missing subagent definition the `subagent` key names.
        let prompt_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        std::fs::create_dir_all(prompt_path.parent().unwrap()).expect("mkdir");
        std::fs::write(&prompt_path, "# widget lane").expect("write prompt");
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n\
             subagent = \"impl\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert!(remaining.is_empty(), "all drift fixable: {remaining:?}");
        let def_path = env.repo_dir.join(".claude/agents/impl.md");
        let def = std::fs::read_to_string(&def_path).expect("definition scaffolded");
        assert!(def.contains("name: impl"), "name frontmatter: {def}");
        // tm update has no model to declare (that's init's interactive
        // question), so it scaffolds the placeholder for the setup session
        // to fill — never a wrong model.
        assert!(
            def.contains("model: TODO-set-the-subagent-model"),
            "placeholder model awaiting the operator: {def}"
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains(&def_path.display().to_string()),
            "scaffold named in: {rendered}"
        );
    }

    #[test]
    fn existing_subagent_definition_is_never_overwritten() {
        let env = test_env();
        let prompt_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        std::fs::create_dir_all(prompt_path.parent().unwrap()).expect("mkdir");
        std::fs::write(&prompt_path, "# widget lane").expect("write prompt");
        let def_path = env.repo_dir.join(".claude/agents/impl.md");
        std::fs::create_dir_all(def_path.parent().unwrap()).expect("mkdir agents");
        let customized = "---\nname: impl\nmodel: my/hand-picked-model\n---\ncustom body\n";
        std::fs::write(&def_path, customized).expect("write def");
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n\
             subagent = \"impl\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert!(remaining.is_empty());
        assert_eq!(
            std::fs::read_to_string(&def_path).expect("read def"),
            customized,
            "user-customized definition untouched"
        );
    }

    #[test]
    fn up_to_date_repo_writes_nothing() {
        let env = test_env();
        let config = "schema_version = 1\n";
        write_repo_config(&env, config);

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert!(remaining.is_empty());
        assert_eq!(read_repo_config(&env), config, "config byte-identical");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("up to date"),
            "noop message in: {rendered}"
        );
        assert!(
            !rendered.contains("Wrote"),
            "no writes reported: {rendered}"
        );
    }

    #[test]
    fn existing_lane_prompt_content_is_never_touched() {
        let env = test_env();
        let prompt_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        std::fs::create_dir_all(prompt_path.parent().unwrap()).expect("mkdir");
        let customized = "# my hand-tuned lane prompt\n";
        std::fs::write(&prompt_path, customized).expect("write prompt");
        // No stamp: update has something to fix, but the prompt isn't it.
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert!(remaining.is_empty());
        assert_eq!(
            std::fs::read_to_string(&prompt_path).expect("read prompt"),
            customized,
            "user-customized asset untouched"
        );
    }

    #[test]
    fn config_comments_and_values_survive_the_stamp_write() {
        let env = test_env();
        write_repo_config(
            &env,
            "# hand-written note about this repo\n\
             schema_version = 0\n\
             [backend]\n\
             provider = \"github\" # inline comment\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        run_update(&ctx, true, &mut FakePrompter::new(), &mut out).expect("update should succeed");

        let written = read_repo_config(&env);
        assert!(
            written.contains("schema_version = 1"),
            "stamp bumped: {written}"
        );
        assert!(
            written.contains("# hand-written note about this repo"),
            "comment preserved: {written}"
        );
        assert!(
            written.contains("provider = \"github\" # inline comment"),
            "value and inline comment preserved: {written}"
        );
    }

    #[test]
    fn newer_stamp_is_left_alone_and_remains_drift() {
        let env = test_env();
        let config = "schema_version = 999\n";
        write_repo_config(&env, config);

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert_eq!(remaining, vec![DriftFinding::StampNewer { found: 999 }]);
        assert_eq!(
            read_repo_config(&env),
            config,
            "a newer stamp must never be downgraded"
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("update tskmstr"),
            "binary-is-old remedy in: {rendered}"
        );
    }

    #[test]
    fn missing_skill_is_reported_not_fixed_but_stamp_still_bumps() {
        let env = test_env();
        write_repo_config(
            &env,
            "[work.audit]\n\
             dir = \".\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect("update should succeed");

        assert_eq!(remaining.len(), 1, "skill gap remains: {remaining:?}");
        assert!(matches!(remaining[0], DriftFinding::MissingSkill { .. }));
        assert!(
            !env.repo_dir.join(".claude/skills/ticket-audit").exists(),
            "tm never authors user-supplied skill content"
        );
        assert!(
            read_repo_config(&env).contains("schema_version = 1"),
            "the stamp gap is independent of the skill gap"
        );
    }

    #[test]
    fn running_update_twice_is_idempotent() {
        let env = test_env();
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut first_out = Vec::new();
        run_update(&ctx, true, &mut FakePrompter::new(), &mut first_out)
            .expect("first update should succeed");
        let config_after_first = read_repo_config(&env);
        let prompt_path = env.repo_dir.join(".tskmstr/prompts/widget-lane.md");
        let prompt_after_first = std::fs::read_to_string(&prompt_path).expect("read prompt");

        let mut second_out = Vec::new();
        let remaining = run_update(&ctx, true, &mut FakePrompter::new(), &mut second_out)
            .expect("second update should succeed");

        assert!(remaining.is_empty());
        assert_eq!(read_repo_config(&env), config_after_first);
        assert_eq!(
            std::fs::read_to_string(&prompt_path).expect("read prompt"),
            prompt_after_first
        );
        let rendered = String::from_utf8(second_out).expect("utf8");
        assert!(
            !rendered.contains("Wrote"),
            "second run writes nothing: {rendered}"
        );
    }

    #[test]
    fn offers_agent_setup_for_only_the_new_assets() {
        let env = test_env();
        // One scaffoldable lane prompt plus one user-supplied skill gap:
        // both should reach the setup session; nothing else should.
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n\
             [work.audit]\n\
             dir = \".\"\n",
        );

        let launched: RefCell<Vec<AgentInvocation>> = RefCell::new(Vec::new());
        let launcher = |invocation: &AgentInvocation| {
            launched.borrow_mut().push(invocation.clone());
            Ok(())
        };
        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &launcher);
        let mut prompter = FakePrompter::new().with_confirm(true);
        let mut out = Vec::new();
        run_update(&ctx, false, &mut prompter, &mut out).expect("update should succeed");

        let launched = launched.borrow();
        assert_eq!(launched.len(), 1, "exactly one setup session");
        let args = launched[0].args.join(" ");
        assert!(
            args.contains("widget-lane.md"),
            "scaffolded lane prompt named in setup prompt: {args}"
        );
        assert!(
            args.contains("ticket-audit"),
            "missing skill named in setup prompt: {args}"
        );
    }

    #[test]
    fn declining_the_offer_keeps_the_static_skeleton() {
        let env = test_env();
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut prompter = FakePrompter::new().with_confirm(false);
        let mut out = Vec::new();
        let remaining =
            run_update(&ctx, false, &mut prompter, &mut out).expect("update should succeed");

        assert!(remaining.is_empty());
        assert!(
            env.repo_dir
                .join(".tskmstr/prompts/widget-lane.md")
                .exists(),
            "declining the session keeps the scaffold"
        );
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("tm update"),
            "decline message names how to fill assets out later: {rendered}"
        );
    }

    #[test]
    fn yes_skips_the_agent_setup_offer() {
        let env = test_env();
        write_repo_config(
            &env,
            "[work.lanes.widget]\n\
             prompt_file = \".tskmstr/prompts/widget-lane.md\"\n",
        );

        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut prompter = FakePrompter::new();
        let mut out = Vec::new();
        run_update(&ctx, true, &mut prompter, &mut out).expect("update should succeed");

        assert!(
            prompter.messages.is_empty(),
            "--yes asks nothing: {:?}",
            prompter.messages
        );
    }

    #[test]
    fn skill_authored_by_the_setup_session_clears_remaining_drift() {
        let env = test_env();
        write_repo_config(
            &env,
            "schema_version = 1\n\
             [work.audit]\n\
             dir = \".\"\n",
        );

        // A setup session that actually authors the missing skill: the
        // post-session re-check must see it and report the repo clean.
        let skill_dir = env.repo_dir.join(".claude/skills/ticket-audit");
        let launcher = |_: &AgentInvocation| {
            std::fs::create_dir_all(&skill_dir).expect("author skill");
            Ok(())
        };
        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &launcher);
        let mut prompter = FakePrompter::new().with_confirm(true);
        let mut out = Vec::new();
        let remaining =
            run_update(&ctx, false, &mut prompter, &mut out).expect("update should succeed");

        assert!(
            remaining.is_empty(),
            "session-authored skill counts: {remaining:?}"
        );
    }

    #[test]
    fn missing_repo_config_is_an_error() {
        let env = test_env();
        let runner = ClaudeRunner;
        let ctx = ctx(&env, &runner, &no_launcher);
        let mut out = Vec::new();

        let err = run_update(&ctx, true, &mut FakePrompter::new(), &mut out)
            .expect_err("missing config should error");
        assert!(matches!(err, CheckCliError::MissingRepoConfig { .. }));
    }
}
