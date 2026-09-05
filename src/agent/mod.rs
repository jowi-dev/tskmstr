//! Backend-agnostic AI coding agent trait, and the runner-agnostic types
//! every consumer sees.
//!
//! [`AgentRunner`] is the interface every lane-run/review-fix orchestration
//! function in [`crate::work`] depends on, mirroring the ticket-backend
//! pattern [`crate::ticketing::provider::TicketProvider`] established for
//! issue #3 (see that module's doc comment) and `docs/decisions/0003-ticket-providers.md`'s
//! one-enum-one-dispatch precedent: [`crate::config::AgentKind`] is the one
//! enum a new agent adapter extends, `main.rs`'s `agent_runner_for` is the
//! one `match` on it outside config parsing, and every caller downstream of
//! that factory holds a `&dyn AgentRunner` rather than naming a concrete
//! runner. [`crate::agent::claude::ClaudeRunner`] is the only implementation
//! so far; see GitHub issue #17 and `docs/plans/agent-runner.md` for the
//! full phase plan this module is phase 3 of.
//!
//! Phase 2 moved the invocation builder — [`RunMode`], [`InvocationInputs`],
//! [`AgentInvocation`], and the `TSKMSTR_RUN_ID`/`TSKMSTR_SESSION_RUN_ID`
//! env-var contract — out of `src/work/claude.rs` (deleted) and behind
//! [`AgentRunner::build_invocation`]. This phase moves result parsing —
//! [`RunOutcome`], [`OutcomeParseError`], and the `parse_run_outcome` logic —
//! out of `src/work/runner.rs` and behind [`AgentRunner::parse_outcome`],
//! and adds [`AgentRunner::resume_command`] so every user-facing resume hint
//! is sourced from the adapter instead of a `claude --resume` literal
//! scattered across callers. Phase 4 moves interactive shell-string
//! rendering ([`AgentRunner::interactive_shell_command`],
//! [`AgentRunner::tmux_command_line`]), the default audit/cleanup prompt
//! templates, the default lane-prompt path, and the branch-owner fallback
//! behind the trait, and moves the shared [`shell_quote`] helper here from
//! `src/work/audit.rs`. Later phases add telemetry deployment and pricing.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::runs::ModelUsageMap;

pub mod claude;

/// Quotes `s` as a single POSIX shell word: wraps it in single quotes,
/// escaping any embedded single quote as `'\''`. Needed because
/// [`crate::work::tmux::TmuxOps::new_session_with_command`]'s `command`
/// argument is a single string tmux hands to the user's `$SHELL -c` —
/// unlike the rest of this codebase's `Command`/argv-based shelling-out
/// (which never touches a shell's string-splicing rules at all), this one
/// positional string must itself be valid shell syntax.
///
/// Shared by every caller that renders a shell command line for a
/// tmux-hosted session: [`crate::work::audit::launch_audit`],
/// [`crate::work::bugbot::launch_cleanup`], and
/// [`AgentRunner::tmux_command_line`]'s default implementation. Moved here
/// (from `src/work/audit.rs`) in phase 4 of GitHub issue #17 so it sits
/// alongside the shell-string rendering it backs.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Environment variable holding the tracked run id, exported for the
/// `TSKMSTR_RUN_ID`-gated hooks (see `src/agent/claude/hooks.rs`, not yet
/// moved) to pick up. Mirrors the detached wrapper's `export
/// TSKMSTR_RUN_ID="$RUN_ID"`, which only runs `if [ -n "$RUN_ID" ]` — i.e.
/// the export is conditional on a run actually being tracked, not
/// unconditional.
///
/// This is tm's own run-tracking contract, not any particular agent's — an
/// adapter's [`AgentRunner::build_invocation`] must honor the
/// [`RunMode`]-to-env-var mapping documented on that enum regardless of
/// which agent it drives.
pub const TSKMSTR_RUN_ID: &str = "TSKMSTR_RUN_ID";

