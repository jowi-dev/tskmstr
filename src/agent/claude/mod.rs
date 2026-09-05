//! [`ClaudeRunner`]: the `claude` [`crate::agent::AgentRunner`] implementation,
//! ported from devtools' `~/devtools/work.ml`'s `run_lane`. This module owns
//! only the translation from already-resolved run inputs into the exact
//! argv/env `claude` needs — no process spawning, no environment reads, no
//! clock reads. `src/work/runner.rs` is the thing that actually spawns a
//! process from an [`crate::agent::AgentInvocation`].
//!
//! The reference invocation, from `work.ml`'s `run_lane` (lines ~495-516):
//!
//! ```text
//! env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDECODE \
//!   claude -p "$(cat '<prompt_tmp>')" \
//!     --model '<model>' \
//!     --settings '<tm_hooks_settings>' \
//!     --permission-mode '<permission_mode>' \
//!     --output-format json \
//!     --max-turns '<max_turns>' \
//!   > '<out_json>' 2>> '<log>'
//! ```
//!
//! `work.ml` builds this as one shell string and reads the prompt via
//! `"$(cat prompt_tmp)"` command substitution — a shell-level way of
//! inlining a file's contents into one argv slot. This port builds a
//! [`std::process::Command`]'s argv directly with no intervening shell, so
//! the equivalent is simpler: the caller resolves the prompt text (from the
//! lane's prompt file, with the `"\n\nWork ticket: <ticket>."` suffix
//! already appended when a ticket was given — see `run_lane` lines
//! ~478-482) and this module places that text as the literal `-p` argument.
//! Output redirection (`> out_json 2>> log`) is a spawn-time concern, not an
//! argv concern, so it isn't modeled here.
//!
//! `--model`, `--settings`, `--permission-mode`, `--output-format`, and
//! `--max-turns` are *always* present in `work.ml`'s output: there is no
//! code path that omits any of them. Where a lane/run option is absent,
//! `run_lane` substitutes a hardcoded default and still passes the flag —
//! it never falls back to omitting a flag and trusting `claude`'s own
//! default. This module ports that: absent `model`/`max_turns`/
//! `permission_mode` resolve to the same defaults (`"fable"`, `"200"`,
//! `"acceptEdits"`) rather than being left out of argv.
//!
//! # Two run modes
//!
//! Everything above describes [`RunMode::Headless`], the only shape that
//! existed while `work.ml` was the reference. Issue #2 phase 3 makes work
//! and fix runs *interactive* tmux-hosted sessions by default
//! ([`RunMode::Interactive`]): the prompt goes positionally rather than via
//! `-p`, `--output-format`/`--max-turns` are dropped (nothing parses the
//! output and there is no turn budget to enforce on a session a human is
//! steering), and — the part with no visible symptom when it is wrong — the
//! run id travels as [`TSKMSTR_SESSION_RUN_ID`](crate::agent::TSKMSTR_SESSION_RUN_ID)
//! rather than [`TSKMSTR_RUN_ID`](crate::agent::TSKMSTR_RUN_ID). See
//! [`RunMode`].

use std::path::{Path, PathBuf};

use crate::agent::{
    AgentInvocation, AgentRunner, InvocationInputs, OutcomeParseError, RunMode, RunOutcome,
    shell_quote,
};
use crate::work::naming::expand_tilde;

/// Driver model default when a lane/run doesn't specify one, mirroring
/// `run_lane`'s `let model = match opts.model with Some m -> m | None ->
/// "fable"`.
const DEFAULT_MODEL: &str = "fable";

/// `--max-turns` default, mirroring `run_lane`'s `let max_turns = match
/// opts.max_turns with Some t -> t | None -> "200"`.
const DEFAULT_MAX_TURNS: &str = "200";

/// `--permission-mode` default, mirroring `run_lane`'s `let permission_mode
/// = match opts.permission_mode with Some m -> m | None -> "acceptEdits"`.
const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";

