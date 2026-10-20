# ADR-0010: One Model Resolution Chain for Every Agent Launch

**Status:** Accepted
**Date:** 2026-10-20
**Amends:** ADR-0008 decision 5

## Problem

tm launched models nobody configured. Each launch path picked its model in
its own way. Lane runs read `--model`, then the lane's `model`, then
`[work] default_model`. Review fix/auto, the `tm init` setup session and
usage-limit fallback attempts passed no model at all. Merge conflict
sessions read only `[work.merge] model`, and the usage probe hard-coded
`haiku`. With no model given, the claude adapter substituted `fable`, and
opencode used whatever `~/.config/opencode/opencode.json` named. On the
development machine that was `venice/claude-sonnet-5`, billed per token.
Interactive audit/create/cleanup sessions left `--model` out, so claude's
own or an enterprise-pinned default applied.

The result was real spend on models nobody chose: lanes ran Sonnet through
Venice on opencode fallbacks, and runs launched Fable after Opus was the
intended model, with `default_model = "opus"` set in both configs.
GitHub issue #143 tracks it.

ADR-0008 decision 5 contributed: a fallback attempt "always omits
`--model`" because model spellings are runner-specific. That was inaccurate
for claude, which turned the missing model into `fable`, and on opencode it
handed the choice to a config file tm does not own.

## Decision

1. **Every launch passes an explicit model.** `InvocationInputs::model` is
   a `String`, not an `Option`, and `AgentRunner::interactive_shell_command`
   and `usage_probe_invocation` take a model argument. No adapter holds a
   default model. A new launch path cannot forget to choose one.

2. **One resolution chain, in one place.** `crate::work::model::resolve_model`
   walks the candidates for a launch on runner R, most specific first, and
   keeps the first one R's `validate_lane_model` accepts:
   1. `tm work run --model` (lane runs only);
   2. the surface's own key: lane `model` (lane runs, review fix/auto),
      `[work.merge] model`, `[work.audit] model`, `[work.create] model`,
      `[work.review_watch] model`. The setup session has none;
   3. `[work] default_model`;
   4. `[agent.<R>] default_model`;
   5. R's built-in default.

3. **Built-in defaults live in one table in the config module.**
   `AgentKind::builtin_models` names claude's `opus`, opencode's
   `venice/z-ai-glm-5-3-flash`, and claude's usage probe model `haiku`.
   The new keys `[agent.<runner>] default_model` and
   `[agent.claude] usage_probe_model` live in the per-runner tables
   (`RawRunnerConfig`, renamed from `RawRunnerUsageConfig`) and merge key by
   key, repo over global, like the usage keys there.
   `usage_probe_model` on a runner with no probe is a config error.

4. **Fallback attempts use the same chain (amends ADR-0008 decision 5).**
   A fallback attempt resolves its model on the fallback runner. A lane
   model the fallback runner accepts is kept, and one it rejects (`opus`
   on opencode) is skipped down the chain, typically to
   `[agent.opencode] default_model`. Runner-specific spelling is still the
   reason not to forward a model blindly. The fix is to validate the
   candidate, not to drop it.

5. **Interactive sessions join the chain.** Audit, create and review_watch
   cleanup sessions always pass `--model`, falling back to `[work]
   default_model` and then the runner's default. This deliberately
   overrides an enterprise model pin when nothing more specific is set.

6. **`tm check` shows the result.** It lists the model each surface
   resolves to on each runner it can launch on, and flags a configured
   model the runner rejects. A rejection on the preferred runner is drift.
   A rejection on a priority-routing fallback runner is advisory, since a
   lane model spelled for the preferred runner is expected to be skipped
   there.

## Consequences

- Existing configs keep working: `opus` as a lane `model` or `[work]
  default_model` still reaches claude lanes unchanged. The new keys only
  fill gaps.
- A claude launch with nothing configured now runs `opus`, not `fable`. An
  opencode launch with nothing configured runs
  `venice/z-ai-glm-5-3-flash`, not opencode's own default.
- `PRICE_TABLE`s stay in the adapters: they describe models for cost
  estimates, they don't choose one to launch.
- Out of scope, tracked as the follow-up to #143: the guard hook's model
  text, and a test that allows model names only in the defaults table and
  the price tables.
