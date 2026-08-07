#!/usr/bin/env bash
# PreToolUse hook — matcher: Edit|Write|MultiEdit|NotebookEdit
#
# Lane policy: during a tracked lane run (TSKMSTR_RUN_ID set), the main loop
# may not edit files directly — it must delegate the change to a subagent
# (Agent tool, model: sonnet) and review the diff instead. Subagents are the
# delegates and remain allowed to edit.
#
# Context: lane run AX-403 cost $15.81 because the main loop did 8 Edits and
# 34 Bash calls itself with only 3 Agent delegations. This guard forces the
# offload.
#
# Interactive sessions (no TSKMSTR_RUN_ID) are unaffected — exit 0 instantly.

[ -z "${TSKMSTR_RUN_ID:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)

# Subagent context: agent_id/agent_type are present only when the hook fires
# inside a subagent call. Subagents are the delegates — always allowed.
AGENT_ID=$(printf '%s' "$INPUT" | jq -r '.agent_id // empty' 2>/dev/null)
[ -n "$AGENT_ID" ] && exit 0

REASON="Lane policy: do not edit files from the main loop. Delegate this change to a subagent (Agent tool, model: sonnet) with a precise spec, then review its diff. The main loop only plans, reviews, and verifies."

jq -nc --arg reason "$REASON" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: $reason
  }
}'

exit 0
