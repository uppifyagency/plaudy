# Plaude Local — Engineering Handoff (post Fase 0 + Fase 1, entering Fase 2)

> Authoritative handoff for the next engineer/agent. Everything below is grounded in the current source tree under
> `/Users/vladvrinceanu/Desktop/PROGETTI ANTYGRAVITY/Plaude Local/handy`. Citations are `path:line` against the live files
> (verified this session). No speculation — where something is *proposed* (Fase 2 design) it is labelled as such.

---

## 1. TL;DR

- **Plaude Local** is an open-source, local-first alternative to Plaud (AI meeting/call recorder), built on the
  **Handy** speech-to-text app (Tauri 2: Rust backend in `handy/src-tauri/`, React/TS frontend in `handy/src/`), targeting **macOS + iPhone**.
- The repo is **not a git repository yet**. Ponytail is active project-local; nWave is available for backend/architecture work.
- **Fase 0 (DONE, demoed):** long-form **mic** "sessions" — faithful un-VAD-gated capture streamed to disk as raw PCM, finalized to a 16 kHz WAV, whole-file transcribed, written as one History row. New `managers/session.rs`. 4 unit tests pass.
- **Fase 1 (DONE, demoed):** **system/loopback audio** capture via the CoreAudio **Process Tap** API (`audio_toolkit/audio/system_audio.rs`), feeding the *same* capture seam as the mic. Uses the Audio-Recording TCC permission, not Screen Recording.
- Both phases are driven **only from the CLI today** (`--toggle-session`, `--toggle-system-session`). There is **no frontend caller** and **no system-audio Tauri command** yet.
- **NEXT TASK 1 — Fase 2 diarization** ("who said what"): a *local* speaker engine (sherpa-onnx) running as an offline pass over the saved WAV inside `session.rs::finalize`, one new SQLite migration (`transcription_segments` + `speakers`), `TimedSegment` threaded end-to-end, and a speaker-labeled timeline UI.
- **NEXT TASK 2 — Sessions UI**: add a `start_system_session` backend command, then build a frontend Sessions view (start/stop button + Mic/System selector) wired to the existing commands so users leave the CLI behind.
- **Build caveat (this Mac):** CLT-only, no full Xcode, no Homebrew. You **must** export `HANDY_FORCE_AI_STUB=1` and `CMAKE_POLICY_VERSION_MINIMUM=3.5` (details in §2).

---

## 2. How to build & run

**Machine reality (this Mac):** Apple Silicon M1 Pro, macOS 26, **Command Line Tools only (NO full Xcode), NO Homebrew.**
Rust 1.95 (`~/.cargo`), Bun 1.3.14 (`~/.bun`), standalone CMake 4.x (`~/.local/bin`) — **none are on the non-interactive PATH.**

