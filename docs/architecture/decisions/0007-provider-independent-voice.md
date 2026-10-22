# ADR-0007: Voice belongs above coding-provider adapters

**Status:** accepted (2026-10-22)

## Context

The user requested voice conversations with Codex, then chose a shared speech-to-
text → existing text agent → text-to-speech pipeline so other providers can use
it too. Local recognition must avoid a large resident memory footprint.

## Decision

The Solution session view owns explicit voice controls and the capture/request/
playback lifecycle. It submits recognized speech through the existing composer
and queue funnel, preserving typed drafts, image attachments and cold-session
resume behavior. It never routes audio directly into a provider's private
protocol. Read-aloud responds to actual completed-turn events, because queued
turns can start without an observable Idle state.

Speech and agent cancellation are distinct. Voice activity detection and Stop
Speaking cancel the audio source, pending synthesis and queued speech. They do
not cancel or restart the coding agent. Request generations and current-dialog/
Solution checks reject stale transcription and synthesis after navigation.
History replacement or rewind invalidates the voice cursor.

The existing audio crate supplies bounded PCM16 capture, adaptive energy-based
voice activity detection and cancellable playback. Speech shares the application
output mixer so echo cancellation sees a single reverse stream. Every started
speech segment ends explicitly, including short discarded noises.

A configurable OpenAI-compatible HTTP service supplies STT/TTS. SPK LAIHub is
an independently managed service and owns installation, model processes,
CPU limits, retention and idle unloading. Sawe owns only capture/playback and
API requests. It never launches or terminates inference engines and has no model
idle timer or load/unload commands. Active Hands-free sends an activity ping
every 30 seconds; stopping, navigation or closing cancels those pings. LAIHub
alone applies retention policy, including the user's requirement to retain
models during active real-time conversation. Default speech remains on loopback
port 8095; the LAIHub control panel can use its separate port 5195. The previous
standalone Python runtime is removed from Sawe. Endpoint credentials are selected
from environment variables, scoped by origin, and excluded from transcript and
settings data. Loopback connections bypass proxies.

## Consequences

Coding adapters retain their existing text, permissions and queue semantics.
Other speech engines can implement the same transport without changing chats.
The first version sends complete utterances after silence; it does not expose
incremental recognized words. A provider-native realtime mode remains possible
as a separate future interaction model, rather than an implicit replacement.

## References

- [Voice conversations guide](../../guides/voice-conversations.md)
- [Local runtime evidence](../../findings/2026-10-local-voice-runtime.md)
- [Implementation plan](../../plans/2026-10-22-provider-independent-voice.md)
