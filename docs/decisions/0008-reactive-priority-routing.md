# ADR-0008: Reactive Priority Routing Between Agent Runners

**Status:** Accepted
**Date:** 2026-09-22

## Problem

ADR-0004 gave `tm` a seam for more than one `AgentRunner` (`claude`,
`opencode`), but `[agent].runner` still picks exactly one, for the whole
process, for good. In practice the subscription-billed runner (`claude`)
is the preferred choice while its rolling usage window has capacity, but
hitting that window mid-lane-run today just fails the run — there's no way
to say "try the pay-per-token runner (`opencode`/Venice) instead, and flip
back once the window resets" without a human noticing and editing config.
GitHub issue #54 tracks routing lane runs between runners automatically by
usage availability, building on ADR-0004's seam and the opencode adapter
(#41).

## Decision

1. **Reactive, not proactive — there is no stable quota API to poll.**
   Neither `claude` nor `opencode` expose a "how much usage do I have
   left" endpoint `tm` can check before spawning. So `tm` learns "this
   runner is exhausted" the only way available: by hitting the limit once,
   classifying the response, and remembering it until the window should
   have reset. A proactive design (poll first, spawn second) isn't on the
   table until such an API exists; this ADR explicitly chooses the reactive
   shape over waiting for one.

2. **Rate-limit classification is adapter-owned.** `RunOutcome` gains
   `rate_limit: Option<RateLimitInfo>` (`RateLimitInfo { reset_at:
   Option<i64> }`), populated inside each `AgentRunner::parse_outcome`
   implementation, not by a shared parser — the shapes are adapter-specific
   text/payload formats with nothing in common except the concept. `claude`
   scans the `result` text (case-insensitive) for usage-limit phrasings
   verified against the CLI bundle it ships (`usage limit reached`,
   `credit balance ... too low`, `out of extra usage`, `rate limit`);
   `opencode` matches its `error` event's `name`/`data.message` against a
   similar set (`rate limit`/`rate_limit`, `429`, `quota`, `credit`, for
   Venice's "insufficient credits" wording). Classification runs
   independently of `is_error` — Claude's legacy headless usage-limit shape
   (`Claude AI usage limit reached|<epoch>`) has historically arrived with
   `is_error: false`, so gating classification on `is_error: true` would
   miss it. A **terse-result length guard** matters here too: a long,
   ordinary work summary can legitimately contain the word "limit" (a rate
   limiter the agent implemented, a discussion of API limits) without being
   a rate-limit response — real usage-limit results are short, so
   classification only fires on a short result text, not a substring match
   over an arbitrarily long summary. This is the same "don't let a
   coincidental substring misfire" concern ADR-0004's telemetry parsing
   already takes seriously elsewhere.

3. **Window state lives in `runs.db`, not in `Config`.** A new v11
   migration adds `agent_windows (agent TEXT PRIMARY KEY, exhausted_until
   INTEGER NOT NULL)`, with `RunStore::set_agent_exhausted_until`/
   `agent_exhausted_until`/`clear_agent_window`. This is *process state*,
   not configuration: it changes on every run based on what actually
   happened, has a natural expiry, and needs to be shared across `tm`
   invocations (a `tm work run --fg` in one terminal and a board-launched
   detached run in another must see the same "claude is exhausted" fact).
   `runs.db` already plays this role for run state generally, so a new
   table there — not a sidecar file, not `Config` — is the natural fit.
   When a rate-limited outcome carries no `reset_at` (most of them don't:
   only Claude's legacy shape observes one), the window holds for
   `DEFAULT_EXHAUSTED_HOLD_SECS = 900` (15 minutes) rather than refusing to
   probe again indefinitely — re-probing this often is cheap, since the run
   still completes on the fallback runner; a premature probe costs one
   spawn, not the whole run. A successful (zero-exit, non-failed) outcome
   on a runner clears its window immediately, so stale state self-heals
   rather than waiting out the full hold after the real window has already
   reset.

4. **`[agent]` merge is whole-table replacement, not key-by-key.**
   `runner` (single mode) and `strategy`/`order` (priority mode) are two
   mutually exclusive declarations of the same thing — which runner(s) this
   `tm` invocation may use — so a repo `[agent]` table that sets any key is
   the *sole* source for all three keys, never merged field-by-field
   against the global table. Key-by-key merge would let a global `runner =
   "claude"` silently leak into a repo that opted into `strategy =
   "priority"` with no `runner` key of its own, tripping the
   runner-vs-strategy conflict rule inconsistently depending on layering
   order — a repo with no `[agent]` table at all still inherits the global
   one untouched, since there's nothing to conflict with.

5. **`model` is passed through only for the preferred agent's own
   attempt.** A fallback invocation always omits `--model`/its equivalent
   and gets that runner's own configured default instead. Model strings
   are runner-specific in practice — `"fable"` for `claude`, opencode's
   `"provider/model"` spelling — so blindly forwarding the preferred
   runner's model string to a fallback runner would either be nonsense or,
   worse, silently resolve to the wrong model. `order[0]` (`Config.agent`)
   is the only attempt this applies to, since fallback attempts by
   construction are never the preferred agent.

6. **Scope: lane runs only, v1.** `tm review fix`, `tm ticket
   audit`/`create`, and interactive tmux sessions all keep using the single
   preferred runner (`Config.agent`), no fallback — those surfaces don't
   go through `prepare_run_lane`/`run_agent_and_finish`'s attempt loop at
   all. Board-launched lane runs route automatically anyway, since the
   board spawns child `tm work run` processes that go through the same
   lane-run path a terminal invocation does — no separate wiring needed
   for the TUI. Widening fallback to the other surfaces is real follow-up
   territory, not a v1 requirement: those are lower-volume, more
   interactive-feeling surfaces where a silent runner switch is a bigger
   surprise than "try a different agent for one attempt".

7. **A fallback re-runs the prompt from scratch — sessions aren't
   portable.** There is no cross-runner session handoff: `claude`'s
   session id means nothing to `opencode` and vice versa. So a fallback
   attempt is a fresh invocation with the same resolved prompt text, not a
   resume — the first attempt's partial work (if any) is abandoned. This
   is a real cost (wasted tokens/turns on the exhausted runner before the
   limit hit), accepted because there is no alternative: retrying with
   context would require a shared session format across adapters that
   doesn't exist and isn't likely to.

## What this does not change

`runs` gets no new runner column — `run_events` records the rate-limit and
fallback trail for a given run (which agent hit its limit, when, and which
agent took over), which is enough to steer the *next* run via
`agent_windows` without needing per-run runner attribution as a first-class
column. Full per-run runner recording (which agent actually produced the
final result) is `#43`'s telemetry-parity territory, not this ADR's.

See `docs/plans/gh-54-priority-routing.md` for the full eight-phase plan
this ADR closes out, and GitHub issue #54 for the original request.
