# Handy Architecture — 05: Transcription Engines & Coordinator

> **Abstract.** This document forensically dissects the speech-to-text core of Handy: the `TranscriptionManager` (the engine owner / lifecycle controller), its CI mock twin `transcription_mock.rs`, and the `TranscriptionCoordinator` (a single-threaded state machine that serialises all record→transcribe→paste lifecycle events). The `TranscriptionManager` wraps the external `transcribe-rs` crate, which provides eight concrete inference back-ends (Whisper.cpp plus seven ONNX-runtime models: Parakeet, Moonshine, Moonshine-Streaming, SenseVoice, GigaAM, Canary, Cohere). The subsystem owns: a single hot-loaded model behind a `Mutex`, a background idle-watcher thread that unloads the model after a configurable timeout, a panic-isolation layer (`catch_unwind`) so a crashing engine never poisons the app, GPU/accelerator preference plumbing, and per-engine language/parameter mapping. Audio enters as a `Vec<f32>` (16 kHz mono PCM) and exits as a cleaned `String`. The coordinator sits one layer up, debouncing hotkeys and gating concurrent recordings. This file documents every public type and function with `file:line` citations, the threading/locking model, error paths, persisted state touched, platform `cfg` gates, and — critically — the concrete extension points and gaps for re-targeting Handy into a Plaud-style long-form, multi-speaker, summarising, cloud-syncing, mobile recorder.

---

## 1. Files in this subsystem — responsibilities

| File | Path | Responsibility |
|------|------|----------------|
| `transcription.rs` | `src-tauri/src/managers/transcription.rs` | The real `TranscriptionManager`: owns the loaded engine, model lifecycle (load/unload/idle-unload), per-engine dispatch, accelerator settings, GPU device enumeration. |
| `transcription_mock.rs` | `src-tauri/src/managers/transcription_mock.rs` | CI-only no-op stand-in copied over `transcription.rs` during `test.yml` so test runs avoid whisper/Vulkan native deps. |
| `transcription_coordinator.rs` | `src-tauri/src/transcription_coordinator.rs` | Single-threaded command-serialiser (state machine `Idle → Recording → Processing`) that turns raw hotkey/signal events into `start()`/`stop()` action calls without races. |

Tightly-coupled neighbours read for data-flow (not the subsystem proper, but cited):
- `src-tauri/src/actions.rs` — `TranscribeAction` (the `ShortcutAction` that the coordinator drives; calls `tm.transcribe()`).
- `src-tauri/src/managers/model.rs` — `EngineType` enum + `ModelInfo`; model file resolution.
- `src-tauri/src/settings.rs` — `ModelUnloadTimeout`, accelerator enums, language/custom-word settings.
- `src-tauri/src/audio_toolkit/text.rs` — `apply_custom_words`, `filter_transcription_output` post-processing.
- `src-tauri/src/commands/history.rs` — `retry_history_entry_transcription` (the other `transcribe()` caller).
- `src-tauri/src/shortcut/handler.rs`, `signal_handle.rs`, `utils.rs` — feed events into the coordinator.

---

## 2. Types, enums, traits, and public functions (with citations)

### 2.1 `transcription.rs`

#### `struct ModelStateEvent` — `transcription.rs:31-37`
Serializable event payload emitted to the frontend over the Tauri `"model-state-changed"` channel. Fields: `event_type: String` (`"loading_started" | "loading_completed" | "loading_failed" | "unloaded"`), `model_id: Option<String>`, `model_name: Option<String>`, `error: Option<String>`. Mirrored in the mock at `transcription_mock.rs:12-18`.

