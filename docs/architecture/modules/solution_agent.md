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
recognized text through ordinary compose/queue behavior, preserving typed drafts.
Actual `Stopped` events delimit spoken replies; state polling alone can miss
back-to-back turns. Navigation, history replacement and generation checks reject
stale results. Interrupting audio never cancels the coding agent.

The audio crate's shared mixer provides the AEC reverse stream. Started speech
segments always have an explicit ending, including discarded short noises. See
[ADR-0007](../decisions/0007-provider-independent-voice.md) for the invariants.
