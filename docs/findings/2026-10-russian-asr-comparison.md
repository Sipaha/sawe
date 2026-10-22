# Russian ASR model screening

Measured 2026-10-22 on Ryzen 7 260 / RTX 5070 Laptop, using the same first
30 GOLOS crowd test WAVs (118.638 seconds, 152 reference words). Original
SHA-256 inputs and all 210 results across seven engines were checked.
Scoring was independently cross-checked with a full-matrix edit distance.
Operational evidence, scripts and full transcripts are attached to task
`sawe-russian-whisper-comparison-20261022`.

| Local model                       | Strict WER | Integer-normalized WER | Warm median | Process RAM |     VRAM |
| --------------------------------- | ---------: | ---------------------: | ----------: | ----------: | -------: |
| GigaAM v3 CTC INT8, CPU           |      1.97% |                  1.97% |       85 ms |     429 MiB |        0 |
| T-one greedy, CPU                 |      5.26% |                  5.26% |      169 ms |     664 MiB |        0 |
| GigaAM v3 E2E RNN-T INT8, CPU     |      9.87% |                  5.92% |       97 ms |     419 MiB |        0 |
| Whisper large-v3-turbo Q5_0, CUDA |     11.18% |                  6.58% |      135 ms |     482 MiB |  996 MiB |
| Current Whisper small Q5_1, CUDA  |     29.61% |                 26.32% |       78 ms |    ~398 MiB | ~642 MiB |
| Whisper base Q5_1, CUDA           |     50.66% |                 48.03% |       43 ms |     440 MiB |  410 MiB |

Digit formatting is useful output, not an application defect. Literal WER
penalizes `60` versus `шестьдесят`. The separate integer metric expands values
below one million and grouped thousands; it does not normalize all ordinal
forms, cases, dates, fractions or acronyms. Keep both scores and original text.

GigaAM CTC was the best candidate in this sample, with 3 edits and 27/30 exact
phrases, without a GPU. It emits unpunctuated lowercase Russian. E2E adds
punctuation and numeric formatting. T-one used its official greedy pipeline
with 16→8 kHz preprocessing; its separate 5.09 GiB KenLM beam decoder was not
tested. Turbo is multilingual and substantially improves on small.

GigaAM used the MIT-licensed `istupakov/gigaam-v3-onnx` conversion, revision
`322c3b29492673eb7d0b434bfa9dfb8653e34d02`, ONNX ASR 0.12.0 / ORT 1.31.0,
CPUExecutionProvider with four threads. T-one source was
`3c5b6c015038173840e62cea99e10cdb1c759116`, model revision
`106f3b0b32a9e107eb613312e4ebc61ff3d53926` (Apache-2.0).
Whisper used the existing whisper.cpp 1.8.3 CUDA build, Russian, beam three.
All downloaded model hashes were checked; no production settings changed.

Warm times exclude loading, microphone capture, VAD and agent response.
Whisper includes loopback HTTP; ONNX uses direct WAV-to-text calls. Small's
memory figures are earlier runtime samples. CPU benchmarks were serialized;
the network voice test sometimes overlapped. This short command corpus is
not spontaneous developer dictation; model training exposure is unknown.

[Subscribed Codex Voice](2026-10-codex-subscription-voice.md) also worked without
an API key. Its readiness-gated continuous test yielded 29/152 edits (19.08%),
including three empty clips. RTP was sent for every clip; the cause of omissions
is not localized. This includes network/VAD/turn handling and cannot be assigned
to an exposed Whisper model. Keep it separate from standalone ASR inference.

Sources: [GOLOS and its license](https://github.com/sberdevices/golos),
[GigaAM](https://github.com/salute-developers/GigaAM),
[ONNX conversion](https://huggingface.co/istupakov/gigaam-v3-onnx),
[T-one](https://github.com/voicekit-team/T-one),
[Whisper turbo](https://huggingface.co/openai/whisper-large-v3-turbo).
