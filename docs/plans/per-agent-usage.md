# Plan: Per-agent usage breakdown in `tm runs show`

## Problem

`tm runs show <TICKET>` breaks down usage/cost by **model** (`claude-fable-
5`, `claude-sonnet-5`, ...) but not by **agent** (orchestrator vs.
`elixir-implementer`, `Explore`, `audit-hook-logging`, ...). Jowi can't see
which step of a lane pipeline burned the tokens.

## Ground truth: what data actually exists

Inspected a real completed run's transcript
(`~/.claude/projects/-Users-jowi-Worktrees-axiom-ax-404/*.jsonl`, session id
matching `tm runs show --json AX-404`'s `session_id`) and the deployed hooks
(`~/devtools/claude-hooks/tm-usage.sh`, `tm-event.sh`).

- The orchestrator's transcript does **not** interleave subagent turns as
  `isSidechain` messages. Instead, every completed `Agent`/`Task` tool call
  produces one `user`-type transcript line whose `toolUseResult` is a rich
  summary object: `{status, agentType, resolvedModel, usage: {input_tokens,
  output_tokens, cache_read_input_tokens, cache_creation_input_tokens, ...},
  totalTokens, totalDurationMs, totalToolUseCount, toolStats: {readCount,
  bashCount, editFileCount, linesAdded, linesRemoved, ...}}`.
- **This means per-agent usage does not require parsing a separate subagent
  transcript file.** It is a summary blob, already computed by the harness,
  attached to the tool call that spawned the agent — in the *same*
  transcript the Stop hook already reads. Confirmed for `Explore`,
  `elixir-implementer` (x3), `general-purpose` (haiku), by index in the
  AX-404 transcript.
- Per Claude Code's documented hook payload shape, `PostToolUse` receives
  `tool_response` — the same object as `toolUseResult` — inline in the hook
  stdin JSON. So `tm-event.sh`'s existing `PostToolUse` hook can read
  `agentType`/`resolvedModel`/`usage`/`totalTokens`/`toolStats` **without
  touching the transcript at all**, the moment an `Agent`/`Task` call
  returns. Cheapest, most direct capture point.
- `tm-event.sh` already special-cases `tool_name == "Agent" or "Task"` for
  its `summary` field (`tool_input.description`), but discards
  `tool_input.subagent_type` and all of `tool_response`. `tool_input` also
  carries `subagent_type` (e.g. `"elixir-implementer"`) — verified in the
  same transcript.
- Separately, `tm-event.sh` already stamps an `"agent"` field on **other**
  tool events (`Bash`, `Edit`, ...) from `.agent_type` on the hook payload,
  present only when the tool call happens **inside** a subagent's own
  execution — attributing a subagent's internal tool calls. That's
  complementary, not the same signal as this plan's addition (attributing
  an agent invocation's total usage).
- `tm-usage.sh` (Stop/SubagentStop) sums `message.usage` over top-level
  `assistant` transcript lines. Cross-checked against AX-404: this sum
  captures only the **orchestrator's own** turns — it excludes the large
  `claude-fable-5` usage in the run's authoritative `model_usage` column,
  which comes from `claude -p`'s own `modelUsage` result (harness-computed
  across orchestrator + all subagents). **`tm-usage.sh`'s live snapshot
  already undercounts** relative to the final number — pre-existing, not
  introduced here, but per-agent data will make the gap visible (sum of
  per-agent tokens plus the orchestrator's own live figure will not
  necessarily reconcile to the authoritative per-model total; see "Hard
  parts").
- **Cost.** `costUSD` is never computed by tskmstr — copied verbatim, per
  model, from `claude -p`'s `modelUsage[model].costUSD` (`src/runs/mod.rs`
  `ModelUsage.cost_usd`, `parse_model_usage`). tskmstr has no local
  $/token rate table, and there is **no per-agent costUSD available
  anywhere** — the harness never attributes cost below the whole-process,
  per-model level. Per-agent breakdown must therefore be **tokens-only**;
  do not invent a per-agent cost by dividing the model total
  proportionally (misrepresents cache-heavy vs. cache-light agents on the
  same model). A real per-agent cost requires tskmstr to own a rate table
  keyed by model — a separate decision, out of scope here.

## Schema/event design

Reuse the existing "full snapshot, latest wins" convention (ADR-0001, same
as `checklist`/`usage`) rather than inventing a diff/cumulative protocol.

New event kind: **`agent_usage`**. One event per completed `Agent`/`Task`
call (not a running total) — each invocation is a discrete, finished unit
of work, so there's nothing to accumulate; repeat invocations of the same
`agent_type` each get their own event, keeping "5 distinct types across 8
calls" visible rather than collapsed.

```json
{"agentType": "elixir-implementer", "description": "Implement AX-404 UI threading",
 "model": "claude-sonnet-5", "outputTokens": 1143, "inputTokens": 2,
 "cacheReadInputTokens": 87519, "cacheCreationInputTokens": 3012,
 "totalToolUseCount": 38, "durationMs": 193659}
```

