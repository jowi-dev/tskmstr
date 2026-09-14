# GH-51: declare the lane model when scaffolding a lane

`tm init`'s lane scaffold never wrote the lane's `model`, so a fresh lane's
runs resolved through `prepare_run_lane`'s chain (`--model` > lane `model` >
`[work] default_model` > **nothing**) and landed on the agent CLI's own
default. On opencode that default is the user's configured global model (in
practice `venice/claude-sonnet-5`), so a lane meant to run a cheap model
could silently burn the most expensive one — discovered only after the fact,
by noticing the model column in the TUI.

The fix: the lane scaffold asks for and writes the lane's `model`,
runner-spelled, so the question is asked once instead of becoming a silent
fallback.

## Settled design decisions

### Which key — the lane's `model`, not `[work] default_model`

The question writes `[work.lanes.<name>] model`, the per-lane key. It runs
inside the lane add/update flow (`lane_step`), where the name, base branch,
and prompt file are already gathered, so writing the lane's own `model`
there keeps the scaffold self-contained and lets a multi-lane repo carry a
different model per lane. A `[work] default_model` would cover every lane at
once, but offering both in one question is over-engineering; a lane run
resolves `[work] default_model` as a fallback anyway, so a user who wants
the work-level key can still hand-write it.

### Runner-aware choices and `--yes` defaults

Spelling is runner-specific, so the question adapts per selected runner
through two `AgentRunner` methods:

- `lane_model_prompt()` returns the accepted-spelling help text shown in the
  prompt plus the `--yes` default.
- `validate_lane_model()` rejects a mis-spelled answer so an interactive run
  re-prompts instead of writing a value the runner can't use.

| Runner | Interactive default | `--yes` outcome | Accepted spelling |
|---|---|---|---|
| claude | `fable` | writes `model = "fable"` | bare name (`fable`, `sonnet`); a `/` is rejected as opencode spelling |
| opencode | *(blank)* | leaves `model` unset, prints the consequence | `provider/model` (e.g. `venice/z-ai-glm-5-3`) |

claude's `fable` default matches `ClaudeRunner`'s always-pass-`fable`
convention, so writing it is consistent rather than surprising. opencode has
no safe universal default across providers, so its `--yes` outcome is to
**leave `model` unset and print that the run will use opencode's own default
model** — the documented, deterministic outcome the acceptance criteria
require. An interactive opencode run offers a blank default and, on an empty
answer, leaves the key unset with the same consequence line.

Offering a live list from `opencode models` was considered and dropped: it
shells out to a CLI that may be slow or offline mid-wizard, and the
validated `provider/model` free-text answer already prevents the silent
fallback the ticket is about. The pricing/roles table in the opencode
adapter's module doc remains the reference for which model to pick.

### Interactive empty answer leaves the key unset

Both runners treat an empty interactive answer as "leave `model` unset"
(printing opencode's consequence line), so a user re-running `tm init` over
a lane that intentionally has no model isn't forced to invent one. claude's
non-blank default means accepting the default still writes `fable`; a user
must actively clear it to leave it unset.

## Acceptance criteria mapping

- `tm init` on a fresh repo asks the question with runner-appropriate
  choices and writes the key it selected; `--yes` produces a deterministic,
  documented outcome per runner (claude writes `fable`, opencode leaves
  unset and prints the consequence).
- A lane initialized for opencode with a glm model writes `model =
  "venice/z-ai-glm-5-3"`, which `prepare_run_lane` resolves so the run's TUI
  shows that model from the first turn.
- README's init section documents the question; the agent docs
  cross-reference it.
