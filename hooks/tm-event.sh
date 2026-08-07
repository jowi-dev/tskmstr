#!/usr/bin/env bash
# PostToolUse hook — matcher: *
#
# Emits a `tm runs event` telemetry entry for every tool call made during an
# autonomous lane run (TSKMSTR_RUN_ID set), so tm runs watch/reap can see
# activity and heartbeat freshness. Interactive human sessions do not have
# TSKMSTR_RUN_ID set and must pay near-zero cost here.
#
# detail JSON: {"tool": <name>, "summary": <string>, "agent": <agent_type>}
# summary and agent are omitted when empty. "agent" comes from agent_type in
# the hook payload, which is present only inside a subagent call — so it
# attributes subagent tool calls and is absent for main-loop calls.
#
# Always exits 0 — a telemetry hook must never disturb the session, and
# never prints to stdout/stderr on success or failure.

[ -z "${TSKMSTR_RUN_ID:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0
command -v tm >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)

[ -z "$TOOL_NAME" ] && exit 0

# TodoWrite is covered by tm-checklist.sh, and TaskCreate/TaskUpdate are
# covered by tm-tasklist.sh — skip both to avoid double-logging the same
# progress update as both a tool event and a checklist snapshot.
[ "$TOOL_NAME" = "TodoWrite" ] && exit 0
[ "$TOOL_NAME" = "TaskCreate" ] && exit 0
[ "$TOOL_NAME" = "TaskUpdate" ] && exit 0

# Derive a short human-readable summary from tool_input, tailored per tool.
# Unknown tools fall through to an empty summary (omitted from the detail).
SUMMARY=$(printf '%s' "$INPUT" | jq -r '
  .tool_input as $i
  | if .tool_name == "Bash" then
      ($i.command // "" | gsub("\\s+"; " ") | .[0:120])
    elif (.tool_name == "Read" or .tool_name == "Edit" or .tool_name == "Write" or .tool_name == "NotebookEdit") then
      ($i.file_path // "")
    elif (.tool_name == "Grep" or .tool_name == "Glob") then
      ($i.pattern // "")
    elif (.tool_name == "Agent" or .tool_name == "Task") then
      ( ($i.model // "") as $m
        | ($i.description // "") as $d
        | if $m != "" then ($m + ": " + $d) else $d end
      )
    elif .tool_name == "Skill" then
      ($i.skill // "")
    elif (.tool_name == "WebFetch" or .tool_name == "WebSearch") then
      ($i.url // $i.query // "")
    else
      ""
    end
' 2>/dev/null)

AGENT_TYPE=$(printf '%s' "$INPUT" | jq -r '.agent_type // empty' 2>/dev/null)

DETAIL=$(jq -nc \
  --arg t "$TOOL_NAME" \
  --arg s "${SUMMARY:-}" \
  --arg a "${AGENT_TYPE:-}" \
  '{tool: $t} + (if $s != "" then {summary: $s} else {} end) + (if $a != "" then {agent: $a} else {} end)' \
  2>/dev/null)

[ -z "$DETAIL" ] && exit 0

tm runs event "$TSKMSTR_RUN_ID" --kind tool --detail "$DETAIL" >/dev/null 2>&1

exit 0
