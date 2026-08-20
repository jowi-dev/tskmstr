//! Pure `claude` invocation builder for the lane runner, ported from
//! devtools' `~/devtools/work.ml`'s `run_lane`. This module owns only the
//! translation from already-resolved run inputs into the exact argv/env
//! `claude` needs — no process spawning, no environment reads, no clock
//! reads. Step 8 (`src/work/runner.rs`, not yet written) is the only thing
//! that actually spawns a process from a [`ClaudeInvocation`].
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
//! argv concern, so it isn't modeled here — step 8 owns it.
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
//! run id travels as `TSKMSTR_SESSION_RUN_ID` rather than
//! `TSKMSTR_RUN_ID`. See [`RunMode`].

use std::path::PathBuf;

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

/// Environment variable holding the tracked run id, exported for the
/// `TSKMSTR_RUN_ID`-gated hooks (see `src/work/hooks.rs`) to pick up.
/// Mirrors the detached wrapper's `export TSKMSTR_RUN_ID="$RUN_ID"`, which
/// only runs `if [ -n "$RUN_ID" ]` — i.e. the export is conditional on a
/// run actually being tracked, not unconditional.
const TSKMSTR_RUN_ID_VAR: &str = "TSKMSTR_RUN_ID";

/// Environment variable naming the pre-registered run row an *interactive*
/// session should adopt, read by
/// [`crate::runs::session::register_session`]. The same variable
/// [`crate::work::audit::SESSION_RUN_ID_ENV`] has always used for
/// tmux-hosted audit sessions.
const TSKMSTR_SESSION_RUN_ID_VAR: &str = "TSKMSTR_SESSION_RUN_ID";

/// How a run hosts its `claude` process — the fork every difference between
/// the two invocation shapes hangs off.
///
/// # The run-id environment variable is the load-bearing difference
///
/// `TSKMSTR_RUN_ID` is this codebase's flag for "a supervisor owns this
/// run's lifecycle". Three separate places read it that way:
/// `hooks/tm-session-end.sh` exits 0 immediately when it is set, and
/// [`crate::runs::session::register_session`] and
/// `crate::runs::session::finish_session` both short-circuit to a no-op on
/// it.
///
/// [`RunMode::Headless`] *has* that supervisor (`tm work __supervise`, see
/// `src/work/detach.rs`), which calls `RunStore::finish_run` itself, so
/// gating the session-level machinery off is correct there.
///
/// [`RunMode::Interactive`] has no supervisor at all. The SessionEnd hook is
/// the only thing that will ever finish the run, so it must not be gated
/// off: the run id travels as `TSKMSTR_SESSION_RUN_ID` instead, which
/// `register_session` treats as "adopt this pre-registered row" (the same
/// mechanism [`crate::work::audit::launch_audit`] has always used).
///
/// Swapping the two variables produces **no visible symptom**: `claude`
/// runs, the work gets done, and the run row simply sits at `running` until
/// `tm runs reap` eventually marks it failed. That is why
/// `run_mode_decides_which_run_id_env_var_claude_receives` pins the full env
/// set of both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// One-shot `claude -p`, spawned by a `setsid`'d supervisor (or,
    /// with `--fg`, synchronously by the invoking process). Machine-readable
    /// `--output-format json`, bounded by `--max-turns`, finished by the
    /// supervisor.
    Headless,
    /// A steerable `claude` session hosted in a tmux window: the prompt is
    /// positional (as in [`crate::work::audit::claude_command`]), there is
    /// no `--max-turns` budget to enforce and no result JSON to parse, and
    /// the run row is adopted and finished by the session's own hooks.
    Interactive,
}

