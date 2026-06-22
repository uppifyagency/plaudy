# 06 — Model Management (Download · Verify · Extract · Load/Unload)

> **Abstract.** Handy's model-management subsystem owns the full lifecycle of every speech-to-text model: it maintains an in-memory catalog of ~16 predefined models plus auto-discovered custom `.bin` files, downloads them over HTTP (with resume, cancellation, and throttled progress events), verifies them with SHA-256, extracts directory-based models from `.tar.gz` archives atomically, deletes them, and answers "where is this model on disk?" queries. The catalog and all disk I/O live in `ModelManager` (`src-tauri/src/managers/model.rs`); the Tauri command surface that the React frontend invokes lives in `src-tauri/src/commands/models.rs`. The subsystem owns *acquisition and storage* of model files only — actual in-memory loading/unloading of inference engines is delegated to `TranscriptionManager` (`src-tauri/src/managers/transcription.rs`), which calls back into `ModelManager::get_model_path` / `get_model_info`. This document is a forensic, line-cited reference and a Plaud-oriented extension map.

---

## 1. Files & responsibilities

| Path | Responsibility |
|---|---|
| `src-tauri/src/managers/model.rs` | The whole model catalog + file lifecycle: hard-coded `ModelInfo` registry, custom-model discovery, download (resume/cancel/throttle), SHA-256 verify, tar.gz extract, delete, path resolution, on-disk status reconciliation, two one-time migrations. |
| `src-tauri/src/commands/models.rs` | Thin Tauri `#[command]` wrappers exposing the manager to the frontend (`get_available_models`, `download_model`, `delete_model`, `set_active_model`, `cancel_download`, status queries) + the shared `switch_active_model` helper used by both IPC and the tray. |
| `src-tauri/src/portable.rs` (collaborator) | Provides `app_data_dir()`, the portable-aware root under which `models/` is created. Determines whether models live in `%APPDATA%/.../models` or `<exe>/Data/models`. |
| `src-tauri/src/managers/transcription.rs` (consumer) | Loads/unloads the actual inference engines. Calls `ModelManager::get_model_info` + `get_model_path`. Owns the idle-unload watcher and the `is_loading` mutex/condvar. Not part of this subsystem but the primary downstream consumer. |
| `src-tauri/src/settings.rs` (collaborator) | `get_settings` / `write_settings` — persists `selected_model`, `selected_language`, `model_unload_timeout`. The manager reads/writes `selected_model` during auto-select and stale-clear. |
| `src-tauri/src/lib.rs` (wiring) | `ModelManager::new(app_handle)` constructed once in `initialize_core_logic` (lib.rs:150-151), wrapped in `Arc`, registered as managed state (lib.rs:164), commands registered in the `invoke_handler` (lib.rs:394). |
| `src/stores/modelStore.ts` (frontend consumer) | Zustand store that invokes the commands and subscribes to every `model-*` event to drive the UI. |

---

## 2. Core types

### 2.1 `EngineType` enum — `model.rs:20-30`
```rust
pub enum EngineType { Whisper, Parakeet, Moonshine, MoonshineStreaming, SenseVoice, GigaAM, Canary, Cohere }
```
Tags each model with the inference backend. Drives the big `match` in `TranscriptionManager::load_model` (`transcription.rs:302-380`) that decides which engine `::load()` to call. `#[derive(Serialize, Deserialize, Type)]` → crosses the IPC boundary and is mirrored in `src/bindings.ts`.

### 2.2 `ModelInfo` struct — `model.rs:32-53`
The catalog record for one model. Fields of note:
- `id` / `name` / `description` — identity + UI display.
- `filename` — **on disk name**. For file models this is the `.bin` (e.g. `ggml-small.bin`); for directory models it is the *extracted directory name* (e.g. `parakeet-tdt-0.6b-v2-int8`), **not** the `.tar.gz`.
- `url: Option<String>` — download source; `None` for custom models (signals "not downloadable").
- `sha256: Option<String>` — expected digest; `None` skips verification (custom models).
- `size_mb: u64` — advertised size, used by the UI; real size comes from HTTP `content-length`.
- `is_downloaded` / `is_downloading` / `partial_size` — **runtime** state, reconciled from disk by `update_download_status`.
- `is_directory: bool` — the central branch: directory models are `.tar.gz` that get extracted; file models are moved into place as-is.
- `engine_type`, `accuracy_score`, `speed_score`, `supports_translation`, `is_recommended`, `supported_languages: Vec<String>`, `supports_language_selection`, `is_custom`.

