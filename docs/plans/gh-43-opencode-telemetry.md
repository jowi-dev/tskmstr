# GH-43: opencode telemetry plugin

Follow-up to #41 (`docs/plans/gh-41-opencode-runner.md`). The #41 adapter
returns `None` from `deploy_telemetry`/`install_user_hooks`, leaving three
telemetry surfaces claude's hook scripts provide empty for opencode runs:
per-model token attribution (`runs.model_usage`), interactive auto-finish,
and session-id export to tool subprocesses. This plan pins the opencode
**plugin** contract those surfaces need, verified against the installed CLI,
and records the design the implementation binds to.

## The opencode plugin contract this feature binds to

Everything below was verified against opencode **v1.15.13** — the version
currently installed from nixpkgs, the same one #41's adapter targets. The
ticket was written against v1.18.11's docs; the plugin surface is materially
the same, but the specifics below are what the installed binary actually
does. Treat the event set and hook shapes as unstable across opencode
versions, exactly as #41 treats the CLI contract.

Verification method: the CLI is a Bun-compiled single binary
(`.opencode-wrapped`, ~150 MB). The plugin API, its embedded doc block, and
the bus-event schemas were read out of the binary's embedded JS/strings;
plugin auto-discovery was confirmed empirically with `opencode debug info`.

### Plugin module shape

A plugin is a `.ts`/`.js` module exporting `default` (or any named export)
of type `Plugin = (input: PluginInput, options?) => Promise<Hooks>` — a
**function** that returns a hooks object (`{}` if it registers nothing), not
a plain object literal. `PluginInput` destructures to `{ client, project,
directory, $ }` (`$` is a shell helper).

```ts
import type { Plugin } from "@opencode-ai/plugin"
export default (async ({ client, directory, $ }) => {
  return { /* hooks */ }
}) satisfies Plugin
```

The full hook surface (each mutates its `output` arg in place, returns
`void`): `event`, `config`, `chat.message`, `chat.params`, `chat.headers`,
`tool.execute.before`, `tool.execute.after`, `tool.definition`,
`command.execute.before`, `shell.env`, `permission.ask`, and several
`experimental.*` hooks. Object-shaped (not callbacks): `tool`, `auth`,
`provider`. This feature uses exactly two: `shell.env` and `event`.

### Auto-discovery (verified empirically)

Plugins are auto-discovered (no config entry needed) from any `*.ts`/`*.js`
file in a `plugin/` **or** `plugins/` directory under:

- the project (`<cwd>/.opencode/plugin/`), and
- the **global config dir** (`~/.config/opencode/plugin/`, or
  `$OPENCODE_CONFIG_DIR/plugin/`).

Confirmed: dropping `tm-probe.js` into `$OPENCODE_CONFIG_DIR/plugin/` made it
appear in `opencode debug info`'s plugin origins as
`file:///.../plugin/tm-probe.js`. `opencode run --pure` (env `OPENCODE_PURE`)
disables all external plugins. There is no per-run `--config`/`--settings`
flag that adds a single plugin file; `OPENCODE_CONFIG`/`OPENCODE_CONFIG_DIR`
replace rather than augment, so they are not usable without disturbing the
user's own config.

### `shell.env` hook — session-id export

For the **shell/bash tool** subprocess (`ShellTool.shellEnv` /
`ShellTool.run`), opencode calls the hook as:

```js
trigger("shell.env", { cwd, sessionID, callID }, { env: {} })
```

and merges the returned `env` into the spawned tool process:
`{ ...process.env, ...hook.env }`. So a plugin sets
`output.env[<var>] = input.sessionID` to export the live session id to every
tool subprocess. (The terminal/Pty path triggers `shell.env` with only
`{ cwd }` — no `sessionID` — but the tool path, which is what
`tm runs register` runs inside, carries it.)

opencode itself exports `OPENCODE=1` and `OPENCODE_PID` into tool env, but
**no** session-id variable, which is why the plugin must supply one. The var
name is whatever `OpencodeRunner::session_env_vars().session_id` returns
(`OPENCODE_SESSION_ID`), closing the loop with
`SessionEnv::from_process_env`.

### `event` hook — per-model usage + session end

