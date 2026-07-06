# Plaude Local — Codebase Documentation

The complete technical reference for the project: the architecture, **what we built and how**, a file‑by‑file map of our changes, the data model, the build system, and what remains. If you are the incoming developer, read this top to bottom once, then keep [HANDOFF.md](HANDOFF.md) (entry‑point) and [HANDOFF-FASE2.md](HANDOFF-FASE2.md) (line‑cited Fase 2 forensics) open alongside.

> **Mental model in one sentence:** we took Handy's short‑dictation pipeline (`capture → VAD → transcribe → paste`) and added a *second* path for **long‑form recording** that taps the same capture seam, streams hours of audio to disk, and on stop runs an offline **transcribe + diarize ("who said what")** pass — extended to **capture the mic and the Mac's system audio simultaneously** (a meeting), merge them into one speaker‑attributed transcript, and expose the whole library to **Claude over a local MCP server** so nothing ever leaves the Mac.

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
| Claude bridge | **local MCP server** (`handy/mcp/`, Bun + `bun:sqlite`, read‑only, stdio) |

---

## 2. Top‑level architecture

### Manager pattern + command/event
Core logic lives in **managers** constructed at startup and held in Tauri managed state ([lib.rs](../handy/src-tauri/src/lib.rs)):

- `AudioRecordingManager` — device + recording lifecycle for dictation.
- `ModelManager` ([managers/model.rs](../handy/src-tauri/src/managers/model.rs)) — model catalog, download/verify/extract, **bundled‑model install**.
- `TranscriptionManager` ([managers/transcription.rs](../handy/src-tauri/src/managers/transcription.rs)) — single resident ASR model, idle‑unload watcher.
- `HistoryManager` ([managers/history.rs](../handy/src-tauri/src/managers/history.rs)) — `history.db`, including the segments/speakers overlay and the per‑row transcript status.
- `SessionManager` ([managers/session.rs](../handy/src-tauri/src/managers/session.rs)) — **our** long‑form + dual‑stream recording lifecycle.

Frontend → backend via **commands** (`commands/*.rs`); backend → frontend via **events** (`historyUpdatePayload`, `session-state-changed`, `model-download-progress`, …). All typed through `tauri-specta` into [src/bindings.ts](../handy/src/bindings.ts). **Claude** → backend data via the local MCP server, which reads `history.db` directly (read‑only, no app required).

### The pipelines
- **Dictation (upstream):** `capture → VAD gate → resample → transcribe → post‑process → paste + history`. Short, latency‑sensitive.
- **Long‑form session (ours):** `capture → faithful tap → stream to disk → (stop) → WAV → diarize + transcribe → speaker‑labelled history`. Multi‑hour, throughput‑oriented.
- **Meeting (dual‑stream, ours):** the long‑form path with **two tracks at once** — mic + system audio — mixed into one playable WAV and **merged into one chronological "who said what" transcript** (mic = "Me", system = diarized remote speakers).

---

## 3. The capture seam — the central reuse

Both pipelines, and both audio sources, feed **one** consumer through a `chunk_sink`:

- `AudioRecorder::with_chunk_sink(sink)` ([recorder.rs](../handy/src-tauri/src/audio_toolkit/audio/recorder.rs)) installs a faithful tap.
- In `handle_frame`, **if a `chunk_sink` is present every frame is forwarded verbatim and the function early‑returns before the VAD/dictation accumulator** — the bounded‑RAM fix that makes multi‑hour capture possible.
- Both the cpal mic callback and the CoreAudio system IOProc run through the **same** `run_consumer` (in `recorder.rs`), which resamples to **16 kHz mono** before the sink — so every track, from either source, is already 16 kHz mono f32 by the time it hits disk. (This is why mixing two tracks is trivial — see §5a.)

**Consequence:** system‑audio capture did *not* need a parallel pipeline, and dual capture is just *two recorders feeding two sinks*. A session is now a `Vec<Track>`, each `Track` wrapping one `ActiveRecorder { Mic | System }`; the PCM→WAV→transcribe→history *tail* is shared and source‑agnostic.

---

## 4. Fase 0 — long‑form sessions

**File:** [managers/session.rs](../handy/src-tauri/src/managers/session.rs).

