# Issue 007 — Add NVIDIA Parakeet as a faster ASR backend (future)

**Status:** OPEN — parked for a future focused pass (operator request 2026-06-07)
**Area:** `src/daemon/asr/` (the `Asr` trait), `Cargo.toml` features `asr-sherpa` / `asr-nim`

## Goal

Add NVIDIA Parakeet as an ASR backend — substantially faster (and often more accurate) than
whisper-medium for the supported languages. The `Asr` trait + `make_asr` factory already reserve the
namespace (`asr-sherpa`, `asr-nim` return "not implemented in v1 — see Wave 6").

## ⚠️ Load-bearing open question (verify FIRST, before implementing) — Protocol 3

The operator's primary use is **Russian** voice notes. The canonical NVIDIA Parakeet models
(`parakeet-ctc`, `parakeet-tdt`, `parakeet-rnnt`) are historically **English-only**. If Parakeet is
fed Russian audio with an English model, it will mis-transcribe to English garbage — **re-introducing
the exact "transcribes Russian as English" problem v0.8.1 just fixed** (set_language auto).

Before building: confirm a **multilingual Parakeet variant that includes Russian** exists (e.g. a
newer `parakeet-tdt-*-v3` multilingual release or NVIDIA Canary), OR scope this as English-only and
keep whisper for Russian (runtime backend pick per language / per `[asr] backend`). This MUST be
resolved before any code — otherwise the feature regresses the operator's main use case.

## Backend options

- **sherpa-onnx (local ONNX)** — fits claudebase's local/no-API/single-binary ethos; needs the
  sherpa-onnx Rust binding + the Parakeet ONNX model download. Wire under the reserved `asr-sherpa`
  feature. Preferred if a Russian-capable ONNX export exists.
- **NVIDIA NIM (cloud HTTP)** — simplest integration (HTTP + `NVIDIA_API_KEY`) but breaks the
  local-first/offline property and adds a network dependency. Wire under reserved `asr-nim`.

## When picked up

Run through the pipeline (or the `ondemand-asr-integration-specialist`): PRD → architect (backend
choice + language matrix) → plan → implement behind the reserved feature flag → golden-fixture test
(Russian + English) → release. Selectable at runtime via `daemon.toml [asr] backend`.

## Facts

### Verified facts
- `make_asr` reserves `"sherpa-nemo"` and `"nim"` (return "not implemented in v1 — see Wave 6") — source: `src/daemon/asr/mod.rs` read this session — salience: high
- Cargo.toml features `asr-sherpa` / `asr-nim` exist (empty, reserved) — source: `Cargo.toml` read this session — salience: medium
- v0.8.1 fixed Russian-as-English via `set_language("auto")` — source: `src/daemon/asr/whisper.rs` + issue 006 — salience: high

### External contracts
- **NVIDIA Parakeet models** — language support (Russian?) — source: NVIDIA NGC / HuggingFace (NOT verified this session — operator parked before the web check) — verified: no — assumption — salience: high
- **sherpa-onnx** — Rust binding + Parakeet ONNX export availability — source: sherpa-onnx docs (not opened) — verified: no — assumption — salience: medium

### Assumptions
- Parakeet is English-focused; Russian support needs a specific multilingual variant — risk: building English-only Parakeet regresses the operator's Russian use — how to verify: web/NGC check at pickup — salience: high

### Open questions
- Does a Russian-capable Parakeet (or Canary) model exist for local ONNX use? — needs: external research at pickup — salience: high

## Decisions

### Inbound validation
- Operator asked to "connect Parakeet (faster than Whisper)" then parked it for later — challenged: yes (flagged the Russian-language regression risk) — outcome: parked as this issue with the language caveat front-and-center — salience: high

### Decisions made
- Park as a future pipeline pass rather than rush a backend that may not support Russian — Q1 hack? no | Q2 sane? yes | Q3 alternatives? build-now (rejected — unverified language fit) | Q4 cause? n/a | Q5 n/a — salience: high

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
