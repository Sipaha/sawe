# Speech service ownership and historical measurements

Sawe now uses SPK LAIHub exclusively through a compatible speech API. Model
installation, inference workers, CPU limits, retention and idle unloading belong
to LAIHub. Sawe owns capture/playback and request cancellation, with activity
pings only while Hands-free is active. See the
[voice guide](../guides/voice-conversations.md) and
[ADR-0007](../architecture/decisions/0007-provider-independent-voice.md).

The default speech endpoint remains `http://127.0.0.1:8095/v1`. LAIHub can expose
it through its dedicated speech listener while its control panel stays on 5195.
Sawe does not require Python or bundle a local inference service. The removed
Python helper and its regression tests remain available in Git history through
commit `96fffc91c4`; installed model data is preserved independently of source.

## Historical GigaAM measurements

The former Python service at source `d473af2246` recognized the same 30 GOLOS
recordings with 3/152 word edits. Complete first HTTP requests took 3.9–5.0s;
subsequent recreated-worker reloads with cached files took 1.25–1.30s. Warm
requests took approximately 105–129ms. A 29.54s repeated-phrase recording took
0.99s, with about 473MiB peak worker RSS. These are historical, cache/workload
specific observations, not performance guarantees for LAIHub's native Go engines.
The [Russian model comparison](2026-10-russian-asr-comparison.md) remains a
small accuracy pilot, not a general corpus score.

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