The `event` hook fires for every bus event as `{ type, properties }`.

Per-model usage comes from `message.updated` where
`properties.info.role === "assistant"`. The assistant message
(`Session.Message.Assistant`) carries:

- `model: { id, providerID, variant }` — usage key is `providerID + "/" + id`
  (matches opencode's `provider/model` spelling and
  `display_model_name`'s split-on-first-`/`).
- `cost: number` (optional) — USD, cumulative per message.
- `tokens: { input, output, reasoning, cache: { read, write } }` (optional).

Mapping to tm's `ModelUsage` shape (`{ inputTokens, outputTokens,
cacheReadInputTokens, cacheCreationInputTokens, costUSD }`): `input`→
`inputTokens`, `output`→`outputTokens`, `cache.read`→`cacheReadInputTokens`,
`cache.write`→`cacheCreationInputTokens`, `cost`→`costUSD`. Each assistant
message's fields are cumulative for that message; the plugin keeps a
per-message-id latest-wins map and sums across message ids per model.

Session end: there is **no** clean session-end / TUI-exit bus event exposed
to plugins on 1.15.13. `session.idle` / `session.status{status:idle}` fire
after *every* turn, not once at exit, so they cannot stand in for claude's
`SessionEnd`. Since a plugin runs in-process in the opencode server, the
plugin installs a `process` exit handler (`SIGINT`/`SIGTERM`/`beforeExit`)
and finishes the run there — this is the SessionEnd analog.

## Design

Mirror claude's telemetry ownership exactly (`docs/plans/agent-runner.md`
phase 5), adapting only the delivery mechanism.

- **`deploy_telemetry` stays `None`.** claude needs a per-run
  `--settings` flag; opencode discovers the globally-installed plugin with no
  flag, so there is nothing to deploy per run. The acceptance rule
  (`AgentRunner::deploy_telemetry` doc, ADR-0004 pt 3) explicitly permits
  `None` — run tracking never depends on it.
- **`install_user_hooks` installs the plugin** into
  `~/.config/opencode/plugin/tm-telemetry.js` (or
  `$OPENCODE_CONFIG_DIR/plugin/`), the exact parallel to claude's
  `tm work hooks install --user`. Idempotent copy-if-missing-or-stale,
  returning an `InstallReport`; `user_hooks_installed` checks the same file.
  This is the settled answer to the ticket's open "plugin distribution"
  question: global-config-dir install is non-invasive (never touches the
  user's repo or `opencode.json`), auto-discovered by both lane and
  interactive sessions, and follows the existing user-hooks pattern.
- **The plugin self-gates on tm's run-id env vars**, so it is a no-op in any
  opencode session tm didn't launch:
  - `shell.env`: always export `OPENCODE_SESSION_ID = input.sessionID` (cheap,
    harmless, and the only thing AC "session identity" needs).
  - `event`: accumulate per-model usage regardless; on exit, look at
    `TSKMSTR_SESSION_RUN_ID` (interactive) — if set, `tm runs finish <id>
    --status done [--model-usage <map>]`, mirroring `tm-session-end.sh`. When
    `TSKMSTR_RUN_ID` is set instead (a headless lane run), emit
    `tm runs event <id> --kind usage --detail {"models":...}` as usage
    accrues, mirroring `tm-usage.sh` — the lane supervisor's finish folds the
    latest usage snapshot into `runs.model_usage`.

### Distribution / versioning

The plugin is embedded in the `tm` binary via `include_str!` (same as claude's
hook scripts) and rewritten on `install`, so it upgrades in lockstep with
`tm`. No npm publish, no separate version. `tm check`/`tm update` can later
gain a staleness probe against the installed copy if drift becomes a concern.

## Acceptance criteria mapping

- interactive auto-finish → plugin exit handler + `TSKMSTR_SESSION_RUN_ID`.
- per-model usage recorded → `event` accumulation → `--model-usage` (interactive)
  / `--kind usage` event folded by the supervisor (lane).
- session identity → `shell.env` exports `OPENCODE_SESSION_ID`.
- claude runs unaffected → claude's `deploy_telemetry`/`install_user_hooks`
  are untouched; opencode's live behind its own adapter.
</content>
</invoke>
