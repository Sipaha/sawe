# Voice conversations

Voice works above the existing Solution chat adapters, so Codex, Claude and Kimi
use the same microphone and speech controls. The default speech service is local
at `http://127.0.0.1:8095/v1`; a coding-provider subscription is not used for audio.

## Start the local engines

From the Sawe repository, install and launch the pinned local runtime:

```sh
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime setup --backend cuda --cuda-arch 120 --model small
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime serve --backend cuda --model small
```

Setup downloads whisper.cpp, a multilingual quantized model and a Russian Piper
voice. It does not install system packages. The runtime directory holds models,
its Python environment and caches; keep it when cleaning unrelated scratch files.
The `120` CUDA architecture above matches the verified RTX 5070 Laptop. Other
GPUs can use `--cuda-arch native`; use `--backend cpu` when CUDA is unavailable.
For the smaller base model, use `--model base` during setup and serve. The model
selected by the local server's launch command determines recognition; the API
model field is accepted for compatibility.

The service listens on loopback only. Models load on demand; Whisper releases
its model after 60 idle seconds. The microphone never starts automatically.
See [local runtime details](../findings/2026-10-local-voice-runtime.md) for pinned
revisions, limits, licensing and measured performance.

## Chat controls

- **Microphone:** start a recording; click again to finish and transcribe it.
  A recording is limited to 30 seconds. Silence and very short clicks are ignored.
- **Hands-free:** listen continuously, submit each phrase after a pause and enable
  read-aloud. Click again to end the voice session. Recognition is based on
  completed utterances, rather than live partial text.
- **Speaker:** toggle AI-generated read-aloud for newly completed replies.
- **Stop speaking:** cancel playback and pending speech. The coding agent keeps
  running; its existing Stop control remains a separate action.

In hands-free mode, the start of speech interrupts playback before recognition
finishes. Echo cancellation uses the same output mixer as application sounds.
Echo cancellation is unavailable on Windows GNU and FreeBSD. Noise-based voice
activity detection can also react to loud non-speech sounds.

Recognized text follows the ordinary send/queue path. If a typed draft, images or
an in-progress resume would be affected, recognition appends to the draft for
review instead of submitting it. Agent errors use the existing draft-restoration
path. Read-aloud speaks the last assistant message of each completed turn and
skips tool output, reasoning, fenced code and images. The complete text stays in
chat; speech is limited to 32,000 characters per reply and 12 queued chunks.

Changing the visible chat/Solution, hiding its dialog, rewinding history, clearing
context or closing the view ends its voice activity. Voice choices are not
persisted across application restarts. These actions are also available in the
command palette: `Toggle Voice Recording`, `Toggle Voice Conversation`,
`Toggle Read Aloud` and `Stop Speaking` in the Solution Agent namespace.

## Use another compatible speech service

Configure `solution_agent.voice` in Sawe settings. For example, an external
OpenAI-compatible service uses:

```json
{
  "solution_agent": {
    "voice": {
      "api_url": "https://api.openai.com/v1",
      "api_key_env": "SAWE_VOICE_API_KEY",
      "transcription_model": "gpt-4o-mini-transcribe",
      "speech_model": "gpt-4o-mini-tts",
      "speech_voice": "coral"
    }
  }
}
```

Provide the key through the named environment variable before launching Sawe;
never put it into settings JSON. `OPENAI_API_KEY` is a fallback only for the
official OpenAI origin. Full `transcription_url` and `speech_url` overrides allow
local recognition and a different synthesis service (whisper.cpp's own server
uses `/inference`). Keys never cross origins implicitly. Explicit
`transcription_api_key_env` / `speech_api_key_env` select separate credentials.
HTTPS is required except on loopback. Local audio connections bypass application
and environment HTTP proxies.
