#!/usr/bin/env bash
# Stop / Notification / UserPromptSubmit hook.
#
# Emits the "awaiting input" / "resumed" telemetry a registered interactive
# session (audit/create) needs so `tm runs watch` can derive a waiting-for-
# input badge instead of showing an idling session as indistinguishable from
# a hung one. Looked up via a marker file at
# ${XDG_DATA_HOME:-~/.local/share}/tskmstr/sessions/<session_id> containing
# the run id, same as tm-session-end.sh. Exits 0 immediately during
# autonomous lane runs (TSKMSTR_RUN_ID set) — lane runs are headless, so
# "awaiting input" is meaningless there.
#
# Dispatches on hook_event_name from the stdin JSON:
#   Stop             -> `tm runs event <id> --kind await` (turn ended;
#                        interactively that means "waiting for the user").
#   Notification     -> `tm runs event <id> --kind await` with
#                        --detail '{"message": ...}' when the payload
#                        carries a message (permission prompt / idle nag),
#                        omitted otherwise.
#   UserPromptSubmit -> `tm runs event <id> --kind resume` (user replied;
#                        Claude is working again).
# Any other event name is a no-op.
#
# Always exits 0 — a telemetry hook must never disturb the session, and
# never prints to stdout/stderr on success or failure.

[ -n "${TSKMSTR_RUN_ID:-}" ] && exit 0

SESSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tskmstr/sessions"
[ -n "$(ls -A "$SESSIONS_DIR" 2>/dev/null)" ] || exit 0

command -v jq >/dev/null 2>&1 || exit 0
command -v tm >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)
SESSION_ID=$(printf '%s' "$INPUT" | jq -r '.session_id // empty' 2>/dev/null)

[ -n "$SESSION_ID" ] || exit 0

MARKER="$SESSIONS_DIR/$SESSION_ID"
[ -f "$MARKER" ] || exit 0

RUN_ID=$(cat "$MARKER" 2>/dev/null)
[ -n "$RUN_ID" ] || exit 0

EVENT_NAME=$(printf '%s' "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)

case "$EVENT_NAME" in
  Stop)
    tm runs event "$RUN_ID" --kind await >/dev/null 2>&1
    ;;
  Notification)
    MESSAGE=$(printf '%s' "$INPUT" | jq -r '.message // empty' 2>/dev/null)
    if [ -n "$MESSAGE" ]; then
      DETAIL=$(printf '%s' "$INPUT" | jq -c '{message: .message}' 2>/dev/null)
      tm runs event "$RUN_ID" --kind await --detail "$DETAIL" >/dev/null 2>&1
    else
      tm runs event "$RUN_ID" --kind await >/dev/null 2>&1
    fi
    ;;
  UserPromptSubmit)
    tm runs event "$RUN_ID" --kind resume >/dev/null 2>&1
    ;;
esac

exit 0
