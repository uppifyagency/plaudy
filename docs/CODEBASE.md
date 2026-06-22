# Plaude Local — Codebase Documentation

The complete technical reference for the project: the architecture, **what we built and how**, a file‑by‑file map of our changes, the data model, the build system, and what remains. If you are the incoming developer, read this top to bottom once, then keep [HANDOFF-FASE2.md](HANDOFF-FASE2.md) open for line‑cited detail.

> **Mental model in one sentence:** we took Handy's short‑dictation pipeline (`capture → VAD → transcribe → paste`) and added a *second* path for **long‑form recording** that taps the same capture seam, streams hours of audio to disk, and on stop runs an offline **transcribe + diarize ("who said what")** pass before writing a speaker‑labelled transcript to History.

---

## 1. Tech stack

| Layer | Tech |
| --- | --- |
| Shell | **Tauri 2** (Rust core + system WebView) |
| Backend | **Rust** (`handy/src-tauri/`) |
| Frontend | **React + TypeScript + Tailwind**, Zustand store, `bun` (`handy/src/`) |
| Type bridge | **tauri‑specta** — Rust commands/events → generated `src/bindings.ts` |
| ASR | **transcribe‑rs** (Whisper.cpp + Parakeet/Moonshine/… ONNX via `ort`) |
| Diarization | **sherpa‑onnx 1.13.3** (`shared` feature) — pyannote segmentation + speaker embedding + clustering |
| VAD | Silero VAD (`silero_vad_v4.onnx`) |
| Audio I/O | `cpal` (mic), **CoreAudio Process Tap** (system audio, macOS), `rubato` (resample) |
| Persistence | **SQLite** (`rusqlite` + `rusqlite_migration`), `history.db` |

---

## 2. Top‑level architecture

### Manager pattern + command/event
Core logic lives in **managers** constructed at startup and held in Tauri managed state ([lib.rs](../handy/src-tauri/src/lib.rs)):

- `AudioRecordingManager` — device + recording lifecycle for dictation.
- `ModelManager` ([managers/model.rs](../handy/src-tauri/src/managers/model.rs)) — model catalog, download/verify/extract, **bundled‑model install**.
- `TranscriptionManager` ([managers/transcription.rs](../handy/src-tauri/src/managers/transcription.rs)) — single resident ASR model, idle‑unload watcher.
- `HistoryManager` ([managers/history.rs](../handy/src-tauri/src/managers/history.rs)) — `history.db`, including the Fase 2 segments/speakers overlay.
- `SessionManager` ([managers/session.rs](../handy/src-tauri/src/managers/session.rs)) — **our** long‑form recording lifecycle.

Frontend → backend via **commands** (`commands/*.rs`); backend → frontend via **events** (e.g. `historyUpdatePayload`, `model-download-progress`). All typed through `tauri-specta` into [src/bindings.ts](../handy/src/bindings.ts).

### The two pipelines
- **Dictation (upstream):** `capture → VAD gate → resample → transcribe → post‑process → paste + history`. Short, latency‑sensitive.
- **Long‑form session (ours):** `capture → faithful tap → stream to disk → (stop) → WAV → diarize + transcribe → speaker‑labelled history`. Multi‑hour, throughput‑oriented.

---

## 3. The capture seam — the central reuse

Both pipelines, and both audio sources, feed **one** consumer through a `chunk_sink`:

- `AudioRecorder::with_chunk_sink(sink)` ([recorder.rs](../handy/src-tauri/src/audio_toolkit/audio/recorder.rs)) installs a faithful tap.
- In `handle_frame`, **if a `chunk_sink` is present every frame is forwarded verbatim and the function early‑returns before the VAD/dictation accumulator** — this is the bounded‑RAM fix that makes multi‑hour capture possible (the dictation accumulator would otherwise grow without bound).
- Frames flow as `AudioChunk::Samples(Vec<f32>)`; the producer signals end with a single `AudioChunk::EndOfStream`.

