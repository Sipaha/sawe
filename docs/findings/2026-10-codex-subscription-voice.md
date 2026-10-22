# Codex voice through a ChatGPT subscription

Verified on 2026-10-22 with Codex CLI `0.160.1` on Linux and an existing
ChatGPT Pro login. This is a research result, not a shipped Sawe voice backend.
The current [Sawe voice controls](../guides/voice-conversations.md) still use
the configured independent STT/TTS service.

## Transport and protocol

- An isolated app-server `account/read` reported `type: chatgpt`, `planType: pro`.
- `thread/realtime/start` with WebSocket transport rejected this login with
  `realtime conversation requires API key auth`; this does not establish that
  subscribed voice is unavailable over another transport.
- WebRTC with realtime `v3` and audio output reached the subscription backend.
  The default provider failed with `invalid_quicksilver_alpha_header`:
  `AVAS requires OpenAI-Alpha: quicksilver=v2.`
- A process-only custom provider with `requires_openai_auth = true` and
  `http_headers = { "OpenAI-Alpha" = "quicksilver=v2" }` allowed the same Pro
  login to start a real voice session, negotiate SDP and receive Russian
  transcripts. No Platform API key was supplied.
- A custom provider needs its own ID: overriding reserved `openai` is rejected.
  Realtime `v2` cannot be substituted here: the client reports that AVAS calls
  require `v1` or `v3`. Text output is also restricted to realtime `v2`.

The isolated provider configuration was:

```toml
model_provider = "voice-probe"
[model_providers.voice-probe]
name = "Voice probe"
wire_api = "responses"
requires_openai_auth = true
http_headers = { "OpenAI-Alpha" = "quicksilver=v2" }
```

All configuration, logs and ephemeral sessions stayed under the Solution's
`.tmp/voice-benchmark-ru`. Temporary authentication copies were removed when
each process ended; credentials were excluded from evidence. No user config,
installed Codex binary or production Sawe session was changed.

## Recognition and limits

The public GOLOS phrase `заказать яблоки зеленые` produced the correct native
user transcript `Заказать яблоки зеленые`; local small Q5_1 had produced
`Закосот с яблоки. Зеленую.` The exact remote transcription model is not exposed,
so call this **Codex Voice**, not `whisper-1` or Whisper large-v3.
Use app-server `thread/realtime/transcript/delta` with `role: user` consistently:
RTC `input_transcript.added` can be absent even when app-server has a transcript.
Keep original responses and transport errors; do not retry inaccurate text to
improve an accuracy score. WebRTC uses resampled Opus audio and streaming input,
so its total session time is not comparable to a completed-WAV HTTP request.

Operational evidence and benchmark attachments belong to task
`sawe-russian-whisper-comparison-20261022`. Related background:
[OpenAI Voice documentation](https://learn.chatgpt.com/docs/features/voice) and
[subscription usage pricing](https://learn.chatgpt.com/docs/pricing).
