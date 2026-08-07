#!/usr/bin/env bash
# PreToolUse hook — matcher: Bash|Grep
#
# Nudges toward graphify's knowledge graph at grep time, mirroring what
# `graphify claude install` wires directly into a project's own
# .claude/settings.json (matcher Glob|Grep, same additionalContext message).
# This copy adds two things graphify's own hook doesn't do, so lane runs in
# axiom worktrees get equivalent value without depending on axiom's repo
# .claude, which a fresh git worktree never sees:
#   - also fires on Bash commands that shell out to grep/rg, not just the
#     Grep tool, since agents frequently reach for `rg` directly
#   - rate-limits itself per session so it nudges a handful of times, not on
#     every single grep call
#
# No-op (fast, silent, exit 0) whenever graphify-out/graph.json doesn't exist
# in cwd — this script is wired into the shared lane-runner settings, so it
# also runs, harmlessly, for lanes/repos that never ran graphify at all.

command -v jq >/dev/null 2>&1 || exit 0

set -uo pipefail

[ -f graphify-out/graph.json ] || exit 0

INPUT=$(cat)
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)

case "$TOOL_NAME" in
  Grep)
    ;;
  Bash)
    CMD=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
    [ -z "$CMD" ] && exit 0
    printf '%s' "$CMD" | grep -Eq '(^|[^a-zA-Z0-9_])(grep|rg)([^a-zA-Z0-9_]|$)' || exit 0
    ;;
  *)
    exit 0
    ;;
esac

# Rate-limit: at most 5 nudges per session, tracked in a per-session counter
# file under graphify-out/ so it's colocated with the graph it's nudging
# about and cleaned up whenever graphify-out/ is (e.g. `graphify update`
# regenerating it, or the worktree being torn down).
SESSION_ID=$(printf '%s' "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$SESSION_ID" ] && SESSION_ID="unknown"

COUNT_FILE="graphify-out/.claude-nudge-count.$SESSION_ID"
COUNT=0
[ -f "$COUNT_FILE" ] && COUNT=$(cat "$COUNT_FILE" 2>/dev/null)
case "$COUNT" in ''|*[!0-9]*) COUNT=0 ;; esac

if [ "$COUNT" -ge 5 ]; then
  exit 0
fi

echo $((COUNT + 1)) > "$COUNT_FILE" 2>/dev/null

jq -nc '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    additionalContext: "graphify: Knowledge graph exists. Read graphify-out/GRAPH_REPORT.md for god nodes and community structure before searching raw files."
  }
}'

exit 0