#### `enum LoadedEngine` — `transcription.rs:39-48`
The private sum type holding exactly one live inference engine. Variants and their backing `transcribe-rs` types:
- `Whisper(WhisperEngine)` — whisper.cpp (GGML), GPU via Vulkan/Metal.
- `Parakeet(ParakeetModel)` — NVIDIA Parakeet TDT (ONNX).
- `Moonshine(MoonshineModel)` — Useful Sensors Moonshine (ONNX).
- `MoonshineStreaming(StreamingModel)` — streaming Moonshine variant.
- `SenseVoice(SenseVoiceModel)` — Alibaba SenseVoice (ONNX).
- `GigaAM(GigaAMModel)` — Russian-focused GigaAM (ONNX).
- `Canary(CanaryModel)` — NVIDIA Canary multilingual (ONNX).
- `Cohere(CohereModel)` — Cohere/Aya ASR (ONNX).

This enum is the **single most important extension surface** in the subsystem (see §8). One-of-N at a time; never more than one model resident.

#### `struct LoadingGuard` — `transcription.rs:52-55` + `Drop` `transcription.rs:57-63`
RAII guard returned by `try_start_loading()`. On `Drop` it locks `is_loading`, sets it `false`, and `notify_all()` on the condvar — guaranteeing the loading flag resets even on early-return or panic. Mock equivalent is a zero-sized no-op (`transcription_mock.rs:21`, returned at `:40-42`).

#### `struct TranscriptionManager` — `transcription.rs:65-76`
`#[derive(Clone)]` — all fields are `Arc`, so clones share state. Fields:
- `engine: Arc<Mutex<Option<LoadedEngine>>>` — the hot model. `None` = unloaded.
- `model_manager: Arc<ModelManager>` — to resolve paths, download status, supported languages.
- `app_handle: AppHandle` — for `emit()` and reading settings/state.
- `current_model_id: Arc<Mutex<Option<String>>>` — id of the resident model.
- `last_activity: Arc<AtomicU64>` — epoch-ms of last use; drives idle unload.
- `shutdown_signal: Arc<AtomicBool>` — stops the watcher thread.
- `watcher_handle: Arc<Mutex<Option<JoinHandle<()>>>>` — the idle watcher thread.
- `is_loading: Arc<Mutex<bool>>` + `loading_condvar: Arc<Condvar>` — load-in-progress gate that transcription waits on.

**Public / notable functions:**