- `SessionManager` holds `app`, `recordings_dir`, `active: Mutex<Option<ActiveSession>>`.
- Capture is **un‑VAD‑gated** (every frame incl. silence), streamed to disk as **raw little‑endian i16 PCM** (`*.session.pcm`), flushed per frame so a crash loses <30 ms.
- **Crash recovery:** `recover_interrupted()` finalizes any orphan `*.session.pcm` at startup (before models load) as its own single‑track session, so a recovered session keeps its audio (empty transcript, by design). It infers the source from the `.mic.`/`.system.` infix in the filename.
- **Path‑collision safety:** session ids are `session-{millis}-{seq}` (`AtomicU64`), so two sessions in the same millisecond can't clobber each other.

---

## 5. Fase 1 — system / loopback audio (macOS)

**File:** [audio_toolkit/audio/system_audio.rs](../handy/src-tauri/src/audio_toolkit/audio/system_audio.rs), gated `cfg(all(target_os = "macos", target_arch = "aarch64"))`.

`SystemAudioRecorder` captures everything the Mac plays via the CoreAudio **Process Tap** API (macOS 14.4+):

- Global mono `CATapDescription` → `AudioHardwareCreateProcessTap` → a private **aggregate device** → a realtime **IOProc block** that downmixes f32 to mono and pushes `AudioChunk::Samples` into the shared `run_consumer` (which resamples to 16 kHz).
- Uses the **Audio‑Recording TCC permission** (`NSAudioCaptureUsageDescription`), *not* Screen Recording — so **no purple banner**.
- Hardening: format validation, the **`EndOfStream`‑once** emission on stop (so the consumer's drain doesn't fall back to a 2 s timeout and truncate the tail), and an immediate `drop(block)` so the recorder is `Send`.

---

## 5a. Fase 3 — dual‑stream "meeting" capture + the menu‑bar "graffetta"

The product thesis: one click captures **both sides of a conversation** — the mic (you) and the Mac's system audio (the call) — and they land as a single speaker‑attributed transcript.

### Tracks, start, stop ([session.rs](../handy/src-tauri/src/managers/session.rs))
- A session is **`ActiveSession { tracks: Vec<Track>, wav_path }`**. Each `Track` = `{ recorder: ActiveRecorder, writer: JoinHandle, pcm_path, source }`. A solo track behaves exactly as Fase 0/1; two tracks are the meeting.
- **`start_sources(&[Source])`** is **best‑effort multi‑source**: it builds a track per source via `build_track` (recorder started *first*, so a failed start leaves no orphan file), skips any source that fails (system‑audio permission denied, nothing playing, unsupported target) with a warning, and errors **only if no track started**. This is the "seamless / self‑healing" capture — one click records the mic, and the system audio too whenever available.
  - **Deadlock‑safety (learned the hard way):** `start_sources` `drop(guard)`s the `active` mutex **before** emitting `SessionStateChanged`. The tray listener (lib.rs) runs *inline on the same thread* and calls `is_active()`, which re‑locks the non‑reentrant `std::sync::Mutex`; emitting under the held guard deadlocks the start path. `stop()` is naturally safe (it `.take()`s into a temporary). A review caught this — the unit tests don't traverse the emit→listener path, so it stayed green while the live app would hang. **Always drop the lock before emitting an event whose listener may re‑enter the manager.**
- **`toggle_sources`** / `toggle`/`start` are thin wrappers; the menu‑bar action passes `[Mic, SystemAudio]`.
- **`stop()`** tears down every track (drain → join writer → collect `(pcm_path, source)`), emits `active:false` immediately for UI snappiness, then spawns `finalize_session` off‑thread.
- Per‑track PCM naming: `session-{id}.mic.session.pcm` / `…system.session.pcm` (`source_suffix`), so recovery can tell sources apart. The session's single output is `session-{id}.wav`.

### Mixing + finalize
- **`mix_tracks(&[&[f32]])`** sums the equal‑rate mono tracks **by reference** (never cloning multi‑hour buffers), pads shorter tracks with silence, and clamps to `[-1,1]` → one playable 16 kHz WAV (you hear the whole meeting). *(Plain sum+clamp; a soft limiter is the named upgrade path.)*
- **`finalize_session`** is a **cleanup guard** around `finalize_session_inner`: it **always** deletes the source PCMs afterward (success, discard, or error) so a deterministic failure can never make `recover_interrupted` re‑finalize the same files (and re‑insert a row) every startup.
- `finalize_session_inner`: decode each PCM → if all empty, discard → mix + `save_wav_file` → **create a pending history row immediately** (`save_pending_entry`, status `transcribing`) so the session shows in History the instant you stop → if a model is resident, transcribe each track: **single track → diarize + `align`; dual → mic labelled `"Me"`, system diarized** → `merge_segments` interleaves them chronologically → **`drop_bleed`** removes the mic's echo of the system audio (speaker bleed, §6.1) → `save_segments` + `update_transcription` flips status to `done`/`failed` (the flat transcript is rebuilt from the de‑duped segments, so the bleed copy is gone from it too). A per‑track transcription error is graceful (partial transcript, still `done`); only an all‑failed session becomes `failed`.

