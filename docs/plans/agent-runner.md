# Agent-runner adapter (GitHub issue #17)

Put the AI runner (Claude Code today) behind an `AgentRunner` trait,
mirroring the ticket-backend pattern from issue #3 and ADR-0003, so an
alternative agentic coding tool (e.g. opencode) can be added without a
redesign. This phase is a **pure refactor**: `ClaudeRunner` is the only
implementation, and every argv, settings file, parse result, and printed
string is identical before and after. The existing test suite is the spec.

## Module layout

Mirrors `ticketing/provider.rs` + `jira/` + `github/`:

- `src/agent/mod.rs` — the `AgentRunner` trait and the runner-agnostic
  types every consumer sees: `RunMode`, `InvocationInputs`,
  `AgentInvocation` (renamed from `ClaudeInvocation`; same serde shape so
  in-flight supervisor state files keep deserializing), `RunOutcome`,
  `OutcomeParseError`, `SessionEnvVars`, plus the shared `shell_quote`
  helper (moved from `work/audit.rs`).
- `src/agent/claude/mod.rs` — `ClaudeRunner`, owning the invocation
  builder (moved from `src/work/claude.rs`), the result-JSON parser
  (moved from `src/work/runner.rs`), the model defaults
  (`"fable"`/`"200"`/`"acceptEdits"`), the price table, the resume-hint
  string, and the session env var names.
- `src/agent/claude/hooks.rs` — moved from `src/work/hooks.rs`
  (settings-JSON schema + the eight embedded hook scripts).
- `src/agent/claude/hooks_install.rs` — moved from
  `src/work/hooks_install.rs` (`~/.claude/settings.json` merge/install,
  `CLAUDE_CONFIG_DIR` resolution).

`src/work/runner.rs` keeps `ProcessSpawner`/`SpawnRequest` — process
spawning is already runner-agnostic and stays where it is.

## The trait

```rust
pub trait AgentRunner {
    /// Short name, e.g. "claude" — branch-owner fallback and display.
    fn name(&self) -> &'static str;
    /// Build one run's argv/env (headless or interactive).
    fn build_invocation(&self, inputs: InvocationInputs) -> AgentInvocation;
    /// Shell command string for a tmux-hosted audit/bugbot session.
    fn interactive_shell_command(&self, model: Option<&str>, prompt: &str) -> String;
    /// Render an interactive invocation into the tmux window's shell
    /// line, reading the prompt back from `prompt_file`. Owns the
    /// "prompt sits at args[0]" convention.
    fn tmux_command_line(&self, invocation: &AgentInvocation, prompt_file: &Path) -> String;
    /// Deploy telemetry artifacts (hook scripts + settings file) into
    /// `deploy_dir`; returns the settings path to hand to the invocation,
    /// or None for a runner with no telemetry. Run start/finish
    /// recording must not depend on this returning Some.
    fn deploy_telemetry(&self, deploy_dir: &Path) -> Result<Option<PathBuf>, AgentError>;
    /// Merge this runner's user-level hooks into the user's own agent
    /// settings (`tm work hooks install --user`). `Ok(None)` for a
    /// runner with no user-level telemetry.
    fn install_user_hooks(&self, req: &UserHooksRequest) -> Result<Option<InstallReport>, AgentError>;
    /// Whether user-level hooks are already installed (`tm init` probe).
    fn user_hooks_installed(&self, home: &Path) -> bool;
    /// Parse a finished headless run's stdout into a RunOutcome.
    fn parse_outcome(&self, raw: &str) -> Result<RunOutcome, OutcomeParseError>;
    /// Env var names carrying session identity inside a live session.
    fn session_env_vars(&self) -> SessionEnvVars; // { session_id, pid }
    /// Price table lookup for estimated interactive-session costs.
    fn price_for_model(&self, model: &str) -> Option<ModelPrice>;
    /// Short display form of a reported model name (claude strips the
    /// "claude-" prefix).
    fn display_model_name<'a>(&self, model: &'a str) -> &'a str;
    /// User-facing resume hint, e.g. `claude --resume <id>`.
    fn resume_command(&self, session_id: &str) -> String;
    /// Default prompt templates and paths.
    fn default_audit_prompt_template(&self) -> &'static str;   // "/ticket-audit {key}"
    fn default_cleanup_prompt_template(&self) -> &'static str; // "/bugbot-triage {key} {findings_file}"
    fn default_lane_prompt_path(&self, home: &Path, lane: &str) -> PathBuf; // ~/.claude/prompts/<lane>.md
}
```

