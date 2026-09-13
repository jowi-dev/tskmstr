//! [`OpencodeRunner`]: the second [`crate::agent::AgentRunner`] implementation,
//! proving the seam issue #17 built for `claude` generalizes to a genuinely
//! different CLI. `docs/plans/gh-41-opencode-runner.md` is the verified
//! opencode v1.18.11 CLI contract this module binds to — argv shapes, the
//! NDJSON event stream, session identity, and the runner-neutral `[work]`
//! key mapping all live there; this module's doc comments point at it
//! rather than restating it.
//!
//! # Invocation shapes
//!
//! Headless (`RunMode::Headless`):
//!
//! ```text
//! opencode run --format json [--model <provider/model>] [--auto] -- <prompt>
//! ```
//!
//! Interactive (`RunMode::Interactive`, the default `opencode` TUI command):
//!
//! ```text
//! opencode [--model <provider/model>] [--auto] --prompt <prompt>
//! ```
//!
//! # Three deliberate divergences from `claude`
//!
//! - **No default model.** `--model` is omitted entirely when
//!   [`crate::agent::InvocationInputs::model`] is `None`, letting opencode's
//!   own multi-provider default-model resolution apply — unlike
//!   [`crate::agent::claude::ClaudeRunner`]'s always-pass-`fable` convention,
//!   which makes sense only for a single-provider CLI.
//! - **Empty `env_remove`.** `claude`'s `env_remove` strips
//!   `ANTHROPIC_*`/`CLAUDECODE` for subscription-billing safety; opencode's
//!   provider credentials are explicit config or env keys the user intends
//!   as credentials, so stripping anything would break legitimate setups.
//! - **No telemetry.** [`OpencodeRunner::deploy_telemetry`] and
//!   [`OpencodeRunner::install_user_hooks`] return `Ok(None)` — per
//!   ADR-0004 point 3, run start/finish recording never depends on `Some`.
//!   opencode-plugin telemetry (model-usage attribution, interactive
//!   SessionEnd-equivalent finish) is deferred to a follow-up issue.
//!
//! # Model pricing and roles reference
//!
//! Sourced from `opencode models --verbose venice` on 2026-09-13. Prices are
//! per-million tokens. The "role" column maps to Claude's fable/sonnet tier
//! split — see the lane's `model` config for the active reasoning model and
//! the lane prompt for the subagent model.
//!
//! | Model | Role | Input/M | Output/M | Cache Read/M | Cache Write/M |
//! |---|---|---|---|---|---|
//! | `venice/claude-fable-5` | Heavy reasoning (claude equivalent) | $12.00 | $60.00 | $1.20 | $15.00 |
//! | `venice/claude-opus-5` | Heavy reasoning | $6.00 | $30.00 | $0.60 | $7.50 |
//! | `venice/claude-sonnet-5` | Fast workhorse (claude equivalent) | $3.00 | $15.00 | $0.30 | $3.75 |
//! | `venice/z-ai-glm-5-3` | **Reasoning default for pckr** | $0.50 | $2.00 | $0.05 | $0.00 |
//! | `venice/z-ai-glm-5-3-flash` | **Subagent default for pckr** | $0.05 | $0.20 | $0.005 | $0.00 |
//! | `venice/deepseek-v4-pro-0813` | Heavy reasoning alternative | $1.65 | $4.95 | $0.165 | $0.00 |
//! | `venice/deepseek-v4-flash` | Cheap subagent alternative | $0.138 | $0.275 | $0.028 | $0.00 |
//! | `venice/qwen-3-7-max` | Heavy reasoning alternative | $2.70 | $10.80 | $0.27 | $0.00 |
//! | `venice/qwen-3-7-plus` | Mid-tier alternative | $0.50 | $2.00 | $0.05 | $0.00 |
//! | `venice/grok-4-6` | Heavy reasoning alternative | $2.27 | $11.35 | $0.23 | $0.00 |
//! | `venice/kimi-k3` | Heavy reasoning alternative | $3.75 | $15.00 | $0.375 | $0.00 |
//! | `venice/openai-gpt-55` | Heavy reasoning alternative | $6.25 | $25.00 | $0.625 | $0.00 |
//!
//! **Cost comparison for the glm-5-3 + glm-5-3-flash stack vs claude:**
//!
//! | Stack | Reasoning (in/out) | Subagent (in/out) | Reasoning cost vs fable |
//! |---|---|---|---|
//! | claude fable + sonnet | $12/$60 | $3/$15 | 1x (baseline) |
//! | glm-5-3 + glm-5-3-flash | $0.50/$2.00 | $0.05/$0.20 | **24x cheaper** |
//! | deepseek-v4-pro + v4-flash | $1.65/$4.95 | $0.138/$0.275 | **7x cheaper** |

