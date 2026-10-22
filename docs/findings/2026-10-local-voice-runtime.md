# Local voice runtime

`script/voice-local` runs speech independently of the coding agent. It binds only
`127.0.0.1:8095` and exposes `/v1/audio/transcriptions` (multipart PCM16 mono
16 kHz WAV, 0.1–60 seconds) and `/v1/audio/speech` (JSON input, WAV output).
No API key is required. Browser-origin requests are rejected.

## Setup and launch

Keep runtime files and caches within the Solution. From the Sawe repository:

```sh
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime setup
script/voice-local --runtime-dir /home/spk/.spk/sawe/ss/Sawe1/.tmp/voice-runtime serve
```

The default is Russian GigaAM v3 CTC INT8 on CPU, using Python 3.11+ with a venv.
Setup installs ONNX ASR 0.12.0, ONNX Runtime 1.31.0 and pinned dependencies.
It downloads the MIT-licensed `istupakov/gigaam-v3-onnx` conversion at revision
`322c3b29492673eb7d0b434bfa9dfb8653e34d02`, checking model/config/vocabulary and
attribution hashes. It does not need CUDA, Whisper, PyTorch or a Hub client.
Serve opens only explicit local files and never downloads models or packages.

Explicit `--model small` or `--model base` retains multilingual Whisper Q5_1.
For CUDA add `--backend cuda` during setup and serve; CUDA setup requires a
working toolkit, with `--cuda-compiler` defaulting to `/usr/local/cuda/bin/nvcc`
and `--cuda-arch native`. The optional Whisper source is pinned to v1.8.3 commit
`2eeeba56e9edd762b4b38467bab96c2517163158`. `manifest.json` records downloaded
revisions and hashes; existing Whisper files remain when setting up GigaAM.

Piper 1.3.0 remains a separate CPU synthesis process.
Piper's Russian `denis` medium model card declares a CC0 dataset. The Piper
engine is GPLv3; downloaded runtime artifacts are not committed or bundled.

## Resource and cancellation behavior

One request per engine runs at a time; same-engine overlap returns HTTP 429.
Transcription interrupts active synthesis and proceeds without waiting for it.
Requests are limited to
4 MiB and speech text to 4,000 characters. GigaAM startup and inference IPC share
a 120-second request budget; startup has an additional 60-second cap. Framed
WAV/JSON pipes bound response size and both nonblocking writes and reads.
Invalid responses, EOF and timeout reset/reap the worker for the next request.

Recognition loads lazily and unloads after 3,600 idle seconds (`--idle-seconds`).
While Hands-free is active, Sawe calls `/v1/audio/keepalive` with an empty JSON
object immediately and every 30 seconds. This refreshes activity without loading
the model, interrupting Piper or taking the STT request lock. Stop/navigation/
close cancels the client task; a client crash cannot leave a permanent pin.
The extension is limited to eligible loopback service URLs, skips explicit STT
overrides and quietly disables unsupported endpoints. `/health` reports model,
engine and idle interval without loading or keeping the model active.

GigaAM accepts `ru` or `auto`; `en` is rejected explicitly. Whisper keeps its
multilingual options; CUDA uses beam three and CPU uses greedy decoding. Piper uses
four CPU threads by default and exits after each
utterance. SIGTERM/SIGINT stops owned model children. On Linux a parent-death signal also
prevents orphan model processes if the sidecar is killed abruptly. A persistent
owner thread launches children: Linux parent-death signals track the spawning
thread, so launching from a disposable HTTP thread would unload a warm model.
The expected parent PID is passed before spawn and checked before/after installing
the guard. Expired launch futures also reap a child delivered during cancellation.
Canceling playback in the
app does not cancel the coding agent. Killing the owned sidecar also cancels its
inference. This is utterance-based recognition, not token-streamed transcription.

## Verification

`PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s script/tests -p test_voice_local.py -v`
checks HTTP bounds, busy/error responses, browser rejection, WAV validation,
model checksums/offline setup, worker framing/timeouts/recovery, idle release,
keepalive and process cleanup without model downloads. Real fresh-setup HTTP
verification preserved the 3/152 word edits on the same 30 GOLOS recordings as
the [model comparison](2026-10-russian-asr-comparison.md).

Under concurrent release compilation, complete first GigaAM HTTP requests took
3.9–5.0 seconds, subsequent worker reloads about 2.1 seconds, and warm requests
about 105–129 ms. These include Python/process/ORT startup; the earlier isolated
model load plus first inference was about one second. After compilation, full
worker-cold requests took 4.34 seconds initially and 1.25–1.30 seconds on later
reloads with cached files. These are different startup/cache conditions, not a
one-second initial service startup guarantee. A 29.54-second recording
completed in 0.99 seconds with about 473 MiB peak worker RSS; short recordings
used about 437 MiB plus a 31 MiB idle sidecar. No GPU is used for recognition.

Actual isolated UI checks used real CPAL/PipeWire, GigaAM and Piper with the
installed Codex in a separate throwaway project: typed drafts survived dictation, recognized speech followed the
ordinary send path, and voice input stopped playback within 0.528 seconds of the
test waveform starting. Virtual microphone buses were primed/routed before
playback; this is not a physical microphone or acoustic AEC accuracy claim.
Immediate/30-second keepalives and cancellation were observed, with the same
model retained beyond the old one-minute timeout. A real hour was not waited;
short-TTL and controlled-clock tests exercise expiry. Actual release startup
and dictation preserved the typed draft and reached the ordinary send path.
The intended PATH fixture was not used because login-shell environment loading
selected the installed Codex; the child executable was checked through `/proc`.
The main editor was preserved and needs a normal restart for the new client.

## Earlier Whisper measurements

The following historical measurements use a 6.91-second synthesized utterance,
not a microphone or accuracy corpus. Timings under concurrent compilation must
not be treated as idle-machine performance.

Measured on AMD Ryzen 7 260 / RTX 5070 Laptop 8 GB, four CPU threads:

| Model/backend             | Warm request, 6.91s audio | Sampled RSS | GPU memory |
| ------------------------- | ------------------------- | ----------- | ---------- |
| base Q5_1 / CPU           | 4.87–6.04s                | 159 MiB     | —          |
| small Q5_1 / CPU          | 11.70–26.81s              | 352 MiB     | —          |
| base Q5_1 / CUDA, beam 3  | 0.10–0.11s                | 401 MiB     | 404 MiB    |
| small Q5_1 / CUDA, beam 3 | 0.16–0.17s                | 398 MiB     | 642 MiB    |

CPU runs overlapped compilation. CUDA small model load took 1.51s after driver
initialization; first CUDA initialization in the earlier run took 6.73s. Piper
synthesized the 6.91s test phrase in 2.90s with 206 MiB peak RSS, including cold
Python/model startup. Small recognized the phrase better than base, but still
made word errors; this sample does not establish microphone accuracy. Live HTTP
checks verified cold/warm recognition, WAV speech, and actual idle process reap.