**Consequence:** system‑audio capture did *not* need a parallel pipeline. The CoreAudio IOProc downmixes to mono `f32` and pushes into the *same* channel the cpal mic callback uses. `session.rs` picks the producer behind an `ActiveRecorder { Mic | System }` enum; the PCM→WAV→transcribe→history *tail* is shared.

---

## 4. Fase 0 — long‑form mic sessions

**File:** [managers/session.rs](../handy/src-tauri/src/managers/session.rs).

- `SessionManager` holds `app`, `recordings_dir`, `active: Mutex<Option<ActiveSession>>`.
- Lifecycle: `toggle(source)` → `start(source)` / `stop()`, plus `recover_interrupted()` at boot.
- Capture is **un‑VAD‑gated** (every frame incl. silence), streamed to disk as **raw little‑endian i16 PCM** (`*.session.pcm`), flushed per frame so a crash loses <30 ms.
- On stop, `finalize()` runs **off‑thread**: read PCM→f32 (`chunks_exact(2)` so a torn trailing byte is dropped, not panicked) → write a mono **16 kHz WAV** → best‑effort transcribe **only if a model is resident** → write one History row → delete the PCM.
- **Crash recovery:** `recover_interrupted()` finalizes any orphan `*.session.pcm` at startup (before models load), so a recovered session keeps its audio (empty transcript, by design).
- **Path‑collision safety:** session ids are `session-{millis}-{seq}` (`AtomicU64`), so two sessions in the same millisecond can't clobber each other.

---

## 5. Fase 1 — system / loopback audio (macOS)

**File:** [audio_toolkit/audio/system_audio.rs](../handy/src-tauri/src/audio_toolkit/audio/system_audio.rs), gated `cfg(all(target_os = "macos", target_arch = "aarch64"))`.

`SystemAudioRecorder` captures everything the Mac plays via the CoreAudio **Process Tap** API (macOS 14.4+):

- Global mono `CATapDescription` → `AudioHardwareCreateProcessTap` → a private **aggregate device** → a realtime **IOProc block** that downmixes f32 to mono and pushes `AudioChunk::Samples` into the shared `chunk_sink`.
- Uses the **Audio‑Recording TCC permission** (`NSAudioCaptureUsageDescription`), *not* Screen Recording — so **no purple banner**.
- Hardening present in code: format validation (asserts LinearPCM/float/32‑bit before casting the buffer), the **`EndOfStream`‑once** emission on stop (mirrors the cpal callback so the consumer's drain doesn't fall back to a 2 s timeout and truncate the tail), and an immediate `drop(block)` after `AudioDeviceCreateIOProcIDWithBlock` so the recorder is `Send` (required to live in Tauri state).

objc2 deps for this live under the macOS‑aarch64 target block in `Cargo.toml`.

---

## 6. Fase 2 — speaker diarization ("who said what")

This is the bulk of our work. Four pieces, built **domain‑first** (pure logic tested in isolation, then adapters around it).

### 6.1 Pure core — `align()` ([managers/diarization.rs](../handy/src-tauri/src/managers/diarization.rs))
Types `SpeakerTurn`, `AsrSegment`, `TimedSegment` + `align(asr, turns)`: assign each ASR segment the speaker whose turn overlaps it **most** (deterministic tie‑break to the lower speaker id; no overlap → `speaker_id: None` = graceful "unknown"). No I/O, no engine — **6 unit tests**.

### 6.2 Engine adapter — `DiarizationManager`
Wraps `sherpa_onnx::OfflineSpeakerDiarization` (cfg‑gated like the system recorder). Loads `segmentation.onnx` (pyannote‑3.0) + `embedding.onnx` (NeMo TitaNet‑small) from `<app_data>/models/diarization/` and runs an offline pass over the 16 kHz mono samples → `Vec<SpeakerTurn>` (ms).

