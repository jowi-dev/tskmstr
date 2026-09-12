# GH-41: opencode AgentRunner adapter

The second `AgentRunner` implementation (see `docs/decisions/0004-agent-runners.md`
and `docs/plans/agent-runner.md`), proving the seam issue #17 built. Wiring
follows the recipe on `AgentKind`'s doc comment: an `AgentKind::Opencode`
variant, one factory arm in `main.rs`'s `agent_runner_for`, a new
`src/agent/opencode/` module, and the literal grep guard extended to
`opencode`.

## The opencode CLI contract this adapter binds to

Everything below was verified against opencode **v1.18.11** — the installed
binary's embedded bundle and the `v1.18.11` tag of github.com/sst/opencode
(`packages/opencode/src/cli/cmd/run.ts` is the whole JSON formatter; the
docs site documents neither the event stream nor exit codes). Treat the
event set as unstable across opencode versions.

### Headless (`RunMode::Headless`)

```text
opencode run --format json [--model <provider/model>] [--auto] -- <prompt>
```

- The prompt is positional; `--` guards against prompts starting with `-`
  (run.ts folds `j["--"]` into the message).
- stdout is pure NDJSON, one event per line, each shaped
  `{"type", "timestamp", "sessionID", ...}`. All human-readable output
  (including the permission auto-reject warning) goes to stderr.
- Event types (complete set): `tool_use`, `step_start`, `step_finish`,
  `text`, `reasoning` (only with `--thinking`), `error`. There is **no**
  terminal summary event — the stream just ends when the session goes idle.
- `step_finish` carries the telemetry: `part.cost` (USD) and `part.tokens`
  (`{input, output, reasoning, cache: {read, write}}`), **per step**; run
  totals are sums. No event carries the model name in JSON mode (the
  assistant message object would, but messages are never emitted), which is
  why `RunOutcome::model_usage` stays `None` — see Telemetry below.
- Exit code 1 when any `session.error` was seen or the initial prompt
  request failed; 0 otherwise.

### Interactive (`RunMode::Interactive`)

```text
opencode [--model <provider/model>] [--auto] --prompt <prompt>
```

The default `opencode` command *is* the TUI; `--prompt` is placed in the
editor and auto-submitted through the normal submit path, so a prompt like
`/ticket-audit GH-9` **is** interpreted as a custom command when one by that
name exists (unknown names fall through to literal text). That keeps the
claude-shaped `/ticket-audit {key}` / `/bugbot-triage {key} {findings_file}` /
`/ticket-create` prompt-template defaults meaningful — the user supplies
same-named opencode commands (`.opencode/command/*.md` or
`~/.config/opencode/command/*.md`), or relies on skills: opencode
auto-registers every discovered skill as a slash command, **and it
discovers Claude-format skills** (`.claude/skills/**/SKILL.md`, project and
home) natively, so an existing tm skill set works unchanged.

Because `--prompt` is a flag (not positional like claude's interactive
prompt), the adapter overrides `AgentRunner::tmux_command_line` rather than
relying on the default impl's prompt-at-`args[0]` convention.

Resume: `opencode --session <id>` (`resume_command`).

### Runner-neutral `[work]` key mapping (ADR-0004 point 4)

| key | opencode mapping |
|---|---|
| `default_model` | `--model`, passed through verbatim in opencode's `provider/model` spelling (e.g. `anthropic/claude-sonnet-4-5`). **Absent means the flag is omitted** and opencode's own default-model resolution applies (config `model` key, then most-recently-used) — unlike claude's always-pass-`fable` convention, because a multi-provider CLI's default belongs to its own config. |
| `permission_mode` | `bypassPermissions` (and absent, matching the issue #29 lane default) map to `--auto`, the only CLI-level bypass opencode has. **Any other value is documented-ignored**: no flag is passed, opencode's own `permission` config (or `OPENCODE_PERMISSION` env JSON) governs, and a headless run auto-REJECTS every permission ask it isn't configured to allow — it never hangs. There is no plan/acceptEdits analog. |
| `max_turns` | **Documented-ignored.** opencode has no CLI turn budget; the only equivalent is per-agent `steps` in the user's own opencode config (`agent.<name>.steps` + `--agent`), which tm doesn't own. |

### Telemetry (deferred parts)

`deploy_telemetry` and `install_user_hooks` return `None` per ADR-0004
point 3 — run start/finish recording works regardless. What still lands in
`runs.db` for cross-runner comparison, straight from `parse_outcome`:

- `cost_usd`: sum of `step_finish` costs — authoritative (opencode computes
  cost natively per step), feeding `runs.cost_usd` and every cost rollup.
- `num_turns`: count of `step_finish` events (one per agentic step).
- `is_error`: `Some(true)` on any `error` event; `Some(false)` on a clean
  stream with at least one completed step; `None` (suspicious →
  `Interrupted`) when the stream ended without either.
- `result`: completed `text` parts joined with blank lines (feeds the
  PR-URL scrape).

Deliberately absent, tracked by the follow-up telemetry issue:
- `model_usage` (`None`): the JSON stream never names the model, so
  per-model token attribution needs an opencode **plugin** (opencode's
  analog of claude's hook scripts — its `shell.env` hook could also export
  a session id to subprocesses, which opencode itself doesn't do).
- The SessionEnd-triggered interactive finish and checklist/task events:
  same plugin. Until then interactive opencode runs are finished by the
  issue #26 liveness reap.
- `price_for_model` returns `None`: opencode is multi-provider, a static
  price table is a poor fit, and headless cost is already authoritative.

### Session identity

`session_env_vars` names `OPENCODE_SESSION_ID` / `OPENCODE_PID`. opencode
1.18.11 exports `OPENCODE=1` and `OPENCODE_PID` into its own process env at
startup (inherited by tool subprocesses) but **no session-id variable** —
verified against the full source tree. `SessionEnv::from_process_env`
degrades absent vars to `None` by design, so the session-id name is a
forward-compatible placeholder that starts working if opencode ever sets
it. `env_remove` is empty: claude's billing-safety strip exists because
stray `ANTHROPIC_*` vars silently flip billing off the subscription;
opencode's provider credentials are explicit (its auth store, or env keys
the user *intends* as credentials), so stripping would break legitimate
setups.

### Paths

- `skills_dir`: `<base>/.opencode/skills` (opencode's native dir; it also
  reads `skill/` singular and the Claude-compat dirs, so `tm init`'s
  skill probe may under-detect — acceptable for an advisory probe).
- `default_lane_prompt_path`: `~/.config/opencode/prompts/<lane>.md`,
  mirroring claude's `~/.claude/prompts/<lane>.md` convention inside
  opencode's XDG config dir.
