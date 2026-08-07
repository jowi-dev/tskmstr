#!/usr/bin/env bash
# PostToolUse hook — matcher: TaskCreate|TaskUpdate
#
# Emits a full checklist snapshot to the tskmstr run store whenever the
# agent uses the newer task-list tools (TaskCreate/TaskUpdate), mirroring
# what tm-checklist.sh does for TodoWrite. Unlike TodoWrite, these tools
# mutate a single task per call rather than handing over the whole list, so
# state is accumulated across calls in a small per-run JSON file
# (taskId -> {text, done, deleted}) and the full snapshot is re-derived and
# re-emitted after every mutation. Fires during autonomous lane runs
# (TSKMSTR_RUN_ID set) or a registered interactive session (audit/create),
# looked up via a marker file at
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

TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null)
[ "$TOOL_NAME" = "TaskCreate" ] || [ "$TOOL_NAME" = "TaskUpdate" ] || exit 0

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/tm-hooks"
STATE_FILE="$STATE_DIR/tasklist-$RUN_ID.json"

mkdir -p "$STATE_DIR" 2>/dev/null || exit 0

if [ ! -f "$STATE_FILE" ]; then
  printf '{}' > "$STATE_FILE" 2>/dev/null || exit 0
fi

CURRENT=$(cat "$STATE_FILE" 2>/dev/null)
[ -z "$CURRENT" ] && CURRENT='{}'
printf '%s' "$CURRENT" | jq -e . >/dev/null 2>&1 || CURRENT='{}'

# The created task's id lives in the structured response
# (.tool_response.task.id — the "Task #7 created successfully" text the
# model sees is rendering only and never reaches the hook payload). Older
# Claude Code versions exposed a plain-text response under tool_response or
# tool_output, so fall back to parsing "Task #N" out of the response text.
RESPONSE_TASK_ID=$(printf '%s' "$INPUT" | jq -r '.tool_response.task.id // empty' 2>/dev/null)
if [ -z "$RESPONSE_TASK_ID" ]; then
  RESPONSE_TEXT=$(printf '%s' "$INPUT" | jq -r '(.tool_response // .tool_output // "") | if type == "string" then . else tostring end' 2>/dev/null)
  RESPONSE_TASK_ID=$(printf '%s' "$RESPONSE_TEXT" | grep -oE 'Task #[0-9]+' | head -1 | tr -d 'Task #')
fi

UPDATED=""

if [ "$TOOL_NAME" = "TaskCreate" ]; then
  [ -z "$RESPONSE_TASK_ID" ] && exit 0

  SUBJECT=$(printf '%s' "$INPUT" | jq -r '.tool_input.subject // empty' 2>/dev/null)
  [ -z "$SUBJECT" ] && exit 0

  UPDATED=$(printf '%s' "$CURRENT" | jq -c \
    --arg id "$RESPONSE_TASK_ID" --arg text "$SUBJECT" \
    '.[$id] = {text: $text, done: false, deleted: false}' 2>/dev/null)
else
  # tool_input.taskId is the authoritative target; fall back to the id
  # parsed from the response text if it's ever missing.
  TASK_ID=$(printf '%s' "$INPUT" | jq -r '.tool_input.taskId // empty' 2>/dev/null)
  [ -z "$TASK_ID" ] && TASK_ID="$RESPONSE_TASK_ID"
  [ -z "$TASK_ID" ] && exit 0

  STATUS=$(printf '%s' "$INPUT" | jq -r '.tool_input.status // empty' 2>/dev/null)
  NEW_SUBJECT=$(printf '%s' "$INPUT" | jq -r '.tool_input.subject // empty' 2>/dev/null)

  # Calls that only touch dependency fields (e.g. addBlockedBy) carry no
  # status/subject change — nothing the checklist snapshot cares about.
  if [ -z "$STATUS" ] && [ -z "$NEW_SUBJECT" ]; then
    exit 0
  fi

  UPDATED=$(printf '%s' "$CURRENT" | jq -c \
    --arg id "$TASK_ID" --arg status "$STATUS" --arg subject "$NEW_SUBJECT" \
    '
    .[$id] //= {text: ("Task #" + $id), done: false, deleted: false}
    | if $status == "completed" then .[$id].done = true
      elif $status == "deleted" then .[$id].deleted = true
      elif $status != "" then .[$id].done = false
      else . end
    | if $subject != "" then .[$id].text = $subject else . end
    ' 2>/dev/null)
fi

[ -z "$UPDATED" ] && exit 0
printf '%s' "$UPDATED" | jq -e . >/dev/null 2>&1 || exit 0

TMP_FILE="$STATE_FILE.tmp.$$"
printf '%s' "$UPDATED" > "$TMP_FILE" 2>/dev/null || exit 0
mv "$TMP_FILE" "$STATE_FILE" 2>/dev/null || { rm -f "$TMP_FILE" 2>/dev/null; exit 0; }

DETAIL=$(printf '%s' "$UPDATED" | jq -c '
  {items: [
    to_entries
    | map(select(.value.deleted != true))
    | sort_by(.key | tonumber)
    | .[]
    | {text: .value.text, done: .value.done}
  ]}
' 2>/dev/null)

[ -z "$DETAIL" ] && exit 0

tm runs event "$RUN_ID" --kind checklist --detail "$DETAIL" >/dev/null 2>&1

exit 0