`ModelPrice` and `estimate_cost_usd(price, usage)` stay in
`runs/pricing.rs` (they're runner-agnostic arithmetic); only the
claude-keyed `PRICE_TABLE` moves into the adapter.

## Selection

Mirrors ADR-0003's one-enum-one-dispatch rule:

- `AgentKind` (`Claude`, the only variant) in `src/config/mod.rs`,
  parsed from an optional `[agent] runner = "claude"` key, defaulting to
  `Claude` when absent so every existing config keeps working.
- An unrecognized runner name is `ConfigError::InvalidRunner`, failing
  config load with a clear message, mirroring `InvalidProvider`.
- One factory, `agent_runner_for(&Config) -> &'static dyn AgentRunner`,
  in `main.rs` next to `ticket_provider_for` — the only `match` on
  `AgentKind` outside config parsing.

## Config-key decision (issue NOTES)

`[work].default_model` / `default_max_turns` / `default_permission_mode`
stay as **generic concepts** each adapter maps onto its own flags —
model, turn budget, and permission posture are meaningful for any
agentic CLI; only the flag spellings are claude's, and those live in the
adapter. No config migration.

## Hook-scripts decision (issue NOTES)

The bash hook scripts and the settings schema become **adapter-owned
assets** (they parse Claude Code's hook stdin and transcript JSONL, so
they are meaningless to any other runner). A runner that supplies no
telemetry returns `None` from `deploy_telemetry`, and the run store
still records start/finish — the store is already opaque to runner
shape.

## Phases (one commit each, tests first where behavior is new)

1. **Config**: `AgentKind`, `[agent] runner` parsing, `InvalidRunner`.
2. **Module + invocation**: `src/agent/` skeleton, move the invocation
   builder, rename `ClaudeInvocation` → `AgentInvocation`, thread
   `runner: &dyn AgentRunner` through `RunLaneDeps` and
   `prepare_run_lane`/`prepare_review_fix`, factory in `main.rs`.
3. **Outcome + resume**: `parse_outcome` behind the trait,
   `run_claude_and_finish` → `run_agent_and_finish` taking the runner,
   `resume_command` replacing the three printed hint sites, supervisor
   path constructs its runner from config.
4. **Interactive + shell-string rendering**: `tmux_command_line`,
   `interactive_shell_command` (replacing `audit::claude_command`),
   default audit/cleanup prompt templates, `default_lane_prompt_path`,
   branch-owner fallback via `name()`.
5. **Telemetry**: move `hooks.rs`/`hooks_install.rs` into the adapter,
   `deploy_telemetry` + `install_user_hooks` + `user_hooks_installed`,
   `CLAUDE_CONFIG_DIR` read moves inside the adapter.
6. **Session identity + pricing**: `session_env_vars` feeding
   `SessionEnv`, `PRICE_TABLE` move, `display_model_name`,
   `estimate_missing_costs` takes the runner.
7. **Guard + docs**: a grep-guard test asserting no claude/anthropic
   literals in non-test code outside `src/agent/`; ADR-0004; README.

## Grep-guard semantics

The acceptance criterion is "no `claude` literals outside the adapter
module, verifiable by grep". The guard test walks `src/**/*.rs`
excluding `src/agent/`, strips `//` comments (which covers doc comments
and clap help text) and everything from the first `#[cfg(test)]` line to
end of file (test modules sit at file end in this codebase), then
asserts no case-insensitive `claude` or `anthropic` substring remains.
Test fixtures asserting claude behavior stay legal; functional literals
do not.
