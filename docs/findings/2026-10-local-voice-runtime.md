# Local voice runtime

`script/voice-local` runs speech independently of the coding agent. It binds only
`127.0.0.1:8095` and exposes `/v1/audio/transcriptions` (multipart PCM16 mono
16 kHz WAV, 0.1–60 seconds) and `/v1/audio/speech` (JSON input, WAV output).
No API key is required. Browser-origin requests are rejected.

## Setup and launch

Keep runtime files and caches within the Solution. From the Sawe repository:

```sh
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime setup --backend cuda --cuda-arch 120 --model small
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime serve --backend cuda --model small
```

CUDA requires a working toolkit; `--cuda-compiler` defaults to
`/usr/local/cuda/bin/nvcc`. Use `--backend cpu` for both commands without CUDA;
`--model base` selects the smaller multilingual base Q5_1 model. Downloads occur
only during explicit `setup`. The script pins whisper.cpp v1.8.3 commit
`2eeeba56e9edd762b4b38467bab96c2517163158`, model repository revisions, model
SHA256 checksums and Piper 1.3.0 dependencies; `manifest.json` records downloads.
Piper's Russian `denis` medium model card declares a CC0 dataset. The Piper
engine is GPLv3; downloaded runtime artifacts are not committed or bundled.

## Resource and cancellation behavior

One request per engine runs at a time; same-engine overlap returns HTTP 429.
Transcription interrupts active synthesis and proceeds without waiting for it.
Requests are limited to
4 MiB, speech text to 4,000 characters, and inference to 120 seconds. Whisper
loads lazily, stays warm during conversation, and unloads after 60 idle seconds
(`--idle-seconds`). Piper uses four CPU threads by default and exits after each
utterance. SIGTERM/SIGINT stops owned model children. On Linux a parent-death signal also
prevents orphan model processes if the sidecar is killed abruptly. Canceling playback in the
app does not cancel the coding agent. Killing the owned sidecar also cancels its
inference. This is utterance-based recognition, not token-streamed transcription.

## Verification

`PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s script/tests -p test_voice_local.py -v`
checks HTTP bounds, busy/error responses, browser rejection, WAV validation,
model checksum validation, idle model release and process cleanup without models.
The live Russian benchmark uses a 6.91-second synthesized test utterance, not a
microphone or an accuracy corpus. Timings under concurrent compilation must not
be treated as idle-machine performance.
