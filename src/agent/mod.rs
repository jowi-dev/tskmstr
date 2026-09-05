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
//! `src/work/audit.rs`. This phase (5) moves telemetry — hook script
//! deployment and user-level hooks install — behind
//! [`AgentRunner::deploy_telemetry`], [`AgentRunner::install_user_hooks`],
//! and [`AgentRunner::user_hooks_installed`], with [`AgentError`] as the
//! backend-agnostic error every caller downstream of those methods depends
//! on: `src/agent/claude/hooks.rs` and `src/agent/claude/hooks_install.rs`
//! (moved from `src/work/`) are now adapter-private, called only from
//! [`crate::agent::claude::ClaudeRunner`]'s impls. This phase (6) moves
//! session identity and pricing: [`SessionEnvVars`] and
//! [`AgentRunner::session_env_vars`] replace the `CLAUDE_CODE_SESSION_ID`/
//! `CLAUDE_PID` literals [`crate::runs::session::SessionEnv::from_process_env`]
//! used to read directly, [`AgentRunner::price_for_model`] replaces
//! `crate::runs::pricing::price_for_model`'s claude-keyed table lookup (the
//! table itself moved into [`crate::agent::claude::ClaudeRunner`]), and
//! [`AgentRunner::display_model_name`] replaces
//! `crate::runs::format_model_usage_compact`'s `strip_prefix("claude-")`.
//! `crate::runs::pricing::ModelPrice`/`estimate_cost_usd` stay
//! runner-agnostic arithmetic, unchanged in shape.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::runs::ModelUsageMap;
use crate::runs::pricing::ModelPrice;

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
    /// Path to the generated hooks `--settings` JSON file, verbatim
    /// [`AgentRunner::deploy_telemetry`]'s return value. `None` when the
    /// configured runner deploys no telemetry at all — `ClaudeRunner`'s
    /// `deploy_telemetry` always returns `Some`, so every current caller
    /// still gets a `--settings` flag, but a future telemetry-less runner
    /// must be able to omit it. See [`AgentRunner::deploy_telemetry`]'s doc
    /// comment for the acceptance rule this shape exists to satisfy.
    pub settings_path: Option<PathBuf>,
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