/// Already-resolved inputs for one run's `claude` invocation. Everything
/// here is data the caller has already produced (prompt text read from the
/// prompt file and ticket-suffixed, the deployed hooks settings path, etc.)
/// — this struct carries no unresolved file paths that still need reading and
/// no clock/env reads of its own.
pub struct ClaudeInvocationInputs {
    /// Final prompt text: the `-p` value when headless, `claude`'s
    /// positional argument when interactive. Already includes the
    /// `"\n\nWork ticket: <ticket>."` suffix if a ticket was given —
    /// ticket-suffixing is `run_lane`'s job, not this module's.
    pub prompt: String,
    /// Driver model override. `None` resolves to [`DEFAULT_MODEL`].
    pub model: Option<String>,
    /// `--max-turns` override. `None` resolves to [`DEFAULT_MAX_TURNS`].
    /// Ignored entirely under [`RunMode::Interactive`], which passes no
    /// turn budget at all.
    pub max_turns: Option<String>,
    /// `--permission-mode` override. `None` resolves to
    /// [`DEFAULT_PERMISSION_MODE`].
    pub permission_mode: Option<String>,
    /// Path to the generated hooks `--settings` JSON file
    /// (`deploy_tm_hooks`'s return value in `work.ml`, step 6's
    /// `deploy_hooks()` in this port). Always passed — `work.ml` has no
    /// code path that runs `claude` without `--settings`.
    pub settings_path: PathBuf,
    /// The run id returned by `tm runs start`, if this run is tracked.
    /// `None` (or, defensively, `Some("")`) means untracked: no run-id
    /// variable is set at all, matching the wrapper script's `if [ -n
    /// "$RUN_ID" ]` guard.
    pub run_id: Option<String>,
    /// Whether this run is a headless `claude -p` lane run or an interactive
    /// tmux-hosted session. Decides the argv shape *and* which run-id
    /// environment variable carries `run_id` — see [`RunMode`], which is
    /// where the consequences of getting that wrong are spelled out.
    pub mode: RunMode,
}

/// A fully resolved `claude -p` invocation: the program, its argv, and the
/// environment deltas to apply before spawning. Pure data — nothing here
/// spawns a process, reads the environment, or reads the clock. Consumed by
/// step 8's process-spawning code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaudeInvocation {
    /// The program to spawn, always `"claude"`.
    pub program: String,
    /// Full argv (excluding the program name itself), in the exact order
    /// `work.ml` emits them: `-p`, `--model`, `--settings`,
    /// `--permission-mode`, `--output-format json`, `--max-turns`.
    pub args: Vec<String>,
    /// Environment variables to set before spawning (currently just
    /// `TSKMSTR_RUN_ID`, when tracked).
    pub env_set: Vec<(String, String)>,
    /// Environment variables that MUST be removed before spawning `claude`,
    /// mirroring `work.ml`'s `env -u ANTHROPIC_API_KEY -u
    /// ANTHROPIC_AUTH_TOKEN -u CLAUDECODE`.
    ///
    /// **Billing-safety, read before touching this list.** If any of these
    /// three are present in the spawned process's environment, `claude`
    /// silently bills the raw Anthropic API (or, for `CLAUDECODE`, behaves
    /// as if it's already inside a Claude Code session) instead of using
    /// the interactive claude.ai subscription these lane runs are meant to
    /// ride on. There is no error, no warning, no nonzero exit — the run
    /// completes normally and the bill just lands somewhere unexpected.
    /// This is the one thing in this whole module that must never be
    /// "simplified away"; see `docs/plans/runner-port.md` §7 Risks
    /// ("Billing-safety env stripping") and the dedicated regression test
    /// `billing_safety_env_vars_are_always_stripped`.
    pub env_remove: Vec<String>,
}

