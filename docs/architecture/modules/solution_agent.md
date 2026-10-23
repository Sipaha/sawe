# Solution agent voice boundary

Solution chats own voice independently of their coding-provider adapter. See the
[voice guide](../../guides/voice-conversations.md) for setup and user controls.

`agent_settings::VoiceSettings` configures a compatible STT/TTS service.
Its local default targets the compatible speech API on port 8095. SPK LAIHub
owns the engines, installation and all model lifetime/idle policy. Sawe has no
model workers, load/unload commands or retention settings. A cancellable
Hands-free task sends activity pings every 30 seconds to eligible local APIs.
Unsupported services quietly disable that extension; custom endpoints and model
request fields remain independent of coding-provider selection.
`voice_service::VoiceService` transcribes bounded PCM WAV recordings and requests
bounded WAV speech; it uses the HTTP client's complete-request timeout. Endpoint
overrides isolate credentials by origin, and loopback transport bypasses proxies.
`speech_text` removes code blocks, images and formatting from spoken prose.

The session view owns capture, request tasks and cancellable playback. It sends
recognized fragments with an `[audio]` prefix through ordinary compose/queue
behavior, preserving typed drafts.
An incremental prose cursor queues sentences and pre-tool commentary while the
agent is running; actual `Stopped` events flush the final tail without replay.
Short chunks and bounded queue backpressure limit synthesis latency. Reasoning,
tool output, fenced code, images and child-agent output stay silent.
One global weak view owner keeps Hands-free on the original dialog across tab
navigation; claiming it in another dialog synchronously stops the old capture.
Ordinary dictation/read-aloud retain visibility checks. Closing, history replacement
and request generations reject stale results. Interrupting audio suppresses the
old reply until new user input and never cancels the coding agent. Microphone
candidates instead retain audio without affecting playback. Activity confirmed
by 120ms of voiced frames pauses playback provisionally while preserving unread
audio and queued prose; empty/failed STT resumes it. Playback created
while capture/transcription is pending starts paused.

The audio crate's shared mixer provides the AEC reverse stream. Started speech
segments have at least 120ms of voiced frames and always have an explicit
ending. Short discarded candidates emit neither event and never pause audio. See
[ADR-0007](../decisions/0007-provider-independent-voice.md) for the invariants.
