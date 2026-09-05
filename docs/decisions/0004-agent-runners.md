# ADR-0004: Agent Runners

**Status:** Accepted
**Date:** 2026-09-05

## Problem

Every AI-session surface in tskmstr — `tm work run`'s invocation builder,
`tm review fix`'s fix pass, `tm ticket audit`/`create`'s interactive
sessions, the outcome-JSON parser, the resume hint, the telemetry hooks,
session-identity env vars, and the price table behind cost estimates — was
hard-wired to Claude Code: a literal `"claude"` program name, `claude
--resume`/`claude --model` strings, and `.claude/`-rooted paths scattered
across `src/work/` and `src/cli/`. GitHub issue #17 tracks putting the AI
coding agent behind the same kind of seam ADR-0003 gave the ticket backend,
so an alternative agentic coding tool (e.g. opencode) can be added without
touching every call site that happens to know Claude Code's argv shape.

## Decision

1. **An `AgentRunner` trait in `src/agent/` replaces direct claude coupling
   at every call site.** `src/agent/mod.rs` defines the trait and the
   runner-agnostic types every consumer depends on (`RunMode`,
   `InvocationInputs`, `AgentInvocation`, `RunOutcome`, `OutcomeParseError`,
   `SessionEnvVars`, `AgentError`, `InstallReport`); `src/agent/claude/`
   holds `ClaudeRunner`, the sole implementation, and the hook-script/
   settings-JSON assets that used to live in `src/work/`. This is a **pure
   refactor**: every argv, settings file, parse result, and printed string
   is identical before and after, and the existing test suite is the spec —
   tests moved files and gained runner parameters along the way, but every
   behavioral assertion passes unchanged, the same discipline ADR-0003's
   phase 1 held itself to.

2. **One discriminant enum, one dispatch site.** `src/config/mod.rs`'s
   `AgentKind` (`Claude`, the only variant so far) is parsed from an
   optional `[agent] runner = "claude"` key, defaulting to `Claude` when
   `[agent]` is absent so every existing config keeps working unchanged. An
   unrecognized runner name is `ConfigError::InvalidRunner`, failing config
   load with a clear message rather than silently defaulting or panicking.
   Exactly one `match` on `AgentKind` exists in the whole codebase,
   `main.rs`'s `agent_runner_for`, mirroring `ticket_provider_for`'s
   dispatch on `BackendKind`.

3. **Telemetry is adapter-optional.** `AgentRunner::deploy_telemetry` and
   `install_user_hooks` return `Option`, not a bare value — a runner with
   no telemetry of its own returns `None` from both, and `tm`'s run
   start/finish recording (`RunStore::start_run`/`finish_run`) never
   depends on either returning `Some`: only the telemetry-driven extras
   (session-usage cost, checklist/task events, the `SessionEnd`-triggered
   interactive finish) are lost for a telemetry-less runner, not run
   tracking itself. The bash hook scripts and settings-JSON schema
   (`src/agent/claude/hooks.rs`, `hooks_install.rs`) are claude-adapter
   assets — they parse Claude Code's hook stdin and transcript JSONL, so
   they are meaningless to any other runner and stay adapter-private.

4. **Generic config concepts stay runner-neutral.** `[work].default_model`,
   `default_max_turns`, and `default_permission_mode` are meaningful for
   any agentic CLI — model choice, turn budget, and permission posture
   aren't claude-specific ideas, only the flag spellings that carry them
   are. Those three config keys stay exactly where they are; each adapter
   maps them onto its own argv (`claude`'s `--model`/`--max-turns`/
   `--permission-mode`). No config migration.

5. **The grep guard enforces the boundary.**
   `no_agent_literals_outside_the_adapter_module` (in `src/agent/mod.rs`'s
   tests) walks every `.rs` file under `src/` except `src/agent/**`,
   strips `//` comments and everything from the first `#[cfg(test)]` line
   onward, and asserts no case-insensitive `claude`/`anthropic` substring
   remains in what's left. Two files are allowlisted, each for the reason
   decisions 1 and 2 above already justify: `src/config/mod.rs` (the
   `AgentKind::Claude` discriminant — the one-enum rule sanctions the name
   living there, the same way `BackendKind::Jira`/`Github` already do) and
   `src/main.rs` (the one factory dispatch site, `agent_runner_for`'s
   `match`). Nothing else in the codebase outside `src/agent/` may name
   claude or Anthropic in functional code; a test fixture asserting
   claude-specific behavior stays legal because it lives inside a
   `#[cfg(test)]` block, which the guard already excludes.

See `docs/plans/agent-runner.md` for the full seven-phase plan this ADR
closes out, and GitHub issue #17 for the original request.

## What still stands from ADR-0002 and ADR-0003

`src/work/runner.rs`'s `ProcessSpawner`/`SpawnRequest` are already
runner-agnostic and untouched by this refactor — process spawning never
needed to know which agent it was spawning. And the shape of this decision
mirrors ADR-0003's addendum almost exactly: adding a real second adapter
later means a new `AgentKind` variant, a new `AgentRunner` implementation,
one new arm in `agent_runner_for`, and nothing else — no change to
`RunLaneDeps`, `prepare_run_lane`/`prepare_review_fix`, the TUI, or any
`tm work`/`tm review`/`tm ticket` command, the same guarantee ADR-0003 gave
a third ticket backend.
