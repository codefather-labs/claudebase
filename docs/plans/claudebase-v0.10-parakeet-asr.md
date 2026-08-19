# Parakeet as a second ASR backend

**Status:** DELIVERED as an opt-in backend. Operator decision 2026-08-19: whisper remains the
default and Parakeet is a self-build option, documented in the README. Slice 2 (installer support)
and Slice 3 (selection) are done; Slice 4 (a golden audio fixture) and the packaging question in R-2
remain open and are what a future default-flip would need.
**Target:** v0.10.0 (additive — a new backend, selectable at runtime)
**Supersedes:** `docs/issues/007-nvidia-parakeet-asr-backend.md`, whose load-bearing open question is
now answered.

## Why

Whisper-medium on this machine takes 37–70 seconds to transcribe a four-second note, depending on
what else is running. That is not a tuning problem — it was tuned, twice, and the measurements are in
the README. It is the model.

## The question that had to be answered first

Issue 007 parked this feature on one risk: the operator dictates in **Russian**, and the canonical
Parakeet models were English-only. Building an English-only backend would re-introduce the exact
"Russian transcribed as English" bug that `set_language("auto")` fixed in v0.8.1.

**Answered: `parakeet-tdt-0.6b-v3` is multilingual and includes Russian.** 25 European languages,
automatic language detection with no prompting, automatic punctuation and capitalisation, CC-BY-4.0
open weights.

## The shape

A second implementation of the existing `Asr` trait, behind the already-reserved `asr-sherpa`
feature, selected by `[asr] backend = "parakeet"` in daemon.toml. Whisper stays. Nothing about the
pipeline above the trait changes: parking in `pending_voice`, the worker, the concurrency permit and
the `[telegram_voice_message]` prefix are all backend-agnostic already.

**Runtime: sherpa-onnx, not NeMo.** NeMo is PyTorch and Python; this daemon is a single Rust binary
and the whole point of local ASR here is that nothing else has to be installed. sherpa-onnx runs the
ONNX export through ONNX Runtime, on CPU, and has Rust bindings whose build script fetches a
prebuilt library. The int8 export is ~670 MB on disk against whisper-medium's 1.5 GB.

**Not NIM.** NVIDIA's hosted endpoint would be the least work and the most wrong: it puts the
operator's dictated messages on someone else's machine, and voice notes are the most private thing
this system carries. The reserved `asr-nim` name stays reserved.

## Slices

### Slice 0 result (2026-08-19)

Measured with `examples/asr_ab.rs`, both backends over the same buffer, four
threads each, on this 16-core machine.

A 7.1-second Russian voice note from the operator, load ~2.5:

| | cold | warm | transcript |
|---|---|---|---|
| whisper-medium | 46.9 s | 45.7 s | «Так, это тест транскрипта голоса под паракет, новая модель.» |
| parakeet-tdt-v3 | 6.7 s | **0.65 s** | «Так, это тест транскрипта голоса под Паракит. Новую модель.» |

A 4-second synthetic tone, machine idle:

| | cold | warm | transcript |
|---|---|---|---|
| whisper-medium | 22.0 s | 21.0 s | `(electronic music)` |
| parakeet-tdt-v3 | 2.9 s | **0.17 s** | *(empty)* |

**70x on real speech, warm.** Both read the sentence correctly; they differ on
the transliteration of the product name («паракет» / «Паракит»), on a case
ending, and on where the sentence breaks. Neither reading is demonstrably wrong
without asking the speaker. On the non-speech tone Parakeet returning nothing is
the more honest answer than whisper's `(electronic music)`.

A 92.7-second Russian note dense with technical terms, load ~3.3:

| | cold | warm | vs realtime |
|---|---|---|---|
| whisper-medium | 128.3 s | 126.5 s | 1.36x SLOWER than the audio |
| parakeet-tdt-v3 | 16.0 s | **10.2 s** | 9x faster than the audio |

12x here rather than 70x, and the difference is structural: whisper always
processes 30-second windows, so a 7-second note costs a full window and a
92-second one costs four. The speedup is therefore largest on short notes, which
is what a dictated message usually is.

**On the technical strings — the ones that actually decide this — Parakeet is
not more accurate. It is differently wrong.**

| spoken | whisper | parakeet |
|---|---|---|
| harness | `harness` | `Харнес` |
| Claude Code | `клад кода` | `клоткода` |
| claudebase | `клад бейс` | `Cloud Base` |
| колбеки | `колбеки` | `callback` |
| loop engineering | `луп инжиниринг` | `loop engineering` |
| nvidia parakeet | `nvidia para kit` | `Nvidia Parkit` |
| **whisper ai medium** | `whisper ai medium` | **`whisper`** |

Neither model got a single product name right. Parakeet punctuates, capitalises
and splits sentences, where whisper returns one unbroken lowercase run — a large
readability win. But Parakeet DROPPED "ai medium" from the model name, and a
dropped phrase is worse than a mangled one precisely because it leaves no trace:
a wrong word is visible, an absent one is not.

**The speed question is answered. The accuracy question is answered differently
than expected: neither backend can be trusted on exact strings, which is already
what the channel contract tells agents.** The remaining
argument against Parakeet is the dropped phrase, and two samples cannot say
whether that is characteristic or a one-off.

A second thing blocks the default independently: the released binary does not
carry `asr-sherpa`, because shared linking means shipping ~31 MB of `.so`
beside it (R-2). Flipping the default is a release-pipeline decision as much as
an accuracy one.

