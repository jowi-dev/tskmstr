#!/usr/bin/env bash
# PostToolUse hook — matcher: *
#
# Emits a `tm runs event` telemetry entry for every tool call made during an
# autonomous lane run (TSKMSTR_RUN_ID set) or a registered interactive session
# (audit/create), so tm runs watch/reap can see activity and heartbeat
# freshness. Registered sessions are looked up via a marker file at
# ${XDG_DATA_HOME:-~/.local/share}/tskmstr/sessions/<session_id> containing
# the run id. Unregistered interactive sessions must pay near-zero cost here
# — a single directory-emptiness check, before stdin is even read.
#
# detail JSON: {"tool": <name>, "summary": <string>, "agent": <agent_type>}
# summary and agent are omitted when empty. "agent" comes from agent_type in
# the hook payload, which is present only inside a subagent call — so it
# attributes subagent tool calls and is absent for main-loop calls.
#
# For a completed Agent/Task call whose tool_response carries a `usage`
# object, a second "agent_usage" event is additionally emitted:
# {"agentType": <string>, "description": <string>, "model": <string>,
#  "outputTokens"/"inputTokens"/"cacheReadInputTokens"/
#  "cacheCreationInputTokens": <int>, "totalToolUseCount": <int>,
#  "durationMs": <int>} — the additive per-agent usage breakdown consumed by
# `tm runs show`'s "Agent usage" section. description is omitted when empty;
# the whole emission is skipped when usage, resolvedModel, or the agent type
# is absent (async spawn responses, or a row that can't aggregate).
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

tm runs event "$RUN_ID" --kind tool --detail "$DETAIL" >/dev/null 2>&1

# For completed Agent/Task calls, additionally emit an agent_usage event
# carrying the harness's own per-invocation usage summary (tool_response),
# alongside — never instead of — the tool event above. A background/async
# spawn's PostToolUse payload has no `usage` yet (just agentId/status); its
# eventual completion arrives as a *later* PostToolUse (for whichever call
# polls/awaits it) that does carry usage, so gating on `.tool_response.usage`
# being non-null naturally skips the spawn and captures the real completion
# whenever it lands.
if [ "$TOOL_NAME" = "Agent" ] || [ "$TOOL_NAME" = "Task" ]; then
  AGENT_DETAIL=$(printf '%s' "$INPUT" | jq -c '
    .tool_response as $r
    | ($r.usage // null) as $u
    | if $r == null or $u == null then null else
        ($r.resolvedModel // null) as $model
        | (.tool_input.subagent_type // $r.agentType // null) as $agentType
        | if $model == null or $agentType == null then null else
            (.tool_input.description // "") as $desc
            | {agentType: $agentType, model: $model}
              + (if $desc != "" then {description: $desc} else {} end)
              + {
                  outputTokens: ($u.output_tokens // 0),
                  inputTokens: ($u.input_tokens // 0),
                  cacheReadInputTokens: ($u.cache_read_input_tokens // 0),
                  cacheCreationInputTokens: ($u.cache_creation_input_tokens // 0),
                  totalToolUseCount: ($r.totalToolUseCount // 0),
                  durationMs: ($r.totalDurationMs // 0)
                }
        end
      end
  ' 2>/dev/null)

  if [ -n "$AGENT_DETAIL" ] && [ "$AGENT_DETAIL" != "null" ]; then
    tm runs event "$RUN_ID" --kind agent_usage --detail "$AGENT_DETAIL" >/dev/null 2>&1
  fi
fi

exit 0