/// Environment variable naming the pre-registered run row an *interactive*
/// session should adopt, read by
/// [`crate::runs::session::register_session`]. The same variable
/// [`crate::work::audit::SESSION_RUN_ID_ENV`] has always used for
/// tmux-hosted audit sessions.
///
/// Like [`TSKMSTR_RUN_ID`], this is tm's own contract: any adapter's
/// [`AgentRunner::build_invocation`] must set this (never
/// [`TSKMSTR_RUN_ID`]) for [`RunMode::Interactive`].
pub const TSKMSTR_SESSION_RUN_ID: &str = "TSKMSTR_SESSION_RUN_ID";

/// How a run hosts its agent process — the fork every difference between
/// the two invocation shapes hangs off.
///
/// # The run-id environment variable is the load-bearing difference
///
/// [`TSKMSTR_RUN_ID`] is this codebase's flag for "a supervisor owns this
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
/// off: the run id travels as [`TSKMSTR_SESSION_RUN_ID`] instead, which
/// `register_session` treats as "adopt this pre-registered row" (the same
/// mechanism [`crate::work::audit::launch_audit`] has always used).
///
/// Swapping the two variables produces **no visible symptom**: the agent
/// runs, the work gets done, and the run row simply sits at `running` until
/// `tm runs reap` eventually marks it failed. That is why
/// `run_mode_decides_which_run_id_env_var_claude_receives` (in
/// `src/agent/claude/mod.rs`) pins the full env set of both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// One-shot headless invocation, spawned by a `setsid`'d supervisor (or,
    /// with `--fg`, synchronously by the invoking process). Machine-readable
    /// output, bounded by a turn budget, finished by the supervisor.
    ///
    /// The default, despite the CLI defaulting to
    /// [`RunMode::Interactive`]: this is the self-contained shape, needing
    /// no tmux server and no in-session adoption, so it is the safer thing
    /// for a programmatic caller that has not thought about window hosting
    /// to fall into. The CLI always states its choice explicitly.
    #[default]
    Headless,
    /// A steerable session hosted in a tmux window: the prompt is
    /// positional (as in [`AgentRunner::interactive_shell_command`]), there is
    /// no turn budget to enforce and no result JSON to parse, and the run
    /// row is adopted and finished by the session's own hooks.
    Interactive,
}

/// Already-resolved inputs for one run's agent invocation. Everything here
/// is data the caller has already produced (prompt text read from the
/// prompt file and ticket-suffixed, the deployed hooks settings path, etc.)
/// — this struct carries no unresolved file paths that still need reading
/// and no clock/env reads of its own. Renamed from `ClaudeInvocationInputs`
/// (`src/work/claude.rs`, deleted): the shape is claude's today because
/// claude is the only adapter, but nothing in it is claude-specific.
pub struct InvocationInputs {
    /// Final prompt text: the headless invocation's `-p` value, or the
    /// interactive invocation's positional argument. Already includes the
    /// `"\n\nWork ticket: <ticket>."` suffix if a ticket was given —
    /// ticket-suffixing is the caller's job, not this module's.
    pub prompt: String,
    /// Driver model override. `None` resolves to the adapter's own default.
    pub model: Option<String>,
    /// Turn-budget override. `None` resolves to the adapter's own default.
    /// Ignored entirely under [`RunMode::Interactive`], which passes no
    /// turn budget at all.
    pub max_turns: Option<String>,
    /// Permission-mode override. `None` resolves to the adapter's own
    /// default.
    pub permission_mode: Option<String>,
    /// Path to the generated hooks `--settings` JSON file
    /// (`deploy_tm_hooks`'s return value in `work.ml`, `hooks::deploy_hooks`
    /// in this port). Always passed — there is no code path that runs the
    /// agent without a settings file.
    pub settings_path: PathBuf,
    /// The run id returned by `tm runs start`, if this run is tracked.
    /// `None` (or, defensively, `Some("")`) means untracked: no run-id
    /// variable is set at all, matching the wrapper script's `if [ -n
    /// "$RUN_ID" ]` guard.
    pub run_id: Option<String>,
    /// Whether this run is a headless one-shot lane run or an interactive
    /// tmux-hosted session. Decides the argv shape *and* which run-id
    /// environment variable carries `run_id` — see [`RunMode`], which is
    /// where the consequences of getting that wrong are spelled out.
    pub mode: RunMode,
}

