# GH-194: drive headless opencode lanes through the shared server

`[agent.opencode] mode = "server"` runs a headless opencode lane as a
session on the user's opencode 2.x background server instead of as its own
`opencode run` process. The goal is memory: each `opencode run` boots a
full opencode runtime, while a session inside the shared server costs only
the server's per-session growth.

The default stays `mode = "process"`, one `opencode run` per lane.
Interactive lanes (the TUI in the ticket's tmux window) are out of scope
and always run the TUI.

## What was verified, and against what

Verified on 2026-11-19 against the installed CLI **opencode 2.2.5**,
talking to the background service, which reports **2.3.0**
(`opencode api GET /api/info`). Spec source: `opencode api GET
/openapi.json` (OpenAPI 3.1, title "opencode HttpApi"). Each finding below
came from a live call, not from the spec alone.

- **`opencode run` already attaches in 2.x.** Without `--standalone`,
  `opencode run` is a client of the background service. Process mode is
  therefore "one client runtime per lane plus the shared server", the same
  shape #192 measured as `run --attach` on 1.18. Process mode's NDJSON
  contract (`step_start`, `tool_use`, `text`, `step_finish` with `cost`,
  `sessionID` on every event) is unchanged on 2.2.5, so the existing
  adapter keeps working.
- **Authentication.** The service writes its URL and a generated password
  to `~/.local/state/opencode/service.json`, and every `/api/*` route
  except `/api/info` answers 401 without it. tm does not read that
  password. It calls the server through `opencode api <METHOD> <path>
  [-d <json>]`, opencode's own authenticated client, which resolves the
  service the same way `opencode run` does. One `opencode api` call takes
  about 50 ms and peaks at about 82 MB RSS, then exits. A long-lived
  `opencode api GET /api/event` subscriber holds about 70 MB, which is why
  the driver polls instead of streaming (see below).
- **Query parameters** go inline in the path (`/api/session/<id>/message?order=asc`);
  `opencode api --param` did not apply them.

### The calls a lane needs

| Step | Call | Verified behavior |
|---|---|---|
| create | `POST /api/session` `{title, location: {directory}, model: {providerID, id}}` | Returns `{data: Session.Info}`; id starts with `ses`. Tools run with cwd = `location.directory`. |
| environment | `PUT /api/session/<id>/environment` `{variables: {...}}` | **Replaces** the environment of the session's shell tools: a call that set only `TSKMSTR_RUN_ID` and `PATH` left `HOME` empty. The driver sends its whole environment. |
| prompt | `POST /api/session/<id>/prompt` `{text}` | Returns at once with the queued user message (`data.time.created`); the agent loop runs in the server. |
| status | `GET /api/session/<id>` | `Session.Info` has `cost` (USD), `tokens`, `outcome` (`succeeded`/`failed`/`interrupted`) and `time.{busy, idle}`. An unknown id prints `{"_tag":"SessionNotFoundError"}` with `HTTP 404 Not Found` on stderr and exit 1. |
| messages | `GET /api/session/<id>/message?type=assistant&order=asc&limit=N`, then `?type=assistant&cursor=<cursor.next>&limit=N` | One assistant message per model step, with `cost`, `tokens`, `finish`, `error` and `content[]` (`text`, `reasoning`, `tool`). `cursor.next` is always present and means "after the last item of this page". |
| permissions | `GET /api/session/<id>/permission`, `POST /api/session/<id>/permission/<rid>/reply` `{decision}` | `decision` is `once`, `always` or `reject`. |
| interrupt | `POST /api/session/<id>/interrupt` | `{interrupted: true}`; the running shell tool is killed and `outcome` becomes `interrupted`. |

## Design: a tm driver process stands in for `opencode run`

Every part of a headless lane run assumes one process per lane: the
detached supervisor spawns the invocation, records its pid, writes its
stdout to the run's output file, and hands that file to `parse_outcome`.
Server mode keeps that shape and changes only what the process is.

In server mode the opencode adapter rewrites the headless invocation

```text
opencode run --format json --model <provider/model> [--auto] -- <prompt>
```

into

```text
<tm> work __agent-driver opencode run --format json --model <provider/model> [--auto] -- <prompt>
```

with the same environment. `tm work __agent-driver` is a hidden
subcommand. Its first argument names the runner, and the rest is that runner's own
headless argv, so the driver parses exactly what process mode would have
run. The driver:

1. creates a session at its cwd (the lane worktree) with the requested
   model,
2. sends its own environment as the session environment,
3. sends the prompt,
4. polls every 2 seconds until `Session.Info.time.idle` is at or after the
   prompt's `time.created` and `outcome` is set. Each poll it answers any
   pending permission request and writes newly completed assistant messages
   to stdout,
5. exits 0 on `succeeded` and 1 otherwise.

Its stdout is the same NDJSON `opencode run --format json` writes: per
completed assistant message, a `step_start`, one `text` event per text
part, one `tool_use` event per tool part, and a `step_finish` carrying the
message's `cost` and `tokens`, all with `sessionID`. An assistant error, or
a failed idle message, becomes an `error` event with `error.name` = the
structured error's `type` and `error.data.message` = its `message` plus
`(HTTP <status>)` when present. `parse_outcome`, the viewer's
`progress_lines`, and the GH-54 rate-limit classifier read it unchanged.

Polling, not the event stream: holding `GET /api/event` open needs a
resident `opencode api` (about 70 MB) per lane, and tm has no HTTP client
of its own that could speak to the server without reading its password.
Each poll makes two or three `opencode api` calls of about 50 ms each.

## The four gaps

### Per-lane environment

Tools execute inside the server, so the variables tm sets on the lane
process (`TSKMSTR_RUN_ID`, `TSKMSTR_SESSION_RUN_ID`, `TSKMSTR_SUBAGENT`,
the #65 build-slot `PATH` and slot variables) do not reach them on their
own. The driver inherits them as ordinary process environment and copies
its whole environment into the session with `PUT .../environment`. Because
that call replaces the session environment, sending only tm's variables
would drop `HOME` and the rest. Variables in the invocation's `env_remove`
are already absent from the driver's environment, so they stay out of the
session too.

### Memory accounting

#66 samples the run's process tree. In server mode that tree is the driver
plus its short-lived `opencode api` children, which is the lane's real
client-side cost. The session's growth inside the shared server (about
140 MB per session on 1.18) belongs to no lane's tree and is not
attributed. Memory admission therefore under-counts a server-mode lane by
that amount. It still sees the server's whole footprint through free
system memory, so it cannot over-admit without bound. Attributing server
growth per session is not possible from outside the server.

### Liveness and kill

The run's recorded pid is the supervisor's, and the driver is its child,
exactly where `opencode run` sat. Reap (#26) needs no change. The driver
turns the session's state into its own exit, so:

- **Kill.** On `SIGTERM`, `SIGINT` or `SIGHUP` the driver interrupts the
  session (`POST .../interrupt`) before exiting, so closing the lane's
  tmux window or a polite kill stops the agent too. `SIGKILL` (hibernation's
  stop signal, #64) cannot be caught; the session then runs to its own end
  inside the server with nobody reading it. That is the one leak, and
  hibernation only targets interactive sessions, which server mode does
  not cover. tm has no `tm runs kill` command; the ticket's acceptance item
  maps to these signal paths.
- **Session-level liveness.** If the session disappears (404) or the
  server stops answering for 5 consecutive polls, the driver writes an
  `error` event and exits 1, and the supervisor finishes the run as failed.

### Outcome parsing

Covered by the driver's NDJSON (see the design section): cost, turns and
session id come from the assistant messages. Rate-limit errors keep their
type and message, so `classify_rate_limit_error` still matches `429`,
`quota`, `credit` and `rate limit`.

## Permissions

`opencode run --auto` makes the client reply `once` to every permission
request, and without `--auto` it replies `reject`. Explicitly denied
actions never produce a request. The driver does the same, keyed on
whether its argv carries `--auto` (or `--dangerously-skip-permissions`,
which the adapter emits today and which 2.x treats as `--auto`). A request
nobody answers would block the session forever.

## Configuration

```toml
[agent.opencode]
mode = "server"   # default "process"
```

`mode` merges repo over global like the other `[agent.<runner>]` keys. Any
value other than `process` or `server` is a config error, and so is
`server` on a runner without a server mode (claude). It applies to
headless lane runs (`tm work run --headless`/`--fg`), including a
usage-limit fallback attempt that lands on opencode.

## Measurement

Acceptance: 5 concurrent server-mode lanes peak under half the memory of
5 process-mode lanes, measured the way #192 measured (sum of client RSS
plus server RSS, sampled during the run). Results are recorded below once
taken.