### The "graffetta" (menu‑bar) ([tray.rs](../handy/src-tauri/src/tray.rs), [lib.rs](../handy/src-tauri/src/lib.rs))
- The tray menu has a **`toggle_session`** item whose label flips between `startRecording`/`stopRecording` based on `SessionManager::is_active()`; present in both the Idle and Recording menus.
- Its handler spawns a thread and calls `toggle_sources([Mic, SystemAudio])` — the one‑click meeting capture.
- A **`SessionStateChanged::listen`** in `lib.rs` keeps the tray icon (Idle/Recording) in sync however a session is toggled — tray, CLI, or the Sessions panel — making session state single‑sourced.

---

## 5b. Seamless auto‑capture — per‑process trigger + supervisor (live‑validated E2E 2026‑07‑05)

The "it just works" layer: the app quietly senses when **another app** is playing audio (a call, a meeting) and records it as a meeting session with no click. **Opt‑in** (`auto_capture_enabled`, default `false`) until a real‑meeting validation; the menu‑bar icon becomes an **ear** (`TrayIconState::Listening`, `resources/tray_listening.png`) whenever any session records — the honest signal.

Three layers, strictly separated (all `unsafe` in the sensor; all decisions pure):

```
output_sensor.rs (FFI, macOS)  →  AutoCaptureDecider (pure)  →  run_supervisor (I/O shell)  →  SessionManager
   "who is emitting audio?"        debounced start/stop           probation · cooldown · manual‑respect
```

