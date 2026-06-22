# 00 — Gap Analysis: Handy → Plaude Local (Plaud alternative)

> **STATUS (2026-06-19):** Phase 0 (mic long-form sessions) and Phase 1 (macOS system/loopback capture) of the §3 roadmap are **DONE, adversarially reviewed, and demoed live**. Phase 2 (diarization) is next. This document is the *original plan* — for what is actually built and the next-agent plan, read [`../HANDOFF-FASE2.md`](../HANDOFF-FASE2.md) (it is the source of truth for current state).

> **Purpose.** Map the **target Plaud-style product** (local-first AI voice recorder: long-form + system/call audio capture, speaker diarization, multilingual ASR, AI summaries + "ask my recordings", a conversation library with timeline/segments/search/export, macBook↔iPhone sync, iPhone capture) onto **Handy as it exists today**, citing the exact functions/structs to reuse or extend.
>
> **TL;DR.** Handy gives us the *boring 60%* almost for free: device capture, VAD, resampling, an 8-engine ASR layer with panic isolation, a model download/verify/extract pipeline, an LLM post-process stack (cloud + Apple Intelligence), a typed IPC surface, and a SQLite+WAV history store. The *defining 40%* of a Plaud product — **system/loopback capture, diarization, long-form streaming, the segment/speaker data model, sync, and a real iPhone app** — is **net-new or deep-extension** work, and the iPhone path is where the Handy/Tauri base is weakest.
>
> Source of truth for every citation below: the sibling subsystem docs `01`–`12` in this folder, re-verified against `handy/src-tauri/src/...`.

---

## 1. Capability Matrix

Legend for columns:
- **Handy today** — what exists now.
- **Reuse as-is** — code we can call unchanged.
- **Needs extension** — existing code/struct to modify (cited).
- **Net-new build** — modules that do not exist at all.
- **Hardest risk** — the one thing most likely to hurt.

### 1.1 Capture

| Capability | Handy today | Reuse as-is | Needs extension | Net-new build | Hardest risk |
|---|---|---|---|---|---|
| **Microphone capture** | Full: cpal worker thread, native-rate → mono → 16 kHz/30 ms frames; `AudioRecorder` (`audio_toolkit/audio/recorder.rs`), orchestrated by `AudioRecordingManager` (`managers/audio.rs:146`). | `AudioRecorder::open/start/stop`; `create_audio_recorder` (`managers/audio.rs:120`); device enum `list_input_devices`. | — | — | None — solid. |
| **System / loopback audio** (meetings, app audio) | **None.** Input devices only; `build_stream` (`recorder.rs:224`) opens a cpal *input* stream and force-downmixes to mono (`recorder.rs:~250`). Entitlements grant only `device.microphone` + `device.audio-input` (`Entitlements.plist`). | The OS-agnostic consumer `run_consumer` (`recorder.rs:395`) — it doesn't care who the producer is. | `get_cpal_host` (`audio_toolkit/utils.rs:3`) + `device.rs`; add a "system audio" pseudo-device to `get_effective_microphone_device` (`managers/audio.rs:191`). | **macOS ScreenCaptureKit / CoreAudio process-tap producer** that emits `AudioChunk::Samples` into `run_consumer`; new `Entitlements.plist` + `NSScreenCaptureUsageDescription`. | **macOS capture API choice + entitlements + consent UX.** This is decision (a) §2. |
| **Call audio (both sides)** | None. | Same consumer. | Run two `FrameResampler`+pipelines (mic + system) tagged by channel/source. | Two-stream mux; a "merge/align" stage before transcription. | OS call-audio access (esp. iOS — see §2c) and legal **consent**. |
| **Import audio files** | **None** as a product feature. The *retry* path proves the plumbing: `retry_history_entry_transcription` (`commands/history.rs:64`) reads a WAV via `read_wav_samples` and runs `tm.transcribe` under `spawn_blocking`. | `read_wav_samples`; `spawn_blocking(tm.transcribe)` pattern. | — | `import_audio_file(path)` command: decode arbitrary formats (add `symphonia`) → 16 kHz mono `Vec<f32>` → existing transcribe/diarize/save path. | Format decoding breadth (m4a/mp3/opus) — `symphonia` covers most. Low risk. |
| **Long-form (hours) recording** | **None.** Whole recording is buffered in RAM: `processed_samples` grows unbounded until `stop()` returns one `Vec<f32>` (`recorder.rs:409,516`). VAD also *drops* silence, so timing is destroyed. 30 s lazy-close + short-clip padding are dictation-tuned. | `MicrophoneMode::AlwaysOn` (`update_mode`, `audio.rs:363`); `ModelUnloadTimeout::Never` (`settings.rs:213`) to keep the model hot. | Replace the RAM accumulator with a `with_chunk_sink(mpsc::Sender<Vec<f32>>)` builder mirroring `with_level_callback` (`recorder.rs:~57`); disable `lazy_stream_close`/`STREAM_IDLE_TIMEOUT`. | Rolling **disk writer** + chunked/streaming session object; crash-recovery/journaling. | **Crash resilience** for multi-hour RAM-only capture, and keeping the model resident without OOM. Decision (d)/(e) §2. |