use std::path::{Path, PathBuf};

use crate::agent::{
    AgentError, AgentInvocation, AgentRunner, InstallReport, InvocationInputs, OutcomeParseError,
    RunMode, RunOutcome, SessionEnvVars, shell_quote,
};
use crate::runs::pricing::ModelPrice;
use crate::work::naming::expand_tilde;

/// Price table for venice-routed models, sourced from `opencode models
/// --verbose venice` on 2026-09-13. Add an entry here for any new model that
/// shows up in interactive session usage and needs estimated-cost support.
/// Keys are the full `provider/model` string. See the module doc comment for
/// the full pricing/roles reference table.
///
/// Headless lane runs get authoritative cost from `parse_outcome`'s
/// `cost_usd` (opencode computes cost natively per step), so this table
/// only matters for interactive sessions where `estimate_missing_costs`
/// fills in estimated costs.
const PRICE_TABLE: &[(&str, ModelPrice)] = &[
    (
        "venice/claude-sonnet-5",
        ModelPrice {
            input_per_million: 3.00,
            output_per_million: 15.00,
            cache_read_per_million: 0.30,
            cache_write_per_million: 3.75,
        },
    ),
    (
        "venice/claude-opus-5",
        ModelPrice {
            input_per_million: 6.00,
            output_per_million: 30.00,
            cache_read_per_million: 0.60,
            cache_write_per_million: 7.50,
        },
    ),
    (
        "venice/claude-fable-5",
        ModelPrice {
            input_per_million: 12.00,
            output_per_million: 60.00,
            cache_read_per_million: 1.20,
            cache_write_per_million: 15.00,
        },
    ),
    (
        "venice/claude-sonnet-4-5",
        ModelPrice {
            input_per_million: 3.75,
            output_per_million: 18.75,
            cache_read_per_million: 0.375,
            cache_write_per_million: 4.69,
        },
    ),
    (
        "venice/claude-sonnet-4-6",
        ModelPrice {
            input_per_million: 3.60,
            output_per_million: 18.00,
            cache_read_per_million: 0.36,
            cache_write_per_million: 4.50,
        },
    ),
    (
        "venice/claude-opus-4-5",
        ModelPrice {
            input_per_million: 6.00,
            output_per_million: 30.00,
            cache_read_per_million: 0.60,
            cache_write_per_million: 7.50,
        },
    ),
    (
        "venice/z-ai-glm-5-3",
        ModelPrice {
            input_per_million: 0.50,
            output_per_million: 2.00,
            cache_read_per_million: 0.05,
            cache_write_per_million: 0.00,
        },
    ),
    (
        "venice/z-ai-glm-5-3-flash",
        ModelPrice {
            input_per_million: 0.05,
            output_per_million: 0.20,
            cache_read_per_million: 0.005,
            cache_write_per_million: 0.00,
        },
    ),
    (
        "venice/deepseek-v4-flash",
        ModelPrice {
            input_per_million: 0.138,
            output_per_million: 0.275,
            cache_read_per_million: 0.028,
            cache_write_per_million: 0.00,
        },
    ),
    (
        "venice/deepseek-v4-pro-0813",
        ModelPrice {
            input_per_million: 1.65,
            output_per_million: 4.95,
            cache_read_per_million: 0.165,
            cache_write_per_million: 0.00,
        },
    ),
    (
        "venice/grok-4-6",
        ModelPrice {
            input_per_million: 2.27,
            output_per_million: 11.35,
            cache_read_per_million: 0.23,
            cache_write_per_million: 0.00,
        },
    ),
    (
        "venice/qwen-3-7-max",
        ModelPrice {
            input_per_million: 2.70,
            output_per_million: 10.80,
            cache_read_per_million: 0.27,
            cache_write_per_million: 0.00,
        },
    ),
];

/// The `opencode` CLI [`AgentRunner`] implementation. Zero-sized, mirroring
/// [`crate::agent::claude::ClaudeRunner`]: a single `&'static OpencodeRunner`
/// (in production, a leaked `Box::new(OpencodeRunner)`; see `main.rs`'s
/// `agent_runner_for`) serves every call.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpencodeRunner;

