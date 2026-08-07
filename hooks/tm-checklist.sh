#!/usr/bin/env bash
# PostToolUse hook — matcher: TodoWrite
#
# Emits a full checklist snapshot to the tskmstr run store whenever the agent
# updates its todo list, so `tm runs watch` can render live progress. Only
# fires during autonomous lane runs (TSKMSTR_RUN_ID set).
#
# Always exits 0 — a telemetry hook must never disturb the session, and
# never prints to stdout/stderr on success or failure.

[ -z "${TSKMSTR_RUN_ID:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0
command -v tm >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)
TODOS=$(printf '%s' "$INPUT" | jq -c '.tool_input.todos // empty' 2>/dev/null)

[ -z "$TODOS" ] && exit 0
[ "$TODOS" = "null" ] && exit 0
[ "$TODOS" = "[]" ] && exit 0

DETAIL=$(printf '%s' "$TODOS" | jq -c '{items: [ .[] | {text: .content, done: (.status == "completed")} ]}' 2>/dev/null)

[ -z "$DETAIL" ] && exit 0

tm runs event "$TSKMSTR_RUN_ID" --kind checklist --detail "$DETAIL" >/dev/null 2>&1

exit 0
