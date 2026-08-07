#!/usr/bin/env bash
# Stop / SubagentStop hook.
#
# Emits a live per-model token usage snapshot to the tskmstr run store so
# `tm runs watch` can render cost as it accrues, not just at the end. Only
# fires during autonomous lane runs (TSKMSTR_RUN_ID set).
#
# detail JSON (full snapshot, latest wins — same convention as the checklist
# event): {"models": {"<model>": {"inputTokens": N, "outputTokens": N,
# "cacheReadInputTokens": N, "cacheCreationInputTokens": N}}}
#
# Always exits 0 — a telemetry hook must never disturb the session, and
# never prints to stdout/stderr on success or failure.

[ -z "${TSKMSTR_RUN_ID:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0
command -v tm >/dev/null 2>&1 || exit 0

set -uo pipefail

INPUT=$(cat)
TRANSCRIPT=$(printf '%s' "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)

[ -z "$TRANSCRIPT" ] && exit 0
[ -r "$TRANSCRIPT" ] || exit 0

DETAIL=$(jq -c -s '
  {
    models: (
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
    )
  }
' "$TRANSCRIPT" 2>/dev/null)

[ -z "$DETAIL" ] && exit 0
[ "$DETAIL" = "{\"models\":{}}" ] && exit 0

tm runs event "$TSKMSTR_RUN_ID" --kind usage --detail "$DETAIL" >/dev/null 2>&1

exit 0