/// A fully resolved agent invocation: the program, its argv, and the
/// environment deltas to apply before spawning. Pure data — nothing here
/// spawns a process, reads the environment, or reads the clock.
///
/// Renamed from `ClaudeInvocation` (`src/work/claude.rs`, deleted), but
/// **the serde shape is deliberately unchanged**: field names `program`,
/// `args`, `env_set`, `env_remove` round-trip through the detached
/// supervisor's state file (`src/work/detach.rs`'s `SupervisorState`,
/// written by one process and read back by a re-exec'd one that shares no
/// memory with it), so renaming or reshaping any field here would break an
/// in-flight supervised run across the rename.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentInvocation {
    /// The program to spawn, e.g. `"claude"`.
    pub program: String,
    /// Full argv (excluding the program name itself), in adapter-defined
    /// order.
    pub args: Vec<String>,
    /// Environment variables to set before spawning (currently just the
    /// tracked run id, under [`TSKMSTR_RUN_ID`] or
    /// [`TSKMSTR_SESSION_RUN_ID`] depending on [`RunMode`]).
    pub env_set: Vec<(String, String)>,
    /// Environment variables that MUST be removed before spawning the
    /// agent process. See [`crate::agent::claude::ClaudeRunner`]'s
    /// `build_invocation` doc comment for the claude-specific
    /// billing-safety rationale this field exists to satisfy.
    pub env_remove: Vec<String>,
}

/// Errors from [`AgentRunner::parse_outcome`].
#[derive(Debug, Error)]
pub enum OutcomeParseError {
    /// The raw result text did not parse as JSON at all.
    #[error("failed to parse agent result JSON: {0}")]
    Malformed(#[from] serde_json::Error),

    /// The JSON parsed, but `session_id` was missing, empty, or not a
    /// string. See [`AgentRunner::parse_outcome`]'s doc comment for why
    /// `session_id` is the one field every adapter must treat as required.
    #[error("agent result JSON is missing a non-empty session_id")]
    MissingSessionId,
}

/// The typed result of one finished headless agent invocation, parsed from
/// whatever raw result text the adapter's process wrote (e.g. `claude -p
/// --output-format json`'s stdout). The exact raw shape is adapter-owned —
/// see [`crate::agent::claude::ClaudeRunner`]'s `parse_outcome` for the
/// `claude`-specific field mapping this shape mirrors today — but this
/// struct itself is runner-agnostic: it's what every caller downstream of
/// [`AgentRunner::parse_outcome`] depends on.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// The resumable session id, required — see
    /// [`OutcomeParseError::MissingSessionId`]. Feeds `tm runs finish
    /// --session-id` and [`AgentRunner::resume_command`].
    pub session_id: String,
    /// Estimated cost in USD for this invocation, absent when the adapter's
    /// result carried no such field.
    pub cost_usd: Option<f64>,
    /// Number of turns/steps the agent took, absent when the adapter's
    /// result carried no such field.
    pub num_turns: Option<u64>,
    /// Whether the agent reported this invocation as an error, verbatim —
    /// `None` when the underlying field is entirely absent from the result,
    /// distinct from an explicit `false`.
    ///
    /// An absent value is exactly the shape a mid-run event like a
    /// usage-limit forced model switch can leave behind — the turn ends
    /// gracefully (the process exits 0) but never reports an error field at
    /// all. The caller ([`crate::work::run::run_agent_and_finish`]) treats
    /// `None` here as suspicious — `RunStatus::Interrupted`, not `Done` —
    /// while `Some(true)`/`Some(false)` still drive `Failed`/`Done` exactly
    /// as before.
    pub is_error: Option<bool>,
    /// The free-text summary/response, absent when missing, distinct from an
    /// explicit empty string.
    pub result: Option<String>,
    /// Per-model token/cost usage, present only when the adapter's result
    /// carried a non-empty map — an empty map normalizes to `None`. Feeds
    /// `tm runs finish --model-usage` (only passed by the caller when this
    /// is `Some`).
    pub model_usage: Option<ModelUsageMap>,
}

/// Backend-agnostic AI coding agent operations. See the module doc comment
/// for how this relates to [`crate::agent::claude::ClaudeRunner`] and
/// `main.rs`'s `agent_runner_for`.
///
/// This phase's trait carries the invocation-building methods plus outcome
/// parsing and the resume hint. Later phases (interactive shell-string
/// rendering, telemetry deployment, session identity, pricing) add more
/// methods here — see `docs/plans/agent-runner.md`'s phase list.
pub trait AgentRunner {
    /// Short name, e.g. `"claude"` — branch-owner fallback and display.
    fn name(&self) -> &'static str;