/// The `claude` CLI [`AgentRunner`] implementation. Zero-sized: it holds no
/// state of its own, so a single `&'static ClaudeRunner` (or, in production,
/// a leaked `Box::new(ClaudeRunner)`; see `main.rs`'s `agent_runner_for`)
/// serves every call.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeRunner;

impl AgentRunner for ClaudeRunner {
    fn name(&self) -> &'static str {
        "claude"
    }

    /// Build the [`AgentInvocation`] for one run from already-resolved
    /// [`InvocationInputs`], applying the same model/max-turns/
    /// permission-mode defaults `work.ml`'s `run_lane` applies when a
    /// lane/run option is absent.
    ///
    /// [`InvocationInputs::mode`] forks both halves of the result:
    ///
    /// | | [`RunMode::Headless`] | [`RunMode::Interactive`] |
    /// |---|---|---|
    /// | prompt | `-p <prompt>` | positional, `args[0]` |
    /// | `--output-format json` | yes (parsed by [`AgentRunner::parse_outcome`]) | no — nothing parses an interactive session's stdout |
    /// | `--max-turns` | yes | no — meaningless for a steerable session |
    /// | run id travels as | `TSKMSTR_RUN_ID` | `TSKMSTR_SESSION_RUN_ID` |
    ///
    /// `--model`, `--settings` and `--permission-mode` are common to both.
    /// `--settings` especially: it is what deploys the SessionEnd hook that
    /// finishes an interactive run, so an interactive invocation needs it at
    /// least as much as a headless one does.
    ///
    /// Read [`RunMode`] before touching the env-set fork.
    fn build_invocation(&self, inputs: InvocationInputs) -> AgentInvocation {
        let model = inputs.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let permission_mode = inputs
            .permission_mode
            .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_string());

        let mut args = Vec::new();
        match inputs.mode {
            RunMode::Headless => {
                args.push("-p".to_string());
                args.push(inputs.prompt);
            }
            // Positional prompt, mirroring `interactive_shell_command`'s
            // `claude <prompt>`. Kept at `args[0]` so
            // `AgentRunner::tmux_command_line`'s default impl can swap it
            // for a `"$(cat <prompt file>)"` read without re-deriving the
            // argv.
            RunMode::Interactive => args.push(inputs.prompt),
        }
        args.push("--model".to_string());
        args.push(model);
        args.push("--settings".to_string());
        args.push(inputs.settings_path.to_string_lossy().into_owned());
        args.push("--permission-mode".to_string());
        args.push(permission_mode);
        if inputs.mode == RunMode::Headless {
            let max_turns = inputs
                .max_turns
                .unwrap_or_else(|| DEFAULT_MAX_TURNS.to_string());
            args.push("--output-format".to_string());
            args.push("json".to_string());
            args.push("--max-turns".to_string());
            args.push(max_turns);
        }

        let mut env_set = Vec::new();
        let run_id = inputs.run_id.filter(|id| !id.is_empty());
        if let Some(run_id) = run_id {
            let var = match inputs.mode {
                RunMode::Headless => crate::agent::TSKMSTR_RUN_ID,
                RunMode::Interactive => crate::agent::TSKMSTR_SESSION_RUN_ID,
            };
            env_set.push((var.to_string(), run_id));
        }

        AgentInvocation {
            program: "claude".to_string(),
            args,
            env_set,
            // **Billing-safety, read before touching this list.** If any of
            // these three are present in the spawned process's environment,
            // `claude` silently bills the raw Anthropic API (or, for
            // `CLAUDECODE`, behaves as if it's already inside a Claude Code
            // session) instead of using the interactive claude.ai
            // subscription these lane runs are meant to ride on. There is no
            // error, no warning, no nonzero exit — the run completes
            // normally and the bill just lands somewhere unexpected. This is
            // the one thing in this whole module that must never be
            // "simplified away"; see `docs/plans/runner-port.md` §7 Risks
            // ("Billing-safety env stripping") and the dedicated regression
            // test `billing_safety_env_vars_are_always_stripped`.
            env_remove: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "CLAUDECODE".to_string(),
            ],
        }
    }

    /// Parses a `claude -p --output-format json` result (`raw`, the raw
    /// stdout contents) into a [`RunOutcome`]. The single parsing path §4 of
    /// `docs/plans/runner-port.md` calls for: both the foreground and
    /// detached run paths go through [`crate::work::run::run_agent_and_finish`],
    /// which calls this instead of duplicating `jq`-style field extraction.
    ///
    /// ## Field-by-field mapping from `work.ml`'s `jq` calls
    ///
    /// `run_lane` (lines ~524-563) and the detached wrapper script (lines
    /// ~615-622) read the same `out_json` file with these `jq` expressions,
    /// fed into these `tm runs finish` flags:
    ///
    /// | `jq` expression | [`RunOutcome`] field | `tm runs finish` flag |
    /// |---|---|---|
    /// | `.session_id // empty` | `session_id: String` (required) | `--session-id` |
    /// | `.num_turns // empty` | `num_turns: Option<u64>` | (printed in `--fg` summary; not a finish flag) |
    /// | `.total_cost_usd // empty` | `cost_usd: Option<f64>` | (printed in `--fg` summary; not a finish flag) |
    /// | `.is_error` | `is_error: Option<bool>` (absence is kept, not defaulted) | drives the `done`/`failed`/`interrupted` status passed to `finish_run` |
    /// | `.result // empty` / `.result // "no result field"` | `result: Option<String>` | scraped for the PR-URL fallback, not passed directly |
    /// | `.modelUsage // empty` | `model_usage: Option<ModelUsageMap>` | `--model-usage` (only when present and non-empty, per the wrapper's `[ -n "$MODEL_USAGE" ]` guard) |
    ///
    /// `jq`'s `// empty` and `// false` are the OCaml side's way of
    /// tolerating absent fields: a missing field becomes an empty string
    /// (falsy in a shell `[ -n ... ]` test) or `false`, never a hard error.
    /// This port keeps that tolerance for every field except `session_id`:
    /// `work.ml` never checks whether `session_id` came back non-empty
    /// before using it (e.g. printing `claude --resume %s`), so an
    /// empty/missing session id would already be a silently-broken run on
    /// the OCaml side. Rust can do better than silently propagating an
    /// empty string, so this treats a missing or empty `session_id` as a
    /// hard [`OutcomeParseError`] instead — a run with no session id to
    /// resume isn't a usable outcome, only an unparseable one.
    fn parse_outcome(&self, raw: &str) -> Result<RunOutcome, OutcomeParseError> {
        let parsed: RawResult = serde_json::from_str(raw)?;

        let session_id = parsed
            .session_id
            .filter(|id| !id.is_empty())
            .ok_or(OutcomeParseError::MissingSessionId)?;

        let model_usage = parsed.model_usage.filter(|models| !models.is_empty());

        Ok(RunOutcome {
            session_id,
            cost_usd: parsed.total_cost_usd,
            num_turns: parsed.num_turns,
            is_error: parsed.is_error,
            result: parsed.result,
            model_usage,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("claude --resume {session_id}")
    }

    /// Builds `claude --model <model> <prompt>` (or `claude <prompt>` when
    /// `model` is `None`), every value [`shell_quote`]d. `--model` is
    /// emitted only when `model` is `Some`, so an unconfigured launch keeps
    /// the exact command shape it had before the option existed. Moved
    /// verbatim from `work::audit::claude_command` (deleted); shared by
    /// [`crate::work::audit::launch_audit`] and
    /// [`crate::work::bugbot::launch_cleanup`], which host their sessions
    /// the same way.
    fn interactive_shell_command(&self, model: Option<&str>, prompt: &str) -> String {
        match model {
            Some(model) => format!(
                "claude --model {} {}",
                shell_quote(model),
                shell_quote(prompt)
            ),
            None => format!("claude {}", shell_quote(prompt)),
        }
    }

    fn default_audit_prompt_template(&self) -> &'static str {
        "/ticket-audit {key}"
    }

    fn default_cleanup_prompt_template(&self) -> &'static str {
        "/bugbot-triage {key} {findings_file}"
    }

    /// `~/.claude/prompts/<lane>.md` — `work.ml`'s default lane-prompt
    /// convention. The `.claude` directory is claude's, so this default is
    /// claude-owned rather than a runner-agnostic constant.
    fn default_lane_prompt_path(&self, home: &Path, lane: &str) -> PathBuf {
        expand_tilde(&format!("~/.claude/prompts/{lane}.md"), home)
    }
}