### Required environment (every shell that builds)

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # standalone CMake 4.x rejects the old policy floors in native deps
export HANDY_FORCE_AI_STUB=1              # see "AI stub" below — REQUIRED on CLT-only machines
```

### Why `HANDY_FORCE_AI_STUB=1` is mandatory here

`build.rs` decides whether to compile the real Apple Intelligence Swift bridge or the stub:

- `handy/src-tauri/build.rs:154-155` — `let has_foundation_models = framework_path.exists() && env::var("HANDY_FORCE_AI_STUB").is_err();`
- The CLT SDK ships `FoundationModels.framework` (`build.rs:148-149`) **but NOT** the `FoundationModelsMacros` Swift plugin that `@Generable` needs (full Xcode only). The framework's presence is a **false positive** on CLT (`build.rs:150-153`).
- With the var set, the build uses `STUB_SWIFT_FILE = "swift/apple_intelligence_stub.swift"` (`build.rs:121`).
- **Upgrade path:** install full Xcode, unset `HANDY_FORCE_AI_STUB`, and the real Apple Intelligence bridge compiles. Forgetting the flag *without* full Xcode = build failure.

### Prerequisites already done

- `bun install` has been run in `handy/`.
- VAD model is present: `handy/src-tauri/resources/models/silero_vad_v4.onnx` (~1.8 MB) — required by the capture pipeline.

### Build / run

```bash
cd "/Users/vladvrinceanu/Desktop/PROGETTI ANTYGRAVITY/Plaude Local/handy"
bun tauri dev      # dev: hot-reload frontend + Rust backend
# or
cd src-tauri && cargo build    # backend only
```

### App-data paths (macOS)

```
~/Library/Application Support/com.pais.handy/
├── history.db          # SQLite (transcription_history; Fase 2 will add child tables)
└── recordings/         # *.session.pcm (in-flight), session-<millis>-<seq>.wav (finalized)
```

### Live demo commands (the ONLY way to drive sessions today)

Sessions are driven through `tauri_plugin_single_instance`, so you launch a *second* invocation of the running app with a flag:

- `--toggle-session` → mic session toggle. Routed at `lib.rs:501-505` → `SessionManager::toggle(Source::Mic)`. CLI flag at `cli.rs:28`.
- `--toggle-system-session` → system-audio session toggle. Routed at `lib.rs:506-510` → `toggle(Source::SystemAudio)`. CLI flag at `cli.rs:32`.

Artifacts land in `recordings/` (PCM while recording → WAV on stop), and a transcript row appears in the **History** view via the existing `HistoryUpdatePayload::Added` event.

---

## 3. Architecture orientation

### The dictation pipeline (Handy's original path)

`capture → VAD → resample → transcribe → post-process → history`

The forensic architecture docs live in `docs/handy-architecture/` and are the canonical reference. Most relevant for this work:

- `02-audio-toolkit-capture-pipeline.md` — cpal capture & the consumer loop
- `03-voice-activity-detection.md` — Silero VAD gating
- `04-managers-audio-recording.md` — the `AudioRecorder` manager
- `05-transcription-engines-and-coordinator.md` — `TranscriptionManager`, single-model residency
- `06-model-management.md` — `ModelManager` download/verify/extract pipeline
- `09-history-persistence-sqlite.md` — `history.db` schema & migrations
- `11-frontend-react.md` — the settings-shell frontend
- `FASE1-system-audio-research.md` — the CoreAudio Process Tap research that produced Fase 1

### The key reuse insight: a producer-agnostic capture seam

Both Fase 0 (mic) and Fase 1 (system audio) feed the **same** consumer via a `chunk_sink`:

- `recorder.rs` exposes `AudioRecorder::with_chunk_sink(sink: mpsc::Sender<Vec<f32>>)` (`recorder.rs:69`).
- The consumer loop `run_consumer(...)` (`recorder.rs:413-421`) takes `in_sample_rate, vad, sample_rx, cmd_rx, level_cb, chunk_sink, stop_flag`.
- In `handle_frame` (`recorder.rs:452-483`): **if a `chunk_sink` is present, it sends every frame faithfully and EARLY-RETURNS before the VAD/dictation accumulator** (`recorder.rs:468-471`). This is the "faithful, un-gated, bounded-RAM" tap for long-form sessions.
- Frames flow as `AudioChunk::Samples(Vec<f32>)` / `AudioChunk::EndOfStream` (`recorder.rs:28-31`); the producer signals end via `EndOfStream` on a shared `stop_flag`.

**Why this matters:** the system-audio recorder did NOT need a parallel pipeline. The CoreAudio IOProc block downmixes to mono f32 and pushes into the *same* `AudioChunk::Samples`/`chunk_sink` channel the cpal mic callback uses. Any future capture source (e.g. a mic+system mux) just needs to emit `AudioChunk::Samples` + one `AudioChunk::EndOfStream` on stop.

`session.rs` selects the producer behind an `ActiveRecorder` enum (`session.rs:56-60`) built by `build_recorder()` (`session.rs:236`); the PCM→WAV→transcribe→history *tail* is shared regardless of source.

---

## 4. What was built this session (file-by-file, cited)

### Fase 0 — mic sessions

**`handy/src-tauri/src/managers/session.rs` (new, ~395 lines).** Holds `SessionManager` (Tauri-managed `Arc`), fields `app: AppHandle`, `recordings_dir: PathBuf`, `active: Mutex<Option<ActiveSession>>` (`session.rs:95-99`).
- Lifecycle: `toggle(source)` (`session.rs:117`), `start(source)` (`session.rs:127`), `stop()` (`session.rs:158`), `recover_interrupted()` (`session.rs:197`).
- Capture is **un-VAD-gated** (every frame incl. silence), streamed to disk as **raw little-endian i16 PCM** in `*.session.pcm` (`PCM_SUFFIX` at `session.rs:43`), flushed per frame by `spawn_pcm_writer` so a kill loses <30 ms.
- On stop, `finalize()` (`session.rs:287-323`) runs **off-thread**: reads PCM→f32 (`read_pcm_i16`, `session.rs:328`), writes a mono 16 kHz WAV via `save_wav_file` (`session.rs:296`), best-effort transcribes **only if a model is resident** (`tm.is_model_loaded()`), then writes one row via `hm.save_entry(file_name, transcript, false, None, None)` (`session.rs:318`) and deletes the PCM.
- **Crash recovery:** `recover_interrupted()` finalizes any orphan `*.session.pcm` at startup; it runs *before any model loads*, so recovered sessions get an empty-transcript row but **keep the audio** (by design — see §5).

**`handy/src-tauri/src/audio_toolkit/audio/recorder.rs` (extended).** Added the faithful tap `with_chunk_sink` (`recorder.rs:69`) and the `handle_frame` early-return (`recorder.rs:468-471`) — **the unbounded-RAM fix** for multi-hour meetings (without it, the dictation accumulator would grow without bound).

**`handy/src-tauri/src/commands/session.rs` (new).** `start_session` (`commands/session.rs:12`, **hardcodes `Source::Mic` at line 14**), `stop_session` (`:20`), `is_session_active` (`:28`). Registered in `collect_commands!` at `lib.rs:426-428`. **No frontend caller** — only the auto-generated wrappers in `src/bindings.ts:729/737/745`.

**CLI driver.** `cli.rs:28` `pub toggle_session: bool`; routed at `lib.rs:501-505`.

**Wiring.** `managers/mod.rs:4 pub mod session`; `commands/mod.rs:4 pub mod session`; `SessionManager::new` at `lib.rs:160`, managed, `recover_interrupted()` at `lib.rs:174` (after history + transcription managers are managed).

**Tests.** 4 unit tests in `session.rs` (`#[cfg(test)]` at `session.rs:336`; tests at `:345, :362, :376, :385`) — **pass**.
**DEMO:** a real mic session produced a History transcript (Italian, matching the speaker).

### Fase 1 — system/loopback audio

**`handy/src-tauri/src/audio_toolkit/audio/system_audio.rs` (new, ~466 lines), gated `#![cfg(all(target_os = "macos", target_arch = "aarch64"))]` (`system_audio.rs:12`).** `SystemAudioRecorder` captures all system output via the CoreAudio **Process Tap** API:
- global mono `CATapDescription` → `AudioHardwareCreateProcessTap` → a private **aggregate device** (NSDictionary toll-free-bridged to CFDictionary via `build_aggregate_description`) → a realtime **IOProc block** that downmixes f32 frames to mono and pushes `AudioChunk::Samples` into the shared `chunk_sink`.
- Fields incl. `tap_id, aggregate_id, io_proc_id, stop_flag` (`system_audio.rs:62-74`). Public surface: `new, with_vad, with_level_callback, with_chunk_sink (:104-107), open (:109-300), start (:302-307), stop (:309-318), close (:320-333)`; `Drop` calls `close` (`:363-368`). `teardown_coreaudio` (`:339-360`) destroys aggregate+IOProc+tap in reverse order on every error path.
- **Permission:** uses the **Audio-Recording TCC permission** (`NSAudioCaptureUsageDescription`, `Info.plist:7`) — **NOT** Screen Recording (no purple banner). `NSMicrophoneUsageDescription` is at `Info.plist:5`.