- **Safe‑by‑default:** `diarize()` no‑ops unless both model files exist, so the default install behaves exactly as before and sherpa's onnxruntime is only ever initialized when diarization actually runs.
- The shared consts `DiarizationManager::{SUBDIR, SEG_FILE, EMB_FILE}` are the **single source of truth** for filenames — the downloader, the bundled‑install, and the engine all reference them so they cannot drift (guarded by a unit test).

### 6.3 ASR with timings — `transcribe_with_segments` ([transcription.rs](../handy/src-tauri/src/managers/transcription.rs))
`TranscriptionManager::transcribe()` throws ASR segments away. Rather than duplicate ~290 lines, the body was refactored into a private `transcribe_inner` returning `(String, Vec<AsrSegment>)`; `transcribe` and `transcribe_with_segments` are one‑line wrappers (the delicate `catch_unwind`/engine block is untouched). `to_asr_segments` converts transcribe‑rs seconds → ms at that single boundary.

### 6.4 Finalize wiring ([session.rs](../handy/src-tauri/src/managers/session.rs))
Gated on `tm.is_model_loaded()`, `finalize` now: **diarize first** (borrows `samples`) **then** `transcribe_with_segments` (consumes them — avoids cloning a multi‑hour buffer) → `align` → `save_entry` (flat canonical transcript) **+** `save_segments` (speakers + segments). If ASR yields no segments it skips the overlay; startup recovery (no model) skips the whole pass. **78→79 lib tests green.**

### 6.5 Read side + UI
- `get_session_segments(history_id) -> Vec<PersistedSegment>` ([commands/history.rs](../handy/src-tauri/src/commands/history.rs)), registered in `collect_commands!`.
- `SpeakerTimeline` in [HistorySettings.tsx](../handy/src/components/settings/history/HistorySettings.tsx) renders `Speaker N · mm:ss · text` per segment when an entry has segments, else the existing flat `<p>` (canonical `transcription_text` preserved).

### 6.6 Models: download **and** bundle
- **Auto‑download:** `ModelManager::download_diarization_models()` ([model.rs](../handy/src-tauri/src/managers/model.rs)) fetches the two **bare `.onnx`** files (HuggingFace mirror for segmentation, k2‑fsa GitHub release for embedding), SHA256‑pinned, into `<app_data>/models/diarization/`. Bare files mean the existing file‑based download/verify path is reused — **no `.tar.bz2`/bzip2 branch, no entry in the ASR catalog, no `EngineType::Diarizer`**. Exposed as commands `download_diarization_models` + `is_diarization_available`.
- **Bundled + auto‑install (for fresh clones):** the models are committed under `handy/src-tauri/resources/models/diarization/` (covered by `bundle.resources = ["resources/**/*"]`). `ModelManager::migrate_bundled_diarization_models()` copies them into `<app_data>/models/diarization/` on first run (mirrors `migrate_bundled_models`). So **clone → build → run → diarization works offline**, no download.

> **Why diarization is a *separate* slot, not the ASR model slot:** `TranscriptionManager` keeps exactly one resident ASR model with an idle‑unload watcher. Loading a diarizer into that slot would evict the user's ASR model and thrash the watchers. `DiarizationManager` has its own lifecycle; in `finalize` we diarize then transcribe sequentially.

> **The dual‑onnxruntime question (now closed):** `ort` (ASR) static‑links onnxruntime; sherpa would too in `static` mode → duplicate‑symbol link error. The fix is `sherpa-onnx { default-features = false, features = ["shared"] }` (dylib) + an `@loader_path` rpath in `.cargo/config.toml`. Two runtimes co‑loading *and initialized at once* was the last unknown — **live‑validated across 5 finalizes with zero crashes** (2026‑06‑22).

---

## 7. Data model — `history.db`