    /// Build one run's argv/env (headless or interactive).
    fn build_invocation(&self, inputs: InvocationInputs) -> AgentInvocation;

    /// Parse a finished headless run's raw result text into a
    /// [`RunOutcome`].
    ///
    /// Hard errors on unparseable input ([`OutcomeParseError::Malformed`])
    /// or a missing/empty `session_id`
    /// ([`OutcomeParseError::MissingSessionId`]): a run with no session id
    /// to resume isn't a usable outcome, only an unparseable one. Every
    /// other field is tolerant of absence: an empty `model_usage` map
    /// normalizes to `None`, and `is_error`'s absence is preserved as `None`
    /// rather than defaulted to `false` — see [`RunOutcome::is_error`]'s doc
    /// comment for why that distinction matters to the caller.
    fn parse_outcome(&self, raw: &str) -> Result<RunOutcome, OutcomeParseError>;

    /// User-facing resume hint for a finished run's session id, e.g. `claude
    /// --resume <id>`.
    fn resume_command(&self, session_id: &str) -> String;

    /// Shell command string for a tmux-hosted audit/bugbot session: the
    /// adapter's CLI with an optional `--model` and `prompt` as its
    /// positional argument. Replaces `work::audit::claude_command`.
    fn interactive_shell_command(&self, model: Option<&str>, prompt: &str) -> String;

    /// Render `invocation` into the shell command line a tmux window runs,
    /// reading the prompt back from `prompt_file` rather than embedding it
    /// directly (a fix prompt has no length bound, and `ARG_MAX` is a real
    /// ceiling).
    ///
    /// Two things this must not lose:
    ///
    /// - **The `env -u` prefix.** [`AgentInvocation::env_remove`] is
    ///   billing-safety critical and there is no `tmux` flag that *unsets*
    ///   an environment variable (`-e` only sets), so it has to be
    ///   re-expressed as `env -u` inside the command string. See that
    ///   field's doc comment.
    /// - **The double quotes around `$(cat ...)`.** Unquoted, the shell
    ///   would word-split the prompt into hundreds of arguments.
    ///
    /// `invocation.args[0]` is the prompt under [`RunMode::Interactive`]
    /// (the prompt is positional there), and it is what gets replaced by
    /// the `cat`; every later argument is passed through [`shell_quote`]d.
    /// This "prompt sits at `args[0]`" convention is part of
    /// [`AgentInvocation`]'s interactive contract — an adapter whose
    /// interactive invocation doesn't shape its argv this way must override
    /// this default rather than rely on it.
    ///
    /// Provided default, ported from `work::interactive::tmux_command_line`
    /// (deleted); `ClaudeRunner` uses it unmodified.
    fn tmux_command_line(&self, invocation: &AgentInvocation, prompt_file: &Path) -> String {
        let mut parts = vec!["env".to_string()];
        for var in &invocation.env_remove {
            parts.push("-u".to_string());
            parts.push(var.clone());
        }
        parts.push(invocation.program.clone());
        parts.push(format!(
            "\"$(cat {})\"",
            shell_quote(&prompt_file.to_string_lossy())
        ));
        for arg in invocation.args.iter().skip(1) {
            parts.push(shell_quote(arg));
        }
        parts.join(" ")
    }

    /// Default prompt template used when `[work.audit].prompt` is unset,
    /// e.g. `"/ticket-audit {key}"`.
    fn default_audit_prompt_template(&self) -> &'static str;

    /// Default prompt template used when `[work.review_watch].prompt` is
    /// unset, e.g. `"/bugbot-triage {key} {findings_file}"`.
    fn default_cleanup_prompt_template(&self) -> &'static str;

    /// Default lane-prompt file path when neither `--prompt` nor the lane's
    /// `prompt_file` is given, e.g. `~/.claude/prompts/<lane>.md`.
    fn default_lane_prompt_path(&self, home: &Path, lane: &str) -> PathBuf;
}