- `fn new(app_handle, model_manager) -> Result<Self>` — `transcription.rs:79-164`. Builds the struct and **spawns the idle-watcher thread** (`thread::spawn` at `:97`). The watcher loops every 10 s (`:100`), bails on `shutdown_signal`, skips when `ModelUnloadTimeout::Immediately` (handled elsewhere, `:113`), keeps the idle timer fresh while `AudioRecordingManager::is_recording()` (`:119-125`), and otherwise unloads the model when `idle_ms > limit_ms` (`:127-156`).
- `fn lock_engine(&self) -> MutexGuard<Option<LoadedEngine>>` — `transcription.rs:167-172`. Poison-tolerant lock: on `PoisonError` it logs and `into_inner()` to recover (defensive even though `transcribe()` uses `catch_unwind` to avoid poisoning).
- `fn is_model_loaded(&self) -> bool` — `transcription.rs:174-177`.
- `fn try_start_loading(&self) -> Option<LoadingGuard>` — `transcription.rs:183-193`. Atomic test-and-set of `is_loading`; returns `Some(guard)` if it claimed the load slot, `None` if a load is already running.
- `fn unload_model(&self) -> Result<()>` — `transcription.rs:195-226`. Drops the engine (`*engine = None`), clears `current_model_id`, emits `"unloaded"`.
- `fn now_ms() -> u64` — `transcription.rs:228-233`. Wall-clock epoch ms.
- `fn touch_activity(&self)` — `transcription.rs:236-238`. Resets `last_activity` to now.
- `fn maybe_unload_immediately(&self, context: &str)` — `transcription.rs:241-251`. If `ModelUnloadTimeout::Immediately` and a model is loaded, unload right now. Called after each transcription and on empty audio.
- `fn load_model(&self, model_id: &str) -> Result<()>` — `transcription.rs:253-413`. The big one. Emits `"loading_started"`; looks up `ModelInfo` (`get_model_info`, errors "Model not found"); rejects if `!is_downloaded` (emits `"loading_failed"`); resolves `model_path`; then a **`match model_info.engine_type`** (`:302-380`) constructs the right engine. Note the per-engine load knobs: Whisper `WhisperEngine::load(&path)` (`:304`); Parakeet/SenseVoice/GigaAM/Canary/Cohere load with `&Quantization::Int8`; Moonshine with `MoonshineVariant::Base` + `Quantization::default()`; MoonshineStreaming with a `0` look-ahead arg + default quant. On success stores the engine + id, calls `touch_activity()`, emits `"loading_completed"`.
- `fn initiate_model_load(&self)` — `transcription.rs:416-433`. Fire-and-forget background load of `settings.selected_model` **if** not already loading/loaded. Spawns a thread, sets/clears `is_loading`, and `notify_all()`s the condvar.
- `fn get_current_model(&self) -> Option<String>` — `transcription.rs:435-438`.
- `fn transcribe(&self, audio: Vec<f32>) -> Result<String>` — `transcription.rs:440-733`. **The core inference path** (full walk-through in §4 and §5).
- `fn apply_accelerator_settings(app: &AppHandle)` — `transcription.rs:738-769` (free function). Maps `WhisperAcceleratorSetting`/`OrtAcceleratorSetting`/`whisper_gpu_device` settings onto `transcribe_rs::accel` global atomics (`set_whisper_accelerator`, `set_whisper_gpu_device`, `set_ort_accelerator`). Called at startup (`lib.rs:160`) and on settings change.
- `struct GpuDeviceOption { id, name, total_vram_mb }` — `transcription.rs:771-776`; `static GPU_DEVICES: OnceLock<Vec<GpuDeviceOption>>` — `:778`; `fn cached_gpu_devices() -> &'static [GpuDeviceOption]` — `:780-803` (enumerates GGML/Vulkan devices once; **skips enumeration on x86_64 CPUs lacking FMA3** to avoid a SIGILL crash, `:788-792`).
- `struct AvailableAccelerators { whisper, ort, gpu_devices }` — `transcription.rs:805-810`; `fn get_available_accelerators() -> AvailableAccelerators` — `:813-828`. Reports which back-ends are compiled in.
- `impl Drop for TranscriptionManager` — `transcription.rs:830-854`. Only the **last** clone (`Arc::strong_count(&self.engine) == 1`, guarded at `:838`) signals shutdown and joins the watcher; earlier clones (from `initiate_model_load`/watcher) drop harmlessly.

### 2.2 `transcription_mock.rs`
Same public surface, all no-ops: `is_model_loaded()->false` (`:36-38`), `transcribe()->Ok(String::new())` (`:60-62`), `load_model`/`unload_model`/`initiate_model_load` trivially succeed. `apply_accelerator_settings` is a no-op (`:66`); `get_available_accelerators` returns empty vecs (`:83-89`). Swapped in by `.github/workflows/test.yml:35` (`cp src/managers/transcription_mock.rs src/managers/transcription.rs`).

### 2.3 `transcription_coordinator.rs`

