# Provider-independent voice conversations

**Status:** in progress
**Goal:** Speak to any Solution chat provider and hear replies, with interruptible playback.

## Context

The user chose STT → the existing text-agent adapter → TTS rather than Codex-only realtime. Speech must not bypass send queues, permissions or transcript persistence. Interrupting speech does not cancel the coding agent.

## Scope

- Shared audio capture, bounded recordings, playback cancellation and voice activity detection using the existing audio foundations.
- Configurable speech service independent of the coding provider; credentials stay outside transcript and settings JSON.
- Chat controls for recording/listening, automatic read-aloud and stopping playback. Recognized text follows ordinary send/queue behavior.
- Cancellation on disable, view destruction and stale operations; preserve typed drafts and transcript text.
- Focused deterministic tests plus HTTP/audio fixtures and a real rendered screenshot.

## Architectural decisions

1. Voice is a client interaction layer, not an agent adapter capability.
2. User speech interrupts audio only; agent cancellation remains explicit.
3. Recordings and speech requests are bounded; generation identity rejects late results.
4. Voice starts only by explicit user action and shows recording state.

## Risks

- Acoustic echo can trigger speech detection: use shared echo cancellation where supported and explicit record interruption as fallback.
- No configured speech credentials: provide actionable setup feedback and test with an isolated compatible service.
- Async responses may outlive a recording or view: own cancellation handles and verify stale result rejection.

## Verification

- Focused audio and solution-agent tests with actual assertions for capture bounds, speech segmentation, cancellation and send semantics.
- `cargo build --bin sawe`, touched-crate lint checks and release build for handoff.
- Isolated MCP-driven UI smoke test and screenshot; inspect image.
- Verify successful and failed speech service responses using a local fixture; never claim an untested remote backend.

## Acceptance

- [x] Capture and transcribe speech into the selected chat without losing typed drafts.
- [x] Read newly completed assistant answers without replaying old history or reasoning/tools.
- [x] Interrupt playback without stopping the agent; reject stale audio.
- [x] Explicit visible recording and bounded resources with cleanup.
- [ ] Tests, builds, visual evidence and dedicated documentation complete.

## Verified behavior

Operational progress and release delivery are tracked in task
`sawe-provider-independent-voice`. The completed functional check used a real
CPAL/PipeWire virtual microphone and local whisper/Piper, with a deterministic
coding-provider fixture. It preserved a typed draft, sent a hands-free utterance
through ordinary chat, and interrupted real output within 0.336 seconds of user
waveform playback starting. Screenshots were inspected at 1920×1080 and 1000×700.
The first measurement was corrected for the monitor's default buffering by
requesting 10 ms capture latency. Physical microphone accuracy and acoustic
echo cancellation were not established by this virtual test.