/// Build the [`ClaudeInvocation`] for one run from already-resolved
/// [`ClaudeInvocationInputs`], applying the same model/max-turns/
/// permission-mode defaults `work.ml`'s `run_lane` applies when a lane/run
/// option is absent.
///
/// [`ClaudeInvocationInputs::mode`] forks both halves of the result:
///
/// | | [`RunMode::Headless`] | [`RunMode::Interactive`] |
/// |---|---|---|
/// | prompt | `-p <prompt>` | positional, `args[0]` |
/// | `--output-format json` | yes (parsed by [`crate::work::runner::parse_run_outcome`]) | no — nothing parses an interactive session's stdout |
/// | `--max-turns` | yes | no — meaningless for a steerable session |
/// | run id travels as | `TSKMSTR_RUN_ID` | `TSKMSTR_SESSION_RUN_ID` |
///
/// `--model`, `--settings` and `--permission-mode` are common to both.
/// `--settings` especially: it is what deploys the SessionEnd hook that
/// finishes an interactive run, so an interactive invocation needs it at
/// least as much as a headless one does.
///
/// Read [`RunMode`] before touching the env-set fork.
pub fn build_claude_invocation(inputs: ClaudeInvocationInputs) -> ClaudeInvocation {
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
        // Positional prompt, mirroring `audit::claude_command`'s
        // `claude <prompt>`. Kept at `args[0]` so
        // `interactive::tmux_command_line` can swap it for a
        // `"$(cat <prompt file>)"` read without re-deriving the argv.
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
            RunMode::Headless => TSKMSTR_RUN_ID_VAR,
            RunMode::Interactive => TSKMSTR_SESSION_RUN_ID_VAR,
        };
        env_set.push((var.to_string(), run_id));
    }

    ClaudeInvocation {
        program: "claude".to_string(),
        args,
        env_set,
        env_remove: vec![
            "ANTHROPIC_API_KEY".to_string(),
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            "CLAUDECODE".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_inputs() -> ClaudeInvocationInputs {
        ClaudeInvocationInputs {
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
    fn fully_specified_lane_produces_exact_argv() {
        let invocation = build_claude_invocation(base_inputs());

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
        let inputs = ClaudeInvocationInputs {
            model: None,
            max_turns: None,
            permission_mode: None,
            ..base_inputs()
        };

        let invocation = build_claude_invocation(inputs);

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
        let inputs = ClaudeInvocationInputs {
            prompt: "multi\nline\nprompt with 'quotes' and \"stuff\"".to_string(),
            ..base_inputs()
        };

        let invocation = build_claude_invocation(inputs);

        let p_index = invocation.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(
            invocation.args[p_index + 1],
            "multi\nline\nprompt with 'quotes' and \"stuff\""
        );
    }

    #[test]
    fn settings_flag_carries_the_deployed_hooks_settings_path() {
        let invocation = build_claude_invocation(base_inputs());

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
        let invocation = build_claude_invocation(base_inputs());

        assert_eq!(
            invocation.env_set,
            vec![("TSKMSTR_RUN_ID".to_string(), "run-123".to_string())]
        );
    }

    #[test]
    fn absent_run_id_sets_no_env_vars() {
        let inputs = ClaudeInvocationInputs {
            run_id: None,
            ..base_inputs()
        };

        let invocation = build_claude_invocation(inputs);

        assert!(invocation.env_set.is_empty());
    }

    #[test]
    fn empty_run_id_is_treated_as_untracked_and_sets_no_env_vars() {
        // Defensive: the wrapper script's own guard is `[ -n "$RUN_ID" ]`,
        // i.e. an empty string is equivalent to untracked, not a real id.
        let inputs = ClaudeInvocationInputs {
            run_id: Some(String::new()),
            ..base_inputs()
        };

        let invocation = build_claude_invocation(inputs);

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
        let headless = build_claude_invocation(ClaudeInvocationInputs {
            mode: RunMode::Headless,
            ..base_inputs()
        });
        let interactive = build_claude_invocation(ClaudeInvocationInputs {
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
        let invocation = build_claude_invocation(ClaudeInvocationInputs {
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
        // See the doc comment on `ClaudeInvocation::env_remove`: if any of
        // these three survive into claude's environment, billing silently
        // shifts from the claude.ai subscription to the raw Anthropic API
        // (or claude misbehaves as if already nested in a session), with no
        // error and no nonzero exit to catch it. This must hold regardless
        // of what inputs the caller supplies.
        let invocation = build_claude_invocation(base_inputs());

        assert_eq!(
            invocation.env_remove,
            vec![
                "ANTHROPIC_API_KEY".to_string(),
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "CLAUDECODE".to_string(),
            ]
        );
    }
}
