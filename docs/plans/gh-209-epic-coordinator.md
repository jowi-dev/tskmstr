# GH-209: epic coordinator over tm

An **epic coordinator** is one interactive agent session per epic. It
drives the epic's tickets to merged PRs. **tm is its hands**: it launches,
inspects and merges lanes only through `tm` commands, so every gate tm
applies to a launch still applies. **thatch chat is its voice**: it talks to
the lanes it launched and follows thatch's coordination discipline (one
tracked entry per lane, verify before accepting "done").

This follows ADR-0012: tm keeps dispatch, admission and merge; thatch keeps
the agent-to-agent conversation and the supervision method.

## Why not thatch's own coordinator

thatch already has a coordinator: the `thatch-coordination` skill plus the
`thatch_session_tab` tool. It cannot replace tm's dispatcher:

- `thatch_session_tab` is a model tool on opencode v2 only. tm cannot call
  it, and Claude Code sessions do not have it.
- It knows nothing about tickets, readiness, the audit gate, memory or
  usage admission, lane caps, or `tm merge`'s checks gates.
- thatch 0.3.5 installs `thatch-coordination` for opencode only
  (`OPENCODE_ONLY_SKILLS` in thatch's `src/skills.ts`), so a Claude Code
  coordinator cannot load it.

What tm lacks is the supervision loop, not the dispatcher. So the
coordinator keeps tm's dispatch and borrows the loop.

## Launch and stop

```
tm work coordinator start <EPIC>
tm work coordinator stop <EPIC>
```

`start`:

1. Reads the epic from the tracker and refuses a key that is not an epic.
   Lists its children (`epic_children`) for the opening prompt.
2. Refuses a second coordinator when the epic's session already has a live
   `coordinator` window.
3. Reads `[work.coordinator] prompt_file` if set, then admits the launch as
   a human one (memory budget, usage gate warn), both before any row exists.
4. Pre-registers a run row: `kind = "coordinator"`, `lane = "coordinator"`,
   rooted at the invoking repo's checkout. `coordinator` is an agent kind,
   so the session counts against the memory budget like any agent.
5. Opens a `coordinator` window in the epic's `tm-<scope>-<EPIC>` session,
   running the runner's interactive command with the run id in
   `TSKMSTR_SESSION_RUN_ID`, so the session adopts the row.
6. Stamps the row's tmux session and records a `LaunchRecord`. Both are what
   chat wake (#224) needs to type into the coordinator's pane, and what
   liveness and hibernation already read for every interactive run.

`stop` kills the epic's `coordinator` window and finishes the epic's
running `coordinator` row as `interrupted`. Lanes the coordinator launched
keep running: they are ordinary lane runs with their own awaiters, and the
operator or a new coordinator can pick them up.

The coordinator runs in the repo checkout, not a worktree. It writes no
code. Its lanes get their own worktrees from `tm work run`.

## Dispatch: tm as the hands

The coordinator's prompt gives it these tools, and only these, for acting on
the epic:

| Purpose | Command |
|---|---|
| list the epic's tickets | `tm ticket children <EPIC>` |
| is a ticket launchable | `tm ready <KEY>` (0 ready, 3 stackable, 1 blocked, 4 needs audit) |
| launch a lane | `tm work run --unattended <LANE> <KEY>` (`--from <branch>` when stackable) |
| fleet state | `tm runs`, `tm runs show <KEY>` |
| merge a verified PR | `tm merge <KEY>` |

`--unattended` is the dispatcher's own launch: it enforces usage admission
(refuses instead of warning), routes only to runners an unattended launch
may use, adds the lane completion signal, and starts the awaiter that
finishes the lane's row. Memory admission and `max_lanes` apply to every
`tm work run`.

**Known gap: readiness is prompt-enforced in v1.** `tm work run` checks
blocker stacking and inherited epic blockers, but not the full `tm ready`
decision (the audit gate in particular). The prompt requires `tm ready` to
return 0 or 3 before each launch. Making `tm work run --unattended` refuse a
ticket `tm ready` would not launch is a follow-up, filed separately; it
would close the gap for the dispatcher's own launches too.

## Supervision: thatch as the voice

The prompt carries the discipline from `thatch-coordination`, rewritten for
tm dispatch:

- **One tracked entry per lane**, created before the launch: ticket key,
  lane run id, chat name once the lane registers, status. The task list is
  the fleet map.
- **Reconcile every turn**: read chat mail and answer it, then check
  `tm runs` for lanes that finished, failed or were reaped. There are no
  death notices: a lane tm launched was not spawned by the coordinator's
  session tool, so `tm runs` status replaces thatch's watcher notice.
- **Verify before done**: a lane's "done" is a claim. Before `tm merge`,
  read the PR diff, confirm the checks, and re-run the acceptance criteria
  where cheap.
- **Authority**: the operator outranks the coordinator. A direct operator
  instruction to a lane wins over the coordinator's.

### Waking

A Claude Code coordinator reads chat mail only at its next prompt. Chat
wake (#224) already covers it: the dispatch tick wakes any idle, running
run in the scope that has unread mail, a tmux session and a launch record,
whatever its kind. The coordinator gets both at launch. Lanes get the same
wake, which is why #224 is a hard dependency.

Wakes happen on dispatch ticks only (autopilot on, about 5 minutes apart).
A coordinator waiting on a lane that finishes without sending mail is not
woken; the operator, or the lane's own completion message, prompts it.
The default lane prompt does not yet tell lanes to message their
coordinator; the coordinator's first message to each lane asks for a reply
when it opens its PR or gets stuck.

## Cost

A coordinator is one long interactive session per epic. It spends turns on:

- **launch**: about one turn per ticket (`tm ready` + `tm work run`);
- **mail**: one turn per message round with a lane;
- **verification**: one or two turns per PR (diff read, checks, merge);
- **reconciliation**: one turn per wake.

For a two-ticket epic with one question each, that is roughly 10 to 15
turns. The coordinator's run row records its cost and turns like any
interactive run, so `tm runs --kind coordinator` reports the real numbers
after the dogfood run. Idle hibernation applies: an idle coordinator is
stopped after `[work] idle_hibernate_mins` and resumed on attach. A
hibernated coordinator is not running, so chat wake skips it until the
operator attaches.

## How the board shows it

No new board code in v1. The coordinator is a run row of kind
`coordinator` on the epic's key:

- `tm overview` and `tm runs watch` list it with the other runs, and the
  chat column (#223) shows its conversation and `chat?` badge.
- `s` on the epic's card (epics view, `E`) attaches to the epic's session,
  where the `coordinator` window lives.

A board key to start and stop a coordinator from the epics view is a
follow-up; `tm work coordinator` is the launch path for now.

## Config: `[work.coordinator]`

Same shape as `[work.walkthrough]`:

```toml
[work.coordinator]
prompt_file = ".tskmstr/prompts/coordinator.md"   # or prompt = "..."
model = "opus"
```

`prompt`/`prompt_file` support `{epic}` (the epic key) and `{children}`
(one `KEY  STATUS  SUMMARY` line per child). Unset, the runner's built-in
coordinator prompt is used. `model` joins the usual model chain.

## Skill

The built-in prompt carries the discipline itself because Claude Code has
no `thatch-coordination` skill. The better long-term home is thatch: a
dispatch-agnostic `thatch-coordination` installed for every host, whose
dispatch section names "the dispatch tool you were given". tm's prompt
would then shrink to "use thatch-coordination; your dispatch tool is
`tm work run`". That upstream change is a thatch issue, not part of this
ticket.

## Dogfood run

Acceptance needs a coordinator to drive a two-ticket epic to merged PRs
with at least one lane-coordinator chat exchange. That run needs a human
at the board and real tickets, so it happens after this change merges:

1. File a two-ticket epic of small, independent fixes and audit both.
2. `tm work coordinator start <EPIC>`, attach, and watch it launch both.
3. Confirm at least one chat exchange in the coordinator's chat panel.
4. Confirm both PRs merge through `tm merge`, then
   `tm work coordinator stop <EPIC>`.
5. Record turns and cost from `tm runs --kind coordinator` here.

## Not in this change

- the board start/stop key;
- `tm work run --unattended` enforcing the full `tm ready` decision;
- upstreaming a dispatch-agnostic `thatch-coordination` to thatch;
- the dogfood run itself.