### 1.2 Understanding (diarization, ASR, languages)

| Capability | Handy today | Reuse as-is | Needs extension | Net-new build | Hardest risk |
|---|---|---|---|---|---|
| **Speaker diarization (who said what)** | **None anywhere.** Audio is mono-downmixed (`recorder.rs:~250`); `transcribe` returns a flat `String` (`transcription.rs:701`, only `result.text` used); `HistoryEntry` is a flat blob (`managers/history.rs:56`). | VAD onset/hangover boundaries from `SmoothedVad` (`vad/smoothed.rs`) as **turn seeds**; the model download/verify/extract pipeline carries an ONNX diarizer "for free". | `LoadedEngine` enum (`transcription.rs:39`) + `EngineType` (`managers/model.rs:21`); add a 2nd decorator in the `Box<dyn VoiceActivityDetector>` chain built in `create_audio_recorder` (`audio.rs:124`). | **Diarization stage**: speaker-embedding ONNX (pyannote/3D-Speaker/sherpa-onnx) + clustering, slotted after VAD; a `TimedSegment{start_ms,end_ms,speaker_id,text}` carrier replacing the flat `Vec<f32>`/`String`. | **Engine choice + accuracy on overlapping speech + where it slots.** Decision (b) §2. |
| **Speaker labels / rename / merge** | None. | — | — | `speakers` table + `rename_speaker`/`merge_speakers` commands; UI. | Cluster→identity mapping stability across a session. |
| **Multilingual transcription** | **Strong.** 8 engines (Whisper + 7 ONNX) behind `LoadedEngine` (`transcription.rs:39`); per-engine language/param mapping; custom-word correction; filler/hallucination filtering; `catch_unwind` panic isolation. | `TranscriptionManager::transcribe` (`transcription.rs:440`) wholesale; engine catalog in `managers/model.rs`. | Add CoreML EP on Apple Silicon (ONNX family falls back to CPU on Mac today, `Cargo.toml:103`). | — | Perf for long-form (one-shot model) — see streaming, decision (e). |
| **Live / partial transcripts** | **None.** `transcribe(Vec<f32>)` is batch-only; transcript appears only post-stop. Overlay shows only mic-level + "transcribing…". | `MoonshineStreaming`/`StreamingModel` already referenced (`transcription.rs:572`) — a latent streaming seam. | Promote streaming engine to a first-class incremental API emitting `partial-transcript` events. | Chunked driver over `transcribe`; live caption UI. | Latency/quality of chunk-boundary stitching. |

### 1.3 Intelligence (summaries, action items, chat)