### 2.3 `DownloadProgress` struct — `model.rs:55-61`
`{ model_id, downloaded: u64, total: u64, percentage: f64 }` — payload of the `model-download-progress` event. Consumed in `modelStore.ts:282` to compute a smoothed download speed (EWMA, `modelStore.ts:307-320`).

### 2.4 `DownloadCleanup<'a>` RAII guard — `model.rs:66-86`
Holds borrows of `available_models` + `cancel_flags` and a `model_id`. Its `Drop` impl (`model.rs:73-86`) sets `is_downloading = false` and removes the cancel flag **unless `disarmed`**. This guarantees consistent cleanup on every `?` / `return Err` path in `download_model` without manual cleanup at each exit. It is explicitly `disarmed` only on the success path (`model.rs:1305`) because success additionally sets `is_downloaded = true`.

### 2.5 `ModelManager` struct — `model.rs:88-94`
```rust
pub struct ModelManager {
    app_handle: AppHandle,
    models_dir: PathBuf,
    available_models: Mutex<HashMap<String, ModelInfo>>,
    cancel_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    extracting_models: Arc<Mutex<HashSet<String>>>,
}
```
- `models_dir` — `<app_data>/models`, created if missing (`model.rs:99-105`).
- `available_models` — the live catalog (predefined + custom), behind a `Mutex`.
- `cancel_flags` — per-model `AtomicBool`, set by `cancel_download`, polled by the download loop.
- `extracting_models` — set of model ids currently extracting, so `update_download_status` doesn't delete a live `.extracting` dir (`model.rs:736-743`).

---

## 3. Public/important functions (signatures + behavior + citations)

### Construction
- **`ModelManager::new(app_handle: &AppHandle) -> Result<Self>`** — `model.rs:97-639`. Creates `models_dir`; hard-codes the entire catalog of ~16 models into `available_models` (`model.rs:126-611`); calls `discover_custom_whisper_models` (`model.rs:614`); then runs, in order: `migrate_bundled_models` (`:627`), `migrate_gigaam_to_directory` (`:630`), `update_download_status` (`:633`), `auto_select_model_if_needed` (`:636`). **All catalog entries are literal `insert` calls — "TODO this should be read from a JSON file" at `model.rs:125`.**

### Catalog queries
- **`get_available_models(&self) -> Vec<ModelInfo>`** — `model.rs:641-644`. Clones all catalog values. Backs the `get_available_models` command.
- **`get_model_info(&self, model_id: &str) -> Option<ModelInfo>`** — `model.rs:646-649`. Single-model lookup; used heavily by `TranscriptionManager`.

### Migrations (run at every startup but idempotent)
- **`migrate_bundled_models(&self) -> Result<()>`** — `model.rs:651-676`. Copies any model bundled in `resources/models/` (currently only `ggml-small.bin`) into the user `models_dir` if absent. Uses `app_handle.path().resolve(..., BaseDirectory::Resource)`.
- **`migrate_gigaam_to_directory(&self) -> Result<()>`** — `model.rs:681-720`. Migrates the legacy single-file `giga-am-v3.int8.onnx` to the new `giga-am-v3-int8/` directory layout (`model.int8.onnx` + `vocab.txt` copied from bundled resources). Required by the `transcribe-rs 0.3.x` upgrade. No-op if old file absent or new dir exists (`:685-687`).

### Status reconciliation
- **`update_download_status(&self) -> Result<()>`** — `model.rs:722-772`. Walks every catalog entry and sets `is_downloaded` / `is_downloading=false` / `partial_size` from disk reality. For directory models it also **garbage-collects leftover `<filename>.extracting` dirs** from interrupted extractions — but only if the model is not currently in `extracting_models` (`:736-743`). This is the reconciliation point that makes disk the source of truth.
- **`auto_select_model_if_needed(&self) -> Result<()>`** — `model.rs:774-814`. Two responsibilities: (1) clears `settings.selected_model` if it references a model no longer in the catalog (e.g. a deleted custom file) — `:779-792`; (2) if no model is selected, picks the first *downloaded* model and persists it via `write_settings` — `:794-811`. **Selection order is HashMap iteration order — non-deterministic, and `is_recommended` is not consulted.**