**`session.rs` source wiring.** `enum Source { Mic, SystemAudio }` (`session.rs:47`, Clone/Copy/Debug), `enum ActiveRecorder { Mic(AudioRecorder), System(SystemAudioRecorder) }` (`session.rs:56`, System arm cfg-gated), and `build_recorder(source, tx)` (`session.rs:236`) which constructs/opens/starts the backend. On non-aarch64-macOS the SystemAudio arm returns an error.

**CLI driver.** `cli.rs:32` `pub toggle_system_session: bool`; routed at `lib.rs:506-510`.

**objc2 deps** added under `[target.'cfg(all(target_os = "macos", target_arch = "aarch64"))'.dependencies]` (`Cargo.toml:110-116`): `block2 0.6.2, objc2 0.6.4, objc2-core-audio 0.3.2, objc2-core-audio-types 0.3.2, objc2-core-foundation 0.3.2, objc2-foundation 0.3.2`.

**DEMO:** 27.8 s of a playing **English** video produced an **English** History transcript while prior mic sessions were Italian — proving the source is system audio, not the mic.

### Adversarial-review fixes (present in code now) — and the WHY for each

**Fase 0:**
- **Path-collision id.** `new_session_id()` (`session.rs:226-231`) = `format!("session-{millis}-{seq}")` using `chrono::Utc::now().timestamp_millis()` + a `static SEQ: AtomicU64`. *Why:* two sessions started in the same millisecond would otherwise collide on the PCM/WAV filename and clobber each other.
- **Bounded-RAM streaming.** The `chunk_sink` early-return in `handle_frame` (`recorder.rs:468-471`). *Why:* the dictation accumulator is fine for short dictation but grows without bound over a multi-hour meeting — the tap must bypass it and stream to disk.
- **Torn-byte safety.** `read_pcm_i16` uses `chunks_exact(2)` (`session.rs:328-334`) so a torn trailing byte from a crash is dropped, not a panic.

**Fase 1:**
- **Format validation.** `read_tap_format` asserts `mFormatID == kAudioFormatLinearPCM && (mFormatFlags & kAudioFormatFlagIsFloat) && mBitsPerChannel == 32` (`system_audio.rs:406-415`, called at `:140`). *Why:* the IOProc casts the buffer to `f32`; a non-float/non-32-bit tap format would produce garbage audio silently.
- **Tail-audio-loss + 2 s stall fix (cpal-mirror EndOfStream).** The IOProc emits `AudioChunk::EndOfStream` **exactly once** on `block_stop` via `eos_sent.swap(true, …)` (`system_audio.rs:203-224`), mirroring the cpal mic callback. *Why:* the consumer's stop path drains until `EndOfStream`; without the producer emitting it, `stop()` falls back to a 2 s timeout and truncates the audio tail.
- **`!Send` fix (Block_copy).** `drop(block)` immediately after `AudioDeviceCreateIOProcIDWithBlock` (`system_audio.rs:285`). *Why:* the `RcBlock` is `!Send`; CoreAudio's `Block_copy` already retained its own copy of the block, so dropping the local makes `SystemAudioRecorder` `Send` — required to live in the Tauri-managed `SessionManager` state.

### Build escape hatch (recap)

`HANDY_FORCE_AI_STUB=1` forces the Apple Intelligence Swift **stub** (`build.rs:154-155`, `STUB_SWIFT_FILE` at `:121`) because the CLT SDK lacks the `@Generable` macro plugin (`build.rs:150-153`).

---

## 5. Known debt / gotchas

- **DEBT — no frontend caller for sessions.** `commands/session.rs` (start/stop/is-active) is registered (`lib.rs:426-428`) and has bindings (`bindings.ts:729-746`), but **no UI invokes it**; the live path is CLI flags only. This is exactly NEXT TASK 2.
- **DEBT — no system-audio Tauri command.** Only the **Mic** path is exposed as a command (`commands/session.rs:14` hardcodes `Source::Mic`). System audio is reachable **only** via `--toggle-system-session`. A `Source::SystemAudio` command must be added before a UI selector can pick it.
- **FRAGILITY — the 2 s drain timeout is a FALLBACK, not a handshake.** The correct stop relies on the producer (cpal callback or system_audio IOProc) emitting `EndOfStream` on `stop_flag`. The drain uses `recv_timeout(Duration::from_secs(2))` with a warn on timeout. If a *future* source forgets to emit `EndOfStream`, `stop()` silently waits 2 s and may truncate tail audio.
- **DEFERRED — mic + system two-track mux.** `ActiveRecorder` (`session.rs:56`) is an either/or enum. Capturing mic **and** system audio simultaneously (both sides of an in-person + remote meeting) is not implemented; both share one `chunk_sink`, so mixing would need a summing stage.
- **PLATFORM GATING.** `SystemAudioRecorder` + all objc2 deps are `cfg(all(target_os = "macos", target_arch = "aarch64"))`. CoreAudio Process Tap requires **macOS 14.4+**. On Intel/non-macOS, `Source::SystemAudio` returns an error. No runtime OS-version guard beyond the cfg + API availability.
- **TCC churn (ad-hoc signing).** Dev builds are ad-hoc signed; the bundle identity can change between builds, so macOS may **re-prompt** for the Audio-Recording / Microphone permission. Expect occasional re-grants during development.
- **Single-model residency.** `TranscriptionManager` keeps exactly one ASR model resident (`transcription.rs:67`, `engine: Arc<Mutex<Option<LoadedEngine>>>`), with an idle-unload watcher and immediate-unload. This directly shapes the Fase 2 diarizer design (it must NOT share that slot — see §6).
- **No segments/speakers schema yet.** `history.db` stores only the flat `transcription_text`. Fase 2 adds the structured overlay (one migration).
- **Transcript-loss window (by design, low risk).** `finalize()` writes the WAV *before* transcribing and skips transcription when no model is resident (`session.rs:302-310`), so recovered/at-startup sessions get an **empty-transcript row but keep the audio**. Fase 2 re-transcription/diarization should account for empty-transcript rows.
- **Full Xcode needed later.** Required for the real Apple Intelligence Swift bridge (the `@Generable` macro) and for any iOS/iPhone work. Drop `HANDY_FORCE_AI_STUB` once installed.

