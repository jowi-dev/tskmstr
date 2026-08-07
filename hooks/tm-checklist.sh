#!/usr/bin/env bash
# PostToolUse hook — matcher: TodoWrite
#
# Emits a full checklist snapshot to the tskmstr run store whenever the agent
# updates its todo list, so `tm runs watch` can render live progress. Fires
# during autonomous lane runs (TSKMSTR_RUN_ID set) or a registered
# interactive session (audit/create), looked up via a marker file at
# ${XDG_DATA_HOME:-~/.local/share}/tskmstr/sessions/<session_id> containing
# the run id. Unregistered interactive sessions pay only a cheap
# directory-emptiness check per hook fire.
#
# Always exits 0 — a telemetry hook must never disturb the session, and
# never prints to stdout/stderr on success or failure.

RUN_ID="${TSKMSTR_RUN_ID:-}"
if [ -z "$RUN_ID" ]; then
  SESSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tskmstr/sessions"
  [ -n "$(ls -A "$SESSIONS_DIR" 2>/dev/null)" ] || exit 0
fi

command -v jq >/dev/null 2>&1 || exit 0
command -v tm >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)

if [ -z "$RUN_ID" ]; then
  SESSION_ID=$(printf '%s' "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)
  [ -n "$SESSION_ID" ] || exit 0
  MARKER="$SESSIONS_DIR/$SESSION_ID"
  [ -f "$MARKER" ] || exit 0
  RUN_ID=$(cat "$MARKER" 2>/dev/null)
  [ -n "$RUN_ID" ] || exit 0
fi

TODOS=$(printf '%s' "$INPUT" | jq -c '.tool_input.todos // empty' 2>/dev/null)

[ -z "$TODOS" ] && exit 0
[ "$TODOS" = "null" ] && exit 0
[ "$TODOS" = "[]" ] && exit 0

DETAIL=$(printf '%s' "$TODOS" | jq -c '{items: [ .[] | {text: .content, done: (.status == "completed")} ]}' 2>/dev/null)

[ -z "$DETAIL" ] && exit 0

tm runs event "$RUN_ID" --kind checklist --detail "$DETAIL" >/dev/null 2>&1

exit 0
