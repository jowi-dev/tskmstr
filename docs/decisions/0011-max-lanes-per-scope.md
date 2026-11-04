# ADR-0011: `[dispatch] max_lanes` Counts Lanes per Scope

**Status:** Accepted
**Date:** 2026-11-04

## Problem

The autopilot lane cap counted lanes on the whole machine, but everything
around it works per project. `RunStore::running_lanes` selected every
`running` lane row with no scope filter. The dispatcher's cap check
(`crate::work::dispatch::tick`) and the board's autopilot summary
(`crate::work::autopilot::load_status`) both used that count.

On 2026-10-22 a lane in a Jira project had been waiting for input for hours
and filled the default cap of 1. Autopilot in jowi-dev/tskmstr would then
have refused every launch on the lane cap, and nothing on the tskmstr board
said why. Raising `max_lanes` works around it but leaves the mismatch in
place. GitHub issue #170 tracks it.

## Decision

1. **The cap is per scope.** A scope is one ticket backend and project, for
   example `github:jowi-dev/tskmstr`. A tick counts only the running lanes
   whose run scope matches the scope it is ticking. The query is
   `RunStore::running_lane_tickets`.

2. **Legacy unscoped runs count for every scope.** Rows with `scope = ''`
   predate scoped runs, so tm cannot tell which project they belong to.
   They count toward every scope's cap, the same rule `in_hand` and the
   other scoped run queries follow.

3. **A lane-cap refusal names the lanes it counted.** `Refusal::MaxLanes`
   carries the counted tickets, and its message lists them, for example
   `not launching: 2 lanes running (GH-7, GH-8), [dispatch] max_lanes is 2`.

4. **The board's `lanes n/m` figure uses the same per-scope count**, so it
   agrees with what the dispatcher will do. The overview's system header,
   which reports the machine's load, still counts every running lane.

## Alternatives considered

- **Per machine (the previous behavior).** Declined. The memory budget
  (`[work] memory_budget_gb`, GitHub issue #66) and the build slots (GitHub
  issue #65) already guard the machine's resources, so a second
  machine-wide count only duplicates them. It also lets one project's idle
  lane block autopilot in every other project.

## Consequences

- With `max_lanes = 1` and autopilot on in two projects, the machine can run
  two autopilot lanes at once. Memory admission still refuses a launch the
  machine cannot hold.
- `max_lanes` now means "lanes per project", so a config that raised it to
  work around the old cross-project count can lower it again.
