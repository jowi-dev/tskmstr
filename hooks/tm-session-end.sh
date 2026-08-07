#!/usr/bin/env bash
# SessionEnd hook.
#
# Finishes a registered interactive session's run (audit/create) when the
# Claude Code session that owns it terminates, so sessions the user never
# `--record`ed or exited cleanly still get a final status and a model_usage
# snapshot. Looked up via a marker file at
# ${XDG_DATA_HOME:-~/.local/share}/tskmstr/sessions/<session_id> containing
# the run id. Exits 0 immediately during autonomous lane runs
# (TSKMSTR_RUN_ID set) — the lane wrapper owns finish there.
#
# Aggregates the transcript into a bare per-model map (same aggregation as
# tm-usage.sh, without the {"models": ...} wrapper) and finishes the run with
# --model-usage when that map is non-empty; otherwise finishes without it.
# The marker is removed either way so a dead session never shadows a future
# one with the same id.
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

TRANSCRIPT=$(printf '%s' "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

MAP=""
if [ -n "$TRANSCRIPT" ] && [ -r "$TRANSCRIPT" ]; then
  MAP=$(jq -c -s '
    map(select(.type == "assistant" and .message.usage != null))
    | group_by(.message.model)
    | map({
        key: .[0].message.model,
        value: {
          inputTokens: (map(.message.usage.input_tokens // 0) | add // 0),
          outputTokens: (map(.message.usage.output_tokens // 0) | add // 0),
          cacheReadInputTokens: (map(.message.usage.cache_read_input_tokens // 0) | add // 0),
          cacheCreationInputTokens: (map(.message.usage.cache_creation_input_tokens // 0) | add // 0)
        }
      })
    | from_entries
  ' "$TRANSCRIPT" 2>/dev/null)
fi

if [ -n "$MAP" ] && [ "$MAP" != "{}" ]; then
  tm runs finish "$RUN_ID" --status done --model-usage "$MAP" >/dev/null 2>&1
else
  tm runs finish "$RUN_ID" --status done >/dev/null 2>&1
fi

rm -f "$MARKER" 2>/dev/null

exit 0
