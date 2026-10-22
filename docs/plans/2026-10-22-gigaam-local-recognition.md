# GigaAM CTC as the local recognition default

**Status:** ready to dispatch
**Goal:** Use the verified low-memory Russian GigaAM v3 CTC INT8 CPU model for
Sawe's existing provider-independent voice conversations.

## Context

The user selected GigaAM CTC after comparing 30 public Russian recordings.
It produced 3/152 word edits, warm median 85 ms and about 429 MiB process RAM
without a GPU. The measured load plus first inference was about one second;
full sidecar cold request time must also be measured. See the
[comparison](../findings/2026-10-russian-asr-comparison.md).

## Scope

1. `script/voice-local`: default model `gigaam-v3-ctc`, CPU INT8 through pinned
   ONNX ASR / ONNX Runtime, pinned model files and SHA-256 verification. Explicit
   `small` and `base` retain the existing optional Whisper path. GigaAM setup
   must not build/download Whisper or require CUDA. Keep Piper synthesis.
2. Keep the public loopback STT/TTS HTTP contract, bounds, busy behavior,
   transcription priority, browser rejection, timeouts and proxy bypass.
3. Use an owned persistent child for GigaAM inference, loaded lazily and killed
   after 60 idle seconds. Enforce CPU thread limits and offline model loading.
   Do not retain recognized text in logs or create orphan model processes.
4. `solution_agent` voice default names GigaAM; custom endpoint/model overrides
   remain intact. Existing voice controls/composer/queue behavior remain intact.
5. Update the guide, runtime architecture/evidence and ADR-0007/FORK decision.
   Historical benchmark findings remain historical evidence. No README changes.

## Design and ownership

The coding adapters remain text-based. The existing sidecar remains the engine
boundary; do not add native Codex audio routing or a new voice-only agent.
Use model selection in the runtime CLI to distinguish GigaAM from optional
Whisper. GigaAM accepts Russian/automatic language selection; unsupported
languages must fail explicitly rather than imply English support.

The worker IPC must have bounded input/output, an explicit response timeout and
process reap on timeout/failure/shutdown. Preserve the persistent launcher
thread: Linux parent-death signals belong to the spawning thread. Model and
runtime installation stay in the Solution runtime directory. Setup is explicit;
serve never downloads packages/models or contacts model repositories.

Runtime implementation/tests are delegated in an isolated git worktree. The
supervisor owns Rust defaults, docs, integration, live sidecar migration and
build/UI verification. Independent review is read-only.

## Out of scope

Punctuation/number rewriting, language translation, a different TTS voice,
new microphone controls, cloud credentials, startup registration and installing
system packages. CTC output is unpunctuated Russian; digits are not considered
a product error. Do not delete the working Whisper runtime/models.

## Risks

- ONNX worker startup adds Python time: measure complete first HTTP request.
- Idle model unloading can race requests: serialize it with transcription.
- Worker crashes/timeouts must reset ownership and allow later recovery.
- Model supply chain/cache corruption: pinned revision/hash checks at setup
  and appropriate startup validation; retain MIT license/attribution.
- Existing shell launch may explicitly request Whisper: replace the owned
  running sidecar only after identifying its PID/command and testing GigaAM.

## Verification

- Offline Python unit/integration tests for both engines and worker lifecycle,
  malformed data, wrong language, timeout/recovery, idle unload and shutdown.
- Real fresh setup and isolated GigaAM HTTP service: all 30 corpus WAVs,
  cold/warm timing, RAM/no VRAM, model unload/reload, Piper and cancellation.
- Focused `solution_agent` tests, strict touched-package Clippy and debug build.
- Actual isolated Sawe UI with virtual microphone: GigaAM transcript through
  ordinary composer/send path, preserved typed draft, read-aloud/interruption.
  Inspect a real screenshot. Preserve the user's live editor.
- Optimized release build and actual release startup/screenshot. Keep logs and
  screenshots under Solution `.tmp`; no release/profile substitution.
- Documentation formatting, diff check, commit and normal push to main.

## Completion

- [ ] Runtime and dependency/model setup implemented and reviewed.
- [ ] Desktop defaults and custom override regressions verified.
- [ ] Real HTTP, lifecycle and UI verification completed with evidence.
- [ ] Working local service switched to GigaAM; previous models preserved.
- [ ] Release built/launched, docs current, changes committed/pushed.

Operational progress, blockers and acceptance are recorded in task
`sawe-gigaam-ctc-default-20261022`.