Field naming mirrors `ModelUsage`'s existing `inputTokens`/`outputTokens`/
`cacheReadInputTokens`/`cacheCreationInputTokens` so `src/runs/mod.rs` can
reuse the `ModelUsage` struct (`#[serde(flatten)]` or a wrapper) instead of
inventing a parallel token shape. No `costUSD` field — per the cost finding
above, it is never available per-agent.

Aggregation in `src/runs/mod.rs`:
- `collect_agent_usage(events) -> Vec<AgentUsageEvent>` — deliberately not
  named `latest_*`: unlike `latest_checklist`/`latest_usage` it is **not**
  latest-wins. Collect *every* `agent_usage`
  event (each is a discrete invocation, not a snapshot of a mutable
  total), oldest first, tolerant of unparseable `detail` like
  `latest_checklist`.
- `aggregate_agent_usage(events) -> BTreeMap<String, AgentUsageTotals>` —
  group by `agentType`, summing tokens/`totalToolUseCount`, counting
  invocations. This is what `tm runs show` renders; raw per-invocation
  data stays in `--json`'s `events` array (verbatim, as today) and
  optionally a dedicated `agent_usage` array for convenience.
- The orchestrating session itself is **not** an `agent_usage` event (it
  never calls itself via the Agent tool). Represent it as a synthetic
  `"orchestrator"` row: per-model authoritative total minus the sum of
  `agent_usage` tokens for that model, floored at zero. Label it clearly
  as a derived remainder, not a captured number — it inherits the
  reconciliation gap noted above.

## Hook changes

Hooks currently live in `~/devtools/claude-hooks/`; ADR-0002 is moving them
into tskmstr as part of the runner port. **Land this event-kind's producer
wherever `tm-event.sh` lives at implementation time** — ported location if
the port has landed, the bash script otherwise. Coordinate-with-runner-port:
don't block on the port finishing, but don't duplicate the hook logic in
two places either.

Change to `tm-event.sh`'s `PostToolUse` handling, gated the same as today
(`TSKMSTR_RUN_ID` set, `jq`/`tm` present):

If `tool_name` is `Agent`/`Task`: read `tool_input.subagent_type` and
`tool_response.{resolvedModel, usage.{input_tokens, output_tokens,
cache_read_input_tokens, cache_creation_input_tokens}, totalToolUseCount,
totalDurationMs}`; if `agentType`/`subagent_type` is present, additionally
emit `tm runs event $TSKMSTR_RUN_ID --kind agent_usage --detail '...'`.
Still always emit the existing `tool` event unchanged (summary line,
tool_counts, timeline) — `agent_usage` is additive, not a replacement.

Two open questions to resolve during implementation, not here:
1. Whether `tool_response` is present on **every** `PostToolUse` invocation
   for `Agent`/`Task`, or only for the synchronous (non-background) form —
   AX-404 shows both `isAsync: true` results (`agentId`, `status`,
   `outputFile`, no `usage` yet) and completed ones with `usage`. A
   background agent's completion likely arrives as a **second**
   `PostToolUse` for whatever polls/awaits it, not a second event for the
   original call. Log real payload shapes for a background-agent run
   before writing the parser; don't assume the synchronous shape covers
   both.
2. `tm-usage.sh` (Stop/SubagentStop) should NOT be touched to try to
   attribute per-agent — it structurally cannot see subagent turns (per
   the ground-truth finding above). Leave it as the orchestrator-only live
   snapshot it already is.

## CLI surface

Extend `src/runs/mod.rs` and `src/cli/runs.rs`:

1. `AgentUsageTotals` struct: agent_type, invocation count, total
   output/input/cache tokens, total tool-use count (start here; revisit a
   model breakdown once real multi-model-per-agent-type data is seen).
2. `format_agent_usage(&AgentUsageMap) -> Vec<String>` mirroring
   `format_model_usage`'s table rendering, plus an `orchestrator` row per
   the remainder calculation above.
3. `tm runs show` (human): **always-on section**, not a flag — same
   pattern as the existing "Model usage"/"Checklist" sections, and the
   point is Jowi glancing at one `show` output. Render only when at least
   one `agent_usage` event exists, titled `Agent usage` after `Model
   usage`. Do **not** add a `--by-agent` flag — this CLI's flags
   (`--model-usage`, `--json`) are input/format switches, not toggles for
   sections that should just be there when data exists.
4. `tm runs show --json`: add `"agent_usage": [...]` array of
   `{agent_type, invocations, outputTokens, inputTokens,
   cacheReadInputTokens, cacheCreationInputTokens, totalToolUseCount}` to
   `ShowJson`. Lean toward **omitting the orchestrator remainder row from
   JSON** initially (consumers can compute `model_usage total -
   sum(agent_usage)` themselves) and revisit once the reconciliation gap
   is measured on real runs.