- `const DEBOUNCE: Duration = 30ms` — `transcription_coordinator.rs:10`.
- `enum Command` — `transcription_coordinator.rs:13-24`. The messages sent to the coordinator thread: `Input { binding_id, hotkey_string, is_pressed, push_to_talk }`, `Cancel { recording_was_active }`, `ProcessingFinished`.
- `enum Stage` — `transcription_coordinator.rs:27-31`. The lifecycle owned solely by the thread: `Idle`, `Recording(String /*binding_id*/)`, `Processing`.
- `struct TranscriptionCoordinator { tx: Sender<Command> }` — `transcription_coordinator.rs:36-38`. Public handle; only holds the channel sender.
- `fn is_transcribe_binding(id: &str) -> bool` — `transcription_coordinator.rs:40-42`. True for `"transcribe"` and `"transcribe_with_post_process"`.
- `fn new(app: AppHandle) -> Self` — `transcription_coordinator.rs:45-117`. Creates an `mpsc::channel`, spawns the worker thread (wrapped in `catch_unwind`, `:49`), runs the `while let Ok(cmd) = rx.recv()` loop (`:53`). Handles debounce of rapid presses (`:63-70`), push-to-talk vs toggle semantics (`:72-92`), `Cancel` (won't reset during `Processing`, `:98-102`), and `ProcessingFinished` (`:104-106`).
- `fn send_input(&self, binding_id, hotkey_string, is_pressed, push_to_talk)` — `transcription_coordinator.rs:121-140`.
- `fn notify_cancel(&self, recording_was_active: bool)` — `transcription_coordinator.rs:142-152`.
- `fn notify_processing_finished(&self)` — `transcription_coordinator.rs:154-158`.
- `fn start(app, stage, binding_id, hotkey_string)` — `transcription_coordinator.rs:161-175` (free fn). Looks up `ACTION_MAP[binding_id]`, calls `action.start(...)`, and **only** transitions to `Recording` if `AudioRecordingManager::is_recording()` actually became true (`:167-174`).
- `fn stop(app, stage, binding_id, hotkey_string)` — `transcription_coordinator.rs:177-184`. Calls `action.stop(...)` and sets `Stage::Processing`.

---

## 3. Threading & concurrency model

There are **four** distinct threads/contexts touching this subsystem:

1. **Idle-watcher thread** — spawned in `TranscriptionManager::new` (`transcription.rs:97`). Wakes every 10 s, reads settings, and may call `unload_model()`. Holds a *clone* of the manager (keeps `engine` strong-count ≥ 2). Stopped via `shutdown_signal` (`AtomicBool`) and joined in `Drop` only by the final clone (`transcription.rs:838-852`).
2. **Background loader thread** — `initiate_model_load` (`transcription.rs:424`) spawns a thread that calls `load_model`, then flips `is_loading` and notifies the condvar.
3. **Coordinator thread** — `TranscriptionCoordinator::new` (`transcription_coordinator.rs:48`). The *only* thread that mutates `Stage`; all hotkey/signal/cancel events funnel through `mpsc` so there are no cross-thread `Stage` races.
4. **Async transcription task** — `TranscribeAction::stop` (`actions.rs:516`) does `tauri::async_runtime::spawn` and inside calls the **blocking** `tm.transcribe(samples)` (`actions.rs:548`). The retry path uses `spawn_blocking` (`commands/history.rs:87`).

**Locks / sync primitives:**
- `engine: Mutex<Option<LoadedEngine>>` — the model. Critically, `transcribe()` **takes** the engine out of the mutex (`engine_guard.take()`, `transcription.rs:514`), **drops the guard** (`:524`), and runs inference with *no lock held* (`:526-634`), then re-inserts it (`:639-640`). This means a long transcription never blocks `is_model_loaded()` / the watcher.
- `is_loading: Mutex<bool>` + `loading_condvar: Condvar` — `transcribe()` waits here (`:464-467`) until any in-flight load completes; `initiate_model_load` and the `LoadingGuard` are the notifiers.
- `current_model_id: Mutex<Option<String>>`, `last_activity: AtomicU64`, `shutdown_signal: AtomicBool` — fine-grained, short-held.
- **Panic isolation:** the engine call is wrapped in `catch_unwind(AssertUnwindSafe(...))` (`transcription.rs:526`). On panic the engine is **not** put back (effectively unloaded), `current_model_id` is cleared, an `"unloaded"` event with the panic message is emitted, and an error returned (`:643-681`). The coordinator thread is likewise `catch_unwind`-wrapped (`transcription_coordinator.rs:49`).

---

## 4. Data flow IN and OUT

**Inbound trigger chain (record → transcribe):**
```
OS hotkey (rdev/tauri)  ─┐
Unix SIGUSR1/2 / CLI    ─┤→ shortcut/handler.rs:38  is_transcribe_binding()
signal_handle.rs:17     ─┘     → TranscriptionCoordinator::send_input()  (mpsc)
                                  → coordinator thread (Stage machine)
                                     → start()/stop()  → ACTION_MAP["transcribe*"]
                                        → TranscribeAction::start  (actions.rs:390)
                                             → tm.initiate_model_load()  (actions.rs:399)
                                             → rm.try_start_recording()
                                        → TranscribeAction::stop   (actions.rs:492)
                                             → rm.stop_recording() -> Vec<f32>
                                             → tm.transcribe(samples) (actions.rs:548)
```

**`transcribe(audio: Vec<f32>)` internal flow** (`transcription.rs:440-733`):
1. Debug-only forced-failure hook `HANDY_FORCE_TRANSCRIPTION_FAILURE` (`:441-446`).
2. `touch_activity()` (`:449`). Empty audio → `maybe_unload_immediately` + `Ok("")` (`:455-459`).
3. **Wait for any in-flight load** via condvar (`:464-467`); error if still no engine (`:469-472`).
4. Read settings; **validate `selected_language`** against `ModelInfo.supported_languages`, falling back to `"auto"` if unsupported (`:480-503`).
5. `take()` the engine, drop the lock, run inference under `catch_unwind`. The big `match &mut engine` (`:528-632`) maps Handy's language string + settings onto each engine's native params:
   - **Whisper** (`:529-557`): `zh-Hans`/`zh-Hant` → `"zh"`; `auto` → `None`; passes `translate: settings.translate_to_english` and `custom_words` joined as `initial_prompt`.
   - **Parakeet** (`:558-568`): `TimestampGranularity::Segment`.
   - **Moonshine / MoonshineStreaming** (`:569-576`): default `TranscribeOptions`, English-only.
   - **SenseVoice** (`:577-595`): language whitelist `zh/en/ja/ko/yue`, `use_itn: true`.
   - **GigaAM** (`:596-598`): default options.
   - **Canary** (`:599-613`): `auto`→`None`, honours `translate_to_english`.
   - **Cohere** (`:614-631`): `zh-*`→`"zh"`.
6. On success re-insert engine (`:639-640`); on panic discard it (`:643-681`).
7. **Post-process the text** (outside the lock): `apply_custom_words` (skipped for Whisper since words go in as prompt, `:685-701`), then `filter_transcription_output` to strip fillers/hallucinations/stutters (`:703-708`, defined `audio_toolkit/text.rs:288-320`). Logs timing, `maybe_unload_immediately`, returns the cleaned `String`.

**Outbound:** the returned `String` flows back into `TranscribeAction::stop`, is optionally LLM post-processed (`process_transcription_output`, `actions.rs:586`), saved to history (`hm.save_entry`, `actions.rs:591`), and pasted on the main thread (`utils::paste`, `actions.rs:609-622`). In parallel the raw PCM is written to a WAV (`save_wav_file`, `actions.rs:542-543`) under the recordings dir.

**Events emitted to frontend:** `"model-state-changed"` (`ModelStateEvent`) for every lifecycle transition. The coordinator emits nothing itself; UI state (tray icon, overlay) is driven by the action layer.

---

## 5. Error handling & edge cases

- **Model not found / not downloaded** — `load_model` emits `"loading_failed"` and returns `Err` (`transcription.rs:271-285`).
- **Engine `::load` failure** — each arm wraps the error, emits `"loading_failed"`, returns `Err` (`:304-379`).
- **Model not loaded at transcribe time** — explicit errors at `:469-472` and `:514-521`.
- **Engine panic** — caught, engine dropped, model id cleared, `"unloaded"` emitted, error surfaced; next call reloads (`:643-681`). This is the headline robustness feature.
- **Mutex poisoning** — defended by `lock_engine` recovery (`:167-172`) and `current_model_id` `unwrap_or_else(into_inner)` (`:662-663`).
- **Empty audio** → empty string, not error (`:455-459`); empty result logged (`:724-728`).
- **Unsupported language** → silent fallback to auto (`:497-502`).
- **Idle unload while recording** — explicitly prevented by touching activity when `is_recording()` (`:119-125`).
- **GPU SIGILL on no-FMA3 CPUs** — enumeration skipped (`:788-792`).
- **Coordinator**: debounces ≤30 ms presses (`:63-70`); ignores presses while busy (`:88-90`); refuses to reset mid-`Processing` on cancel (`:98-102`); `FinishGuard` (`actions.rs:32-39`) guarantees `notify_processing_finished()` fires even if the async task panics. Channel-closed sends just `warn!` (`:138`,`:150`,`:156`).
- **WAV save** is awaited and verified independently of transcription; a failed transcription still saves an empty-text history entry so the user can retry (`actions.rs:630-643`).

---

## 6. State & persistence touched

- **Settings store** (tauri-plugin-store via `get_settings`/`write_settings`): reads `selected_model`, `selected_language`, `translate_to_english`, `custom_words`, `word_correction_threshold`, `custom_filler_words`, `app_language`, `model_unload_timeout`, `whisper_accelerator`, `ort_accelerator`, `whisper_gpu_device`, `always_on_microphone` (`settings.rs:353-430`).
- **Model files on disk**: resolved by `ModelManager::get_model_path`; whisper models are single `.bin` files, ONNX models are **directories** (`ModelInfo.is_directory == true`, `model.rs:279+`).
- **In-memory only**: the loaded engine, `current_model_id`, `last_activity`. No DB rows are written by this subsystem directly — transcripts/WAVs are persisted by `HistoryManager` (`actions.rs:591`) and the recordings dir, not here.
- **`transcribe-rs` global atomics**: accelerator prefs set via `accel::set_*` (`transcription.rs:748-767`) — process-global, not persisted by this module.

---

## 7. Platform-specific branches

- **x86_64 FMA3 guard** — `#[cfg(target_arch = "x86_64")]` + `is_x86_feature_detected!("fma")` to skip GPU enumeration (`transcription.rs:788-792`). The only `cfg` inside the subsystem proper.
- **Accelerator back-ends are compile-time**: which `OrtAccelerator` variants exist (`Cuda`/`DirectMl`/`Rocm`/`CpuOnly`) is decided by `transcribe-rs` build features per OS (macOS→Metal/CoreML & Vulkan, Windows→DirectML/Vulkan, Linux→Vulkan/ROCm/OpenBLAS per AGENTS.md). The mapping is in `apply_accelerator_settings` (`:760-767`).
- **Unix signal triggers** feed the coordinator (`signal_handle.rs`, gated `#[cfg(unix)]` at the call site `lib.rs:173-177`).
- **No iOS/Android code paths exist** in this subsystem — it is desktop-only (Tauri desktop). This is the central mobile gap (§9).
- The CI mock swap is effectively a build-variant branch (`test.yml:35`).

---

## 8. PLAUD relevance — concrete extension points

Handy is a *push-to-talk dictation* tool; Plaud is a *long-form, multi-speaker, summarised, synced, mobile recorder*. The following are the precise functions/structs to modify or wrap.

1. **Add new engines / a diarization or summary stage** — extend `enum LoadedEngine` (`transcription.rs:39-48`), add an `EngineType` variant (`model.rs:21-30`), a `load_model` match arm (`transcription.rs:302-380`), and a `transcribe` match arm (`:528-632`). This is the canonical place to slot in a speaker-diarization model (e.g. pyannote/sherpa-onnx diarizer) as a parallel engine.
2. **Long-form / streaming capture** — `transcribe(audio: Vec<f32>)` (`:440`) is fundamentally **batch**: it takes the *entire* recording at once. For hour-long meetings, wrap it in a chunked driver that slices the incoming PCM stream into windows, calls `transcribe` per window, and stitches results — or promote `MoonshineStreaming`/`StreamingModel` (`:572`) to a first-class incremental API that emits partial transcripts as `"partial-transcript"` events. The `TranscriptionResult` already carries segment timestamps for Parakeet (`TimestampGranularity::Segment`, `:560`) — surface those instead of discarding everything but `.text` (`:641`, `:700`).
3. **Multi-speaker / diarization output** — today `transcribe` returns a flat `String` and **drops timestamps and segments** (only `result.text` is used, `:701`). To support "Speaker A / Speaker B" transcripts, change the return type to a structured `TranscriptSegments { speaker, start, end, text }` and thread it through `TranscribeAction::stop` (`actions.rs:574-628`) and `HistoryManager::save_entry`.
4. **System / call audio capture** — out of scope of this file (lives in `managers/audio.rs`), but the watcher's `is_recording()` check (`transcription.rs:119-125`) and the coordinator's `Stage::Recording` gate (`transcription_coordinator.rs:73-86`) both assume a single mic source; a Plaud product needs a *loopback / system-audio* source plus mic-mix, which means the coordinator's single-`binding_id` `Recording(String)` state must become multi-stream-aware.
5. **AI summaries** — Handy already has an LLM post-process hook (`process_transcription_output` → `post_process_transcription`, `actions.rs:66+`, using `llm_client.rs`). A Plaud-style "meeting summary / action items" feature should reuse this path but run *after* the full transcript exists, not per-utterance. The `transcribe_with_post_process` binding (`actions.rs:709`) is the template.
6. **Cloud / local sync** — the subsystem writes nothing to the cloud. The natural hook is at history-save time (`actions.rs:591`) and the WAV write (`actions.rs:542`); a sync layer would watch the recordings dir + history DB. No code here today.
7. **Mobile (iPhone)** — none of this compiles for iOS. The cleanest reuse is the `transcribe-rs` engine layer itself (whisper.cpp/ONNX run on-device on iOS), wrapped behind a Swift/FFI shim that mirrors `transcribe()`'s param-mapping logic (`:528-632`). The coordinator/state-machine pattern (`transcription_coordinator.rs`) translates directly to a mobile `AVAudioSession`-driven controller.
8. **Idle-unload tuning for long sessions** — the watcher (`:97-159`) is built for short bursts; a recorder must never unload mid-meeting. `ModelUnloadTimeout::Never` (`settings.rs:213`) plus the existing `is_recording()` keep-alive (`:122`) already cover this, but a multi-hour session would benefit from an explicit "session lock" flag rather than relying on 10-s polling.

---

## 9. Gaps vs a Plaud-style product

- **Batch-only, no streaming/partials.** `transcribe` consumes the whole `Vec<f32>` and returns once (`transcription.rs:440-733`). No live/partial transcript, no progress for long recordings.
- **Timestamps & segments discarded.** Only `result.text` is used (`:701`); speaker turns, word/segment timing, and confidence are thrown away even when the engine produces them.
- **No speaker diarization.** No engine variant or post-stage identifies speakers; output is a flat string.
- **Single mic source only.** No system-audio/loopback capture, no call recording, no mic+system mixing. Coordinator state is single-stream (`Recording(String)`).
- **No long-form session model.** No concept of a "recording session" spanning chunks; WAV+transcript are one-shot per hotkey press.
- **No cloud sync / account / multi-device.** Everything is local; no upload, no remote transcripts.
- **No mobile target.** Desktop-Tauri only; zero iOS/Android code.
- **Summaries are generic post-process, not meeting-aware.** The LLM hook exists but isn't structured for action-items/speaker-attributed summaries.
- **Hard single-model residency.** `LoadedEngine` holds exactly one engine (`:39-48`); running ASR + a diarizer simultaneously requires architectural change (two slots or a pipeline).
- **No retention/encryption policy for audio at rest** in this subsystem (WAVs are plain files; retention lives in `RecordingRetentionPeriod`, `settings.rs:158-164`, enforced elsewhere).