### Sensor — [audio_toolkit/audio/output_sensor.rs](../handy/src-tauri/src/audio_toolkit/audio/output_sensor.rs)
- **`external_output_active() -> bool`** — true iff **any process other than ours** is emitting audio. Composition: `list_process_output()` (FFI snapshot) → `any_external_output(procs, own_pid)` (pure, unit‑tested).
- FFI: enumerates CoreAudio **process objects** — `kAudioHardwarePropertyProcessObjectList` on the system object (id 1), then per object `kAudioProcessPropertyPID` and `kAudioProcessPropertyIsRunningOutput`. All via `objc2_core_audio` (already a dependency; constants included — no hand‑rolled FourCCs, no new crates). Same macOS 14.4+ floor as the process tap. Plain property reads: **no tap, no TCC permission, no recording indicator**.
- **Why per‑process (the v1 post‑mortem):** v1 read the *device‑level* `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default output device. From inside this app, once our own tap had **ever** been opened, that flag reads "running" **forever** → 17/17 idle auto‑starts were empty false‑starts and the trigger was shelved. Per‑process attribution + **own‑PID exclusion** removes the root cause *by construction*: we cannot wake ourselves. (Approach spotted dormant in Meetily's `system_detector.rs`; reimplemented clean‑room on raw `objc2_core_audio`.)
- Error posture: any OSStatus error → empty list → `false` → auto‑capture simply never triggers (safe default). Non‑macOS: stub `false` ([audio/mod.rs](../handy/src-tauri/src/audio_toolkit/audio/mod.rs)).
- Tests: 4 pure unit tests (incl. `our_own_output_never_triggers` — the old bug, pinned) + **2 ignored live‑acceptance tests** (`cargo test --lib output_sensor -- --ignored --test-threads=1`): own tap open + silent machine → stays `false`; external `afplay` → `true`. Both passed on this machine 2026‑07‑05.

### Brain — [managers/auto_capture.rs](../handy/src-tauri/src/managers/auto_capture.rs) `AutoCaptureDecider`
Pure state machine `Idle → Arming → Capturing → Trailing`, fed `(audio_present, dt)` per tick, returns `AutoAction::{None, StartCapture, StopCapture}`. Debounce both ways: a blip shorter than `start_after` never starts; a gap shorter than `stop_after` never splits one conversation into many. 6 unit tests. `wants_capture()`/`reset()` let the shell reconcile with manual toggles.

### Supervisor — `run_supervisor` (same file; spawned from `lib.rs`)
Polls the sensor, runs the decider, drives `SessionManager`. Guarantees:
- **Opt‑in gate:** setting off → idles (and cancels a session it had started).
- **Never fights manual control:** stands down whenever a session it didn't start is active; only auto‑stops sessions it auto‑started.
- **Probation** (defense in depth, kept even though the sensor no longer false‑triggers): an auto‑started session must show captured system‑audio RMS (`session.system_audio_heard()`) within `PROBATION`, else `session.cancel()` discards it — **no junk History rows**.
- **START vs STOP signals differ deliberately:** START = per‑process sensor; STOP = captured‑audio silence (`session.system_audio_idle() < 800ms` means "still audible"). A meeting app holds its output stream open even while nobody talks, so "is the app outputting" cannot detect the end of a call — captured silence can.
- **Cooldown** after any auto session ends (finalized or discarded) before sensing resumes.
- When it fires it records a **meeting** (`[Mic, SystemAudio]`) — the mic only ever joins a session that system audio triggered; bare‑mic auto‑record stays a separate explicit opt‑in (privacy posture).

### Parameters (single place: consts atop `auto_capture.rs`)
| Const | Value | Meaning / rationale |
| --- | --- | --- |
| `POLL_INTERVAL` | 250 ms | sensor sampling period (property reads are ~free) |
| `START_AFTER` | 1200 ms | audio must persist before auto‑start — rejects notification pings |
| `STOP_AFTER` | 4 s | captured silence before auto‑finalize — tolerates pauses in a call |
| `PROBATION` | 2000 ms | auto session must hear real audio or be discarded |
| `COOLDOWN` | 8 s | stand‑down after any auto session ends (post‑teardown settle) |
| in‑session presence | `system_audio_idle() < 800 ms` | "still audible" threshold on the captured track |
| `auto_capture_enabled` | `false` | opt‑in gate ([settings.rs](../handy/src-tauri/src/settings.rs); serde default + explicit `get_default_settings()` constructor — update **both** when touching settings) |

### E2E evidence (2026‑07‑05, this machine)
Setting temporarily on → app `--start-hidden` → quiet: no trigger → external `afplay` ≈8 s: auto‑start ≈1.4 s in (`session started (probation)`) → `real audio confirmed` → inter‑sound gaps absorbed by `Trailing` → silence → `speakers quiet → session finalized` → `history.db` **row 79**, status `done`, both tracks 172 800 samples (10.8 s @16 kHz), empty transcript (system *sound*, no speech — correct). Setting restored to `false` afterwards.

---

## 6. Fase 2 — speaker diarization + the dual‑stream merge ("who said what")

Built **domain‑first** (pure logic tested in isolation, then adapters around it).

### 6.1 Pure core — `diarization.rs`
Types `SpeakerTurn`, `AsrSegment`, `TimedSegment` + three pure functions ([managers/diarization.rs](../handy/src-tauri/src/managers/diarization.rs)):
- **`align(asr, turns)`** — assign each ASR segment the speaker whose turn overlaps it **most** (deterministic tie‑break to the lower id; no overlap → `speaker_id: None` = graceful "unknown"). Produces `TimedSegment` with `speaker_label: None`.
- **`label_segments(asr, label)`** — tag every segment of a track with a fixed name (the mic track of a meeting is all `"Me"`); `speaker_id` left `None` so the name is authoritative.
- **`merge_segments(tracks)`** — flatten N tracks and **stable‑sort by `start_ms`** → one chronological timeline. Equal‑timestamp ties keep track order (mic before system). This is how a meeting becomes a single "who said what across both sides" transcript.
- **`drop_bleed(segments, mic_label)`** — removes **acoustic‑bleed duplicates**: when a meeting plays through the **speakers** (no headphones), the mic re‑captures the system audio, so one person appears as both `"Me"` and a diarized speaker. A `"Me"` segment whose time overlaps another speaker's segment with ≥70% word overlap is that echo and is dropped; genuinely distinct mic speech (you actually talking) is kept. *(The real fix is acoustic echo cancellation on the mic input — the named upgrade path; this is the cheap, no‑DSP mitigation. This is purely ours — riffado has no capture, no dual‑stream, hence no analogue.)*
- `TimedSegment` now carries `speaker_label: Option<String>` which, when `Some`, overrides the diarizer‑index→"Speaker N" generation at persist time.
- **Unit‑tested** in isolation (overlap/tie‑break/unknown for `align`; labelling, chronological interleave, stable‑tie, empty for the merge; echo‑drop / keep‑distinct / time‑gated for `drop_bleed`).

### 6.2 Engine adapter — `DiarizationManager`
Wraps `sherpa_onnx::OfflineSpeakerDiarization` (cfg‑gated). Loads `segmentation.onnx` (pyannote‑3.0) + `embedding.onnx` (NeMo TitaNet‑small) from `<app_data>/models/diarization/` → `Vec<SpeakerTurn>` (ms).
- **Safe‑by‑default:** `diarize()` no‑ops unless both model files exist.
- `DiarizationManager::{SUBDIR, SEG_FILE, EMB_FILE}` are the **single source of truth** for filenames (downloader, bundled‑install, engine all reference them; guarded by a test).

### 6.3 ASR with timings — `transcribe_with_segments` ([transcription.rs](../handy/src-tauri/src/managers/transcription.rs))
`transcribe_inner` returns `(String, Vec<AsrSegment>)`; `transcribe` and `transcribe_with_segments` are one‑line wrappers. `to_asr_segments` converts transcribe‑rs seconds → ms at that single boundary.

### 6.4 Read side + UI
- `get_session_segments(history_id) -> Vec<PersistedSegment>` ([commands/history.rs](../handy/src-tauri/src/commands/history.rs)).
- `SpeakerTimeline` + **speaker chips** in [HistorySettings.tsx](../handy/src/components/settings/history/HistorySettings.tsx) (see §9).

### 6.5 Models: download **and** bundle
- **Auto‑download:** `ModelManager::download_diarization_models()` fetches the two bare `.onnx` files, SHA256‑pinned. Commands `download_diarization_models` + `is_diarization_available`.
- **Bundled + auto‑install:** committed under `resources/models/diarization/`; `migrate_bundled_diarization_models()` copies them on first run → **clone → build → run → diarization works offline**.

> **Dual‑onnxruntime (closed):** `ort` (ASR) static‑links onnxruntime; sherpa uses `features = ["shared"]` (dylib) + `@loader_path` rpath to avoid the duplicate‑symbol clash. Two runtimes co‑loading was live‑validated with zero crashes.

---

## 7. Data model — `history.db`

Migrations live in [history.rs](../handy/src-tauri/src/managers/history.rs) as an append‑only `MIGRATIONS` list (tracked by `user_version`; **never edit a shipped migration**).

- **#1** base `transcription_history`; **#2–#4** post‑process columns; **#5** the diarization overlay (`speakers` + `transcription_segments` + index); **#6** the per‑row transcript **`status`** column.

```sql
-- transcription_history (after migration #6)
id, file_name, timestamp, saved, title, transcription_text,
post_processed_text, post_process_prompt, post_process_requested,
status TEXT NOT NULL DEFAULT 'done'      -- 'transcribing' | 'done' | 'failed'

CREATE TABLE speakers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  label TEXT NOT NULL,            -- "Me" (mic) or "Speaker N" (diarized), per history row
  embedding BLOB
);
CREATE TABLE transcription_segments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
  speaker_id INTEGER REFERENCES speakers(id) ON DELETE SET NULL,   -- NULL = unknown
  start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL,
  text TEXT NOT NULL, confidence REAL
);
CREATE INDEX idx_segments_history ON transcription_segments(history_id);
```

- **`TranscriptionStatus`** (`transcribing`/`done`/`failed`, serde `lowercase`, `specta::Type`) flows through `HistoryEntry.status` to the UI. The one‑shot dictation path and every pre‑existing row are `done` via the column default (no code change). Long‑form sessions move `transcribing → done/failed`, filling the silent gap between Stop and the finished row.
- **`save_pending_entry` / `save_entry`** share a private `insert_entry`; **`update_transcription`** now also sets `status`. Both emit `HistoryUpdatePayload` (`added`/`updated`).
- **`write_segments`** uses **two independent speaker namespaces** sharing the `speakers` table: explicit `speaker_label` (e.g. `"Me"`) deduped by string, and diarizer indices deduped by id → `"Speaker N"` in first‑seen order. They number independently, so "Me" never shifts the remote "Speaker 1/2…" sequence.
- **`fail_stale_transcribing()`** (startup self‑healing): flips any row still `transcribing` to `failed` (a finalize that died with a previous process), called before `recover_interrupted`.
- `get_history_entries` is **keyset‑paginated** (`id < cursor ORDER BY id DESC`). `ON DELETE CASCADE` requires `PRAGMA foreign_keys = ON` per connection (`get_connection` sets it).

---

## 8. The local MCP server — Claude over your private library

**Dir:** [handy/mcp/](../handy/mcp/) — a dependency‑free **Bun + `bun:sqlite`** server speaking **newline‑delimited JSON‑RPC 2.0 over stdio**, registered for Claude Code in repo‑root [.mcp.json](../.mcp.json).

- **Writes disabled** (read‑write open + `PRAGMA query_only = ON`) over `history.db` — SQLite itself rejects every write, so it can never alter a recording — and **stdio only, no network listener**, so the "nothing leaves the Mac" promise holds. Not `readonly: true`: the app keeps the DB in **WAL** mode and a readonly connection cannot create the `-shm`/`-wal` sidecars, so it fails with `SQLITE_CANTOPEN` whenever the app is closed (regression‑tested in `db.test.ts`).
- Tools ([db.ts](../handy/mcp/db.ts) query layer, [server.ts](../handy/mcp/server.ts) protocol):
  - **`list_sessions`** — recent sessions (id, title, timestamp, status, snippet, speaker labels).
  - **`get_session`** — one session's full transcript + speaker‑attributed segments.
  - **`search_sessions`** — case‑insensitive search across transcripts **and** segments, snippet centered on the hit.
- DB path defaults to `~/Library/Application Support/com.uppify.plaudy/history.db`; `PLAUDE_DB` overrides it (used by tests).
- **Security:** every query is parameterized (no SQL injection); table/column names are static; tool args only become bound params or integer limits. Verified by `bun test` (14 tests) and a piped JSON‑RPC smoke test, and run **live against the real `history.db`**.
- The hand‑rolled protocol (vs the SDK) is a deliberate dep‑avoidance choice; `@modelcontextprotocol/sdk` is the named upgrade path. Claude Desktop registration is in [handy/mcp/README.md](../handy/mcp/README.md).

---

## 9. Frontend

- **Sessions panel** ([settings/sessions/SessionsSettings.tsx](../handy/src/components/settings/sessions/SessionsSettings.tsx)) — a focused **hero capture experience**: one large record button (idle → ink‑on‑paper mic glyph; recording → red stop square inside a pulsing ring — red is reserved for recording/danger, so the color flip itself is the signal; same rule in `ui/Button.tsx`: `primary` = ink‑on‑paper, `danger` = red), a **Meeting / Microphone / System** segmented mode control (Meeting default → `commands.startMeeting()`; the others → `startSession(source)`), a live **elapsed timer**, and a calm privacy promise. `active` is driven by the `sessionStateChanged` event, so it's correct however a session is toggled. Registered as the `sessions` sidebar section in [Sidebar.tsx](../handy/src/components/Sidebar.tsx) (`SECTIONS_CONFIG`).
- **History view** ([settings/history/HistorySettings.tsx](../handy/src/components/settings/history/HistorySettings.tsx)) — `SpeakerTimeline` (speaker · time · text per segment) plus **speaker chips** (distinct labels at a glance), infinite scroll, optimistic delete + retry. The transcript area is status‑driven: `transcribing` → a "Transcribing…" pulse; `failed` → the retry hint; otherwise the text, or **"No speech detected"** for an empty `done` row.
- i18n keys added under `settings.sessions.*` (modeMeeting/Mic/System, tapToStart, capturing*, privacyNote, …), `settings.history.noSpeech`, and `tray.startRecording/stopRecording`. i18n is build‑blocking (ESLint).

---

## 10. Control surface (commands · events · CLI)

| Kind | Name | Notes |
| --- | --- | --- |
| Command | `start_session(source)` | single‑source session (→ `start_sources([source])`) |
| Command | `start_meeting()` | **dual** mic + system (→ `start_sources([Mic, SystemAudio])`) |
| Command | `stop_session()` / `is_session_active()` | |
| Command | `get_session_segments(id)`, `download_diarization_models`, `is_diarization_available` | |
| Event | `session-state-changed` | `{ active, source }` — drives UI + tray |
| Event | `historyUpdatePayload` | `added`/`updated`/`deleted`/`toggled` |
| CLI | `--toggle-session` / `--toggle-system-session` | single source (mic / system) |
| CLI | `--toggle-meeting` | dual mic + system — the graffetta action, for scripting/headless |
| Setting | `auto_capture_enabled` | opt‑in gate for the seamless auto‑capture supervisor (§5b); default `false` |
| Tray state | `TrayIconState::Listening` | ear icon while any session records (honest signal); dictation keeps the dot |

CLI flags forward to a running primary via the single‑instance plugin (`lib.rs`); all routes converge on `SessionManager`.

---

## 11. File‑by‑file map of our changes

**New files**
| File | What |
| --- | --- |
| `src-tauri/src/managers/session.rs` | Long‑form + dual‑stream session lifecycle (Fase 0/1/3) |
| `src-tauri/src/audio_toolkit/audio/system_audio.rs` | CoreAudio Process Tap system‑audio recorder (Fase 1) |
| `src-tauri/src/managers/diarization.rs` | `align` + `label_segments` + `merge_segments` pure core + `DiarizationManager` |
| `src-tauri/src/commands/session.rs` | `start_session` / `start_meeting` / `stop_session` / `is_session_active` |
| `src-tauri/src/managers/auto_capture.rs` | Seamless auto‑capture: pure `AutoCaptureDecider` + `run_supervisor` shell (§5b) |
| `src-tauri/src/audio_toolkit/audio/output_sensor.rs` | Per‑process "who is emitting audio?" sensor, own PID excluded (§5b) |
| `resources/tray_listening.png` | The menu‑bar ear (SF Symbol render) |
| `resources/models/diarization/{segmentation,embedding}.onnx` | Bundled diarization models |
| `handy/mcp/{db,server}.ts`, `db.test.ts`, `package.json`, `README.md` | Local MCP server |
| `.mcp.json` (repo root) | Registers the MCP server for Claude Code |

**Modified files**
| File | Why |
| --- | --- |
| `src-tauri/src/managers/model.rs` | Diarization model download + bundled auto‑install |
| `src-tauri/src/managers/transcription.rs` | `transcribe_with_segments` (refactor to `transcribe_inner`) |
| `src-tauri/src/managers/history.rs` | Migrations #5/#6, status column, segments/speakers persistence (dual namespace), `save_pending_entry`, `fail_stale_transcribing` |
| `src-tauri/src/tray.rs` | The menu‑bar "graffetta" `toggle_session` item |
| `src-tauri/src/commands/history.rs` | `get_session_segments`, status on retry |
| `src-tauri/src/audio_toolkit/audio/recorder.rs` | `with_chunk_sink` faithful tap + bounded‑RAM early‑return |
| `src-tauri/src/lib.rs` | manager wiring, tray `toggle_session` handler + `SessionStateChanged` listener, `--toggle-meeting`, `start_meeting` registration, `fail_stale_transcribing` at startup |
| `src-tauri/src/cli.rs` | `--toggle-session`, `--toggle-system-session`, `--toggle-meeting` |
| `src-tauri/Cargo.toml` / `.cargo/config.toml` / `build.rs` / `Info.plist` | sherpa+objc2 deps, rpath, stub hatch, audio‑capture usage string |
| `src/components/settings/sessions/SessionsSettings.tsx` | Hero capture UI (new dir) |
| `src/components/settings/history/HistorySettings.tsx` | `SpeakerTimeline` + speaker chips + status states |
| `src/components/Sidebar.tsx`, `settings/index.ts` | `sessions` sidebar section |
| `src/i18n/locales/{en,it}/translation.json` | sessions/history/tray keys |
| `src/bindings.ts` | regenerated (tauri‑specta): `start_meeting`, `TranscriptionStatus`, `SessionStateChanged`, status field |

---

## 12. Build, run & test

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5 HANDY_FORCE_AI_STUB=1
cd handy

bun tauri dev                       # full app (regenerates bindings.ts)
cd src-tauri && cargo test --lib    # 102 unit tests (align/merge/mix/drop_bleed, persistence+status, sessions, decider, sensor, …)
# + 2 ignored live‑acceptance tests for the auto‑capture sensor (real CoreAudio):
#   cargo test --lib output_sensor -- --ignored --test-threads=1
cd ../mcp && bun test               # 4 MCP query tests
cargo check --lib                   # fast type‑check     |    bun run lint   (i18n enforced)
```