Migrations live in [history.rs](../handy/src-tauri/src/managers/history.rs) as an append‑only `MIGRATIONS` list (tracked by SQLite's `user_version`; **never edit a shipped migration**). Migration **#5** (ours) adds the diarization overlay:

```sql
CREATE TABLE speakers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  label TEXT NOT NULL,            -- "Speaker N", 1‑based, first‑seen order, per history row
  embedding BLOB
);
CREATE TABLE transcription_segments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  start_ms INTEGER NOT NULL,
  end_ms INTEGER NOT NULL,
  speaker_id INTEGER REFERENCES speakers(id) ON DELETE SET NULL,   -- NULL = unknown speaker
  text TEXT NOT NULL,
  confidence REAL
);
CREATE INDEX idx_segments_history ON transcription_segments(history_id);
```

- The flat `transcription_history.transcription_text` stays the **canonical** full transcript (copy/retry keep working); segments are a structured **overlay**.
- `ON DELETE CASCADE` requires `PRAGMA foreign_keys = ON` **per connection** — `get_connection` now sets it (cascade verified by a test), so pruning a history row removes its segments/speakers.
- Persistence: `write_segments`/`read_segments` are free functions over `&Connection` (in‑memory testable); `save_segments`/`get_segments` are the manager API; `PersistedSegment` is the bindings‑ready read shape.

---

## 8. File‑by‑file map of our changes

Everything below is **our** delta on top of upstream Handy (`git status` against the upstream clone before it was flattened into this repo).

**New files**
| File | What |
| --- | --- |
| `src-tauri/src/managers/session.rs` | Long‑form session lifecycle (Fase 0/1) |
| `src-tauri/src/audio_toolkit/audio/system_audio.rs` | CoreAudio Process Tap system‑audio recorder (Fase 1) |
| `src-tauri/src/managers/diarization.rs` | `align()` pure core + `DiarizationManager` engine (Fase 2) |
| `src-tauri/src/commands/session.rs` | `start_session`/`stop_session`/`is_session_active` commands |
| `resources/models/diarization/{segmentation,embedding}.onnx` | Bundled diarization models |

**Modified files**
| File | Why |
| --- | --- |
| `src-tauri/src/managers/model.rs` | Diarization model download + bundled auto‑install |
| `src-tauri/src/managers/transcription.rs` | `transcribe_with_segments` (refactor to `transcribe_inner`) |
| `src-tauri/src/managers/history.rs` | Migration #5, segments/speakers persistence, FK pragma |
| `src-tauri/src/commands/history.rs` | `get_session_segments` |
| `src-tauri/src/commands/models.rs` | `download_diarization_models`, `is_diarization_available` |
| `src-tauri/src/audio_toolkit/audio/recorder.rs` | `with_chunk_sink` faithful tap + bounded‑RAM early‑return |
| `src-tauri/src/audio_toolkit/audio/mod.rs` | export `system_audio` |
| `src-tauri/src/lib.rs` | manager wiring, CLI routing, command registration, bindings‑export panic→warn fix |
| `src-tauri/src/cli.rs` | `--toggle-session`, `--toggle-system-session` flags |
| `src-tauri/src/commands/mod.rs`, `managers/mod.rs` | module exports |
| `src-tauri/Cargo.toml` / `Cargo.lock` | sherpa‑onnx + objc2 deps |
| `src-tauri/.cargo/config.toml` | `@loader_path` rpath for the sherpa dylib |
| `src-tauri/build.rs` | `HANDY_FORCE_AI_STUB` escape hatch |
| `src-tauri/Info.plist` | `NSAudioCaptureUsageDescription` |
| `src/components/settings/history/HistorySettings.tsx` | `SpeakerTimeline` |
| `src/i18n/locales/en/translation.json` | `settings.history.unknownSpeaker` |
| `src/bindings.ts` | regenerated (tauri‑specta) |

---

## 9. Build system specifics

- **`HANDY_FORCE_AI_STUB=1`** — `build.rs` would compile the real Apple Intelligence Swift bridge because the CLT SDK ships `FoundationModels.framework`, but CLT lacks the `@Generable` macro plugin (full Xcode only) → false positive. The var forces the stub. Drop it once full Xcode is installed.
- **`CMAKE_POLICY_VERSION_MINIMUM=3.5`** — standalone CMake 4.x rejects the pre‑3.5 policy floors that whisper.cpp's build uses.
- **sherpa `shared` + rpath** — `features = ["shared"]` downloads a *prebuilt* `osx-arm64-shared` bundle (no CMake build) and links onnxruntime as a dylib (avoids the duplicate‑symbol clash with `ort`'s static onnxruntime). `.cargo/config.toml` adds `rustflags = ["-C", "link-arg=-Wl,-rpath,@loader_path"]` so the binary finds the dylib at runtime.
- **Bundled resources** — `bundle.resources = ["resources/**/*"]` ships the VAD + diarization models; `BaseDirectory::Resource` resolves them at runtime (dev and production).

---

## 10. Running & testing

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5 HANDY_FORCE_AI_STUB=1
cd handy

bun tauri dev                       # full app (regenerates bindings.ts)
cd src-tauri && cargo test --lib    # 79 unit tests (align, persistence, sha, sessions, …)
cargo check --lib                   # fast type‑check
bun run lint                        # ESLint (enforces i18n: no literal JSX strings)
```

**Inspecting results** (the app writes here):
```bash
~/Library/Application Support/com.pais.handy/history.db        # transcripts + segments + speakers
~/Library/Application Support/com.pais.handy/recordings/       # *.session.pcm (live) / *.wav (finalized)
~/Library/Logs/com.pais.handy/handy.log                        # runtime log
```

---

## 11. Operational gotchas (learned during live testing)

1. **The CLI toggle needs a running primary.** `handy --toggle-system-session` only works as a *second* instance forwarding to a live `bun tauri dev`. With no primary running, it boots its own instance and silently ignores the flag (the flag is only handled in the single‑instance callback in `lib.rs`).
2. **Capture taps the *default output at session start*.** If system audio is routed to headphones/Bluetooth, or muted, the tap records silence (→ empty transcript, the graceful fallback). Verify you can *hear* the audio from the captured output before recording.
3. **An ASR model must be resident at `finalize`.** Diarization+transcription only run when `is_model_loaded()` is true — keep a model selected with `unload_timeout ≠ Immediately`, or warm it with one dictation first.
4. **Bindings‑export is dev‑only and non‑fatal.** It was a `panic` on a read‑only CWD (which swallowed the CLI flag); it now logs and continues, so the toggle works from any directory.

---

## 12. What remains

| Item | Notes |
| --- | --- |
| **Sessions UI** | Start/stop button + Mic/System selector + live indicator. Backend command + 3‑touch frontend. Plan in [HANDOFF §7](HANDOFF-FASE2.md). The biggest UX gap — sessions are CLI‑only today. |
| **Diarization download button** | Command exists; needs a UI home (the Sessions view). Bundling already covers fresh clones. |
| **Clustering threshold tuning** | Only if a rapid‑alternation recording over‑merges speakers. Defaults validated good for long‑turn audio. Lever: `OfflineSpeakerDiarizationConfig` threshold / `num_clusters` in `diarization.rs`. |
| **Mic + system two‑track mux** | `ActiveRecorder` is either/or; capturing both sides of an in‑person + remote meeting needs a summing stage. |
| **iPhone target** | No iOS support upstream. Recommended: iPhone‑as‑capture + Mac‑as‑brain. Needs full Xcode. |
| **Apple Intelligence bridge** | Real Swift `@Generable` bridge needs full Xcode (drop `HANDY_FORCE_AI_STUB`). |

---

*Last updated 2026‑06‑22. For the line‑cited forensic state and the build de‑risk spikes, see [HANDOFF-FASE2.md](HANDOFF-FASE2.md).*