/// Backend-agnostic error from [`AgentRunner::deploy_telemetry`] and
/// [`AgentRunner::install_user_hooks`], mirroring the pattern
/// [`crate::ticketing::error::ProviderError`] established for issue #3: a
/// caller downstream of [`AgentRunner`] (`src/work/run.rs`'s
/// `RunLaneError`/`ReviewFixError`, `main.rs`'s dispatch) depends on one
/// runner-agnostic error rather than naming
/// [`crate::agent::claude::hooks::HooksError`] or
/// [`crate::agent::claude::hooks_install::HooksInstallError`] directly.
/// Unlike [`crate::ticketing::error::ProviderError`]'s field-by-field
/// mirroring (multiple ticket backends, each with genuinely different
/// failure shapes), `claude` is still the only adapter and its telemetry
/// error types already carry high-quality messages, so this wraps them
/// (plus the settings-JSON serialize/write step [`AgentRunner::deploy_telemetry`]
/// folds in) transparently rather than re-deriving each variant.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Hook script deployment failed. See
    /// [`crate::agent::claude::hooks::HooksError`].
    #[error(transparent)]
    Hooks(#[from] crate::agent::claude::hooks::HooksError),

    /// User-level hooks install failed. See
    /// [`crate::agent::claude::hooks_install::HooksInstallError`].
    #[error(transparent)]
    HooksInstall(#[from] crate::agent::claude::hooks_install::HooksInstallError),

    /// Serializing the generated hooks `--settings` JSON to disk failed.
    #[error("failed to serialize settings JSON: {0}")]
    SettingsJson(#[from] serde_json::Error),

    /// A filesystem operation failed (e.g. writing the settings file).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The outcome of an [`AgentRunner::install_user_hooks`] call — everything
/// `tm work hooks install --user` needs to print its summary. Runner-agnostic:
/// moved here from `src/work/hooks_install.rs` (now
/// `src/agent/claude/hooks_install.rs`) in phase 5 of GitHub issue #17
/// (`docs/plans/agent-runner.md`) since [`AgentRunner::install_user_hooks`]
/// returns it directly and a future adapter with its own user-level
/// telemetry would build the same shape rather than a claude-specific one.
#[derive(Debug, Clone, Default)]
pub struct InstallReport {
    /// Hook script filenames written (missing, or present but stale).
    pub scripts_copied: Vec<String>,
    /// Hook script filenames already byte-identical on disk.
    pub scripts_already_present: Vec<String>,
    /// `"<event> -> <script>"` labels newly appended to `settings.json`.
    pub hooks_added: Vec<String>,
    /// `"<event> -> <script>"` labels already present in `settings.json`.
    pub hooks_already_present: Vec<String>,
    /// Where the pre-modification backup was written, if this wasn't a
    /// dry run.
    pub backup_path: Option<PathBuf>,
    /// Whether this call was a dry run (nothing was written).
    pub dry_run: bool,
}

impl InstallReport {
    /// Render a plain-text (no emoji) summary of what changed/would
    /// change, matching the rest of the CLI's output style.
    pub fn write_summary(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.dry_run {
            writeln!(out, "Dry run: no changes written.")?;
        }

        writeln!(out, "Hook scripts:")?;
        if self.scripts_copied.is_empty() {
            writeln!(out, "  copied: (none)")?;
        } else {
            writeln!(out, "  copied: {}", self.scripts_copied.join(", "))?;
        }
        writeln!(
            out,
            "  already up to date: {}",
            if self.scripts_already_present.is_empty() {
                "(none)".to_string()
            } else {
                self.scripts_already_present.join(", ")
            }
        )?;

        writeln!(out, "settings.json hook wiring:")?;
        writeln!(
            out,
            "  added: {}",
            if self.hooks_added.is_empty() {
                "(none)".to_string()
            } else {
                self.hooks_added.join(", ")
            }
        )?;
        writeln!(
            out,
            "  already present: {}",
            if self.hooks_already_present.is_empty() {
                "(none)".to_string()
            } else {
                self.hooks_already_present.join(", ")
            }
        )?;

        match &self.backup_path {
            Some(path) => writeln!(out, "Backup written to: {}", path.display())?,
            None if !self.dry_run => writeln!(out, "Backup written to: (none)")?,
            None => {}
        }

        Ok(())
    }
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

/// The env var names a live agent session exposes its identity through,
/// read by [`crate::runs::session::SessionEnv::from_process_env`] to build
/// an interactive session's [`crate::runs::session::SessionEnv`]. See
/// [`crate::agent::claude::ClaudeRunner`]'s [`AgentRunner::session_env_vars`]
/// impl for the specific vars `claude` sets and the "observed but
/// undocumented" caveat that comes with them.
#[derive(Debug, Clone, Copy)]
pub struct SessionEnvVars {
    /// Env var carrying the live session's id, matching the `session_id`
    /// value the adapter's telemetry hooks receive on stdin.
    pub session_id: &'static str,
    /// Env var carrying the live session's process id.
    pub pid: &'static str,
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

    /// Human-readable product name for user-facing prose, e.g. `"Claude
    /// Code"` — distinct from [`AgentRunner::name`], which is the CLI/program
    /// identifier used for branch-owner fallback and internal display, not
    /// prose a user reads. Used by `tm init`'s hook-install prompt.
    fn display_name(&self) -> &'static str;

    /// Where this runner discovers slash-command skills under a repo
    /// checkout or a home directory, e.g. `<base>/.claude/skills`. Used by
    /// `tm init`'s "does the skill this lane's prompt invokes exist"
    /// probes, once against the repo dir and once against `home`.
    fn skills_dir(&self, base: &Path) -> PathBuf;

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

    /// Default prompt used when `[work.create].prompt` is unset, e.g.
    /// `"/ticket-create"`. Unlike the audit and cleanup templates, no
    /// placeholder substitution applies — a create session starts before any
    /// ticket key exists.
    fn default_create_prompt(&self) -> &'static str;

    /// Default lane-prompt file path when neither `--prompt` nor the lane's
    /// `prompt_file` is given, e.g. `~/.claude/prompts/<lane>.md`.
    fn default_lane_prompt_path(&self, home: &Path, lane: &str) -> PathBuf;

    /// Deploy this runner's telemetry artifacts (hook scripts + a settings
    /// file, for `claude`) into `deploy_dir`, returning the settings path
    /// [`InvocationInputs::settings_path`] should carry, or `Ok(None)` for a
    /// runner with no telemetry.
    ///
    /// **Acceptance rule: run start/finish recording must never depend on
    /// this returning `Some`.** [`crate::runs::RunStore::start_run`]/
    /// `finish_run` are already opaque to runner shape (see
    /// `docs/plans/agent-runner.md`'s "Hook-scripts decision"), so a future
    /// adapter that returns `None` here must still get its runs tracked —
    /// only the telemetry-driven extras (session-usage cost, checklist/task
    /// events, the `SessionEnd`-triggered interactive finish) are lost, not
    /// run tracking itself.
    fn deploy_telemetry(&self, deploy_dir: &Path) -> Result<Option<PathBuf>, AgentError>;

    /// Merge this runner's user-level telemetry hooks into the user's own
    /// agent settings (`tm work hooks install --user`), copying any hook
    /// scripts into a runner-owned directory first. `Ok(None)` is the
    /// no-user-telemetry shape for a runner that has nothing to install
    /// (`claude` never returns it — see [`ClaudeRunner`](crate::agent::claude::ClaudeRunner)).
    ///
    /// `home`/`xdg_data_home` are the already-resolved environment reads a
    /// caller has on hand; any adapter-specific env var (`claude`'s
    /// `CLAUDE_CONFIG_DIR`) is resolved inside the adapter, not by the
    /// caller. `backup_suffix`/`dry_run` mirror
    /// [`crate::agent::claude::hooks_install::install_user_hooks`]'s
    /// same-named parameters.
    fn install_user_hooks(
        &self,
        home: &Path,
        xdg_data_home: Option<&Path>,
        backup_suffix: &str,
        dry_run: bool,
    ) -> Result<Option<InstallReport>, AgentError>;

    /// Whether this runner's user-level telemetry hooks are already
    /// installed (the `tm init` onboarding probe). `false` for a runner
    /// with no user-level telemetry to install.
    fn user_hooks_installed(&self, home: &Path, xdg_data_home: Option<&Path>) -> bool;

    /// Env var names carrying session identity inside a live session,
    /// consumed by [`crate::runs::session::SessionEnv::from_process_env`].
    fn session_env_vars(&self) -> SessionEnvVars;

    /// Price table lookup for estimated interactive-session costs (`tm
    /// ticket audit`/`create` have no authoritative `costUSD` the way a
    /// headless lane run's `modelUsage` does — see
    /// `crate::runs::pricing`'s module docs). `None` for any model this
    /// runner has no price entry for.
    fn price_for_model(&self, model: &str) -> Option<ModelPrice>;

    /// Short display form of a reported model name, e.g. for
    /// [`crate::runs::format_model_usage_compact`]. `claude` strips the
    /// leading `"claude-"` prefix; an adapter whose model names carry no
    /// such prefix returns `model` unchanged.
    fn display_model_name<'a>(&self, model: &'a str) -> &'a str;
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Files allowed to carry a functional (non-test) `claude`/`anthropic`
    /// literal outside `src/agent/`, per
    /// `no_agent_literals_outside_the_adapter_module`'s doc comment.
    const ALLOWLIST: &[&str] = &[
        // `AgentKind::Claude` — ADR-0003's one-enum rule (mirrored by
        // `docs/decisions/0004-agent-runners.md`) sanctions the discriminant
        // living here, the same way `BackendKind::Jira`/`Github` live in
        // this same file.
        "src/config/mod.rs",
        // `agent_runner_for`'s one `match` on `AgentKind` — the one factory
        // dispatch site `docs/plans/agent-runner.md` calls for.
        "src/main.rs",
    ];

    /// Recursively collects every `.rs` file under `dir`.
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("read_dir({}): {err}", dir.display()));
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Strips `//`-to-end-of-line comments (this naively covers `///` and
    /// `//!` doc comments too, and is fine for this purpose: URLs containing
    /// `//` inside a string literal are rare enough in this codebase not to
    /// produce a false negative, and we only care about false positives
    /// disappearing, never new ones appearing) and drops everything from the
    /// first line equal to `#[cfg(test)]` onward (test modules sit at file
    /// end in this codebase), returning the remaining lines paired with
    /// their original 1-indexed line numbers.
    fn strip_comments_and_tests(contents: &str) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for (idx, line) in contents.lines().enumerate() {
            if line.trim() == "#[cfg(test)]" {
                break;
            }
            let stripped = match line.find("//") {
                Some(pos) => &line[..pos],
                None => line,
            };
            out.push((idx + 1, stripped.to_string()));
        }
        out
    }

    /// Grep-guard for `docs/plans/agent-runner.md`'s phase 7 acceptance
    /// criterion: "no `claude`/`anthropic` literals outside the adapter
    /// module, verifiable by grep". Walks every `.rs` file under `src/`
    /// except `src/agent/**`, strips comments and trailing test modules (see
    /// [`strip_comments_and_tests`]), and asserts no case-insensitive
    /// `claude` or `anthropic` substring remains in the functional code —
    /// test fixtures asserting claude-specific behavior stay legal (they
    /// live in `#[cfg(test)]` blocks or inside `src/agent/`), but a
    /// functional literal (a hardcoded `"claude"` program name, a
    /// user-facing string naming Claude Code, an `ANTHROPIC_*` env var) does
    /// not. See [`ALLOWLIST`] for the two sanctioned exceptions.
    #[test]
    fn no_agent_literals_outside_the_adapter_module() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let src_dir = manifest_dir.join("src");
        let agent_dir = src_dir.join("agent");

        let mut files = Vec::new();
        collect_rs_files(&src_dir, &mut files);

        let mut violations = Vec::new();
        for path in &files {
            if path.starts_with(&agent_dir) {
                continue;
            }
            let rel = path
                .strip_prefix(manifest_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if ALLOWLIST.contains(&rel.as_str()) {
                continue;
            }

            let contents = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            for (line_no, line) in strip_comments_and_tests(&contents) {
                let lower = line.to_lowercase();
                if lower.contains("claude") || lower.contains("anthropic") {
                    violations.push(format!("{rel}:{line_no}: {}", line.trim()));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "found claude/anthropic literals outside src/agent/ (and outside the allowlist):\n{}",
            violations.join("\n")
        );
    }
}
