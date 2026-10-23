# Solution agent

## Coding harness boundary

Production sessions use SPK AI Hub through `HubAgentServer` and the existing
`AgentConnection` interface. Harness IDs, names, icons and capabilities come
from the hub catalog; adding an ACP harness requires no editor registry change.
The editor owns Solution context, workspace worktrees, queue policy, transcript
projection and UI. The hub owns native processes, provider protocols, errors,
background artifacts and native transcript retention. `clientSessionKey` is an
opaque context-rotation group scoped by the hub to one client and harness.

`AcpThreadEvent::BackgroundActivity` carries normalized teammate/agent/shell
identity, state, parent tool ID, label and bounded snapshot text. Sawe does not
open native transcript or shell-output paths. Generic UI aging remains local;
authoritative completion and resumption arrive from the hub. Historical native
adapters, parsers and watcher fixtures compile only for tests/test-support.
Neither `solution_agent` nor the production editor depends on native harness
crates. `workspace_hooks` implements editor-owned Git worktree operations; the
hub owns native hook settings and output codecs.

## Voice

Solution chats own voice independently of their coding-provider adapter. See the
[voice guide](../../guides/voice-conversations.md) for setup and user controls.

`agent_settings::VoiceSettings` configures a compatible STT/TTS service.
Its local default targets the compatible speech API on port 8095. SPK AI Hub
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

## Tool-call presentation

Display rows map ranges of raw stream entries to compact contiguous tool groups.
Raw transcript indices and wire data do not change. The first entry anchors the
row and its context menu; the latest entry supplies a bounded preview. Approval
calls and differing message/agent origins break groups. Hidden tool output is
not parsed into chat Markdown. Find temporarily uses individual raw rows and
existing Markdown highlights; scroll offsets map back to raw entry anchors.

The existing conversation preview modal observes the original session/stream
and tool index/ID/creation time. Full group Markdown reads live ACP content or
retained raw results rather than the shortened chat summary. Session changes and
terminal Wakeup events refresh it, coalesced at 100ms;
opening another preview cancels the old subscription/task. It cannot retarget
to another group merely because a different chat was selected. Missing history
shows an explicit unavailable message. See the [tool guide](../../guides/chat-tool-details.md).

The hub catalog also owns context-usage semantics (`context_usage_current`).
The editor hides duplicate permission-mode labels for hub sessions and consumes
`_spk/system_note` recovery notices. Discovery menus show actionable connection
errors on fresh/offline profiles and retain cached harness choices. Endpoint
validation accepts only root HTTP URLs with numeric loopback IPs and valid ports.
Normalized activity projections use `StreamSource::HubSnapshot`; legacy file
sources remain descriptive fixtures rather than production native readers.
