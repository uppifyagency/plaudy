# Plaude Local — Agent Handoff

**For the next AI coding agent (or developer) picking this up.** This is the current operational entry‑point: state, how to build, what's already proven, what to do next, and the traps. Read this first, then [CODEBASE.md](CODEBASE.md) to understand the code and [HANDOFF-FASE2.md](HANDOFF-FASE2.md) for line‑cited Fase 2 forensics.

_Snapshot: 2026‑06‑22._

---

## 0. TL;DR

- **Plaude Local** = local‑first [Plaud](https://www.plaud.ai/) alternative (offline AI voice recorder + "who said what") for macOS, built on **[Handy](https://github.com/cjpais/Handy)** (Tauri 2). We extend the fork **in place** under `handy/`.
- **Fase 0 (mic long‑form sessions), Fase 1 (macOS system‑audio capture), Fase 2 (local diarization)** are **built and validated live**. 79 Rust unit tests green.
- The whole "who said what" chain works end‑to‑end and was tested on **1‑, 2‑, and 3‑speaker** recordings + silence; the two ONNX runtimes (diarizer + ASR) coexist with **zero crashes**.
- Repo is now on GitHub: **`uppifyagency/plaude-local` (private)**, branch `main`. Collaborator **gianni** invited (write, pending).
- **Biggest open task: the Sessions UI** — today sessions are driven only by CLI flags.

---

## 1. Build & run (this exact machine: Apple Silicon, CLT‑only, no Homebrew/Xcode)

Every build shell needs this prelude — the toolchain is installed but not on the non‑interactive PATH:

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # standalone CMake 4.x rejects pre‑3.5 policy floors
export HANDY_FORCE_AI_STUB=1              # CLT lacks @Generable macro plugin → Apple Intelligence stub

cd handy
bun install                # once
bun tauri dev              # full app (regenerates src/bindings.ts)
# backend only:  cd src-tauri && cargo build
# tests:         cd src-tauri && cargo test --lib   # 79 tests
# typecheck:     cargo check --lib                  # ~3s incremental
# lint:          bun run lint                        # i18n is enforced (no literal JSX strings)
```

Drop `HANDY_FORCE_AI_STUB` once **full Xcode** is installed (also needed for the iPhone target).

App data: `~/Library/Application Support/com.pais.handy/` (`history.db`, `recordings/`). Log: `~/Library/Logs/com.pais.handy/handy.log`.

---

## 2. What's done (don't redo)

| Area | State |
| --- | --- |
| Mic long‑form sessions | ✅ `managers/session.rs` — un‑gated capture → PCM → 16 kHz WAV → transcript |
| System audio (loopback) | ✅ `audio_toolkit/audio/system_audio.rs` — CoreAudio Process Tap, Audio‑Recording TCC |
| Diarization core `align()` | ✅ `managers/diarization.rs` — pure, 6 unit tests |
| Diarization engine | ✅ sherpa‑onnx, safe‑by‑default (no‑op without models) |
| ASR with timings | ✅ `transcribe_with_segments` in `managers/transcription.rs` |
| Finalize wiring | ✅ diarize → align → save (gated on `is_model_loaded()`) |
| Schema (migration #5) | ✅ `speakers` + `transcription_segments` + FK cascade |
| Timeline UI | ✅ `SpeakerTimeline` in `HistorySettings.tsx` |
| Model auto‑download | ✅ `download_diarization_models` + `is_diarization_available` commands |
| **Bundled models + auto‑install** | ✅ committed in `resources/models/diarization/`, installed on first run via `migrate_bundled_diarization_models` → **offline‑ready clone** |
| Live validation | ✅ 1/2/3‑speaker + silence + dual‑runtime coexistence, zero crashes (2026‑06‑22) |

Full detail + line citations: [HANDOFF-FASE2.md](HANDOFF-FASE2.md). Architecture + file‑by‑file map: [CODEBASE.md](CODEBASE.md).

---

## 3. What's next (prioritized)

1. **Sessions UI — the top gap.** Replace the CLI flags with a real view: start/stop button, Mic/System selector, live "recording" indicator. Backend: add a `start_system_session` command (today `commands/session.rs` hardcodes `Source::Mic`) or parameterize `start_session(source)`; add a `session-state-changed` event. Frontend: 3‑touch change (new `SessionsSettings.tsx` → export → `SECTIONS_CONFIG` entry). Detailed plan in [HANDOFF-FASE2.md §7](HANDOFF-FASE2.md).
2. **"Enable diarization" download button** — the command exists; it just needs a home in the Sessions view. (Bundling already covers fresh clones, so this is a fallback.)
3. **Clustering threshold tuning** — **only if** a rapid‑alternation recording is seen to over‑merge speakers. Defaults are validated good for long‑turn audio. Don't build pre‑emptively. Lever: `OfflineSpeakerDiarizationConfig` threshold / `num_clusters` in `diarization.rs`.
4. **Mic + system two‑track mux** — `ActiveRecorder` is either/or; capturing both sides of a hybrid meeting needs a summing stage.
5. **iPhone target** — no iOS upstream. Recommended: iPhone‑as‑capture + Mac‑as‑brain. Needs full Xcode.

---

## 4. Operational gotchas (cost real time during the last session — read these)

1. **The CLI toggle needs a running primary.** `handy --toggle-session` / `--toggle-system-session` only work as a *second* instance forwarding the flag to a live `bun tauri dev` (single‑instance plugin, `lib.rs` callback). With no primary running, it boots its own instance and **silently ignores the flag**. Symptom: no `*.session.pcm` appears.
2. **Capture taps the default output at session start.** If system audio is routed to headphones/Bluetooth or muted → the tap records **silence** → empty transcript (graceful fallback, not a bug). Verify you can hear the captured output. Quick check: read the live `*.session.pcm` (raw LE i16) amplitude.
3. **An ASR model must be resident at `finalize`.** Diarization+transcription only run when `is_model_loaded()`. Keep a model selected with `unload_timeout ≠ Immediately`, or warm it with one dictation first.
4. **Bindings export is dev‑only and now non‑fatal.** It used to **panic** (`PermissionDenied`) when the CLI forwarder ran from a read‑only CWD, swallowing the flag. Fixed to log‑and‑continue (`lib.rs` ~446) — the toggle works from any directory now.

---

## 5. Conventions (enforced — respect them)

- **Ponytail is active (level: full).** Lazy‑senior‑dev discipline: YAGNI → stdlib → native → existing dep → one line → minimum. Never cut validation, error handling, security, accessibility. Mark intentional shortcuts with a `ponytail:` comment naming the ceiling + upgrade path. Toggle with `/ponytail lite|full|ultra|off`.
- **nWave** waves/agents are available for backend/architecture work.
- **i18n is build‑blocking.** ESLint errors on literal JSX strings; every visible string must be `t("key")`. Add keys to `src/i18n/locales/en/translation.json`.
- **Migrations are append‑only.** Never edit a shipped `MIGRATIONS` entry in `history.rs` (corrupts the `user_version` chain). Append a new one.
- **Single source of truth for diarization filenames:** `DiarizationManager::{SUBDIR,SEG_FILE,EMB_FILE}` — the engine, the downloader, and the bundled‑install all reference these. Don't hardcode the names elsewhere.
- **Extend Handy's managers/pipeline; cite the real `handy/src-tauri/...` file** when proposing changes. Prefer the producer‑agnostic `chunk_sink` seam over net‑new capture paths.

---

## 6. Repo / git state

- **Remote:** `https://github.com/uppifyagency/plaude-local` (private), branch `main`. Collaborator `gianni` invited (write, pending acceptance).
- **Single repo:** the upstream `handy/.git` was **flattened** (removed) so our changes live in one repo. Upstream history is not preserved here; upstream = `cjpais/Handy`.
- **Models are committed** (`resources/models/diarization/*.onnx`, ~46 MB). They push over a slow link only because `git config http.postBuffer 524288000` is set locally — if you re‑clone elsewhere and push large objects, set it again (default buffer caused an HTTP 408).
- **`.gitignore`** excludes `target/` (~14 GB), `node_modules/`, `dist/`, and **stray `*.onnx` loose in `src-tauri/`** (defensive — only the `resources/models/diarization/` ones are tracked). If you download probe files, keep them out of `src-tauri/` root.
- Build artifacts are NOT committed — a fresh clone must `bun install` + build.

---

## 7. Quick verification recipe (after any change)

```bash
cd handy/src-tauri && cargo check --lib && cargo test --lib   # compiles + 79 tests
# live diarization smoke test (needs a 2+ voice audio source playing through the captured output):
#   1) bun tauri dev (leave running, keep an ASR model resident)
#   2) ./target/debug/handy --toggle-system-session   # start
#   3) ...play audio...                                # then same command to stop
#   4) sqlite3 -readonly ~/Library/Application\ Support/com.pais.handy/history.db \
#        "SELECT label FROM speakers WHERE history_id=(SELECT max(id) FROM transcription_history);"
```

---

*Created 2026‑06‑22. Keep this file current as the agent entry‑point; deep detail lives in [CODEBASE.md](CODEBASE.md) and [HANDOFF-FASE2.md](HANDOFF-FASE2.md).*