Build specifics: **`HANDY_FORCE_AI_STUB=1`** forces the Apple Intelligence stub (CLT lacks the `@Generable` macro plugin); **`CMAKE_POLICY_VERSION_MINIMUM=3.5`** for whisper.cpp under CMake 4.x; **sherpa `shared` + `@loader_path` rpath** to coexist with `ort`'s static onnxruntime; **`bundle.resources = ["resources/**/*"]`** ships VAD + diarization models. A real installable `.app`/`.dmg` bundle (`tauri build`) needs **full Xcode** for signing/notarization — the release *binary* builds today.

**Live‑verification recipes** are in [HANDOFF.md §7](HANDOFF.md) (dual `--toggle-meeting` capture; the MCP `list_sessions` JSON‑RPC). The dual‑stream meeting capture was **validated live with real speech** (mic "Me" + system "Speaker 1", merged) on 2026‑06‑23.

App data: `~/Library/Application Support/com.uppify.plaudy/` (`history.db`, `recordings/`); log: `~/Library/Logs/com.uppify.plaudy/handy.log`.

---

## 13. Operational gotchas

1. **The CLI toggle needs a running primary.** `handy --toggle-meeting` (etc.) only works as a *second* instance forwarding to a live `bun tauri dev`; with no primary it boots its own instance and silently ignores the flag.
2. **Capture taps the *default output at session start*.** If system audio is muted/routed elsewhere, the tap records silence (→ empty transcript, graceful). Verify you can *hear* it.
2a. **Meeting on speakers bleeds into the mic.** When you capture a call through the laptop **speakers** (not headphones), the mic re‑captures the system audio, so one remote person would appear twice (`Me` + `Speaker N`). `drop_bleed` (§6.1) collapses that echo; for the cleanest separation use **headphones** (then `Me` = only your voice). For recording something you're only *listening* to, use **System** mode (no mic) rather than Meeting.
3. **An ASR model must be resident at `finalize`.** Diarization+transcription only run when `is_model_loaded()`; the model unloads on its idle timer, so a long gap before capturing yields an empty (but `done`) row. Keep `unload_timeout ≠ Immediately`, or warm it with one dictation first.
4. **Drop the lock before emitting a re‑entrant event** (the start‑path deadlock, §5a) — the listener runs inline and re‑locks the manager.
5. **Bindings‑export is dev‑only and non‑fatal** (logs and continues on a read‑only CWD).
6. **Never gate auto‑capture on the device‑level "running" flag.** `kAudioDevicePropertyDeviceIsRunningSomewhere` reads perpetually true inside this app once our tap has ever been opened (proved live: 17/17 empty false‑starts). Use the per‑process sensor (§5b) — and keep probation as the second net.
7. **Adding a settings field takes two edits:** the `#[serde(default)]` attribute *and* the explicit field in `get_default_settings()` (`settings.rs`). Forgetting the second compiles the serde path but ships wrong defaults for fresh installs.