impl AgentRunner for OpencodeRunner {
    fn name(&self) -> &'static str {
        "opencode"
    }

    /// The product brand is lowercase in opencode's own materials, unlike
    /// `claude`'s "Claude Code" — this is the exact string a user reads.
    fn display_name(&self) -> &'static str {
        "opencode"
    }

    /// opencode's native skills dir. It also discovers `skill/` (singular)
    /// and Claude-format `.claude/skills`, so `tm init`'s advisory probe
    /// (which only checks this one path) may under-detect skills opencode
    /// would actually find — acceptable for an advisory probe.
    fn skills_dir(&self, base: &Path) -> PathBuf {
        base.join(".opencode/skills")
    }

    /// Build the [`AgentInvocation`] for one run. See the module doc
    /// comment for the two argv shapes and `docs/plans/gh-41-opencode-runner.md`
    /// for the full contract; the permission-mode mapping, `max_turns`
    /// handling, and `settings_path` handling are documented inline below.
    fn build_invocation(&self, inputs: InvocationInputs) -> AgentInvocation {
        // Permission mapping (both modes): `None` or `Some("bypassPermissions")`
        // is the only CLI-level bypass opencode has, so both map to `--auto`.
        // Any other value is documented-ignored — opencode has no
        // plan/acceptEdits analog; its own `permission` config governs, and a
        // headless run auto-rejects any ask it isn't configured to allow
        // rather than hanging.
        let auto = matches!(
            inputs.permission_mode.as_deref(),
            None | Some("bypassPermissions")
        );

        let mut args = Vec::new();
        match inputs.mode {
            RunMode::Headless => {
                args.push("run".to_string());
                args.push("--format".to_string());
                args.push("json".to_string());
                if let Some(model) = inputs.model {
                    args.push("--model".to_string());
                    args.push(model);
                }
                if auto {
                    args.push("--auto".to_string());
                }
                args.push("--".to_string());
                args.push(inputs.prompt);
            }
            RunMode::Interactive => {
                // Kept at args[0]/args[1] ("--prompt", prompt) rather than a
                // bare positional prompt: opencode's interactive prompt is a
                // flag value, not positional like claude's, which is exactly
                // why this adapter overrides `tmux_command_line` instead of
                // relying on the default impl's `args[0]` convention.
                args.push("--prompt".to_string());
                args.push(inputs.prompt);
                if let Some(model) = inputs.model {
                    args.push("--model".to_string());
                    args.push(model);
                }
                if auto {
                    args.push("--auto".to_string());
                }
            }
        }

        // `max_turns` is entirely ignored, both modes: opencode has no CLI
        // turn budget. The only equivalent is per-agent `steps` in the
        // user's own opencode config, which tm doesn't own.
        //
        // `settings_path` is also ignored: `deploy_telemetry` always returns
        // `None` for this runner, so no `--settings`-equivalent flag exists
        // to emit.

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
            program: "opencode".to_string(),
            args,
            env_set,
            // Unlike claude's billing-safety strip, opencode's provider
            // credentials are explicit config or env keys the user intends
            // as credentials — stripping any of them would break a
            // legitimate setup rather than protect one. See the module doc
            // comment.
            env_remove: Vec::new(),
        }
    }

    /// Parses `opencode run --format json`'s NDJSON stdout into a
    /// [`RunOutcome`]. See `docs/plans/gh-41-opencode-runner.md`'s "Telemetry"
    /// section for the field-by-field rationale; the summary:
    ///
    /// | field | source |
    /// |---|---|
    /// | `session_id` | first event with a non-empty `sessionID` |
    /// | `cost_usd` | sum of `step_finish` `part.cost` |
    /// | `num_turns` | count of `step_finish` events |
    /// | `is_error` | `Some(true)` on any `error` event, `Some(false)` on a clean stream with >=1 `step_finish`, else `None` |
    /// | `result` | all `text` events' `part.text` (non-empty after trim), joined with `"\n\n"` |
    /// | `model_usage` | always `None` — see below |
    ///
    /// `model_usage` stays `None`: the JSON stream never names the model
    /// (only assistant *message* objects would, and those are never
    /// emitted), so per-model attribution is deferred to the
    /// opencode-plugin telemetry follow-up; `cost_usd` is already
    /// authoritative via `step_finish.part.cost`.
    ///
    /// Lines are parsed one at a time, tolerating stray non-JSON lines and
    /// blank lines (the contract promises pure NDJSON, but this stays
    /// defensive). If not a single line parses as an event at all, the
    /// first non-empty line's (or the raw string's, if empty/whitespace)
    /// parse error is propagated as [`OutcomeParseError::Malformed`] — this
    /// mirrors `ClaudeRunner::parse_outcome`'s "unparseable input is a hard
    /// error" rule, just applied per-line instead of to the whole payload.
    fn parse_outcome(&self, raw: &str) -> Result<RunOutcome, OutcomeParseError> {
        let mut events: Vec<RawEvent> = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<RawEvent>(line) {
                events.push(event);
            }
        }

        if events.is_empty() {
            let first_non_empty = raw.lines().find(|line| !line.trim().is_empty());
            let to_parse = first_non_empty.unwrap_or(raw);
            let err = serde_json::from_str::<RawEvent>(to_parse).unwrap_err();
            return Err(OutcomeParseError::Malformed(err));
        }

        let session_id = events
            .iter()
            .find_map(|event| event.session_id.clone().filter(|id| !id.is_empty()))
            .ok_or(OutcomeParseError::MissingSessionId)?;

        let step_finishes: Vec<&RawEvent> = events
            .iter()
            .filter(|event| event.event_type.as_deref() == Some("step_finish"))
            .collect();
        let has_error = events
            .iter()
            .any(|event| event.event_type.as_deref() == Some("error"));

        let cost_usd = if step_finishes.is_empty() {
            None
        } else {
            Some(
                step_finishes
                    .iter()
                    .filter_map(|event| event.part.as_ref().and_then(|part| part.cost))
                    .sum(),
            )
        };

        let num_turns = if step_finishes.is_empty() {
            None
        } else {
            Some(step_finishes.len() as u64)
        };

        let is_error = if has_error {
            Some(true)
        } else if !step_finishes.is_empty() {
            Some(false)
        } else {
            None
        };

        let texts: Vec<String> = events
            .iter()
            .filter(|event| event.event_type.as_deref() == Some("text"))
            .filter_map(|event| event.part.as_ref().and_then(|part| part.text.clone()))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        let result = if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n\n"))
        };

        Ok(RunOutcome {
            session_id,
            cost_usd,
            num_turns,
            is_error,
            result,
            // The JSON stream never names the model; see this method's doc
            // comment.
            model_usage: None,
        })
    }

    fn resume_command(&self, session_id: &str) -> String {
        format!("opencode --session {session_id}")
    }

    /// Builds `opencode --model <model> --prompt <prompt>` (or `opencode
    /// --prompt <prompt>` when `model` is `None`), every value
    /// [`shell_quote`]d. Mirrors [`crate::agent::claude::ClaudeRunner`]'s
    /// same-named method; `--model` is omitted (not defaulted) when absent,
    /// same rationale as `build_invocation`'s headless/interactive `--model`
    /// handling.
    fn interactive_shell_command(&self, model: Option<&str>, prompt: &str) -> String {
        match model {
            Some(model) => format!(
                "opencode --model {} --prompt {}",
                shell_quote(model),
                shell_quote(prompt)
            ),
            None => format!("opencode --prompt {}", shell_quote(prompt)),
        }
    }

    /// Overrides the default impl: opencode's interactive prompt is the
    /// value *after* `"--prompt"` (`args[0] == "--prompt"`, `args[1] ==
    /// prompt`, per `build_invocation`), not a bare positional at `args[0]`
    /// like claude's. Since [`AgentInvocation::env_remove`] is always empty
    /// for this runner, no `env -u` prefix is ever needed in practice, but
    /// the general shape stays resilient: if `env_remove` were ever
    /// non-empty, this still emits the same `env -u VAR...` prefix the
    /// default impl does.
    fn tmux_command_line(&self, invocation: &AgentInvocation, prompt_file: &Path) -> String {
        let mut parts = Vec::new();
        if !invocation.env_remove.is_empty() {
            parts.push("env".to_string());
            for var in &invocation.env_remove {
                parts.push("-u".to_string());
                parts.push(var.clone());
            }
        }
        parts.push(invocation.program.clone());
        parts.push(shell_quote("--prompt"));
        parts.push(format!(
            "\"$(cat {})\"",
            shell_quote(&prompt_file.to_string_lossy())
        ));
        for arg in invocation.args.iter().skip(2) {
            parts.push(shell_quote(arg));
        }
        parts.join(" ")
    }

    /// The TUI's `--prompt` interprets a leading `/name` as a custom
    /// command when one by that name exists — opencode also discovers
    /// Claude-format skills natively, so the same skill set backs these
    /// claude-shaped defaults unchanged.
    fn default_audit_prompt_template(&self) -> &'static str {
        "/ticket-audit {key}"
    }

    fn default_cleanup_prompt_template(&self) -> &'static str {
        "/bugbot-triage {key} {findings_file}"
    }

    fn default_create_prompt(&self) -> &'static str {
        "/ticket-create"
    }

    /// `~/.config/opencode/prompts/<lane>.md`, mirroring claude's
    /// `~/.claude/prompts/<lane>.md` convention inside opencode's own XDG
    /// config dir.
    fn default_lane_prompt_path(&self, home: &Path, lane: &str) -> PathBuf {
        expand_tilde(&format!("~/.config/opencode/prompts/{lane}.md"), home)
    }

    /// Always `Ok(None)`: opencode has no telemetry artifacts to deploy in
    /// this phase. Per ADR-0004 point 3, run start/finish recording never
    /// depends on this returning `Some` — only the telemetry-driven extras
    /// (session-usage cost beyond `parse_outcome`'s own `cost_usd`,
    /// checklist/task events, an interactive SessionEnd-equivalent finish)
    /// are lost. The opencode-plugin telemetry follow-up is where this
    /// changes.
    fn deploy_telemetry(&self, _deploy_dir: &Path) -> Result<Option<PathBuf>, AgentError> {
        Ok(None)
    }

    /// Always `Ok(None)`: no user-level telemetry hooks exist for this
    /// runner yet. See [`OpencodeRunner::deploy_telemetry`]'s doc comment.
    fn install_user_hooks(
        &self,
        _home: &Path,
        _xdg_data_home: Option<&Path>,
        _backup_suffix: &str,
        _dry_run: bool,
    ) -> Result<Option<InstallReport>, AgentError> {
        Ok(None)
    }

    /// Always `false`: there is nothing to install yet. See
    /// [`OpencodeRunner::deploy_telemetry`]'s doc comment.
    fn user_hooks_installed(&self, _home: &Path, _xdg_data_home: Option<&Path>) -> bool {
        false
    }

    /// `OPENCODE_SESSION_ID`/`OPENCODE_PID`. Verified against the v1.18.11
    /// source: opencode exports `OPENCODE=1` and `OPENCODE_PID` into its own
    /// process env at startup (inherited by tool subprocesses), but sets
    /// **no** session-id variable. `OPENCODE_SESSION_ID` is a
    /// forward-compatible placeholder name that
    /// [`crate::runs::session::SessionEnv::from_process_env`] degrades to
    /// `None` for by design (an absent var is never an error there) —
    /// harmless today, and it starts working for free if opencode ever
    /// sets it.
    fn session_env_vars(&self) -> SessionEnvVars {
        SessionEnvVars {
            session_id: "OPENCODE_SESSION_ID",
            pid: "OPENCODE_PID",
        }
    }

    /// Looks up `model`'s [`ModelPrice`] in [`PRICE_TABLE`] by exact name
    /// match. `None` for any model not yet priced here. Headless lane runs
    /// already get authoritative cost from `parse_outcome`'s `cost_usd`
    /// (opencode computes cost natively per step); this table only matters
    /// for interactive sessions where `estimate_missing_costs` fills in
    /// estimated costs.
    fn price_for_model(&self, model: &str) -> Option<ModelPrice> {
        PRICE_TABLE
            .iter()
            .find(|(name, _)| *name == model)
            .map(|(_, price)| *price)
    }

    /// Strips a leading `provider/` prefix by splitting on the *first*
    /// `/`, mirroring opencode's own `provider/model` spelling (e.g.
    /// `anthropic/claude-sonnet-4-5` -> `claude-sonnet-4-5`). A model name
    /// with an embedded `/` after the provider (e.g.
    /// `openrouter/meta/llama-3`) keeps everything after the first slash
    /// (`meta/llama-3`); a name with no slash at all is returned unchanged.
    fn display_model_name<'a>(&self, model: &'a str) -> &'a str {
        model.split_once('/').map(|(_, m)| m).unwrap_or(model)
    }
}

