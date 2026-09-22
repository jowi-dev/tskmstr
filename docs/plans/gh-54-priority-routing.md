# GH-54: Priority routing between agent harnesses

Route lane runs between agent harnesses by usage availability: prefer the
subscription-billed runner (claude) while its rolling window has capacity,
fall back to the pay-per-token runner (opencode/Venice) when a usage limit
hits, and flip back after the window resets. Issue #54; builds on the
`AgentRunner` seam (issue #17, ADR-0004) and the opencode adapter (#41).

Routing is **reactive**: there is no stable API to query remaining
subscription capacity, but a usage-limit response is classifiable and
sometimes carries a reset timestamp. So tm learns "claude is exhausted"
by hitting the limit once, remembers it, and skips claude until the
window reopens.

## Config

```toml
[agent]
runner = "claude"              # single-runner mode, unchanged (default)

# or, priority mode:
[agent]
strategy = "priority"
order = ["claude", "opencode"]
```

Rules (validated in `merge_agent`, `src/config/mod.rs`):

- `strategy` absent -> single mode; `runner` behaves exactly as today.
- `strategy = "priority"` requires a non-empty `order`; every entry must
  parse via `AgentKind::parse` and appear once. Setting `runner` alongside
  `strategy`/`order` is a conflict error (the two modes are exclusive).
- `order` present without `strategy = "priority"` is an error — the key
  must not be silently inert.
- Any other `strategy` value is an error listing the accepted values.
- Merge precedence stays whole-key repo-over-global, like `runner` today.

Config surface: `Config.agent: AgentKind` stays the **preferred** agent
(`order[0]` in priority mode), so every existing call site — interactive
sessions, `tm check`, board display, prompt templates — is untouched.
New `Config.agent_fallbacks: Vec<AgentKind>` carries the rest of the
order (empty in single mode). Global-only for v1; no per-lane override.

## Rate-limit classification

`RunOutcome` (src/agent/mod.rs) gains:

```rust
pub struct RateLimitInfo { pub reset_at: Option<i64> } // unix seconds
// on RunOutcome:
pub rate_limit: Option<RateLimitInfo>,
```

Classification is adapter-owned, inside each `parse_outcome`:

- **claude**: scan the `result` text (case-insensitive) for usage-limit
  shapes verified against the claude CLI 2.1.220 bundle: `usage limit
  reached`, `credit balance ... too low`, `out of extra usage`, `rate
  limit`. The legacy headless shape `Claude AI usage limit
  reached|<epoch>` yields `reset_at: Some(epoch)` (text after the last
  `|`); otherwise `reset_at: None`. Classified independently of
  `is_error` — the legacy shape arrived with `is_error: false`. The
  `is_error`-absent mid-run model-switch case stays `Interrupted`,
  unclassified (no text to classify).
- **opencode**: `RawEvent` starts reading the `error` event payload
  (`error.name`, `error.data.message`), which is currently parsed away.
  Match name/message (case-insensitive) against: `rate limit` /
  `rate_limit`, `429`, `quota`, `credit` (Venice "insufficient credits" /
  "out of credits"). `reset_at: None` (no reset timestamp observed in the
  opencode stream).

Unmatched errors keep today's behavior exactly (`rate_limit: None`).

## Window persistence

`runs.db` migration **v11** (append to `MIGRATIONS`, src/runs/mod.rs):

```sql
CREATE TABLE agent_windows (
    agent TEXT PRIMARY KEY,          -- AgentRunner::name(), e.g. "claude"
    exhausted_until INTEGER NOT NULL -- unix seconds
);
```

`RunStore` methods: `set_agent_exhausted_until(agent, until)` (upsert),
`agent_exhausted_until(agent) -> Option<i64>`, and
`clear_agent_window(agent)`.

When a rate-limit outcome has no reset timestamp, hold for
`DEFAULT_EXHAUSTED_HOLD_SECS = 900` (15 min): re-probing is cheap because
the run still completes on the fallback agent — the probe costs one spawn,
not the run. A successful (zero-exit, non-failed) outcome on an agent
clears its window, so stale state self-heals.

## Selection and in-run fallback

Dispatch helper moves next to the adapters: `src/agent/routing.rs` gains
`pub fn runner_for(kind: AgentKind) -> &'static dyn AgentRunner` (the one
match, `Box::leak` of the ZSTs, guard-exempt under `src/agent/`);
`main.rs`'s `agent_runner_for` delegates to it so there is still exactly
one dispatch (ADR-0003/0004 note updated). Also in routing.rs:

```rust
pub fn plan_attempts(order, exhausted_until: impl Fn(&str) -> Option<i64>, now) -> Vec<AgentKind>
```

— the order filtered to agents whose window has passed; if **all** are
exhausted, the single agent with the earliest `exhausted_until` (probe the
soonest-recovering one rather than refusing to run).

**Prepare** (`prepare_run_lane`, src/work/run.rs): `RunLaneDeps` gains
`fallback_runners: Vec<&'static dyn AgentRunner>` (empty everywhere but
the lane path). With fallbacks present, prepare consults `plan_attempts`
(RunStore + clock are already in scope), builds the primary invocation as
today for `attempts[0]`, and builds one invocation per remaining attempt,
stored on `PreparedRun` as

```rust
#[serde(default)]
pub fallbacks: Vec<PlannedFallback { agent: String, invocation: AgentInvocation }>
```

(`serde(default)` keeps the frozen supervisor state-file compatible).
Per-attempt inputs: same resolved prompt text, `permission_mode` and
`max_turns` passed through (each adapter already maps or ignores them),
`settings_path` from that attempt runner's own `deploy_telemetry`, and
**`model` only for the configured preferred agent** (`order[0]`) — model
strings are runner-specific in practice ("fable" vs "provider/model"),
so other attempts omit it and get the runner's own default.

**Run** (`run_agent_and_finish`, src/work/run.rs — signature unchanged):
loop over primary + fallbacks. Per attempt: spawn, parse with that
attempt's runner (fallback runners resolved via `routing::runner_for`),
then:

- `rate_limit: Some(_)` -> persist the window (`reset_at` or now+900s),
  `add_event(run_id, "rate_limited", ...)`, print a fallback notice, and
  continue to the next attempt.
- anything else -> finish exactly as today (Failed / Done / Interrupted),
  with the **active** attempt's runner used for the summary and
  `resume_command`; clear the active agent's window on success.
- all attempts rate-limited -> `Failed`, summary says every agent hit its
  limit (the recorded windows steer the next run).

Only a rate-limit outcome triggers fallback; ordinary failures do not
retry on another agent.

## Scope notes

- Lane runs only (foreground + detached supervisor). `tm review fix`,
  audits, create, and interactive tmux sessions keep the single preferred
  runner; board-launched lane runs route automatically because they spawn
  child `tm work run` processes. Follow-up candidates, not v1.
- `runs` table gets no runner column; the `run_events` row records the
  rate-limit + fallback trail. Per-run runner recording is existing
  follow-up territory (#43 telemetry parity).
- Mid-run fallback re-runs the prompt from scratch on the next agent —
  sessions are not portable across harnesses.
- Stale `ConfigError::InvalidRunner` message (`expected "claude"`) gets
  fixed in passing since this change owns that validation surface.

## Phases

1. Plan doc (this file).
2. `RunOutcome.rate_limit` + claude classification (TDD).
3. opencode error-payload classification (TDD).
4. `[agent]` strategy/order parsing + validation (TDD).
5. runs.db v11 `agent_windows` + RunStore methods (TDD).
6. `routing::runner_for` move + `plan_attempts` (TDD).
7. Prepare/run fallback wiring (TDD).
8. README `[agent]` docs + ADR-0008 (reactive routing decision record).