## Hard parts (do not gloss over these during implementation)

- **Double counting.** The final authoritative `model_usage` column already
  includes subagent tokens (the harness's own end-to-end tally). A UI
  showing "Model usage: sonnet-5 $5.27" next to "Agent usage:
  elixir-implementer 91k tokens" must make clear these are two different
  slices of overlapping data, not additive line items. Don't sum agent
  rows and expect equality against the model total — AX-404's
  orchestrator-only live sum already came out *higher* than the final
  per-model figure for the same model, so "final = orchestrator + Σagents"
  doesn't hold cleanly either. Measure on 3-5 real runs before deciding
  whether to attempt the "orchestrator" remainder row, or label
  agent_usage rows purely informational/non-reconciling.
- **Repeated agent types.** 8 `Agent` calls in AX-404 were 5 distinct
  `subagent_type`s (`Explore` x1, `elixir-implementer` x3,
  `elixir-reviewer` x1, `audit-hook-logging` x1,
  `audit-elixir-scaffolding` x1, `general-purpose` x1). Aggregation must
  group by type and sum, not assume one event per type.
- **Cross-model agents.** `general-purpose` was invoked once with
  `model: "haiku"` here and could use a different model elsewhere;
  `elixir-implementer` always resolved to `claude-sonnet-5` in this run but
  a lane could change its default. If one `agentType` shows usage under
  more than one `resolvedModel`, merging tokens under one row would make a
  cost-conscious read of "which agent is expensive" wrong. Prefer a
  compound key (`agent_type` × `resolvedModel`) even if the default
  rendering collapses to one line when there's only one model.
- **Async/background agents.** See open question 1 — a background agent's
  usage may not appear on the same `PostToolUse` event as its spawn;
  verify against a real background-agent transcript before shipping.
- **Cost stays model-only.** No per-agent `costUSD` field, ever, under
  this design. Don't let a future contributor add one by dividing
  proportionally.

## Ordered, individually-committable steps

Each step is sized for one Sonnet subagent, TDD per this repo's convention
(tempdir SQLite `RunStore`, `tests` module colocated in the same file).

1. **Struct + event parser.** Add `AgentUsageEvent` (agentType,
   description, model, token fields, totalToolUseCount, durationMs) and
   `collect_agent_usage(events) -> Vec<AgentUsageEvent>` to
   `src/runs/mod.rs`, tolerant-parse like `latest_checklist`. Tests: parses
   well-formed events, skips malformed ones, returns all matches in order
   (not just newest), empty for no `agent_usage` events.
2. **Aggregation.** Add `aggregate_agent_usage(&[AgentUsageEvent]) ->
   BTreeMap<(String, String), AgentUsageTotals>` (key = agent_type +
   model) summing tokens/tool-use count/invocations. Tests: single
   invocation, repeated-agent sums, same agent different models stays
   separate, empty input.
3. **Human rendering.** `format_agent_usage` mirroring
   `format_model_usage`'s column layout. Tests: empty -> empty vec, single
   row, multiple rows aligned, agent-with-two-models rendering (decide the
   exact grouping here, test-first).
4. **Wire into `tm runs show` (human).** Print an "Agent usage" section
   after "Model usage" when non-empty. Tests: section present with
   events, absent (no empty header) without.
5. **Wire into `tm runs show --json`.** Add `agent_usage: Vec<...Json>` to
   `ShowJson`, `[]` when empty (matches `tool_counts`' empty-array
   convention). Tests: shape present, empty-array when absent, matches
   human aggregation on a repeated-agent-type fixture.
6. **`agent_usage` kind validation.** Confirm/add a test pinning that `tm
   runs event ... --kind agent_usage --detail '...'` rejects non-JSON
   detail (likely already covered by `event()`'s generic JSON check;
   pin it per-kind so a future refactor can't special-case kinds).
7. **Hook change** (wherever `tm-event.sh`/its successor lives — check
   ADR-0002 port status first). Extend `PostToolUse` for `tool_name in
   (Agent, Task)` to read `tool_input.subagent_type` and
   `tool_response.{resolvedModel,usage,totalToolUseCount,totalDurationMs}`,
   emitting `agent_usage` alongside the existing `tool` event. No
   automated harness exists for these bash hooks; test by piping a
   captured real `PostToolUse` payload through the script with
   `TSKMSTR_RUN_ID` set against a scratch DB, plus one real lane run with
   an `Agent` delegation checked via `tm runs show --json`. If the port
   has already landed in Rust, add a unit test there instead.
8. **(Stretch, separate commit) TUI parity.** `src/tui/ui.rs` /
   `src/tui/event.rs` render `model_usage` in the run detail overlay; add
   an "Agent usage" block reusing step 3's formatter. Lower priority — do
   only once 1-7 are settled and the reconciliation gap is measured.