/// Raw shape of one NDJSON event line from `opencode run --format json`,
/// deserialized leniently: every field is optional so that one event with
/// an unrecognized/missing field doesn't fail the whole line. `type` is
/// named `event_type` here (`type` is a Rust keyword).
#[derive(Debug, Default, serde::Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    #[serde(default)]
    part: Option<RawPart>,
}

/// The `part` payload carried by `step_finish` (`cost`, `tokens` — tokens
/// unused, cost feeds [`RunOutcome::cost_usd`]) and `text` (`text`) events.
#[derive(Debug, Default, serde::Deserialize)]
struct RawPart {
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_inputs() -> InvocationInputs {
        InvocationInputs {
            prompt: "do the thing".to_string(),
            model: Some("anthropic/claude-sonnet-4-5".to_string()),
            max_turns: Some("300".to_string()),
            permission_mode: Some("bypassPermissions".to_string()),
            settings_path: None,
            run_id: Some("run-123".to_string()),
            mode: RunMode::Headless,
        }
    }

    // --- identity/paths/defaults ---

    #[test]
    fn opencode_runner_name_is_opencode() {
        assert_eq!(OpencodeRunner.name(), "opencode");
    }

    #[test]
    fn display_name_is_lowercase_opencode() {
        assert_eq!(OpencodeRunner.display_name(), "opencode");
    }