| Capability | Handy today | Reuse as-is | Needs extension | Net-new build | Hardest risk |
|---|---|---|---|---|---|
| **AI summaries / action items (templates)** | **Plumbing exists, product doesn't.** Generic single-utterance "Improve Transcriptions" prompt only. Full LLM stack: `post_process_transcription` (`actions.rs:66`), structured JSON-schema output (`actions.rs:195`, field literally named `transcription`), 8 cloud providers + Apple Intelligence FFI (`apple_intelligence.rs` / `.swift`), `LLMPrompt` templates with `${output}` (`settings.rs:641`). | `llm_client.rs` (stateless client, all providers); `process_text_with_system_prompt` (Apple); `LLMPrompt`/`PostProcessProvider` settings. | Widen schema `{transcription}` → `{summary,bullets,action_items,decisions,speakers}` (`actions.rs:195`, parse arm `actions.rs:219`, mirror Swift `@Generable` `apple_intelligence.swift:5`). | **Map-reduce windower** over a whole session (wrap `process_transcription_output` `actions.rs:349`); persisted summary artifacts; template library UI (reuse `PostProcessingSettings.tsx` CRUD). | **Token budgeting / chunking** of multi-hour transcripts; no HTTP timeout/retry/cancel in `create_client` (`llm_client.rs:100`) — a hung provider blocks forever. Apple FFI is synchronous on the Tokio task (`actions.rs:162`) — must wrap in `spawn_blocking`. |
| **"Ask my recordings" chat (RAG)** | **None.** No retrieval, no embeddings, no chat session. | The same `llm_client` for generation; Apple Intelligence for local. | — | **RAG subsystem**: embedding model (reuse the model download pipeline + a new `EngineType::Embedding`), a vector index (sqlite-vec/usearch), a chat command + UI. | Local embedding model quality + index over potentially thousands of segments; cross-recording retrieval. |
| **On-device / private by default** | **Partial.** Apple Intelligence path is on-device (macOS aarch64); keyless `custom` provider points at `http://localhost:11434/v1` (Ollama, `settings.rs:603`). | Make `custom`/Apple the default `post_process_provider_id` (`settings.rs:520`). | — | A bundled/managed local GGUF summarizer via `EngineType::Summarizer` + the file-model download path. | Quality vs. cloud on summaries; Apple Intelligence is macOS-aarch64 / iOS 26+ only. |

### 1.4 Library, sync, mobile, privacy