---

## 6. NEXT TASK 1 — Fase 2 diarization (detailed plan)

**Goal:** "who said what" — attribute each transcript segment to a local speaker id, render a speaker-labeled timeline. This touches the **data model**, so do it once, correctly (append-only migration).

### Engine choice

Use the **official `sherpa-onnx` Rust crate** (k2-fsa), **v1.13.3** (`github.com/k2-fsa/sherpa-onnx`). It exposes `OfflineSpeakerDiarization` = pyannote-segmentation-3.0 ONNX + a speaker-embedding ONNX + offline agglomerative clustering — the exact pyannote-style pipeline required, self-contained (its own VAD/segmentation/embedding/clustering), returning `{start, end, speaker}` on a mono 16 kHz WAV.

> **Do NOT use `sherpa-rs` (thewh1teagle)** — it is ARCHIVED/DEPRECATED and its README redirects to the official crate.

**Models** (ride Handy's existing `ModelManager` download/verify/extract):
- Segmentation: `sherpa-onnx-pyannote-segmentation-3-0.tar.bz2` (model.onnx 5.7M / model.int8.onnx 1.5M) from the `speaker-segmentation-models` release.
- Embedding: e.g. `nemo_en_titanet_small.onnx` (English) or `3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx`, from the `speaker-recongition-models` release tag.
- **Note:** existing models are `.tar.gz` (flate2). The pyannote archive is `.tar.bz2` (bzip2) — add a bz2 decode branch in the extract path **or** repackage to `.tar.gz` on Handy's blob host.

### Where it slots — an offline pass over the saved WAV in `finalize()`

`session.rs::finalize()` (`session.rs:287-323`) already has `samples: Vec<f32>` mono 16 kHz right after `save_wav_file(wav_path, &samples)` (`session.rs:296`) — **exactly** sherpa's required format (16 kHz / 16-bit / mono), no resample. Insert the diarization + alignment pass **between** `save_wav_file` and the persist call at `session.rs:318`:

1. Run ASR to get **timed** segments (see "segment seam" below).
2. Run the diarizer over `samples` → speaker turns `{start, end, speaker}`.
3. Align (WhisperX-style): assign each ASR segment the speaker turn with **max temporal overlap**.
4. Persist: `hm.save_entry(...)` (keep the flat `transcription_text` canonical) **+** a new `save_segments(history_id, …)`.
5. **Honor the existing guard:** only do this when `tm.is_model_loaded()` (`session.rs:302-310`). Startup recovery deliberately runs with no model loaded — diarization (heavier) must be skipped there too, or deferred. Do not block startup.

### The already-existing ASR-segment seam (don't re-segment text)

`transcribe-rs` already returns `TranscriptionResult { text: String, segments: Option<Vec<TranscriptionSegment>> }` with `TranscriptionSegment { start: f32, end: f32, text: String }` (**seconds**). Parakeet is even explicitly asked for them (`transcription.rs:558-562` sets `TimestampGranularity::Segment`). But `TranscriptionManager::transcribe()` (`transcription.rs:440`, returns only `String`) **throws the segments away** (collapsed to text downstream). So the diarizer's job is only to assign a `speaker_id` to existing ASR windows — it does **not** re-segment.

- **Add a sibling method** `transcribe_with_segments(&self, audio: Vec<f32>) -> Result<(String, Vec<TimedSegment>)>` next to `transcribe()` (`transcription.rs:440`). Same engine match, but converts `Option<Vec<transcribe_rs::TranscriptionSegment>>` (seconds, f32) → `Vec<TimedSegment>` (ms, i64, `speaker_id = None` initially). Convert seconds→ms at this **single boundary** to avoid double-scaling.
- Keep `transcribe()` for the dictation hot path. This is the **only** change inside the ASR engine — no new `LoadedEngine` arm.

### Single-residency implication — use a SEPARATE diarizer slot

`TranscriptionManager`'s `engine: Arc<Mutex<Option<LoadedEngine>>>` (`transcription.rs:67`) holds exactly one ASR model, with an idle-unload watcher and immediate-unload. **Loading a diarizer into that slot would EVICT the user's ASR model** and the watchers would thrash. Instead:

- **NEW file `managers/diarization.rs`** — a self-contained `DiarizationManager` with its **own** `Arc<Mutex<Option<DiarizerEngine>>>` slot wrapping sherpa-onnx `OfflineSpeakerDiarization`. API e.g. `diarize(samples: &[f32]) -> Result<Vec<SpeakerTurn{start_ms,end_ms,speaker}>>` + an `align(asr_segments, turns) -> Vec<TimedSegment>`. Register it in `lib.rs` managed state next to `TranscriptionManager`. Reuse `ModelManager::get_model_path` for the diarizer dir.
- Because `finalize` is a sequential off-thread file pass, run **ASR first, then load+run the diarizer** (or load-diarize-unload). Two slots avoid thrash.

### EXACT schema migration (append-only — 5th `M::up`)

`history.rs:20-34` holds `static MIGRATIONS: &[M]` (currently 4 `M::up` entries: v1 create table, v2–v4 ALTER ADD COLUMN), run via `to_latest` tracked by the `user_version` pragma, `validate()` in debug. **Append only — never edit the existing 4** (editing corrupts the `user_version` chain on shipped `history.db`).

Append one coherent migration with two tables + an index:

```sql
CREATE TABLE transcription_segments (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id  INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  start_ms    INTEGER NOT NULL,
  end_ms      INTEGER NOT NULL,
  speaker_id  INTEGER,
  text        TEXT NOT NULL,
  confidence  REAL
);
CREATE TABLE speakers (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id  INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  label       TEXT NOT NULL,
  embedding   BLOB
);
CREATE INDEX idx_segments_history ON transcription_segments(history_id);
```

> **`ON DELETE CASCADE` GOTCHA:** it requires `PRAGMA foreign_keys = ON` **per connection**, and `history.rs` `get_connection` (`history.rs:195`) opens a bare `Connection` that does **NOT** enable it. Either enable the pragma on every connection **or** delete child rows explicitly in the existing `delete_entry` / `delete_entries_and_files` paths — otherwise segment/speaker rows leak when history is pruned.

### History API + bindings

- `HistoryManager::save_entry` signature is `(file_name: String, transcription_text: String, post_process_requested: bool, post_processed_text: Option<String>, post_process_prompt: Option<String>)` (`history.rs:219`); it INSERTs and the new `history_id` is the FK (`conn.last_insert_rowid()`). `HistoryEntry` struct at `history.rs:56`, `map_history_entry` reads flat columns at `history.rs:199`.
- Add `struct TimedSegment { start_ms: i64, end_ms: i64, speaker_id: Option<i64>, text: String, confidence: Option<f32> }` and `save_segments(&self, history_id: i64, speakers: &[(String, Option<Vec<u8>>)], segments: &[TimedSegment]) -> Result<()>` (insert speakers first to capture rowids, then segments, in **one transaction**). Either extend `save_entry` or add a sibling `save_entry_with_segments` so the flat `transcription_text` stays the canonical full transcript and segments are the structured overlay.
- Extend `get_entry_by_id` / `get_history_entries` to optionally hydrate segments; surface via `commands/history.rs` and either a new `HistoryUpdatePayload` variant or an extended `HistoryEntry`. The existing `HistoryUpdatePayload::Added` event (`history.rs:44`) already pushes new rows to the UI for free.

### `EngineType` exhaustiveness

`model.rs:21` `enum EngineType` is matched exhaustively in `transcription.rs:302` and `transcription.rs:688-691`. Adding `EngineType::Diarizer` makes those non-exhaustive — **grep `EngineType::` before building** and handle the new variant (the ASR load match should reject/skip it, since the diarizer is NOT loaded into the ASR slot). Register the model bundle as a directory-based catalog entry (copy the Parakeet entry, `is_directory: true`) so it reuses `download_model` (`model.rs:987`) unchanged.

### Timeline UI (frontend)

History renders today in `HistorySettings.tsx` via `commands.getHistoryEntries(cursor, PAGE_SIZE)` with infinite scroll + a live append on `events.historyUpdatePayload`. Today each transcript is a single `<p>{entry.transcription_text}</p>` (`HistorySettings.tsx:413-440`). Fase 2: extend `HistoryEntry` (bindings) with segments **or** add a sibling `getSessionSegments(id) -> TimedSegment[]`, then render a per-segment list (speaker chip + timecode + text) inside `HistoryEntryComponent`. **Keep `transcription_text` populated (flattened)** so copy/retry and non-session entries keep working.

### Fase 2 risks (carry forward)

> **🔬 SPIKE FINDINGS — link de-risk, verified 2026-06-19 (build scripts read AND an actual link+run spike built green).** The duplicate-onnxruntime risk is **confirmed real** AND the mitigation is now **empirically validated**:
> - **✅ EMPIRICAL: it builds, links, and runs.** A throwaway crate depending on `ort 2.0.0-rc.12` (download-binaries → static onnxruntime, Handy's config) + `sherpa-onnx 1.13.3` (`default-features = false, features = ["shared"]`) **compiled and linked clean — no duplicate-symbol error** (`cargo build`, 34s, after the prebuilt downloads). The binary then **ran** (exit 0) with both onnxruntime images loaded. Recipe proven.
> - **⚠️ rpath packaging gotcha (must fix in the real build).** sherpa-onnx-sys copies `libonnxruntime*.dylib` + `libsherpa-onnx-c-api.dylib` next to the binary and *tries* to emit an `@loader_path` rpath — but that `cargo:rustc-link-arg` does **not** propagate to a downstream binary, so the spike first died at startup with `dyld: Library not loaded: @rpath/libsherpa-onnx-c-api.dylib … no LC_RPATH's found`. Fix is one line: add `rustflags = ["-C", "link-arg=-Wl,-rpath,@loader_path"]` in `.cargo/config.toml` (or let Tauri's bundler place + rpath the dylibs). After adding the rpath the binary ran fine. **Not a blocker — but the next agent WILL hit it without this.**
> - **🔬 version skew, the residual risk:** the two onnxruntime images are **different versions** — ort statically bakes pyke `1.24.2`, sherpa ships `libonnxruntime.1.24.4.dylib`. They **co-load** without crashing, but the spike does NOT initialize both (no `OrtEnv`/session created on each). Two ORT runtimes both *active* in one process is the one thing still unproven — settle it in the real Handy integration with models + a WAV. If it misbehaves, make both share ONE onnxruntime (`ort` `load-dynamic` + sherpa `shared`, same dylib).
> - **`ort` static-links onnxruntime too** — `ort-sys 2.0.0-rc.12` with `download-binaries` (Handy's config) emits `cargo:rustc-link-lib=static=onnxruntime` (`build/main.rs:160`). The handoff originally pinned the conflict on sherpa alone; in fact **both** sides static-link by default.
> - **sherpa static (default) = collision.** `sherpa-onnx-sys 1.13.3` in `static` mode emits `static=onnxruntime` (`build.rs:26,248-251`). ort `static=onnxruntime` + sherpa `static=onnxruntime` → **two static onnxruntime archives → duplicate-symbol linker failure.**
> - **sherpa `shared` = the fix at link time.** `--no-default-features --features shared` switches sherpa to `cargo:rustc-link-lib=dylib=onnxruntime` (`build.rs:243-246`) and ships a `libonnxruntime.dylib` next to the binary (`@loader_path` rpath, `copy_unix_runtime_libs`). static(ort) + dylib(sherpa) **do NOT collide at the linker** — different linkage. This is why `shared` works.
> - **No CMake needed.** `shared` downloads a **prebuilt** `sherpa-onnx-v1.13.3-osx-arm64-shared-lib.tar.bz2` from GitHub releases (`build.rs:228-230`) — kills the "source build needs CMake" fear on this CLT-only Mac. Offline escape hatches confirmed: `SHERPA_ONNX_LIB_DIR` (point at a lib dir) and `SHERPA_ONNX_ARCHIVE_DIR` (local archive).
> - **API confirmed real:** `OfflineSpeakerDiarization::create(&config)` / `process(&self, samples: &[f32])` (it's `Send + Sync`) + `OfflineSpeakerDiarizationConfig` / `FastClusteringConfig` / `OfflineSpeakerSegmentationModelConfig` / `OfflineSpeakerDiarizationResult` exist in `sherpa-onnx 1.13.3` (`src/offline_speaker_diarization.rs:13-200`).

- **Mitigation recipe (apply when wiring the dep):** `sherpa-onnx = { version = "1.13.3", default-features = false, features = ["shared"] }` under the macOS-aarch64 target block, behind a `diarization` cargo feature. If runtime dual-runtime trouble appears, switch `ort` to `load-dynamic` and share one `libonnxruntime.dylib`.
- **Overlapping speech / unknown speaker count.** Clustering assigns each region a single speaker (crosstalk degrades). With `num_clusters = -1` results are sensitive to the clustering `threshold` — needs a sane default + a settings knob. Pin a recent sherpa-onnx (an offset-by-one pyannote bug was fixed upstream ~May 2026).
- **Segment availability varies by engine.** `segments` is `Option`; Whisper/Parakeet populate it, some ONNX engines may return `None`. `finalize` must fall back to a single whole-file segment (one speaker) so diarization degrades gracefully rather than dropping the transcript.

---

## 7. NEXT TASK 2 — Sessions UI (detailed plan)

The frontend is a single-window settings shell: `App.tsx` renders a fixed `Sidebar` + a content pane that swaps one component by `currentSection` state. Views are driven entirely by `SECTIONS_CONFIG` in `Sidebar.tsx`. **No router** — navigation is React state, and the sidebar auto-renders every enabled section. Adding a "Sessions" view is a 3-touch change: (1) create the component, (2) export it, (3) add one `SECTIONS_CONFIG` entry.

### 7a. Backend gap — add the system-audio command FIRST

`commands.startSession()` hardcodes `Source::Mic` (`commands/session.rs:14`). For the UI's System option to do anything, add a backend command:

- e.g. `start_system_session` calling `SessionManager::start(Source::SystemAudio)`, **or** parameterize `start_session(source)`.
- Register it in `collect_commands!` (`lib.rs:426-428`) and **regenerate bindings via tauri-specta**. Shipping the System toggle before this exists yields a dead control.

### 7b. Frontend Sessions view (3-touch + wiring)

1. **CREATE** `handy/src/components/settings/sessions/SessionsSettings.tsx` — wrap in `SettingsGroup`; contains (a) a Mic-vs-System source selector (clone the `MicrophoneSelector.tsx` Dropdown pattern, or a `ToggleSwitch` like `PushToTalk.tsx`) and (b) a start/stop `Button` (`ui/Button.tsx`, variant `primary` → `danger` on active).
2. **EXPORT** from `handy/src/components/settings/index.ts`: `export { SessionsSettings } from './sessions/SessionsSettings';`.
3. **NAV** in `handy/src/components/Sidebar.tsx` — add a `sessions` key to `SECTIONS_CONFIG` (lines 34-77): `{ labelKey: 'sidebar.sessions', icon: <Mic/Radio lucide icon>, component: SessionsSettings, enabled: () => true }`. Import the icon and the component.

### 7c. Command wiring — mind the two call conventions

- `commands.startSession()` (`bindings.ts:729`) and `commands.stopSession()` (`:737`) return `Result<null, string>` — **check `.status === 'ok'`**.
- `commands.isSessionActive()` (`bindings.ts:745`) returns a **raw `Promise<boolean>`** (NO Result wrapper) — **`await` the boolean directly**. Mixing the two conventions in one component is an easy bug.
- Branch `start` on the selected source: Mic → `startSession()`; System → the new `start_system_session()`.

### 7d. Live is-active state

There is **no `session-state-changed` event today** (the only tauri-specta event is `events.historyUpdatePayload`). Options:
- **(recommended)** add a backend `session-state-changed` Tauri event and consume it via the `events.*.listen` / `listen<T>(name, cb)` pattern already used in `App.tsx` and `HistorySettings.tsx`. This keeps the indicator correct even when sessions are toggled via the CLI flags.
- **(stopgap)** poll `isSessionActive()` after start/stop and on an interval. This can drift from reality when the CLI flags toggle a session.

Seed local `isActive` from `await commands.isSessionActive()` on mount. **Session start/stop is transient control state — do NOT route it through the `settingsStore` `settingUpdaters` map** (that map is for persisted `AppSettings` keys). Only a Mic|System *preference* (if persisted) belongs in settings (new `AppSettings` field + `settingUpdaters` entry + backend setter command).

### 7e. i18n (build-blocking)

ESLint enforces `i18next/no-literal-string` as an **error** (`eslint.config.js:20`, `markupOnly: true`). Every JSX text literal and every visible `title=` attribute must be `t("key")` (note: `aria-*`/`className`/`style`/`type`/`id`/`name`/`key`/`data-*` are ignored, but visible `title=` is NOT). Add keys to `handy/src/i18n/locales/en/translation.json`: `sidebar.sessions` (next to the existing sidebar block, lines 11-19) and a `settings.sessions.*` block (title, sourceMic, sourceSystem, start, stop, active). A hardcoded string fails `bun run lint` and blocks the build.

### 7f. How the speaker timeline plugs in once Fase 2 lands

The Sessions view controls capture; the **timeline** lives in the History view. Once Fase 2 ships segments through bindings (§6), `HistoryEntryComponent` (`HistorySettings.tsx:413-440`) renders the per-segment speaker-labeled list, reusing the existing `events.historyUpdatePayload` append path for live updates. No change to the Sessions view is needed for the timeline.

---

## 8. Phased checklist (ordered, for the next agent)

### Phase A — Sanity / environment

- [ ] Export PATH + `CMAKE_POLICY_VERSION_MINIMUM=3.5` + `HANDY_FORCE_AI_STUB=1`; run `cargo build` (or `bun tauri dev`) and confirm it compiles clean on this Mac.
- [ ] Run the 4 session unit tests (`session.rs:336+`) — confirm green.
- [ ] Smoke-test both CLI drivers: `--toggle-session` and `--toggle-system-session`; confirm WAVs land in `recordings/` and rows appear in History.

### Phase B — Sessions UI (NEXT TASK 2 — smaller, unblocks daily use)

- [ ] **Backend:** add `start_system_session` (or `start_session(source)`); register in `collect_commands!` (`lib.rs:426-428`); regenerate tauri-specta bindings.
- [ ] (Recommended) add a `session-state-changed` Tauri event; emit on start/stop/recover.
- [ ] **Frontend:** create `SessionsSettings.tsx`; export from `settings/index.ts`; add the `sessions` entry to `SECTIONS_CONFIG`.
- [ ] Wire start/stop (`Result`, check `.status`) + Mic/System selector; seed `isActive` from `isSessionActive()` (raw boolean); subscribe to the new event (or poll).
- [ ] Add i18n keys (`sidebar.sessions`, `settings.sessions.*`); `bun run lint` clean.

### Phase C — Fase 2 diarization build spike (de-risk FIRST)

- [x] **Link analysis (done 2026-06-19, see Spike Findings in §6):** confirmed both `ort` and sherpa static-link onnxruntime by default → real collision; `shared` resolves it at link time; prebuilt `osx-arm64-shared` bundle exists (no CMake). Recipe: `sherpa-onnx = { version = "1.13.3", default-features = false, features = ["shared"] }` behind a `diarization` feature.
- [x] **Confirm link empirically (done 2026-06-19):** throwaway `ort` + `sherpa-onnx(shared)` crate built + linked + ran green (no duplicate-symbol). See Spike Findings in §6.
- [x] **rpath fix (done 2026-06-22):** added to `handy/.cargo/config.toml` → `[target.aarch64-apple-darwin] rustflags = ["-C", "link-arg=-Wl,-rpath,@loader_path"]`. Full Handy crate builds green with `sherpa-onnx` wired in (`cargo check`, 1m26s); the sherpa + onnxruntime dylibs land in `target/debug/`.
- [x] **Runtime coexistence CONFIRMED (live, 2026-06-22).** Both runtimes were *initialized at once* — sherpa's `libonnxruntime` (diarizer) + ort's static onnxruntime (Parakeet V3, kept resident via `unload_timeout = Never`) — across **3 consecutive system-audio session finalizes**, with **zero crash** (backend PID survived all three). The dual-runtime fear is settled; the `ort` → `load-dynamic` fallback is NOT needed.

### Phase D — Fase 2 data model + engine

- [x] **Pure domain core (done 2026-06-19):** `managers/diarization.rs` — `SpeakerTurn` / `AsrSegment` / `TimedSegment` + `align(asr, turns)` (max-overlap speaker assignment, deterministic tie-break, graceful `None`). **6 unit tests, green.**
- [x] **Schema migration #5 (done):** `speakers` + `transcription_segments` (+ `idx_segments_history`) appended to `history.rs` MIGRATIONS, FKs with `ON DELETE CASCADE`/`SET NULL`. Existing 4 untouched.
- [x] **Foreign-keys strategy (done):** `get_connection` now sets `PRAGMA foreign_keys = ON` (cascade verified by a test).
- [x] **Persistence (done):** `write_segments`/`read_segments` (free fns over `&Connection`, in-memory-testable) + `HistoryManager::save_segments`/`get_segments` + `PersistedSegment` (bindings-ready). Maps diarizer-local speaker index → `speakers.id`, labels "Speaker N". **3 unit tests, green** (round-trip, unknown→NULL, delete-cascade).
- [x] **Engine adapter (done 2026-06-22):** `DiarizationManager` in `managers/diarization.rs` wraps `sherpa_onnx::OfflineSpeakerDiarization` (cfg-gated macOS-aarch64, like `system_audio.rs`). Dep added to `Cargo.toml` (`shared`). **Safe-by-default:** `diarize()` no-ops unless `<app_data>/models/diarization/{segmentation,embedding}.onnx` are present, so the default install is unchanged and sherpa's onnxruntime never coexists with ort's unless diarization actually runs. Compiles in the full crate.
- [x] **`transcribe_with_segments` (done 2026-06-22):** `transcription.rs` refactored — the giant `transcribe` body became a private `transcribe_inner` returning `(String, Vec<AsrSegment>)`; `transcribe` and `transcribe_with_segments` are thin wrappers (no duplication; engine/`catch_unwind` block untouched). `to_asr_segments` converts transcribe-rs seconds → ms.
- [x] **model auto-download (done 2026-06-22).** `ModelManager::download_diarization_models()` (`model.rs`) fetches the two models into `<app_data>/models/diarization/` as `segmentation.onnx` + `embedding.onnx` (the engine's drop-in convention is now the *download target*, not a manual step). **Ponytail design — deliberately NOT the handoff's original plan, for three concrete reasons:**
  - **Bare `.onnx`, no archive.** Segmentation = pyannote-3.0 from the HuggingFace mirror (`csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx`); embedding = NeMo TitaNet-small from the k2-fsa `speaker-recongition-models` GitHub release. Both are bare files → the existing file-based download/verify path is reused verbatim. **No `.tar.bz2` branch, no `bzip2` dependency.**
  - **No ASR-catalog entry.** The two models are a private `const DIARIZATION_MODELS` table, **not** rows in `available_models`. Putting them there would let `auto_select_model_if_needed` (`model.rs`, iterates `is_downloaded`) auto-select a diarizer *as the ASR model* — a real bug — and would pollute the model-selector UI.
  - **No `EngineType::Diarizer`.** Avoided entirely → zero churn in the exhaustive `transcription.rs` match arms for a variant that must never reach the ASR loader.
  - **Integrity:** SHA256 pinned from the actual downloaded bytes (seg `220ad67c…1079` / emb `ad4a1802…789e`), verified via the existing `verify_sha256`. Idempotent (skips files already on disk). The shared `DiarizationManager::{SUBDIR,SEG_FILE,EMB_FILE}` consts are the single source of truth so the downloader and engine can't diverge (guarded by a unit test). Skipped (named ceiling in the code): resume/range/cancel — both files are <40 MB, a failed download retries from scratch.
  - **Commands:** `download_diarization_models` + `is_diarization_available` (`commands/models.rs`), registered in `collect_commands!` (`lib.rs`). Progress rides the existing `model-download-progress` event (`model_id = "diarization"`); success emits `diarization-models-ready`.
  - **REMAINING (UI, ~3 lines, intentionally deferred to NEXT TASK 2):** no button calls `download_diarization_models()` yet — the natural home is the **Sessions view** (§7), which doesn't exist. Once it lands, add an "Enable speaker diarization" control that calls `commands.downloadDiarizationModels()` and reflects `commands.isDiarizationAvailable()`. The bindings regenerate on the next `bun tauri dev`.

### Phase E — Wire diarization into finalize + UI

- [x] **finalize wiring (done 2026-06-22):** `session.rs::finalize`, gated on `tm.is_model_loaded()`, now **diarizes first** (borrows `samples`) **then** `transcribe_with_segments` (consumes them — avoids cloning a multi-hour buffer) → `align` → `save_entry` + `save_segments`. Skips segments when ASR yields none; startup recovery (no model) still skips the whole path. **78 lib tests green.**
- [x] **Command + read side (done 2026-06-22):** `get_session_segments(history_id) -> Vec<PersistedSegment>` in `commands/history.rs`, registered in `collect_commands!` (`lib.rs`). Compiles.
- [x] **Timeline UI (done 2026-06-22, eslint-clean):** `SpeakerTimeline` in `HistorySettings.tsx` renders `speaker · mm:ss · text` per segment when an entry has segments, else the existing flat `<p>` (canonical `transcription_text` preserved). Fetches via `getSessionSegments(entry.id)`. ⚠️ tsc verification + visual check happen on the next `bun tauri dev` (regenerates `bindings.ts` with `getSessionSegments`/`PersistedSegment`).
- [x] **i18n (done):** `settings.history.unknownSpeaker` added to `en/translation.json`; eslint `no-literal-string` clean on the changed file.

### Phase F — Validation (the remaining live checks — need real models + a recording)

- [x] **2-speaker diarization CONFIRMED (live, 2026-06-22).** Models dropped into `<app_data>/models/diarization/` (auto-download command also verified; here placed via curl with SHA match). A ~100 s system-audio capture of a 2-person interview produced **2 distinct speakers** persisted to `speakers` + `transcription_segments` (history_id 9: 7 segments, `Speaker 1`/`Speaker 2`). A prior single-presenter video correctly yielded **1 speaker** (history_id 7). End-to-end chain (capture → timed ASR → sherpa diarize → `align` → DB) works.
  - **3-speaker capability CONFIRMED (live, 2026-06-22).** A ~4 min, 3-person panel capture (history_id 10) auto-detected **3 distinct speakers** (`num_clusters = -1`), 30 segments: Speaker 1 = 14, Speaker 2 = 14, Speaker 3 = 1, plus 1 NULL (uncovered segment → graceful unknown). No crash. So the N-speaker chain (auto-count → align → `Speaker N` labels → schema → UI) works for >2 with no code change.
  - **Temporal-block result CONFIRMED CORRECT (user-verified, 2026-06-22).** The 3-person clip resolved into clean contiguous time blocks (Speaker 1 = 0:00–1:23, Speaker 2 = 1:37–4:09, Speaker 3 = one line). The user confirmed the source genuinely had **long per-person turns** (panel format, not rapid back-and-forth) — so the blocking is *accurate*, not under-resolution. The **default `FastClusteringConfig` threshold is fine for long-turn content.**
  - **CALIBRATION (NOT triggered — do not build pre-emptively).** No mis-clustering has been observed on real audio. The clustering `threshold` / `num_clusters` knobs (`OfflineSpeakerDiarizationConfig`, set in [diarization.rs](../handy/src-tauri/src/managers/diarization.rs)) remain a future lever **only if** a rapid-alternation recording is later seen to over-merge voices. Until that is observed, exposing a setting is speculative — leave the defaults.
- [x] **Empty-transcript path CONFIRMED (live, 2026-06-22).** A silent capture (output routed away from the tap; WAV peak 0.2 %, RMS 3) produced an **empty transcript + 0 segments + 0 speakers** with no crash (history_id 8) — the graceful "ASR yields none → skip segments" fallback fired exactly as designed.
- [ ] Confirm history pruning removes child segment/speaker rows (covered by a unit test already; re-confirm live).

> **Operational gotchas surfaced during the live test (2026-06-22):**
> - **CLI toggle needs a running primary.** `handy --toggle-system-session` only works as a *second* instance forwarding to a live `bun tauri dev`; with no primary it boots its own instance and silently ignores the flag (handled only in the single-instance callback, `lib.rs:509`).
> - **Bindings-export panic fixed.** The debug-only `specta_builder.export("../src/bindings.ts")` used to **panic** (`PermissionDenied`) when the CLI forwarder ran from a read-only CWD (e.g. `~`), swallowing the flag. Now logs and continues (`lib.rs` ~446) — the toggle works from any directory.
> - **Capture taps the default output at session start.** If system audio is routed elsewhere (headphones/Bluetooth) or muted, the tap records silence. Verify audio is audible from the *captured* output before recording.