### Custom-model discovery
- **`discover_custom_whisper_models(models_dir: &Path, available_models: &mut HashMap<String, ModelInfo>) -> Result<()>`** (associated fn) — `model.rs:818-935`. Scans `models_dir` for non-hidden `.bin` files that are not a predefined Whisper filename and not already in the map; synthesizes a `ModelInfo` with `url=None`, `sha256=None`, `is_downloaded=true`, `is_custom=true`, both scores `0.0` (UI sentinel hides score bars), empty `supported_languages`. Display name is title-cased from the id (`:881-892`). Unit-tested at `model.rs:1481-1579`.

### Verification
- **`verify_sha256(path: &Path, expected_sha256: Option<&str>, model_id: &str) -> Result<()>`** (associated fn) — `model.rs:941-970`. `None` → no-op (custom). On mismatch or read error it **deletes the partial file** and returns an error mentioning "corrupt", so the next attempt restarts clean. Unit-tested at `model.rs:1593-1648`.
- **`compute_sha256(path: &Path) -> Result<String>`** (associated fn) — `model.rs:973-985`. Streams the file in 64 KiB chunks (`[0u8; 65536]`) into a `Sha256` hasher → lowercase hex. Chunked to handle models up to ~1.7 GB without loading into RAM.

### The download state machine
- **`async fn download_model(&self, model_id: &str) -> Result<()>`** — `model.rs:987-1325`. The heart of the subsystem. Phases:
  1. **Lookup & guards** (`:988-1012`): resolve `ModelInfo`, require `url`; if the final file already exists, clean any `.partial` and return early.
  2. **Resume detection** (`:1015-1022`): if `<filename>.partial` exists, `resume_from = its size`.
  3. **Mark downloading + register cancel flag** (`:1024-1037`) and arm the `DownloadCleanup` guard (`:1041-1046`).
  4. **HTTP request** (`:1049-1056`): `reqwest::Client`, adds `Range: bytes=<resume_from>-` when resuming.
  5. **Range-unsupported fallback** (`:1061-1074`): if we asked to resume but got `200 OK` (not `206`), the server ignored the Range header → delete partial, reset `resume_from=0`, refetch from scratch (prevents corruption from appending a full body to a partial).
  6. **Status check** (`:1077-1084`): non-2xx & non-206 → error.
  7. **Stream to `.partial`** (`:1093-1158`): opens append-or-create; emits an initial progress event; loops over `response.bytes_stream()`, checking `cancel_flag` each chunk (cancel → drop file, keep partial for resume, return `Ok`), writing each chunk, and emitting throttled progress (max 10/s, 100 ms — `:1121-1123, :1148-1157`).
  8. **Size check** (`:1178-1190`): if `content-length` known and the file size mismatches → delete partial, error "Download incomplete".
  9. **SHA-256 verify off-thread** (`:1192-1208`): emits `model-verification-started`, runs `verify_sha256` inside `tokio::task::spawn_blocking` so the async runtime isn't stalled hashing ~1.6 GB, emits `model-verification-completed`.
  10. **Directory models → extract** (`:1211-1297`): see extraction sub-flow below.
  11. **File models → atomic move** (`:1298-1301`): `fs::rename(partial → final)`.
  12. **Success commit** (`:1303-1324`): disarm guard, set `is_downloaded=true / is_downloading=false / partial_size=0`, remove cancel flag, emit `model-download-complete`.

  **Extraction sub-flow** (`:1211-1297`): insert id into `extracting_models`; emit `model-extraction-started`; create temp dir `<filename>.extracting`; `GzDecoder` + `tar::Archive::unpack` into the temp dir. On unpack error: remove temp dir, **delete the corrupt `.partial`** (issue #858, `:1248`), remove from `extracting_models`, emit `model-extraction-failed`, return error. On success: if the archive contained exactly one top-level dir, `rename` that dir to the final location (strips the archive's wrapper dir); otherwise `rename` the temp dir itself (`:1264-1285`). Remove from `extracting_models`, emit `model-extraction-completed`, delete the `.tar.gz` partial.

- **`delete_model(&self, model_id: &str) -> Result<()>`** — `model.rs:1327-1395`. Removes the model file (`remove_file`) or directory (`remove_dir_all`) plus any `.partial`; errors "No model files found to delete" if nothing was removed (`:1375-1377`). Custom models are *removed from the catalog entirely* (no URL to re-download — `:1381-1384`); predefined models are kept and `update_download_status` re-run. Emits `model-deleted`.
- **`get_model_path(&self, model_id: &str) -> Result<PathBuf>`** — `model.rs:1397-1440`. The contract used by `TranscriptionManager::load_model`. Refuses if `!is_downloaded`, if `is_downloading`, or if the final file/dir is absent or a `.partial` still exists. Returns `models_dir/filename`.
- **`cancel_download(&self, model_id: &str) -> Result<()>`** — `model.rs:1442-1472`. Sets the model's `AtomicBool` cancel flag (`store(true, Relaxed)`), eagerly sets `is_downloading=false` for UI responsiveness, re-runs `update_download_status`, emits `model-download-cancelled`. **Does not delete the partial** → cancellation is resumable.

### Command layer (`commands/models.rs`)
- `get_available_models` (`:7-13`), `get_model_info` (`:15-22`) — straight pass-throughs.
- `download_model` (`:24-44`) — awaits `manager.download_model`; on error emits `model-download-failed` `{model_id, error}` then returns the `Err`.
- `delete_model` (`:46-69`) — **if deleting the active model**, first `transcription_manager.unload_model()` and clear `settings.selected_model`, then `manager.delete_model`.
- `switch_active_model(app, model_id) -> Result<(),String>` (`:77-155`) — **shared** by `set_active_model` and the tray. Claims the load slot via `try_start_loading()` (returns "Model load already in progress" if busy — `:84-86`); validates downloaded; persists `selected_model` early; **resets `selected_language` to `"auto"` if the new model doesn't support the current language** (`:109-121`); if `model_unload_timeout == Immediately` it skips eager load and emits a `selection_changed` event (`:127-144`); otherwise `transcription_manager.load_model(model_id)`, reverting `selected_model` on failure (`:147-152`).
- `set_active_model` (`:157-166`), `get_current_model` (`:168-173`), `get_transcription_model_status` (`:175-181`), `is_model_loading` (`:183-191`), `has_any_models_available` (`:193-200`), `has_any_models_or_downloads` (`:202-210`), `cancel_download` (`:212-221`).

> ⚠️ **Two latent bugs worth flagging.** `is_model_loading` (`models.rs:185-191`) returns `current_model.is_none()` — i.e. it returns `true` when *no* model is loaded, the inverse of the name's intent. `has_any_models_or_downloads` (`:202-210`) ignores in-progress downloads despite its name/comment — byte-identical to `has_any_models_available`.

---

## 4. Threading / concurrency model

- **Async download**: `download_model` is `async`, driven by Tauri's tokio runtime. Multiple models can download concurrently (each command is its own task); per-model state is keyed by `model_id` in the shared maps.
- **Locks**: `available_models: Mutex<HashMap>` guards the catalog. Locks are taken in **short, scoped blocks** (e.g. `model.rs:1025-1030`, `:1306-1313`) and dropped before any `.await` — never held across `.await`, avoiding `Send`/deadlock issues. `cancel_flags` and `extracting_models` are `Arc<Mutex<...>>`.
- **Cancellation channel**: a polled `Arc<AtomicBool>` per model, not a tokio channel. The loop checks `cancel_flag.load(Relaxed)` once per streamed chunk (`model.rs:1128`); `cancel_download` sets it (`:1449`).
- **Blocking offload**: SHA-256 hashing runs in `tokio::task::spawn_blocking` (`model.rs:1200-1204`) so the multi-hundred-MB hash doesn't block the reactor. A panic there surfaces as "SHA256 task panicked".
- **Progress throttling**: `Instant`-based, max one `model-download-progress` event per 100 ms (`model.rs:1122-1123, 1148`).
- **Load serialization (downstream)**: engine load is serialized by `TranscriptionManager`'s `is_loading: Mutex<bool>` + `loading_condvar`, claimed via `try_start_loading()` (`transcription.rs:183-193`), released by a `LoadingGuard` on drop. `switch_active_model` uses it to reject overlapping tray double-clicks (`models.rs:84-86`).
- **Poison recovery**: `TranscriptionManager::lock_engine` recovers from a poisoned engine mutex (`transcription.rs:167-172`). `ModelManager` itself uses plain `.lock().unwrap()` everywhere and would propagate a panic on poison.

---

## 5. Data flow in / out

**Inbound:**
- Frontend `modelStore.ts` → Tauri commands `commands::models::*` → `ModelManager` methods (registered in `lib.rs:394` block).
- System tray menu → `switch_active_model` (shared helper, `models.rs:77`).
- `TranscriptionManager::load_model` → `ModelManager::get_model_info` + `get_model_path` (`transcription.rs:269-287`).
- Startup wiring: `lib.rs:150-151` constructs; `lib.rs:164` registers.

**Outbound:**
- `reqwest` HTTP GET (with Range); `flate2::read::GzDecoder` + `tar::Archive` for extraction; `sha2::Sha256` for verification.
- `settings::{get_settings, write_settings}` for `selected_model` / `selected_language`.
- `portable::app_data_dir` for the root.
- **Events** (`AppHandle::emit`), all consumed in `modelStore.ts:282-405`:
  `model-download-progress` (`DownloadProgress`), `model-download-complete` (id), `model-download-failed` (`{model_id,error}`, from command layer), `model-verification-started/-completed` (id), `model-extraction-started/-completed` (id), `model-extraction-failed` (`{model_id,error}`), `model-download-cancelled` (id), `model-deleted` (id). Plus downstream `model-state-changed` (`ModelStateEvent`) from `TranscriptionManager` load/unload.

---

## 6. Error handling & edge cases

- **Resume + range-unsupported server** → detect `200` vs `206`, restart fresh (`model.rs:1061-1074`).
- **Size mismatch** after streaming → delete partial, error (`:1178-1190`).
- **SHA-256 mismatch / read failure** → delete partial inside `verify_sha256`, error mentioning "corrupt" (`:950-968`).
- **Corrupt archive** → temp dir + partial both deleted so resume can't reuse a broken `.tar.gz` (issue #858, `:1242-1262`).
- **Interrupted extraction** → leftover `.extracting` dirs GC'd on next `update_download_status`, guarded against deleting a live extraction via `extracting_models` (`:736-743`).
- **Cancellation mid-stream** → partial kept, returns `Ok` (resumable) (`:1128-1134`).
- **Every error path** in `download_model` resets `is_downloading` + cancel flag via the `DownloadCleanup` guard (`:73-86, :1041-1046`).
- **Deleting the active model** → unload + clear setting first (`models.rs:54-64`).
- **Stale selected model** (deleted custom file) → cleared on startup (`:779-792`).
- **Custom models** → `url=None`/`sha256=None` ⇒ never downloaded, verification skipped, removed from catalog on delete.
- **Load failure** → `switch_active_model` reverts the persisted `selected_model` (`models.rs:147-152`).

---

## 7. State & persistence touched

- **Settings store** (tauri-plugin-store via `settings.rs`): `selected_model`, `selected_language`, reads `model_unload_timeout`. Written in `auto_select_model_if_needed`, `switch_active_model`, and the `delete_model` command.
- **Files on disk** under `<app_data>/models/` (or `<exe>/Data/models/` in portable mode):
  - `*.bin` — Whisper file models.
  - `<model-dir>/` — extracted directory models (Parakeet/Moonshine/SenseVoice/GigaAM/Canary/Cohere).
  - `<filename>.partial` — in-flight or paused download.
  - `<filename>.extracting/` — transient extraction staging dir.
- **No SQLite** is touched here (history DB is a separate manager).
- **Bundled resources**: `resources/models/ggml-small.bin`, `resources/models/gigaam_vocab.txt` (read by migrations).

---

## 8. Platform-specific branches

**Essentially none inside this subsystem** — `model.rs` and `commands/models.rs` contain **no `#[cfg(...)]` gates**. Platform variance is delegated:
- The data root is abstracted by `portable::app_data_dir` (`portable.rs:60-66`), which wraps `app.path().app_data_dir()` (Tauri resolves the OS-specific location).
- GPU/accelerator selection (Metal on macOS, Vulkan elsewhere) is applied in `transcription::apply_accelerator_settings` (`lib.rs:160`), not here.
- **iOS: entirely unsupported.** No `#[cfg(target_os = "ios")]` anywhere; the HTTP/tar/sha2 stack would compile, but the `AppHandle`-centric, multi-GB-download, desktop-filesystem model is not viable for an iPhone target as written.

---

## 9. PLAUD relevance — concrete extension points

1. **Add new `EngineType` variants for Plaud-class models.** Extend the enum at `model.rs:20-30` (e.g. `Diarization`, `SpeakerEmbedding`, `Summarizer`, `VadLarge`) and add matching `::load` arms in `transcription.rs:302-380`. The download/verify/extract pipeline is engine-agnostic and carries these for free.

2. **Register diarization / speaker-ID models in the catalog.** Add `ModelInfo` inserts (pattern at `model.rs:264-326`) for a pyannote-style segmentation model + a speaker-embedding model (wespeaker/titanet). They are directory (`is_directory=true`) `.tar.gz` artifacts — already first-class. **No download-code changes required.**

3. **Externalize the catalog (prerequisite for OTA model updates).** Replace the hard-coded inserts behind the `model.rs:125` TODO with a JSON/remote manifest fetch. Unlocks shipping new Plaud models (summary LLMs, diarizers) without an app release.

4. **Summarization / LLM model acquisition.** A Plaud "AI notes" feature needs a local GGUF LLM (or a cloud key). The GGUF case maps directly onto the file-model path (`*.bin`-style move-into-place). Add `EngineType::Summarizer` + a catalog entry; `get_model_path` already returns the right path. (Cloud summarization lives in post-processing — doc 07.)

5. **Long-form / multi-GB models & streaming.** `download_model` already resumes, throttles, and verifies multi-GB files (Cohere is 1.7 GB). For long-recording diarizers this suffices; the gap is **disk-space pre-checks** (none today) before pulling a 1.7 GB archive.

6. **Mobile (iPhone).** `ModelManager` is desktop-bound (`AppHandle`, app-data `models_dir`, synchronous `fs`). For iOS, wrap the interface (`get_model_info`, `get_model_path`, `download_model`) behind a trait and provide an iOS impl that stores into the app sandbox / iCloud and downloads via `URLSession`. The `ModelInfo`/`EngineType` types and the catalog are portable and should be shared.

7. **Sync of selected model / settings.** `auto_select_model_if_needed` + `switch_active_model` centralize "which model is active." A Plaud cloud-sync layer would observe `selected_model` writes (`settings.rs`) and the `model-deleted` / `model-download-complete` events to mirror model availability across a user's devices.

8. **Per-conversation model routing.** `switch_active_model`'s language-reset logic (`models.rs:109-121`) is the natural place to extend into "choose the best model for this conversation's detected languages / speaker count."

---

## 10. Gaps vs a Plaud-style product

- **No diarization / speaker models** — only single-stream ASR. No speaker embeddings, no segmentation model, no "who spoke when" artifact type.
- **No summarization / LLM model management** — `EngineType` has no summarizer; AI-notes models aren't acquirable here.
- **Hard-coded catalog** (`model.rs:126-611`) — no remote manifest, so new Plaud models require an app release. TODO at `model.rs:125` unaddressed.
- **No disk-space / quota checks** before multi-GB downloads; no eviction/LRU for a device accumulating many models.
- **No integrity re-check at load time** — SHA-256 is verified once at download; later on-disk corruption is undetected until the engine fails to load.
- **Custom-model verification skipped entirely** (`sha256=None`) — fine for user files, but no signing/provenance for a managed Plaud catalog.
- **No cloud model store / device sync** — model availability is per-device, local-only.
- **No iOS support** — no `#[cfg(target_os="ios")]`, no sandbox/iCloud storage, no `URLSession` path; the desktop `fs`/`AppHandle` design doesn't port.
- **Non-deterministic auto-select** — `auto_select_model_if_needed` picks via `HashMap` iteration order (`model.rs:798`); the default model on a fresh install is effectively random among downloaded ones; `is_recommended` is not consulted.
- **Concurrency footguns** — plain `.lock().unwrap()` throughout `ModelManager` (no poison recovery); `is_model_loading` inverted and `has_any_models_or_downloads` a no-op duplicate (§3).
- **No bandwidth control / pause-all / background download service** — needed for a mobile, metered-connection product.
