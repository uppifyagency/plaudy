# Plaude Local

> Open-source, **local-first alternative to [Plaud](https://www.plaud.ai/)** (the AI voice‑recorder / meeting note‑taker) for **macOS** (iPhone planned).
> Everything — capture, transcription, and speaker diarization ("who said what") — runs **on your machine**. No cloud, no account, your audio never leaves the device.

Built as a fork/extension of **[Handy](https://github.com/cjpais/Handy)** (a Tauri 2 push‑to‑talk dictation app), which we use as scaffolding and extend in place to add long‑form recording, system‑audio capture, and local diarization.

---

## What it does today

| Capability | Status | How |
| --- | --- | --- |
| **Push‑to‑talk dictation** (inherited from Handy) | ✅ upstream | Global shortcut → VAD → Whisper/Parakeet → paste |
| **Long‑form mic "sessions"** (multi‑hour, un‑gated) | ✅ built + demoed | `managers/session.rs`, streamed to disk as PCM → WAV → transcript |
| **System / loopback audio capture** (record what plays through the Mac) | ✅ built + demoed | CoreAudio **Process Tap** in `audio_toolkit/audio/system_audio.rs` |
| **Speaker diarization** — "who said what" | ✅ built + validated live | Local **sherpa‑onnx** (pyannote + TitaNet) over the saved WAV in `finalize` |
| **Offline‑ready models** (bundled, no download for a fresh clone) | ✅ | Diarization models in `resources/models/diarization/`, auto‑installed on first run |
| **Sessions UI** (start/stop button + Mic/System selector) | ⏳ next | Today driven by CLI flags only — see [docs/HANDOFF-FASE2.md](docs/HANDOFF-FASE2.md) §7 |
| **iPhone capture** | 🔭 deferred | Recommended path: iPhone‑as‑mic + Mac‑as‑brain (needs full Xcode) |

**Live‑validated (2026‑06‑22):** 1‑, 2‑, and 3‑speaker recordings each separated correctly; silence handled gracefully; the two ONNX runtimes (diarizer + ASR) coexist with zero crashes. See the [handoff](docs/HANDOFF-FASE2.md) for the forensic detail.

---

## Quick start (Apple Silicon, this is the verified setup)

This machine builds **without Homebrew and without full Xcode** (Command Line Tools only). The toolchain (Rust, Bun, standalone CMake) is installed but **not on the non‑interactive PATH**, so every build shell needs the prelude below.

```bash
# 1. Toolchain on PATH + the two required escape hatches
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # standalone CMake 4.x rejects pre‑3.5 policy floors in native deps
export HANDY_FORCE_AI_STUB=1              # CLT lacks the @Generable macro plugin → use the Apple Intelligence stub

# 2. Install JS deps (once) and run
cd handy
bun install
bun tauri dev                              # dev: hot‑reload frontend + Rust backend

# Backend only:  cd handy/src-tauri && cargo build
# Tests:         cd handy/src-tauri && cargo test --lib   # 79 unit tests
```

- **Drop `HANDY_FORCE_AI_STUB` once full Xcode is installed** (also required later for the iPhone target) — then the real Apple Intelligence Swift bridge compiles.
- App data lives at `~/Library/Application Support/com.pais.handy/` (`history.db` + `recordings/`).
- Full platform notes: [handy/BUILD.md](handy/BUILD.md) and [handy/AGENTS.md](handy/AGENTS.md).

### Driving a session today (no UI yet)

Sessions are toggled by launching a second instance of the binary while `bun tauri dev` is running (it forwards the flag via the single‑instance plugin):

```bash
# mic session
"handy/src-tauri/target/debug/handy" --toggle-session
# system‑audio session (record a call / video / podcast)
"handy/src-tauri/target/debug/handy" --toggle-system-session
```

Run the same command again to stop. The transcript (with a speaker‑labelled timeline if 2+ voices) appears in **History**.

---

## Architecture at a glance

```
                    ┌─────────────── capture seam (producer‑agnostic) ───────────────┐
   mic  (cpal) ─────┤                                                                 │
                    │   AudioChunk::Samples ──► chunk_sink (faithful, un‑VAD‑gated)   │
 system (CoreAudio  ┤                                                                 │
  Process Tap) ─────┘                                                                 │
                                                   │
                                                   ▼
                          session.rs:  stream to *.session.pcm  ──(stop)──►  16 kHz mono WAV
                                                   │
                                                   ▼  finalize() (off‑thread)
                    diarize (sherpa) ─┐                                  ┌─ save_entry (flat transcript)
                                      ├─► align (max temporal overlap) ──┤
        transcribe_with_segments ─────┘   "who said what"                └─ save_segments (speakers + segments)
                                                   │
                                                   ▼
                              History UI  ►  SpeakerTimeline  ("Speaker N · mm:ss · text")
```

The key reuse insight: **mic and system audio feed the *same* consumer** via a `chunk_sink`, so there is no parallel pipeline — any future source just emits `AudioChunk::Samples` + one `EndOfStream`.

For the full picture (managers, command/event flow, single‑model residency, the data model) read **[docs/CODEBASE.md](docs/CODEBASE.md)**.

---

## Repository map

```
.
├── README.md                     ← you are here (landing page / index)
├── CLAUDE.md                     ← agent operating instructions for this repo
├── docs/
│   ├── CODEBASE.md               ← extensive technical documentation (start here to understand the code)
│   ├── HANDOFF-FASE2.md          ← forensic engineering handoff (what/how/remains, line‑cited)
│   └── handy-architecture/       ← forensic docs of upstream Handy + the Plaud gap analysis
├── handy/                        ← the app (our fork of Handy — we extend it IN PLACE)
│   ├── src-tauri/                ← Rust backend (Tauri 2)
│   │   ├── src/managers/         ← session.rs, diarization.rs, model.rs, transcription.rs, history.rs …
│   │   ├── src/audio_toolkit/    ← capture: recorder.rs, system_audio.rs, VAD
│   │   ├── src/commands/         ← Tauri command handlers (session.rs, models.rs, history.rs …)
│   │   └── resources/models/     ← bundled models (VAD + diarization seg/emb, auto‑installed on first run)
│   └── src/                      ← React/TS frontend (settings shell + History timeline)
├── .claude/                      ← project‑local Claude Code config + Ponytail install
└── .nwave/                       ← nWave methodology config
```

---

## Documentation index

| Doc | Read it for |
| --- | --- |
| **[docs/CODEBASE.md](docs/CODEBASE.md)** | The complete technical reference: architecture, what we built across Fase 0/1/2 and **how**, file‑by‑file map of our changes, the data model, build‑system specifics, and what remains. **New devs start here.** |
| **[docs/HANDOFF-FASE2.md](docs/HANDOFF-FASE2.md)** | Line‑cited forensic state of the diarization work, the build de‑risk spikes, and the detailed plans for the Sessions UI. |
| **[docs/handy-architecture/](docs/handy-architecture/)** | How upstream Handy works (capture pipeline, VAD, transcription engines, model management, persistence) + the original Plaud gap analysis. |
| **[CLAUDE.md](CLAUDE.md)** / **[handy/AGENTS.md](handy/AGENTS.md)** | Conventions, build commands, i18n rules, and how AI agents should work in this repo. |

---

## What's next (for the incoming developer)

1. **Sessions UI** — replace the CLI flags with a real view: a start/stop button, a Mic/System selector, and a live "recording" indicator. Backend command + 3‑touch frontend change. Plan in [HANDOFF §7](docs/HANDOFF-FASE2.md).
2. **A "Enable diarization" download button** — the `download_diarization_models` command exists and works; it just needs a UI home (the Sessions view). Models are also bundled now, so this is a fallback for non‑bundled builds.
3. **Clustering threshold tuning** — only if a rapid‑alternation recording is ever seen to over‑merge speakers. Defaults are good for long‑turn conversations (validated). Don't build pre‑emptively.
4. **iPhone target** — Handy has no iOS support today. Recommended: iPhone as a capture client streaming to the Mac "brain". Needs full Xcode.

---

## Credits & license

- Built on **[Handy](https://github.com/cjpais/Handy)** by CJ Pais et al. (MIT). Upstream architecture docs are mirrored under `docs/handy-architecture/`.
- Diarization uses **[sherpa‑onnx](https://github.com/k2-fsa/sherpa-onnx)** (k2‑fsa) with pyannote‑segmentation‑3.0 + NeMo TitaNet‑small models.
- This project inherits Handy's **MIT** license. See [handy/LICENSE](handy/LICENSE).

> **Private handoff repo** — this is a working snapshot passed between developers, not a public release. The git history of the upstream Handy clone is intentionally not preserved here; upstream lives at the link above.
