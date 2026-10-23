# Voice conversations

Voice works above the existing Solution chat adapters, so Codex, Claude and Kimi
use the same microphone and speech controls. The default speech service is local
at `http://127.0.0.1:8095/v1`; a coding-provider subscription is not used for audio.

## Connect LAIHub

Sawe is an API client. SPK LAIHub owns model installation, inference processes,
CPU limits, memory retention and idle unloading. Start LAIHub from its repository:

```sh
./build/spk-laihub-browser --browser --listen 127.0.0.1:5195 --speech-listen 127.0.0.1:8095
```

The control panel is on port 5195; the dedicated speech listener on port 8095
matches Sawe's default endpoint. Install/import and manage models in LAIHub.
Select the Russian synthesis voice in LAIHub Settings; LAIHub owns that selection
for connected clients. Sawe never launches, loads, unloads or stops model workers. GigaAM returns
unpunctuated Russian; other languages require a compatible speech service.

While Hands-free is active, Sawe sends an activity ping every 30 seconds. LAIHub
alone decides what that activity means for model retention. Turning voice off,
transferring ownership or closing the chat stops pings. There are no model idle timers or
unload commands in Sawe. Keepalives do not load models or leave a permanent pin.
The microphone never starts automatically.

Custom speech endpoints remain configurable through `solution_agent.voice`.
The API connection is independent of the selected coding provider.

## Chat controls

- **Microphone:** start a recording; click again to finish and transcribe it.
  A recording is limited to 30 seconds. Silence and very short clicks are ignored.
- **Hands-free:** listen continuously, submit each phrase after a pause and enable
  read-aloud. Click again to end the voice session. Recognition is based on
  completed utterances, rather than live partial text.
- **Speaker:** toggle AI-generated read-aloud for new assistant prose as it arrives.
- **Stop speaking:** cancel playback and pending speech. The coding agent keeps
  running; its existing Stop control remains a separate action.

In hands-free mode, playback continues through unconfirmed microphone candidates.
Only after 120ms of voiced frames in a candidate does playback pause while
recognition validates it. Shorter discarded noise never pauses audio; empty
recognition and recognition errors resume the same audio cursor. Nonempty recognized input cancels the old
reply; Stop Speaking still cancels it immediately. Echo cancellation uses the
same output mixer as application sounds.
Echo cancellation is unavailable on Windows GNU and FreeBSD. Noise-based voice
activity detection can also react to loud non-speech sounds, and a recognizer may
occasionally turn noise into nonempty text. Such text is treated as voice input.

Recognized text is prefixed with `[audio]` so the coding agent can identify
voice input. Only the spoken fragment receives this marker when appended to a
typed draft. It follows the ordinary send/queue path. If a typed draft, images or
an in-progress resume would be affected, recognition appends to the draft for
review instead of submitting it. Agent errors use the existing draft-restoration
path. Read-aloud queues complete sentences while the assistant is streaming,
including progress commentary before tools. Tool boundaries and the final turn
event flush remaining prose. It skips tool output, reasoning, fenced code and
images. Short synthesis chunks avoid waiting for a whole reply; at most 12 chunks
are queued, with backpressure preserving the unspoken tail. Each assistant
message is limited to 32,000 source characters for speech. Stop Speaking or
confirmed voice input suppresses the old reply until new user input.

Hands-free stays attached to the original dialog when switching tabs, including
its recognition, reply playback and activity pings. Recognized text continues to
that original dialog, not the newly selected tab. Explicitly enabling Hands-free
in another dialog stops the previous owner before starting the new microphone.
Turning it off, closing the dialog or replacing its history/context ends it.
Ordinary recording and read-aloud without Hands-free still stop on navigation.
Per-chat voice toggles are not persisted across application restarts; the selected
Piper voice remains a persistent LAIHub setting. These actions are available in the
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
