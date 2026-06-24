#!/usr/bin/env bash
# Mock `claude` stream-json harness for claude_native integration tests.
#
# Behavior is driven by env vars so one script covers several test scenarios:
#   MOCK_CLAUDE_CAPTURE   - if set, every received stdin line is appended here
#                           (lets a test assert on what the connection wrote back).
#   MOCK_CLAUDE_CONTROL   - if set, emit a `control_request` (can_use_tool) right
#                           after `init`, then wait for the matching control_response
#                           on stdin before emitting `result`.
#   MOCK_CLAUDE_NO_RESULT - if set, stream text but never emit the final `result`
#                           (the hang scenario).
#   MOCK_CLAUDE_OBEY_INTERRUPT
#                         - if set, the turn streams text but withholds its
#                           `result` until an `interrupt` control_request arrives
#                           on stdin, then emits `result(cancelled)` (the clean
#                           two-stage Stop: interrupt is honored, no kill needed).
#   MOCK_CLAUDE_IGNORE_INTERRUPT
#                         - if set, the turn streams text and never emits a
#                           `result` at all, even after an `interrupt` arrives
#                           (forces the escalation kill+resume path).
#   MOCK_CLAUDE_HOOK_INJECT
#                         - if set, after streaming the text delta the mock
#                           emits a `hook_callback` control_request, reads ONE
#                           line of stdin (the control_response), and if its
#                           response carries the substring `MOCK_HOOK_MARKER`
#                           in `additionalContext`, the final `result.result`
#                           text echoes that marker. Lets the integration test
#                           assert the agent "saw" the injected context.
#
# Faithful to the real `claude --input-format stream-json` binary: it does NOT
# emit `system/init` on startup — it blocks on stdin and emits `init` (echoing
# the `--session-id`/`--resume` id it was given) only AFTER the first user
# message, followed by a text delta and (unless suppressed) a success `result`.
# It exits when stdin reaches EOF.

emit() { printf '%s\n' "$1"; }

# Echo back whatever session id we were launched with, like the real binary.
session_id="mock-session"
prev=""
for arg in "$@"; do
  case "$prev" in
    --session-id|--resume) session_id="$arg" ;;
  esac
  prev="$arg"
done

emitted_init=""

while IFS= read -r line; do
  if [ -n "${MOCK_CLAUDE_CAPTURE:-}" ]; then
    printf '%s\n' "$line" >> "$MOCK_CLAUDE_CAPTURE"
  fi

  # Only react to user turns; ignore control responses for stream sequencing.
  case "$line" in
    *'"type":"user"'*)
      # Real `claude` emits `init` only once the first turn begins.
      if [ -z "$emitted_init" ]; then
        emit '{"type":"system","subtype":"init","session_id":"'"$session_id"'","uuid":"u-init"}'
        emitted_init="1"
      fi

      if [ -n "${MOCK_CLAUDE_CONTROL:-}" ]; then
        emit '{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"t1","input":{"command":"ls"}}}'
        # Wait for the control_response before finishing the turn.
        while IFS= read -r reply; do
          if [ -n "${MOCK_CLAUDE_CAPTURE:-}" ]; then
            printf '%s\n' "$reply" >> "$MOCK_CLAUDE_CAPTURE"
          fi
          case "$reply" in
            *'"type":"control_response"'*) break ;;
          esac
        done
      fi

      emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}},"uuid":"u1","session_id":"mock-session"}'

      if [ -n "${MOCK_CLAUDE_SUBAGENT:-}" ]; then
        # Emit a fake subagent assistant message that carries a tool_use, so
        # the integration test can assert the pump stamps
        # `_meta.claudeCode.parentToolUseId` onto the resulting ToolCall
        # before forwarding it to AcpThread.
        emit '{"type":"assistant","parent_tool_use_id":"toolu_parent_xyz","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_child_abc","name":"Bash","input":{"command":"ls"}}]},"uuid":"u-sub","session_id":"mock-session"}'
      fi

      hook_echo="none"
      if [ -n "${MOCK_CLAUDE_HOOK_INJECT:-}" ]; then
        # Fire a single PostToolUse-style hook_callback and wait for the
        # connection's control_response. If the response carries our marker
        # substring, the final `result` echoes it so the test can prove the
        # round trip.
        emit '{"type":"control_request","request_id":"hookreq-1","request":{"subtype":"hook_callback","callback_id":"pti","tool_use_id":"toolu_mock_1","input":{"tool_name":"Bash"}}}'
        if IFS= read -r hook_reply; then
          if [ -n "${MOCK_CLAUDE_CAPTURE:-}" ]; then
            printf '%s\n' "$hook_reply" >> "$MOCK_CLAUDE_CAPTURE"
          fi
          case "$hook_reply" in
            *MOCK_HOOK_MARKER*) hook_echo="MOCK_HOOK_MARKER" ;;
          esac
        fi
      fi

      if [ -n "${MOCK_CLAUDE_OBEY_INTERRUPT:-}" ]; then
        # Honor a soft interrupt: hold the turn open until an `interrupt`
        # control_request arrives, then end it the way the real `claude` does —
        # NOT a clean cancel, but an `error_during_execution` result (which the
        # connection maps to Cancelled because it knows it requested the stop).
        while IFS= read -r reply; do
          if [ -n "${MOCK_CLAUDE_CAPTURE:-}" ]; then
            printf '%s\n' "$reply" >> "$MOCK_CLAUDE_CAPTURE"
          fi
          case "$reply" in
            *'"subtype":"interrupt"'*) break ;;
          esac
        done
        emit '{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"stop_reason":"tool_use","errors":["[ede_diagnostic] interrupted"],"usage":{"input_tokens":1,"output_tokens":0},"uuid":"u2","session_id":"mock-session"}'
      elif [ -n "${MOCK_CLAUDE_IGNORE_INTERRUPT:-}" ]; then
        # Never emit `result`, even after an interrupt: forces the escalation
        # kill+resume path. Keep capturing stdin so the test can assert the
        # interrupt was written before the kill.
        while IFS= read -r reply; do
          if [ -n "${MOCK_CLAUDE_CAPTURE:-}" ]; then
            printf '%s\n' "$reply" >> "$MOCK_CLAUDE_CAPTURE"
          fi
        done
      elif [ -z "${MOCK_CLAUDE_NO_RESULT:-}" ]; then
        if [ -n "${MOCK_CLAUDE_HOOK_INJECT:-}" ]; then
          emit '{"type":"result","subtype":"success","is_error":false,"result":"echoed: '"$hook_echo"'","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2},"uuid":"u2","session_id":"mock-session"}'
        else
          emit '{"type":"result","subtype":"success","is_error":false,"result":"Hi","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2},"uuid":"u2","session_id":"mock-session"}'
        fi
      fi
      ;;
  esac
done
