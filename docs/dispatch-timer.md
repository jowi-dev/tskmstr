# Running autopilot from a timer (`tm dispatch`)

`tm dispatch tick` runs one pass of the autopilot dispatcher
(`src/work/dispatch.rs`, GitHub issue #115). A systemd user timer, or a
launchd agent on macOS, that runs it every few minutes keeps autopilot
working with no TUI open. This page covers the commands, the shipped unit
files, and the recommended cadence (GitHub issue #117).

## Commands

| Command | What it does |
|---|---|
| `tm dispatch on` | Turn autopilot on for the current repo, and record the repo's main checkout in runs.db so `tick --all` can find it from any directory |
| `tm dispatch off` | Turn autopilot off for the current repo. Running lanes keep running |
| `tm dispatch off --all` | The kill switch: every scope off, so the next tick launches nothing |
| `tm dispatch tick` | One pass for the current repo |
| `tm dispatch tick --all [--json]` | One pass for every scope that is on. This is what the timer runs |
| `tm dispatch status` | Per scope: on or off, running lanes, last tick, time since the last launch, and the refusal blocking it now |

Every command except `status` takes `--scope S` (a scope as `status` prints
it, such as `github:o/r`) or `--all` instead of the current repo.

`tick` exits 0 when every scope ticked, including a scope that was off,
locked by another host, or refused by a gate (lane cap, daily launch cap,
memory, usage). It exits 1 only when a scope could not tick at all, for example because its
config no longer loads. The other scopes still tick.

`--json` prints one array with one object per scope:

```json
[{"scope": "github:o/r",
  "launched": [{"ticket": "GH-7", "lane": "backend", "stacked_on": null}],
  "refused": "not launching: 1 lanes running, [dispatch] max_lanes is 1",
  "skipped": null,
  "notified": ["tm: GH-3 is awaiting your review"],
  "error": null}]
```

`notified` lists the desktop notifications the tick sent.

`skipped` is `"locked"` or `"disabled"` when the tick did nothing.

## How it shares state with the TUI

The on/off flag is the runs.db `autopilot` row that the TUI toggle writes
too. Each tick holds the scope's `flock` for its whole run. A tick that
finds the lock taken by another host (an open `tm board`, or a previous
timer tick that is still running) skips that scope. Two hosts therefore
never launch the same ticket.

`tick --all` visits only scopes that are on, and each tick reads the flag
again after it takes the lock. So `tm dispatch off --all` stops launches
from the next tick on, even for a tick that had already listed its scopes.

A scope is found through the repo that `tm dispatch on` recorded. A scope
turned on some other way (the TUI, or `on --scope`) has no recorded repo,
and `tick --all` reports an error for it until `tm dispatch on` runs inside
that repo once.

All repos must share one runs.db. Do not set a per-repo `run_db_path` for
a repo that autopilot runs in.

## systemd (Linux)

Copy the two units from [`docs/systemd/`](systemd/) and enable the timer:

```sh
mkdir -p ~/.config/systemd/user
cp docs/systemd/tm-dispatch.service docs/systemd/tm-dispatch.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tm-dispatch.timer
```

Then turn autopilot on in each repo you want it to work in:

```sh
cd ~/Projects/my-repo && tm dispatch on
```

Watch it with `tm dispatch status`, `systemctl --user list-timers
tm-dispatch.timer`, and `journalctl --user -u tm-dispatch.service`.

Three settings in `tm-dispatch.service` matter:

- **`KillMode=process`.** The lane runs a tick launches are detached
  supervisors, but `setsid` does not move them out of the service's cgroup.
  Under the default `KillMode=control-group`, systemd kills every one of
  them the moment the oneshot tick exits. This was checked with a oneshot
  unit that starts a `setsid` child: the child died under
  `control-group` and survived under `process`.
- **`PATH`.** A user service gets a minimal `PATH`. `tm`, `git`, `gh`,
  `tmux`, and your agent CLI must all be on it. The shipped value covers
  cargo, `~/.local/bin`, Nix profiles, and `/usr/bin`. Override it in
  `~/.config/tskmstr/dispatch.env` if yours differ.
- **Secrets.** Put `JIRA_API_TOKEN=...` in `~/.config/tskmstr/dispatch.env`
  for a Jira backend. `gh` reads its own stored login.

The service also runs a tick when it starts, so `systemctl --user start
tm-dispatch.service` is a manual tick under the same environment.

A tick shows desktop notifications with `notify-send`, which needs your
session bus. A user service normally inherits `DBUS_SESSION_BUS_ADDRESS`
from the systemd user manager. If no notifications appear, check
`systemctl --user show-environment`. Without `notify-send` the tick sends
no notifications and still runs.

## Cadence: every 5 minutes

The timer uses `OnUnitInactiveSec=5min`. That counts from the end of the
previous tick, so a slow tick never overlaps the next one, on top of the
lock.

Why 5 minutes:

- **Cost.** Most of a tick is `tm ready`'s classification: one tracker
  search plus `gh` calls. On this project that measured 1.7 to 1.9 seconds
  per scope. A launch adds the time `tm work run --unattended` takes to
  create a worktree and hand the agent to its supervisor. At 5 minutes
  that is about 2 seconds of work every 300 seconds per scope.
- **Latency.** A lane run takes tens of minutes. Waiting up to 5 more
  minutes for the next slot to fill changes little, and a lane that
  finishes is replaced on the next tick.
- **Usage probes.** When a usage snapshot is stale, usage admission may
  start a short probe. The run store lets one probe start per snapshot
  max-age window however often ticks run, so a shorter cadence does not
  cost more probes.

1 minute works if you want slots refilled sooner. Don't go below the time
one tick takes on your machine (check `journalctl --user -u
tm-dispatch.service`). Shorter than that, every other start just finds the
lock taken.

## launchd (macOS)

When a job exits, launchd kills the job's process group unless
`AbandonProcessGroup` is true. `setsid` already puts each lane in its own
process group, and the plist below sets `AbandonProcessGroup` as well. Save
this as
`~/Library/LaunchAgents/dev.tskmstr.dispatch.plist`, with the `PATH` set to
where your tools live:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.tskmstr.dispatch</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/bin/env</string><string>tm</string>
    <string>dispatch</string><string>tick</string><string>--all</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>StartInterval</key><integer>300</integer>
  <key>AbandonProcessGroup</key><true/>
  <key>StandardOutPath</key><string>/tmp/tm-dispatch.log</string>
  <key>StandardErrorPath</key><string>/tmp/tm-dispatch.log</string>
</dict>
</plist>
```

Load it with `launchctl bootstrap gui/$(id -u)
~/Library/LaunchAgents/dev.tskmstr.dispatch.plist`. The plist has not been
run on a Mac yet. Check that a launched lane outlives its tick the first
time you use it.