**Slice 0 — measure before building.** Fetch the int8 export, transcribe a real Russian voice note
with `examples/whisper_probe.rs`'s sibling, and compare against whisper-medium on the same audio and
the same machine, at a comparable load. Two numbers decide whether this ships: wall time, and whether
the Russian text is actually right. A backend that is four times faster and subtly wrong on names and
identifiers is worse than the slow one, because the channel contract already tells agents to distrust
exact strings in dictated lines — and that instruction assumes the transcript is broadly correct.

**Slice 1 — the backend.** `src/daemon/asr/parakeet.rs` implementing `Asr`, behind `asr-sherpa`.
Model resolution, download-on-first-use and the `daemon doctor --asr` health check mirror the whisper
backend, which already has all three.

**Slice 2 — model fetch.** The installers already download `ggml-medium.bin`; they gain the Parakeet
export, and skip whichever backend is not configured rather than fetching both by default.

**Slice 3 — selection and defaults.** `[asr] backend = "parakeet" | "whisper"`. Whether Parakeet
becomes the default is a Slice-0 decision, not a Slice-3 one, and it needs the accuracy number, not
just the speed one.

**Slice 4 — a golden fixture.** `tests/fixtures/voice_sample.ogg` is currently a zero-byte stub, so
neither backend has ever been tested on real audio. A short Russian clip plus its expected transcript
turns "did the rewrite break transcription" from a question into a test.

## Risks

**R-1 (medium): the headline speed is a GPU number.** "3,333x realtime" and "6.34% average WER" come
from single-GPU deployment. This machine has an AMD integrated GPU and no CUDA, so the relevant
number is the CPU one, which Slice 0 measures rather than assumes.

**R-2 (medium): a native dependency with a downloading build script.** The `sherpa-onnx` crate
fetches a prebuilt library archive at build time unless `SHERPA_ONNX_LIB_DIR` is set. That is a new
network dependency in the build and a new binary in the release pipeline, on four platforms. The
feature stays non-default for the same reason `asr-whisper` is non-default.

**R-3 (low): two model downloads.** Whisper and Parakeet together are ~2.2 GB. The installers must
fetch only the configured one.

## Facts

### Verified facts
- Whisper-medium takes 37–70s for a 4s note on this machine depending on load — source: `examples/whisper_probe.rs` runs on 2026-08-18 and 2026-08-19 — salience: high
- The `Asr` trait and `make_asr` factory already reserve `"sherpa-nemo"` / `"nim"` and the `asr-sherpa` / `asr-nim` features — source: `src/daemon/asr/mod.rs:79-99`, `Cargo.toml` — salience: high
- Everything above the trait is backend-agnostic: parking, worker, permit, prefix — source: `src/daemon/telegram.rs` (this session's rewrite) — salience: medium
- `tests/fixtures/voice_sample.ogg` is 0 bytes — source: `ls -l` this session — salience: medium

### External contracts
- `nvidia/parakeet-tdt-0.6b-v3` — 25 European languages including Russian, auto language detection, CC-BY-4.0 — source: https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 and https://arxiv.org/pdf/2509.14128 (search results read 2026-08-19, model card not opened directly) — verified: partial — salience: high
- ONNX export exists — source: https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx and the sherpa-onnx conversion script at https://k2-fsa.github.io/sherpa/onnx/pretrained_models/offline-transducer/nemo-transducer-models.html — verified: partial — salience: high
- `sherpa-onnx` Rust crate wraps the C API; build script downloads a prebuilt library unless `SHERPA_ONNX_LIB_DIR` is set — source: https://docs.rs/sherpa-onnx/latest/sherpa_onnx/ — verified: partial — salience: high
- int8 export ≈670 MB disk, ≈2 GB RAM at inference — source: search result summary, not measured — verified: no — assumption — salience: medium

### Assumptions
- Russian accuracy is at least comparable to whisper-medium — risk: a faster backend that is subtly wrong on names, filenames and identifiers is worse than a slow correct one — how to verify: Slice 0, on a real Russian note — salience: high
- CPU inference is materially faster than whisper-medium here — risk: the whole feature buys nothing on this hardware — how to verify: Slice 0 — salience: high

### Open questions
- Does Parakeet become the default, or stay opt-in? — needs: Slice 0 numbers — salience: medium
- Does the release pipeline build `asr-sherpa` for all four platforms, or Linux first? — needs: architect call once R-2 is measured — salience: medium

## Decisions

### Inbound validation
- Asked for "nemotron instead of whisper, as a separate adapter, separate plan and release" — challenged: partly. Nemotron is NVIDIA's LLM family; the ASR model meant here is Parakeet, built with the NeMo toolkit, and the operator confirmed "паракит мультилингвел есть" — outcome: proceeded under the correct name, recorded so the plan is greppable — salience: medium
- "instead of whisper" is implemented as a second backend rather than a replacement — challenged: yes — outcome: whisper stays until Slice 0 proves Parakeet is both faster AND right in Russian; removing the working backend before measuring the new one would leave no way back — salience: high

### Decisions made
- sherpa-onnx over NeMo — alternatives: NeMo (Python + PyTorch, breaks the single-binary property), NIM (hosted, sends the operator's dictation off the machine) — Q1-Q5: hack? no | sane? yes | alternatives? listed | symptom-or-cause? cause, the model is the cost | root-cause-tracked? n/a — salience: high
- Measure before implementing (Slice 0) — the whole feature rests on two numbers nobody has yet — salience: high
- A real audio fixture lands with this work — the existing one is a zero-byte stub, so no transcription test has ever run on audio — salience: medium

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