    #[test]
    fn skills_dir_is_dot_opencode_skills() {
        assert_eq!(
            OpencodeRunner.skills_dir(Path::new("/repo")),
            PathBuf::from("/repo/.opencode/skills")
        );
    }

    #[test]
    fn resume_command_names_the_opencode_cli() {
        assert_eq!(
            OpencodeRunner.resume_command("ses_123"),
            "opencode --session ses_123"
        );
    }

    #[test]
    fn default_audit_prompt_template_is_ticket_audit() {
        assert_eq!(
            OpencodeRunner.default_audit_prompt_template(),
            "/ticket-audit {key}"
        );
    }

    #[test]
    fn default_cleanup_prompt_template_is_bugbot_triage() {
        assert_eq!(
            OpencodeRunner.default_cleanup_prompt_template(),
            "/bugbot-triage {key} {findings_file}"
        );
    }

    #[test]
    fn default_create_prompt_is_ticket_create() {
        assert_eq!(OpencodeRunner.default_create_prompt(), "/ticket-create");
    }

    #[test]
    fn default_lane_prompt_path_is_opencode_config_prompts() {
        assert_eq!(
            OpencodeRunner.default_lane_prompt_path(Path::new("/home/j"), "mylane"),
            PathBuf::from("/home/j/.config/opencode/prompts/mylane.md")
        );
    }