---

## 14. What remains

| Item | Notes |
| --- | --- |
| **Auto‑capture: real‑meeting validation → flip default** | The trigger is fixed and E2E‑validated with synthetic audio (§5b). Validate once on a real Zoom/Meet call with speech, then decide whether `auto_capture_enabled` defaults to `true`. Optional refinement: an app **allowlist** (`kAudioProcessPropertyBundleID` is one more property read in `output_sensor.rs`) so only meeting apps trigger. |
| **Acoustic echo cancellation (AEC)** | `drop_bleed` removes the speaker‑bleed *duplicate* in the transcript, but the mic still *records* the echo into the mixed WAV. True AEC (subtract the system reference from the mic input, in `recorder.rs`/`session.rs`) would clean the audio itself and let `Me` capture only your voice even on speakers. Headphones sidestep it entirely today. |
| **AI title/summary — via MCP (decided 2026‑07‑05)** | No local LLM sidecar. The local MCP server is the path: the user's/client's own agents call `get_session` and produce title/summary on demand with their own subscription. Optional future: persist an agent‑produced title back (would need a tiny write surface or a sidecar table — currently MCP is read‑only by contract). |
| **Diarization download button** | Command exists; needs a UI home in the Sessions view (bundling already covers fresh clones). |
| **Installable app** | `tauri build` release binary works; a signed/notarized `.app`/`.dmg` needs full Xcode. |
| **iPhone target** | No iOS upstream. Recommended: iPhone‑as‑capture + Mac‑as‑brain over Apple's nearby transfer (MultipeerConnectivity / Network.framework peer‑to‑peer), dropping files into the recordings dir for the existing finalize pipeline. Needs full Xcode. |
| **Clustering threshold tuning** | Only if a rapid‑alternation recording over‑merges speakers. Lever: `OfflineSpeakerDiarizationConfig` in `diarization.rs`. |

---

*Last updated 2026‑07‑05 (added §5b seamless auto‑capture; 102 tests). Entry‑point: [HANDOFF.md](HANDOFF.md). Line‑cited forensics: [HANDOFF-FASE2.md](HANDOFF-FASE2.md) (Fase 2), [HANDOFF-AUTOCAPTURE.md](HANDOFF-AUTOCAPTURE.md) (auto‑capture trigger). riffado teardown verdict: [DECISIONS.md](DECISIONS.md).*