/// Raw shape of the `claude -p --output-format json` result, deserialized
/// leniently: every field is optional at this layer so that any one missing
/// field doesn't fail the whole parse. [`ClaudeRunner::parse_outcome`] is the
/// only place that decides which absences are fatal.
#[derive(Debug, serde::Deserialize)]
struct RawResult {
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "modelUsage")]
    model_usage: Option<crate::runs::ModelUsageMap>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_inputs() -> InvocationInputs {
        InvocationInputs {
            prompt: "do the thing".to_string(),
            model: Some("sonnet".to_string()),
            max_turns: Some("300".to_string()),
            permission_mode: Some("plan".to_string()),
            settings_path: PathBuf::from("/Users/jowi/.local/share/tskmstr/hooks/settings.json"),
            run_id: Some("run-123".to_string()),
            mode: RunMode::Headless,
        }
    }

    #[test]
    fn claude_runner_name_is_claude() {
        assert_eq!(ClaudeRunner.name(), "claude");
    }

    #[test]
    fn resume_command_names_the_claude_cli() {
        assert_eq!(
            ClaudeRunner.resume_command("sess-123"),
            "claude --resume sess-123"
        );
    }

    #[test]
    fn interactive_shell_command_quotes_model_and_prompt() {
        assert_eq!(
            ClaudeRunner.interactive_shell_command(Some("opus"), "/ticket-audit PROJ-9"),
            "claude --model 'opus' '/ticket-audit PROJ-9'"
        );
        assert_eq!(
            ClaudeRunner.interactive_shell_command(None, "/ticket-audit PROJ-9"),
            "claude '/ticket-audit PROJ-9'"
        );
    }

    #[test]
    fn default_audit_prompt_template_is_ticket_audit() {
        assert_eq!(
            ClaudeRunner.default_audit_prompt_template(),
            "/ticket-audit {key}"
        );
    }

    #[test]
    fn default_cleanup_prompt_template_is_bugbot_triage() {
        assert_eq!(
            ClaudeRunner.default_cleanup_prompt_template(),
            "/bugbot-triage {key} {findings_file}"
        );
    }

    #[test]
    fn default_lane_prompt_path_is_home_claude_prompts() {
        assert_eq!(
            ClaudeRunner.default_lane_prompt_path(Path::new("/home/j"), "mylane"),
            PathBuf::from("/home/j/.claude/prompts/mylane.md")
        );
    }

    #[test]
    fn tmux_command_line_strips_billing_env_and_reads_the_prompt_from_the_file() {
        let invocation = ClaudeRunner.build_invocation(InvocationInputs {
            prompt: "do the thing".to_string(),
            model: Some("fable".to_string()),
            max_turns: Some("200".to_string()),
            permission_mode: Some("acceptEdits".to_string()),
            settings_path: PathBuf::from("/hooks/settings.json"),
            run_id: Some("7".to_string()),
            mode: RunMode::Interactive,
        });

        let command =
            ClaudeRunner.tmux_command_line(&invocation, Path::new("/state/proj-1.prompt.md"));

        assert_eq!(
            command,
            "env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDECODE claude \
             \"$(cat '/state/proj-1.prompt.md')\" '--model' 'fable' '--settings' \
             '/hooks/settings.json' '--permission-mode' 'acceptEdits'"
        );
        assert!(
            !command.contains("do the thing"),
            "the prompt itself must never reach the command string — it is unbounded"
        );
    }

    #[test]
    fn tmux_command_line_quotes_a_prompt_path_with_a_quote_in_it() {
        let invocation = ClaudeRunner.build_invocation(InvocationInputs {
            prompt: "prompt".to_string(),
            model: Some("fable".to_string()),
            max_turns: Some("200".to_string()),
            permission_mode: Some("acceptEdits".to_string()),
            settings_path: PathBuf::from("/hooks/settings.json"),
            run_id: Some("7".to_string()),
            mode: RunMode::Interactive,
        });

        let command =
            ClaudeRunner.tmux_command_line(&invocation, Path::new("/state/o'brien.prompt.md"));

        assert!(command.contains(r#""$(cat '/state/o'\''brien.prompt.md')""#));
    }

    #[test]
    fn fully_specified_lane_produces_exact_argv() {
        let invocation = ClaudeRunner.build_invocation(base_inputs());

        assert_eq!(invocation.program, "claude");
        assert_eq!(
            invocation.args,
            vec![
                "-p",
                "do the thing",
                "--model",
                "sonnet",
                "--settings",
                "/Users/jowi/.local/share/tskmstr/hooks/settings.json",
                "--permission-mode",
                "plan",
                "--output-format",
                "json",
                "--max-turns",
                "300",
            ]
        );
    }

    #[test]
    fn absent_model_and_max_turns_and_permission_mode_fall_back_to_work_ml_defaults() {
        let inputs = InvocationInputs {
            model: None,
            max_turns: None,
            permission_mode: None,
            ..base_inputs()
        };

        let invocation = ClaudeRunner.build_invocation(inputs);

        // work.ml never omits these flags when the option is absent — it
        // substitutes a hardcoded default and still passes the flag.
        assert!(invocation.args.contains(&"--model".to_string()));
        assert_eq!(
            invocation.args[invocation.args.iter().position(|a| a == "--model").unwrap() + 1],
            "fable"
        );
        assert!(invocation.args.contains(&"--max-turns".to_string()));
        assert_eq!(
            invocation.args[invocation
                .args
                .iter()
                .position(|a| a == "--max-turns")
                .unwrap()
                + 1],
            "200"
        );
        assert!(invocation.args.contains(&"--permission-mode".to_string()));
        assert_eq!(
            invocation.args[invocation
                .args
                .iter()
                .position(|a| a == "--permission-mode")
                .unwrap()
                + 1],
            "acceptEdits"
        );
    }

    #[test]
    fn prompt_is_delivered_as_the_literal_p_argument_value() {
        let inputs = InvocationInputs {
            prompt: "multi\nline\nprompt with 'quotes' and \"stuff\"".to_string(),
            ..base_inputs()
        };

        let invocation = ClaudeRunner.build_invocation(inputs);

        let p_index = invocation.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(
            invocation.args[p_index + 1],
            "multi\nline\nprompt with 'quotes' and \"stuff\""
        );
    }

    #[test]
    fn settings_flag_carries_the_deployed_hooks_settings_path() {
        let invocation = ClaudeRunner.build_invocation(base_inputs());

        let settings_index = invocation
            .args
            .iter()
            .position(|a| a == "--settings")
            .unwrap();
        assert_eq!(
            invocation.args[settings_index + 1],
            "/Users/jowi/.local/share/tskmstr/hooks/settings.json"
        );
    }

    #[test]
    fn run_id_present_sets_tskmstr_run_id_env_var() {
        let invocation = ClaudeRunner.build_invocation(base_inputs());

        assert_eq!(
            invocation.env_set,
            vec![("TSKMSTR_RUN_ID".to_string(), "run-123".to_string())]
        );
    }

    #[test]
    fn absent_run_id_sets_no_env_vars() {
        let inputs = InvocationInputs {
            run_id: None,
            ..base_inputs()
        };

        let invocation = ClaudeRunner.build_invocation(inputs);

        assert!(invocation.env_set.is_empty());
    }

    #[test]
    fn empty_run_id_is_treated_as_untracked_and_sets_no_env_vars() {
        // Defensive: the wrapper script's own guard is `[ -n "$RUN_ID" ]`,
        // i.e. an empty string is equivalent to untracked, not a real id.
        let inputs = InvocationInputs {
            run_id: Some(String::new()),
            ..base_inputs()
        };

        let invocation = ClaudeRunner.build_invocation(inputs);

        assert!(invocation.env_set.is_empty());
    }

    /// **The phase-3 acceptance test.** The two run modes carry *different*
    /// run-id environment variables, and swapping them has no visible
    /// symptom at all — it just leaves every interactive run stuck at
    /// `running` until `tm runs reap`.
    ///
    /// `TSKMSTR_RUN_ID` means "a supervisor owns this run's lifecycle":
    /// `hooks/tm-session-end.sh` exits 0 the moment it is set, and
    /// [`crate::runs::session::register_session`]/`finish_session` both
    /// short-circuit on it. Only the headless path has such a supervisor
    /// (`tm work __supervise`, see `src/work/detach.rs`). An interactive
    /// tmux-hosted run has none: its run row is finished by the SessionEnd
    /// hook, which must therefore *not* be short-circuited, and its
    /// pre-registered row is adopted via `TSKMSTR_SESSION_RUN_ID`.
    ///
    /// This asserts the full env set of both modes, so setting the wrong
    /// variable — or setting both — fails here.
    #[test]
    fn run_mode_decides_which_run_id_env_var_claude_receives() {
        let headless = ClaudeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Headless,
            ..base_inputs()
        });
        let interactive = ClaudeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Interactive,
            ..base_inputs()
        });

        assert_eq!(
            headless.env_set,
            vec![("TSKMSTR_RUN_ID".to_string(), "run-123".to_string())],
            "the headless supervisor owns finish, so it — and only it — sets TSKMSTR_RUN_ID"
        );
        assert_eq!(
            interactive.env_set,
            vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "run-123".to_string())],
            "an interactive run has no supervisor: TSKMSTR_RUN_ID would gate off the \
             SessionEnd hook that is the only thing that finishes it"
        );
    }

    #[test]
    fn interactive_mode_passes_the_prompt_positionally_and_drops_max_turns() {
        let invocation = ClaudeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Interactive,
            ..base_inputs()
        });

        assert_eq!(
            invocation.args,
            vec![
                "do the thing",
                "--model",
                "sonnet",
                "--settings",
                "/Users/jowi/.local/share/tskmstr/hooks/settings.json",
                "--permission-mode",
                "plan",
            ]
        );
        assert!(
            !invocation.args.iter().any(|arg| arg == "-p"),
            "`-p` is one-shot print mode; an interactive session takes its prompt positionally"
        );
        assert!(
            !invocation.args.iter().any(|arg| arg == "--max-turns"),
            "--max-turns is meaningless for a steerable interactive session"
        );
    }

    #[test]
    fn billing_safety_env_vars_are_always_stripped() {
        // See `build_invocation`'s doc comment on `env_remove`: if any of
        // these three survive into claude's environment, billing silently
        // shifts from the claude.ai subscription to the raw Anthropic API
        // (or claude misbehaves as if already nested in a session), with no
        // error and no nonzero exit to catch it. This must hold regardless
        // of what inputs the caller supplies.
        let invocation = ClaudeRunner.build_invocation(base_inputs());

        assert_eq!(
            invocation.env_remove,
            vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "CLAUDECODE".to_string(),
            ]
        );
    }

    // --- parse_outcome ---

    fn full_canned_json() -> String {
        r#"{
            "session_id": "sess-123",
            "total_cost_usd": 1.5,
            "num_turns": 12,
            "is_error": false,
            "result": "opened https://github.com/example/repo/pull/42",
            "modelUsage": {
                "claude-fable-5": {
                    "inputTokens": 100,
                    "outputTokens": 200,
                    "cacheReadInputTokens": 0,
                    "cacheCreationInputTokens": 0,
                    "costUSD": 1.5
                }
            }
        }"#
        .to_string()
    }

    #[test]
    fn parse_outcome_parses_all_fields() {
        let outcome = ClaudeRunner
            .parse_outcome(&full_canned_json())
            .expect("should parse");

        assert_eq!(outcome.session_id, "sess-123");
        assert_eq!(outcome.cost_usd, Some(1.5));
        assert_eq!(outcome.num_turns, Some(12));
        assert_eq!(outcome.is_error, Some(false));
        assert_eq!(
            outcome.result,
            Some("opened https://github.com/example/repo/pull/42".to_string())
        );
        let model_usage = outcome.model_usage.expect("model usage should be present");
        assert_eq!(model_usage["claude-fable-5"].input_tokens, 100);
        assert_eq!(model_usage["claude-fable-5"].cost_usd, Some(1.5));
    }

    #[test]
    fn parse_outcome_tolerates_missing_optional_fields() {
        let json = r#"{"session_id": "sess-abc"}"#;

        let outcome = ClaudeRunner.parse_outcome(json).expect("should parse");

        assert_eq!(outcome.session_id, "sess-abc");
        assert_eq!(outcome.cost_usd, None);
        assert_eq!(outcome.num_turns, None);
        assert_eq!(outcome.is_error, None);
        assert_eq!(outcome.result, None);
        assert_eq!(outcome.model_usage, None);
    }

    #[test]
    fn parse_outcome_leaves_is_error_none_when_absent() {
        // An absent `is_error` is distinct from an explicit `false`: this is
        // exactly the shape a mid-run usage-limit model switch can leave
        // behind (the turn ends gracefully with no `is_error` field at all),
        // and the caller (run_agent_and_finish) must be able to tell the two
        // apart to avoid misclassifying it as a successful `Done` run. See
        // `RunStatus::Interrupted`'s doc comment.
        let json = r#"{"session_id": "sess-abc"}"#;
        assert_eq!(ClaudeRunner.parse_outcome(json).unwrap().is_error, None);
    }

    #[test]
    fn parse_outcome_honors_is_error_explicit_false() {
        let json = r#"{"session_id": "sess-abc", "is_error": false}"#;
        assert_eq!(
            ClaudeRunner.parse_outcome(json).unwrap().is_error,
            Some(false)
        );
    }

    #[test]
    fn parse_outcome_honors_is_error_true() {
        let json = r#"{"session_id": "sess-abc", "is_error": true}"#;
        assert_eq!(
            ClaudeRunner.parse_outcome(json).unwrap().is_error,
            Some(true)
        );
    }

    #[test]
    fn parse_outcome_errors_on_malformed_json() {
        let err = ClaudeRunner.parse_outcome("not json").unwrap_err();
        assert!(matches!(err, OutcomeParseError::Malformed(_)));
    }

    #[test]
    fn parse_outcome_errors_on_missing_session_id() {
        let json = r#"{"num_turns": 3}"#;
        let err = ClaudeRunner.parse_outcome(json).unwrap_err();
        assert!(matches!(err, OutcomeParseError::MissingSessionId));
    }

    #[test]
    fn parse_outcome_errors_on_empty_session_id() {
        // Mirrors the doc comment: work.ml's jq `// empty` turns a missing
        // field into an empty string, which this port treats the same as
        // absent, not as a usable (empty) session id.
        let json = r#"{"session_id": ""}"#;
        let err = ClaudeRunner.parse_outcome(json).unwrap_err();
        assert!(matches!(err, OutcomeParseError::MissingSessionId));
    }

    #[test]
    fn parse_outcome_treats_empty_model_usage_map_as_none() {
        let json = r#"{"session_id": "sess-abc", "modelUsage": {}}"#;
        assert_eq!(ClaudeRunner.parse_outcome(json).unwrap().model_usage, None);
    }
}