| Capability | Handy today | Reuse as-is | Needs extension | Net-new build | Hardest risk |
|---|---|---|---|---|---|
| **Conversation library: timeline, segments, timestamps, speakers** | **None.** History is **per-utterance, flat**: `transcription_history(... transcription_text TEXT NOT NULL)` (4 additive migrations, `managers/history.rs:21-33`); `HistoryEntry` (`history.rs:56`) has no segments/speakers/duration/timestamps. | Schema DDL pattern (`M::up`), `HistoryEntry` contract, the typed `HistoryUpdatePayload` event (`history.rs:44`) as a change feed. | Add a 5th `M::up` (`history.rs:34`): `duration_secs, sample_rate, channels, source, language, model`; update `HistoryEntry`, `map_history_entry` (`history.rs:199`), all SELECTs. | `transcription_segments(history_id, start_ms, end_ms, speaker_id, text)` + `speakers(id, history_id, label, embedding BLOB)` tables; segment-aware `save_entry`; timeline UI replacing the flat `<p>` (`HistorySettings.tsx:413`). | Migration discipline + threading segments end-to-end. Decision (e) §2. |
| **Search** | **None** beyond keyset pagination + a `saved` star. | `get_history_entries` (`history.rs:450`) as the query template. | — | FTS5 virtual table via `M::up`; `search_history` command + UI. | Low risk; SQLite FTS5 is built in. |
| **Export (SRT/VTT/JSON/MD)** | **None** (only "open recordings folder" + clipboard copy of last transcript). | — | — | Exporters from the new segment model. | Low risk *once* segments+timestamps exist (they don't today). |
| **Sync macBook ↔ iPhone** | **None.** 100% local/portable (`portable.rs`); only network feature is the GitHub updater. Hard integer device-local IDs; hard deletes; no UUID/`updated_at`/tombstones; secrets in cleartext (`SecretMap` only redacts Debug, `settings.rs:312`). | `HistoryUpdatePayload` emissions as a ready-made change feed; `reqwest` (json+stream) already a dep. | Add `uuid, updated_at, sync_state` columns; make `cleanup_old_entries` (`history.rs:330`) sync-aware (never prune un-synced). | **`SyncManager`** beside the existing managers (`lib.rs:147`); a sync server *or* peer protocol; OS keychain for tokens; conflict resolution. | **Conflict model + transport + auth + E2E encryption**, and a server/relay you now have to operate. Genuinely hard. |
| **iPhone capture app** | **Scaffolding only.** `#[cfg_attr(mobile, tauri::mobile_entry_point)]` (`lib.rs:316`), iOS icons, `gen/apple/PrivacyInfo.xcprivacy`. No iOS audio, no Xcode project, no mobile config; the entire tray/overlay/signal/paste/enigo surface is desktop-only; cpal iOS support is limited. | The transcribe-rs engine layer + VAD (`SileroVad`/`SmoothedVad` are cfg-clean) could run on-device; `send_transcription_input`/`TranscriptionCoordinator::send_input` are the platform-neutral trigger seam. | cfg-gate desktop-only surfaces (`lib.rs` tray/overlay/signals, `input.rs`). | **iOS audio capture (AVAudioEngine)**, background-audio entitlement, mobile lifecycle, mobile UI. | **The whole iPhone story.** Decision (c) §2 — this is where abandoning parts of the Tauri base is most likely. |
| **Privacy / encryption at rest** | **Weak.** WAVs + `history.db` are plaintext; API keys cleartext in the store. | — | — | Encrypted-at-rest store (SQLCipher/age) + OS keychain for keys/tokens; consent + retention policy for call recording. | Key management across two devices ties into sync. |

---

## 2. The Biggest Architectural Decisions / Risks

### (a) System / loopback + call audio capture

**The problem.** Handy is microphone-only by construction: `build_stream` (`recorder.rs:224`) opens a cpal **input** stream and downmixes to mono; `Entitlements.plist` grants only `device.microphone` + `device.audio-input`. Capturing "the other side" of a meeting/call is the single most defining Plaud capability and Handy has *zero* of it.

**macOS options (ranked):**
1. **ScreenCaptureKit audio capture** (macOS 13+, system + per-app audio, no kext). Best fidelity, official, but requires Screen Recording permission and `NSScreenCaptureUsageDescription`. Recommended primary.
2. **CoreAudio process tap / `CATapDescription`** (macOS 14.4+) — capture a specific process's output, lighter than full screen capture. Good for "record this call app".
3. **Virtual aggregate device** (BlackHole-style) — works on old macOS but requires user-installed driver. Fallback only.

**Where it slots — the good news:** the consumer `run_consumer` (`recorder.rs:395`) is **producer-agnostic**. A ScreenCaptureKit/CoreAudio producer only has to emit `AudioChunk::Samples` into the existing mpsc channel. For two-party calls, run **two** `FrameResampler`+`SmoothedVad` pipelines (mic + system) tagged by source, then merge. This is the cleanest extension point in the whole codebase. The hard parts are **entitlements + signing/notarization** (decision §4) and **consent UX** (recording calls is legally sensitive).

**iOS:** see (c) — the sandbox makes arbitrary system/call audio capture effectively **impossible**; this drives the iPhone strategy.

### (b) Local speaker diarization engine + pipeline slot

**Options that run locally (on-device, no cloud):**
- **sherpa-onnx** speaker segmentation + embedding (pyannote-segmentation-3 + 3D-Speaker/NeMo embeddings, ONNX). **Recommended**: it's ONNX → rides Handy's existing `ort` execution provider and the model download/verify/extract pipeline **with zero new download code**; mature offline clustering included.
- **pyannote.audio** (PyTorch) — best accuracy, but Python/torch runtime is a non-starter for a bundled Rust/Tauri app.
- **diart** (streaming/online diarization) — attractive for *live* speaker labels, but Python; treat as inspiration, not a dependency.

**Where it slots:** **after VAD, fused with transcription**, not before capture.
- VAD (`SmoothedVad`, `vad/smoothed.rs`) already produces onset/hangover boundaries — use those as **turn seeds** so diarization runs on confirmed-speech regions only (cheaper).
- Add a **second decorator** in the `Box<dyn VoiceActivityDetector>` chain assembled in `create_audio_recorder` (`audio.rs:124`), OR (cleaner) run diarization as a distinct stage in `run_consumer` (`recorder.rs:454`) operating on the *native-rate raw buffer* (the embedding model wants more than 16 kHz mono).
- Register it as `EngineType::Diarization` / `SpeakerEmbedding` (`managers/model.rs:21`) + a `LoadedEngine` arm (`transcription.rs:39`) — but note the **single-model-residency** constraint (`LoadedEngine` holds exactly one engine, `transcription.rs:39`): running ASR **and** a diarizer concurrently needs an architectural change (two managed engine slots, or sequential passes over the stored audio).
- Output: replace the flat `String`/`Vec<f32>` with `Vec<TimedSegment{start_ms,end_ms,speaker_id,text}>` threaded through `TranscribeAction::stop` (`actions.rs:492`) into the new segments table.

**Recommended v1 shape:** record raw → ASR with word/segment timestamps → diarize the stored audio → align speaker turns to ASR segments offline. Simpler and more accurate than live diarization; matches Plaud's "process after the meeting" model.

**Hardest risk:** overlapping speech and unknown speaker count; cluster→label stability within and across sessions.

### (c) iPhone strategy — the decisive fork

Three viable strategies:

1. **iPhone-as-capture-only, Mac does the heavy lifting (RECOMMENDED for v1).**
   - iPhone is a thin native **SwiftUI** recorder: AVAudioEngine mic capture (+ optional Voice Memo-style files), local storage, and **sync to the Mac**, which runs diarization/ASR/summaries.
   - Pros: sidesteps iOS's inability to capture system/call audio and the weight of porting transcribe-rs/ONNX/Metal to iOS; reuses 100% of the Mac pipeline. Matches the actual Plaud hardware model (dumb recorder + smart host).
   - Cons: requires the sync subsystem (decision §1.4) to exist; no offline transcription on the phone.

2. **Tauri-mobile (one codebase).**
   - `lib.rs:316` already has `mobile_entry_point`; Tauri 2 supports iOS. But: cpal iOS support is limited (need an AVAudioEngine bridge anyway), transcribe-rs needs a CoreML/Metal iOS build, and the **entire** tray/overlay/signal/enigo/clipboard surface is desktop-only and must be cfg-excluded. You get React UI reuse but pay full native-audio + native-permissions cost regardless.
   - Verdict: viable for the **library/review UI**, weak for capture.

3. **Fully native Swift app.**
   - Best iOS UX and the only path to on-device ASR on iPhone (CoreML Whisper / `WhisperKit`). Most code, least reuse, two front-ends to maintain.

**Recommendation:** **Strategy 1 now, optionally graft Strategy 2's React views for the library later.** The iPhone is where the Handy/Tauri base gives the least; do not let it block the Mac product. (Honest note in §4.)

### (d) Long-form streaming vs. Handy's one-shot model

Handy's model is **press hotkey → buffer everything in RAM → `stop()` returns one `Vec<f32>` → one `transcribe()` call** (`recorder.rs:409,516`; `transcription.rs:440`). For hours-long meetings this is unacceptable: unbounded RAM, no partial results, total loss on crash, and the VAD *discards silence* so pause timing/timestamps are destroyed.

**Target architecture:**
- **Capture:** add `with_chunk_sink(mpsc::Sender<Vec<f32>>)` to the recorder builder (mirror `with_level_callback`, `recorder.rs:~57`); a **rolling disk writer** appends to a growing Opus/WAV file instead of `processed_samples` (`recorder.rs:409`). Keep a **raw/ungated** parallel sink so the saved file is a faithful, replayable session (today VAD drops non-speech in `handle_frame`, `recorder.rs:~449`).
- **Transcription:** wrap `transcribe` in a **chunked driver** (e.g. 30–60 s windows with overlap) emitting `partial-transcript` events, with a final reconcile pass; or promote the latent `StreamingModel` (`transcription.rs:572`).
- **Session keep-alive:** `ModelUnloadTimeout::Never` (`settings.rs:213`) + the `is_recording()` touch (`transcription.rs:122`) so multi-hour sessions never idle-unload; disable `lazy_stream_close`/30 s idle close.
- **Crash recovery:** create the history row first (`status=recording`), checkpoint segments incrementally, recover an in-progress file on restart.
- **Coordinator:** the `Idle/Recording/Processing` machine (`transcription_coordinator.rs:27`) is single-binding and dictation-shaped; add a `Stage::LongRecording{session_id}` + a `SegmentFlush` command and explicit `start/pause/resume/stop` commands (today only shortcut-driven capture + global `cancel_operation` + `is_recording` exist).

**Risk:** keeping a model resident for hours without OOM/GPU pressure; chunk-boundary accuracy; making "store a faithful recording" and "VAD-gated transcription" coexist on one capture.

### (e) Data-model changes (segments / speakers / turns) to history SQLite

Today: one row per utterance, `transcription_text TEXT NOT NULL` (`history.rs:28`), 4 additive migrations, `HistoryEntry` flat (`history.rs:56`), fresh `Connection` per call, no WAL/`busy_timeout`, non-transactional multi-step mutations, hard integer IDs, hard deletes.

**Required changes (one coherent migration set):**
- **`M::up` #5** (`history.rs:34`): add to the parent row `duration_secs, sample_rate, channels, source(mic|system|call|import), language, model, device_label`, plus sync columns `uuid, updated_at, sync_state`, plus summary columns `summary, action_items(JSON), key_topics(JSON)` (or reuse the existing-but-unused `post_processed_text`/`post_process_prompt`).
- **New tables:** `transcription_segments(history_id FK, start_ms, end_ms, speaker_id, text, confidence)` and `speakers(id, history_id, label, embedding BLOB)`. Insert in **one transaction** in a segment-aware `save_entry` (`history.rs:219`).
- **FTS5** virtual table for search.
- **Hardening:** WAL pragma + `busy_timeout`, transaction boundaries (cleanup can currently orphan WAVs, `history.rs:330`; toggle_saved is a non-atomic SELECT+UPDATE), GC for orphaned audio files.
- **Plumb everywhere:** `map_history_entry` (`history.rs:199`), all SELECT column lists, the `HistoryEntry` `#[derive(Type)]` (so `bindings.ts` regenerates), and the React render (`HistorySettings.tsx:413`).

**Risk:** this single change is the **highest-leverage** item for Plaud parity *and* the one that touches the most files; get the schema right once.

---

## 3. Phased Roadmap

> Each phase lists **Handy files to touch** and **net-new modules**. Phases are demo-able end-to-end (walking-skeleton discipline). Build commands per `handy/BUILD.md` (`bun install && bun tauri dev`); macOS Intel needs ONNX Runtime via Homebrew. **Toolchain not yet installed — Phase 0 starts there.**

### Phase 0 — Walking skeleton (prove the spine, no new capability)
**Goal:** build Handy locally, add explicit recording-lifecycle commands and a long-form session that writes a faithful file + a segment-less transcript to a new schema. One mic, no diarization, no system audio.
- **Touch:** `Cargo.toml` (CoreML EP on macOS aarch64, `Cargo.toml:103`); `transcription_coordinator.rs:27` (add `Stage::LongRecording{session_id}`); `commands/audio.rs` (+ `start/pause/resume/stop_session`); `recorder.rs:~57` (`with_chunk_sink`), `recorder.rs:409` (rolling disk writer), `recorder.rs:~449` (parallel raw sink); `managers/history.rs:34` (`M::up` #5: duration/source/uuid/updated_at + WAL/busy_timeout); `actions.rs:492` (`TranscribeAction::stop` → chunked driver).
- **New:** `managers/session.rs` (long-form session FSM + crash-recovery journal); `audio_toolkit/audio/disk_writer.rs`.
- **Demo:** record 30 min from the mic → faithful file on disk → full transcript in a session row → survives a kill -9.

### Phase 1 — System/loopback capture (macOS)
**Goal:** capture a meeting (mic + system audio) as two tagged tracks.
- **Touch:** `Entitlements.plist` + `Info.plist` (ScreenCaptureKit / `NSScreenCaptureUsageDescription`); `audio_toolkit/utils.rs:3` + `audio/device.rs`; `managers/audio.rs:191` (system-audio pseudo-device); `commands/audio.rs` (`get_available_capture_sources`, `set_capture_source`); `GeneralSettings.tsx:31` (source selector); `tauri.conf.json` signing config.
- **New:** `audio_toolkit/audio/sc_kit_macos.rs` (ScreenCaptureKit/CoreAudio-tap producer → `AudioChunk::Samples`); two-stream mux.
- **Demo:** record a Zoom/FaceTime call, both sides captured.

### Phase 2 — Diarization + the segment/speaker data model
**Goal:** "who said what" with timeline UI.
- **Touch:** `managers/model.rs:21` (`EngineType::Diarization`/`SpeakerEmbedding` + catalog entries as `is_directory` tar.gz); `transcription.rs:39` (`LoadedEngine` arm; address single-residency — likely a sequential pass over stored audio); `recorder.rs:454` (diarization stage on native-rate buffer) or a post-capture pass; `managers/history.rs:219` (segment-aware `save_entry` + `transcription_segments`/`speakers` tables, `M::up` #6); `HistorySettings.tsx:413` (speaker-labeled segment list); `bindings.ts` (regenerate).
- **New:** `managers/diarization.rs`; alignment module (turns ↔ ASR segments); `rename_speaker`/`merge_speakers` commands.
- **Demo:** a 2-speaker recording renders Speaker A/B with timestamps; rename A → "Alice".

### Phase 3 — AI summaries, action items, templates, search, export
**Goal:** meeting-level intelligence + a real library.
- **Touch:** `actions.rs:195` (widen schema → summary/action_items/decisions), `actions.rs:219` (parse), `actions.rs:349` (map-reduce windower over full session); `apple_intelligence.swift:5` (`@Generable` mirror) + `actions.rs:162` (wrap Apple FFI in `spawn_blocking`); `llm_client.rs:100` (add timeout/retry/cancel); `PostProcessingSettings.tsx` (template CRUD reuse); `managers/history.rs` (summary columns + FTS5 `M::up` + `search_history`).
- **New:** `summarize_session` command; exporters (`export/{srt,vtt,json,md}.rs`); search UI; per-entry "Summarize" action in `HistorySettings.tsx:360`.
- **Demo:** one click → summary + action items; full-text search; export SRT.

### Phase 4 — "Ask my recordings" (local RAG)
**Goal:** chat over the library, on-device.
- **Touch:** `managers/model.rs:21` (`EngineType::Embedding`); reuse `llm_client`/Apple for generation.
- **New:** `managers/rag.rs` (embed segments, sqlite-vec/usearch index, retrieve+generate); chat command + UI.
- **Demo:** "what did Alice commit to last Tuesday?" → cited answer.

### Phase 5 — Sync + privacy hardening
**Goal:** macBook library is encrypted, syncable.
- **Touch:** `managers/history.rs:330` (sync-aware cleanup), `history.rs:42` (use `HistoryUpdatePayload` as change feed); `settings.rs:312` (OS keychain for tokens/keys); `lib.rs:147` (register `SyncManager`).
- **New:** `managers/sync.rs` + `commands/sync.rs` (`sync_status/sync_now/set_sync_config/pair_device`); encryption-at-rest (SQLCipher/age); a sync relay/server *or* peer protocol.
- **Demo:** library encrypted at rest; change feed produced.

### Phase 6 — iPhone capture app (Strategy 1)
**Goal:** phone records, Mac processes, library syncs both ways.
- **Touch:** cfg-gate desktop-only surfaces (`lib.rs` tray/overlay/signals, `input.rs`); reuse the segment schema + `HistoryUpdatePayload` as the wire format.
- **New:** native **SwiftUI** capture app (AVAudioEngine, background-audio entitlement, local store, sync client) consuming the Phase 5 sync API. (Optionally reuse React library views via Tauri-mobile later.)
- **Demo:** record on iPhone → appears transcribed+diarized in the Mac library.

---

## 4. Hard Truths

### What Handy gives us nearly for free
- **Mic capture + conditioning.** cpal worker thread, native-rate → mono → exact 16 kHz/30 ms frames, FFT level meter, drain-until-`EndOfStream` zero-loss stop. `AudioRecorder` + `AudioRecordingManager` are genuinely good (`recorder.rs`, `managers/audio.rs:146`).
- **A producer-agnostic consumer.** `run_consumer` (`recorder.rs:395`) doesn't care who feeds it — the cleanest seam for system-audio and iOS producers.
- **VAD.** `SileroVad` + `SmoothedVad` are platform-agnostic, cfg-clean, and their onset/hangover boundaries double as diarization turn seeds.
- **An 8-engine multilingual ASR layer** with per-engine param mapping and `catch_unwind` panic isolation — months of work we don't redo (`transcription.rs:39,440,526`).
- **A full model lifecycle** — resumable/cancellable download, SHA-256 verify, tar.gz extract, atomic rename — that is **engine-agnostic**, so a diarizer/embedding/summarizer model rides it with *no new download code* (`managers/model.rs`).
- **An LLM post-process stack** — 8 cloud providers + on-device Apple Intelligence + structured JSON output + prompt templates — so summaries are "new prompts + a wider schema", not new plumbing (`actions.rs:66,195`; `llm_client.rs`; `apple_intelligence.*`).
- **A typed IPC surface** (~95 tauri-specta commands → auto-generated `bindings.ts`) and a typed change-feed event (`HistoryUpdatePayload`) that is a ready-made sync hook.
- **Local SQLite + WAV history** with additive migrations and a portable-aware data dir — the right *shape*, just too flat.
- **An i18n'd React shell + Apple Intelligence Swift bridge build (build.rs)** and packaging for all desktop formats.

### What is genuinely hard (and where the base may have to be abandoned for parts)
1. **iPhone is the real cliff.** Handy's iOS support is *icons + one attribute* (`lib.rs:316`). iOS **cannot capture system/call audio** (sandbox) — Plaud's core mode is impossible on-device — and the entire tray/overlay/signal/enigo/clipboard/CLI surface is desktop-only. **Expect to build a native Swift capture app** (abandoning the Tauri base for the phone's capture layer) and use the Mac as the brain. Tauri-mobile can host the *library UI* but not solve capture.
2. **System/call audio on macOS is an entitlements + signing + consent project, not just code.** ScreenCaptureKit/CoreAudio-tap is doable, but it forces a real **Developer-ID signing + notarization** pipeline (today `signingIdentity: "-"`, `tauri.conf.json:43`) and a legally careful consent UX for call recording.
3. **Diarization accuracy is never "done".** Overlapping speech, unknown speaker counts, and cross-session identity are open problems even for pyannote. sherpa-onnx gets us 80%; the last 20% is research-grade. Also, `LoadedEngine` holds exactly one engine (`transcription.rs:39`) — running ASR + diarizer concurrently is an architectural change, not a config flag.
4. **Long-form breaks Handy's deepest assumption.** Everything from `processed_samples` (`recorder.rs:409`) to the coordinator FSM (`transcription_coordinator.rs:27`) to `transcribe(Vec<f32>)` (`transcription.rs:440`) assumes "short, buffered, one-shot, paste-into-app". Making it streaming + crash-resilient + faithful (raw retention, not VAD-gated) + segment-timestamped is the most invasive cross-cutting change.
5. **Sync is a whole product, not a feature.** No UUIDs, no `updated_at`, no tombstones, hard deletes, cleartext secrets, single-blob settings rewrites. Real sync means a schema overhaul, an OS-keychain migration, E2E encryption, conflict resolution, **and a server/relay you must operate** — the largest net-new surface after iPhone.
6. **Robustness debt to pay down before scaling.** `lock().unwrap()` with no poison recovery, `expect`/`assert` in the resampler (`resampler.rs:18,24`), silently-dropped resampler errors, fail-open VAD with no telemetry, no DB WAL/`busy_timeout`/transactions, no HTTP timeout in `llm_client` (`llm_client.rs:100`). Each is harmless at dictation scale and a real bug at multi-hour, multi-stream, syncing scale.
7. **Per-platform GPU reality.** macOS compiles only the Metal Whisper backend; the ONNX family (Parakeet/Canary/diarizers) falls back to **CPU on Mac** (`Cargo.toml:103`) — painful for long-form and diarization until a CoreML EP is added.

**Bottom line:** Build the Mac product on Handy — it's a strong scaffold and ~60% of the way there. Treat **system-audio capture, diarization, long-form streaming, the segment/speaker schema, and sync** as five real projects (Phases 1–5), and treat the **iPhone as a separate native capture app** (Phase 6) rather than a Tauri port. Do not let the phone block the Mac.