    #[test]
    fn deploy_telemetry_always_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            OpencodeRunner
                .deploy_telemetry(dir.path())
                .expect("should succeed"),
            None
        );
    }

    #[test]
    fn install_user_hooks_always_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        assert!(
            OpencodeRunner
                .install_user_hooks(&home, None, "20260101-000000", false)
                .expect("should succeed")
                .is_none()
        );
    }

    #[test]
    fn user_hooks_installed_is_always_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        assert!(!OpencodeRunner.user_hooks_installed(&home, None));
    }

    #[test]
    fn session_env_vars_name_the_opencode_variables() {
        let vars = OpencodeRunner.session_env_vars();
        assert_eq!(vars.session_id, "OPENCODE_SESSION_ID");
        assert_eq!(vars.pid, "OPENCODE_PID");
    }

    #[test]
    fn price_for_model_finds_known_venice_models() {
        assert!(
            OpencodeRunner
                .price_for_model("venice/claude-sonnet-5")
                .is_some()
        );
        assert!(
            OpencodeRunner
                .price_for_model("venice/z-ai-glm-5-3")
                .is_some()
        );
        assert!(
            OpencodeRunner
                .price_for_model("venice/deepseek-v4-flash")
                .is_some()
        );
    }

    #[test]
    fn price_for_model_returns_none_for_unknown_model() {
        assert_eq!(
            OpencodeRunner.price_for_model("anthropic/claude-sonnet-4-5"),
            None
        );
    }

    #[test]
    fn display_model_name_strips_the_provider_prefix() {
        assert_eq!(
            OpencodeRunner.display_model_name("anthropic/claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            OpencodeRunner.display_model_name("openrouter/meta/llama-3"),
            "meta/llama-3"
        );
        assert_eq!(OpencodeRunner.display_model_name("no-slash"), "no-slash");
    }

    // --- build_invocation ---

    #[test]
    fn headless_exact_argv_with_model_and_auto() {
        let invocation = OpencodeRunner.build_invocation(base_inputs());

        assert_eq!(invocation.program, "opencode");
        assert_eq!(
            invocation.args,
            vec![
                "run",
                "--format",
                "json",
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--auto",
                "--",
                "do the thing",
            ]
        );
    }

    #[test]
    fn interactive_exact_argv_with_model_and_auto() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Interactive,
            ..base_inputs()
        });

        assert_eq!(
            invocation.args,
            vec![
                "--prompt",
                "do the thing",
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--auto",
            ]
        );
    }

    #[test]
    fn model_omitted_when_none_in_both_modes() {
        let headless = OpencodeRunner.build_invocation(InvocationInputs {
            model: None,
            ..base_inputs()
        });
        assert!(!headless.args.iter().any(|a| a == "--model"));

        let interactive = OpencodeRunner.build_invocation(InvocationInputs {
            model: None,
            mode: RunMode::Interactive,
            ..base_inputs()
        });
        assert!(!interactive.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn permission_mode_none_maps_to_auto() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            permission_mode: None,
            ..base_inputs()
        });
        assert!(invocation.args.iter().any(|a| a == "--auto"));
    }

    #[test]
    fn permission_mode_bypass_permissions_maps_to_auto() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            permission_mode: Some("bypassPermissions".to_string()),
            ..base_inputs()
        });
        assert!(invocation.args.iter().any(|a| a == "--auto"));
    }

    #[test]
    fn permission_mode_accept_edits_maps_to_no_flag_at_all() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            permission_mode: Some("acceptEdits".to_string()),
            ..base_inputs()
        });
        assert!(!invocation.args.iter().any(|a| a == "--auto"));
        // No other permission-mode flag exists for opencode at all.
        assert!(!invocation.args.iter().any(|a| a.contains("acceptEdits")));
    }

    #[test]
    fn max_turns_is_ignored_entirely_in_both_modes() {
        let headless = OpencodeRunner.build_invocation(InvocationInputs {
            max_turns: Some("999".to_string()),
            ..base_inputs()
        });
        assert!(!headless.args.iter().any(|a| a == "999"));
        assert!(!headless.args.iter().any(|a| a.contains("max-turn")));

        let interactive = OpencodeRunner.build_invocation(InvocationInputs {
            max_turns: Some("999".to_string()),
            mode: RunMode::Interactive,
            ..base_inputs()
        });
        assert!(!interactive.args.iter().any(|a| a == "999"));
    }

    #[test]
    fn settings_path_never_emits_a_flag() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            settings_path: Some(PathBuf::from("/hooks/settings.json")),
            ..base_inputs()
        });
        assert!(!invocation.args.iter().any(|a| a.contains("settings")));
    }

    #[test]
    fn run_mode_decides_which_run_id_env_var_opencode_receives() {
        let headless = OpencodeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Headless,
            ..base_inputs()
        });
        let interactive = OpencodeRunner.build_invocation(InvocationInputs {
            mode: RunMode::Interactive,
            ..base_inputs()
        });

        assert_eq!(
            headless.env_set,
            vec![("TSKMSTR_RUN_ID".to_string(), "run-123".to_string())]
        );
        assert_eq!(
            interactive.env_set,
            vec![("TSKMSTR_SESSION_RUN_ID".to_string(), "run-123".to_string())]
        );
    }

    #[test]
    fn absent_run_id_sets_no_env_vars() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            run_id: None,
            ..base_inputs()
        });
        assert!(invocation.env_set.is_empty());
    }

    #[test]
    fn empty_run_id_is_treated_as_untracked() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            run_id: Some(String::new()),
            ..base_inputs()
        });
        assert!(invocation.env_set.is_empty());
    }

    #[test]
    fn env_remove_is_always_empty() {
        let invocation = OpencodeRunner.build_invocation(base_inputs());
        assert!(invocation.env_remove.is_empty());
    }

    // --- parse_outcome ---

    #[test]
    fn parse_outcome_parses_a_realistic_multi_line_stream() {
        let raw = [
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_a"}"#,
            r#"{"type":"tool_use","timestamp":2,"sessionID":"ses_a"}"#,
            r#"{"type":"step_finish","timestamp":3,"sessionID":"ses_a","part":{"cost":0.01,"tokens":{"input":10,"output":20}}}"#,
            r#"{"type":"text","timestamp":4,"sessionID":"ses_a","part":{"text":"first message"}}"#,
            r#"{"type":"step_start","timestamp":5,"sessionID":"ses_a"}"#,
            r#"{"type":"step_finish","timestamp":6,"sessionID":"ses_a","part":{"cost":0.02,"tokens":{"input":15,"output":25}}}"#,
            r#"{"type":"text","timestamp":7,"sessionID":"ses_a","part":{"text":"second message"}}"#,
        ]
        .join("\n");

        let outcome = OpencodeRunner.parse_outcome(&raw).expect("should parse");

        assert_eq!(outcome.session_id, "ses_a");
        assert!((outcome.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(outcome.num_turns, Some(2));
        assert_eq!(outcome.is_error, Some(false));
        assert_eq!(
            outcome.result,
            Some("first message\n\nsecond message".to_string())
        );
        assert_eq!(outcome.model_usage, None);
    }

    #[test]
    fn parse_outcome_parses_an_error_stream() {
        let raw = r#"{"type":"error","timestamp":1,"sessionID":"ses_x","error":{"name":"APIError","data":{"message":"boom"}}}"#;

        let outcome = OpencodeRunner.parse_outcome(raw).expect("should parse");

        assert_eq!(outcome.session_id, "ses_x");
        assert_eq!(outcome.is_error, Some(true));
        assert_eq!(outcome.cost_usd, None);
        assert_eq!(outcome.num_turns, None);
    }

    #[test]
    fn parse_outcome_with_text_but_no_step_finish_leaves_is_error_none() {
        let raw = r#"{"type":"text","timestamp":1,"sessionID":"ses_y","part":{"text":"hi"}}"#;

        let outcome = OpencodeRunner.parse_outcome(raw).expect("should parse");

        assert_eq!(outcome.is_error, None);
        assert_eq!(outcome.cost_usd, None);
        assert_eq!(outcome.num_turns, None);
        assert_eq!(outcome.result, Some("hi".to_string()));
    }

    #[test]
    fn parse_outcome_errors_on_empty_input() {
        let err = OpencodeRunner.parse_outcome("").unwrap_err();
        assert!(matches!(err, OutcomeParseError::Malformed(_)));
    }

    #[test]
    fn parse_outcome_errors_on_garbage_input() {
        let err = OpencodeRunner.parse_outcome("not json at all").unwrap_err();
        assert!(matches!(err, OutcomeParseError::Malformed(_)));
    }

    #[test]
    fn parse_outcome_errors_on_missing_session_id() {
        let raw = r#"{"type":"step_finish","timestamp":1,"part":{"cost":0.01}}"#;
        let err = OpencodeRunner.parse_outcome(raw).unwrap_err();
        assert!(matches!(err, OutcomeParseError::MissingSessionId));
    }

    #[test]
    fn parse_outcome_tolerates_blank_lines_and_one_non_json_line() {
        let raw = [
            "",
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_z"}"#,
            "   ",
            "this is not json and should be skipped",
            r#"{"type":"step_finish","timestamp":2,"sessionID":"ses_z","part":{"cost":0.05}}"#,
            "",
        ]
        .join("\n");

        let outcome = OpencodeRunner.parse_outcome(&raw).expect("should parse");

        assert_eq!(outcome.session_id, "ses_z");
        assert_eq!(outcome.cost_usd, Some(0.05));
        assert_eq!(outcome.num_turns, Some(1));
        assert_eq!(outcome.is_error, Some(false));
    }

    // --- interactive_shell_command ---

    #[test]
    fn interactive_shell_command_quotes_model_and_prompt() {
        assert_eq!(
            OpencodeRunner
                .interactive_shell_command(Some("anthropic/opus"), "/ticket-audit PROJ-9"),
            "opencode --model 'anthropic/opus' --prompt '/ticket-audit PROJ-9'"
        );
        assert_eq!(
            OpencodeRunner.interactive_shell_command(None, "/ticket-audit PROJ-9"),
            "opencode --prompt '/ticket-audit PROJ-9'"
        );
    }

    // --- tmux_command_line ---

    #[test]
    fn tmux_command_line_reads_the_prompt_from_the_file_with_no_env_prefix() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            prompt: "do the thing".to_string(),
            model: Some("anthropic/claude-sonnet-4-5".to_string()),
            max_turns: None,
            permission_mode: Some("bypassPermissions".to_string()),
            settings_path: None,
            run_id: Some("7".to_string()),
            mode: RunMode::Interactive,
        });

        let command =
            OpencodeRunner.tmux_command_line(&invocation, Path::new("/state/proj-1.prompt.md"));

        assert_eq!(
            command,
            "opencode '--prompt' \"$(cat '/state/proj-1.prompt.md')\" \
             '--model' 'anthropic/claude-sonnet-4-5' '--auto'"
        );
        assert!(
            !command.contains("do the thing"),
            "the prompt itself must never reach the command string — it is unbounded"
        );
        assert!(
            !command.starts_with("env"),
            "env_remove is empty for opencode, so no env -u prefix should appear"
        );
    }

    #[test]
    fn tmux_command_line_quotes_a_prompt_path_with_a_quote_in_it() {
        let invocation = OpencodeRunner.build_invocation(InvocationInputs {
            prompt: "prompt".to_string(),
            model: None,
            max_turns: None,
            permission_mode: Some("bypassPermissions".to_string()),
            settings_path: None,
            run_id: Some("7".to_string()),
            mode: RunMode::Interactive,
        });

        let command =
            OpencodeRunner.tmux_command_line(&invocation, Path::new("/state/o'brien.prompt.md"));

        assert!(command.contains(r#""$(cat '/state/o'\''brien.prompt.md')""#));
    }
}
