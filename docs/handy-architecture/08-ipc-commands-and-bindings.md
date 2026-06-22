# 08 — Tauri IPC Command Surface + tauri-specta TS Bindings

> **Abstract.** This document is a forensic, line-cited dissection of Handy's Inter-Process Communication (IPC) boundary: the set of `#[tauri::command]` Rust functions exposed to the React/TypeScript webview, the `tauri-specta` machinery that registers them and auto-generates the strongly-typed `src/bindings.ts` client, and the single typed event channel (`HistoryUpdatePayload`) flowing back the other way. The subsystem is the *only* sanctioned way the frontend mutates or reads backend state — every settings toggle, model download, microphone selection, history query, and recording control passes through it. Because Handy is a local-first dictation tool, this surface is deliberately *thin and synchronous*: most commands are simple getters/setters over a `tauri-plugin-store` settings file or thin wrappers over the long-lived `Arc<Manager>` singletons (`AudioRecordingManager`, `ModelManager`, `TranscriptionManager`, `HistoryManager`). For a Plaud-style product (continuous call/conversation capture, diarization, AI summaries, cloud sync, mobile companion), this is the natural seam at which to bolt on new capability: the command list in `lib.rs:326` is the literal API contract, and `bindings.ts` is the literal SDK. We close with concrete extension points and a gap analysis.

---

## 1. Scope & File Map

The "IPC command surface" subsystem spans the `commands/` module plus the registration/codegen site in `lib.rs` and the generated TS client. The five primary files plus their registration and downstream callees:

| Path | Responsibility (1-3 lines) |
|------|----------------------------|
| `src-tauri/src/commands/mod.rs` | Module root for the `commands` namespace. Declares submodules (`audio`, `history`, `models`, `transcription`) and hosts "misc/app-level" commands: cancel, portability, path getters, log level, folder openers, Apple Intelligence probe, Enigo/shortcut lazy init. |
| `src-tauri/src/commands/audio.rs` | Microphone/output device enumeration & selection, mic mode (always-on vs on-demand), Windows mic-permission registry probe, custom-sound existence checks, test-sound playback, recording-state query. |
| `src-tauri/src/commands/transcription.rs` | Model-unload-timeout setter and model-load-status / manual-unload queries against `TranscriptionManager`. (Small file — the *heavy* model commands live in `models.rs`.) |
| `src-tauri/src/commands/models.rs` | Model catalog: list/info, download/cancel/delete, set-active (with language reconciliation + revert-on-failure), current-model & loading-status queries. Also exposes `switch_active_model` as a *shared, non-command* helper reused by the tray menu. |
| `src-tauri/src/commands/history.rs` | Transcription history CRUD: paginated list, toggle-saved, resolve audio file path, delete, retry transcription (re-runs the model on stored WAV), and retention/limit settings updates. |
| `src/bindings.ts` | **Auto-generated** (do not edit). Typed `commands.*` async wrappers around `TAURI_INVOKE`, the `events.historyUpdatePayload` listener factory, and all shared `Type`-derived TS types. This is the de-facto frontend SDK. |

**Registration / codegen site (not in `commands/` but load-bearing):**
- `src-tauri/src/lib.rs:325-438` — builds the `tauri_specta::Builder`, calls `collect_commands![...]` (the full allowlist, `lib.rs:326-429`), `collect_events![HistoryUpdatePayload]` (`lib.rs:430`), exports `bindings.ts` in debug builds (`lib.rs:432-438`), and wires the resulting `invoke_handler()` into `tauri::Builder` (`lib.rs:440`).

**Downstream callees referenced by these commands (read for data-flow accuracy):**
- `src-tauri/src/managers/history.rs` — `HistoryManager`, `PaginatedHistory`, `HistoryEntry`, `HistoryUpdatePayload` (the typed event).
- `src-tauri/src/managers/transcription.rs` — `TranscriptionManager` (`try_start_loading`, `load_model`, `unload_model`, `initiate_model_load`, `transcribe`, `get_current_model`, `is_model_loaded`).
- `src-tauri/src/managers/model.rs` — `ModelManager`, `ModelInfo`.
- `src-tauri/src/managers/audio.rs` — `AudioRecordingManager`, `MicrophoneMode`.
- `src-tauri/src/actions.rs` — `process_transcription_output()` → `ProcessedTranscription` (`actions.rs:343`).
- `src-tauri/src/helpers/clamshell.rs` — `is_laptop()` (registered at `lib.rs:428`, macOS-gated).

> Note: many command *names* that appear in `bindings.ts` (e.g. `change_*_setting`, `change_binding`, `get_available_accelerators`, `start_handy_keys_recording`) are **not** in the five `commands/*.rs` files — they live under `shortcut::` and `shortcut::handy_keys::` and are registered at `lib.rs:327-376`. They are part of the same IPC surface but are documented in the shortcut/settings subsystem docs. This document focuses on the five command files but treats `lib.rs`'s `collect_commands!` as the authoritative full inventory.

---

## 2. The tauri-specta Mechanism (how the surface is built)

Two attribute macros stack on every command:

```rust
#[tauri::command]   // registers the fn with Tauri's invoke router
#[specta::specta]    // captures the type signature for codegen
```

Example: `commands/mod.rs:11-15` (`cancel_operation`). The order is irrelevant; both orderings appear (`mod.rs:53-54` puts `#[specta::specta]` first).

The pipeline (`lib.rs:325-440`):
1. `Builder::<tauri::Wry>::new()` (`lib.rs:325`).
2. `.commands(collect_commands![ ... ])` (`lib.rs:326-429`) — a compile-time inventory. **A command that is not listed here is not reachable from the frontend even if it compiles.** This is the single source of truth for the API contract.
3. `.events(collect_events![managers::history::HistoryUpdatePayload,])` (`lib.rs:430`) — the only typed Rust→JS event.
4. `#[cfg(debug_assertions)] specta_builder.export(Typescript::default().bigint(BigIntExportBehavior::Number), "../src/bindings.ts")` (`lib.rs:432-438`) — regenerates `bindings.ts` **only in dev builds**. `BigIntExportBehavior::Number` is why Rust `i64`/`u64` (e.g. `HistoryEntry.id`, `timestamp`) surface as TS `number`, not `bigint` — a precision caveat for very large IDs/timestamps (see §6).
5. `let invoke_handler = specta_builder.invoke_handler();` (`lib.rs:440`) — handed to `tauri::Builder` so invocations route correctly.

**Generated client shape (`bindings.ts`):**
- Each fallible command (`Result<T, String>` in Rust) becomes an `async` fn returning a discriminated `Result<T, E>` (`bindings.ts:895-897`: `{status:"ok",data} | {status:"error",error}`). The wrapper try/catches `TAURI_INVOKE` and rethrows real `Error`s but boxes string errors into `{status:"error"}` (e.g. `changeBinding`, `bindings.ts:8-15`).
- Each infallible command (plain return) becomes a thin `await TAURI_INVOKE(...)` with no Result wrapper (e.g. `getAvailableTypingTools` `bindings.ts:136-138`; `isPortable` `bindings.ts:442-444`; `isRecording` `bindings.ts:726-728`; `playTestSound` `bindings.ts:704-706`; `setModelUnloadTimeout` `bindings.ts:729-731`).
- Argument names are camelCased automatically: Rust `model_id` → TS `modelId` (`downloadModel`, `bindings.ts:557`), Rust `always_on` → TS `alwaysOn` (`updateMicrophoneMode`, `bindings.ts:629`), Rust `file_name` → TS `fileName` (`getAudioFilePath`, `bindings.ts:764`).
- Events are exposed via `events.historyUpdatePayload` with `listen/once/emit` (`bindings.ts:823-827`, factory `__makeEvents__` at `bindings.ts:899-932`).

---

## 3. Per-Command Catalog (signature → behavior → file:line)

### 3.1 `commands/mod.rs` — app/misc commands

| Command (Rust) | Signature | Behavior | Line |
|---|---|---|---|
| `cancel_operation` | `fn(app: AppHandle)` | Calls `utils::cancel_current_operation(&app)` — aborts in-flight recording/transcription. Infallible, no return. | `mod.rs:11-15` |
| `is_portable` | `fn() -> bool` | Delegates to `portable::is_portable()` (portable-install detection). | `mod.rs:17-21` |
| `get_app_dir_path` | `fn(app) -> Result<String,String>` | Resolves `portable::app_data_dir`, stringifies; errors as formatted string. | `mod.rs:23-30` |
| `get_app_settings` | `fn(app) -> Result<AppSettings,String>` | Returns the full deserialized `AppSettings` via `get_settings(&app)`. Never errors (always `Ok`). | `mod.rs:32-36` |
| `get_default_settings` | `fn() -> Result<AppSettings,String>` | Returns `settings::get_default_settings()` — for "reset to defaults" UI. | `mod.rs:38-42` |
| `get_log_dir_path` | `fn(app) -> Result<String,String>` | Resolves `portable::app_log_dir`. | `mod.rs:44-51` |
| `set_log_level` | `fn(app, level: LogLevel) -> Result<(),String>` | Converts `LogLevel→tauri_plugin_log::LogLevel→log::Level`, stores it into the `FILE_LOG_LEVEL` atomic (`mod.rs:59-62`), then persists `settings.log_level`. Runtime log-level change without restart. | `mod.rs:53-69` |
| `open_recordings_folder` | `fn(app) -> Result<(),String>` | Opens `<app_data>/recordings` via `tauri_plugin_opener`. | `mod.rs:71-85` |
| `open_log_dir` | `fn(app) -> Result<(),String>` | Opens the log dir in OS file browser. | `mod.rs:87-99` |
| `open_app_data_dir` | `fn(app) -> Result<(),String>` | Opens the app-data dir. | `mod.rs:101-113` |
| `check_apple_intelligence_available` | `fn() -> bool` | On macOS+aarch64 calls `apple_intelligence::check_apple_intelligence_availability()`; everywhere else returns `false`. **Platform-gated** (`cfg(all(target_os="macos", target_arch="aarch64"))`). | `mod.rs:115-128` |
| `initialize_enigo` | `fn(app) -> Result<(),String>` | Lazily constructs `EnigoState` (keyboard/mouse simulation) and `app.manage`s it; idempotent via `try_state` check. macOS error path warns about accessibility perms. | `mod.rs:130-162` |
| `initialize_shortcuts` | `fn(app) -> Result<(),String>` | Idempotent (`ShortcutsInitialized` marker, `mod.rs:165`); calls `shortcut::init_shortcuts(&app)` then manages the marker. Called by frontend after macOS accessibility grant. | `mod.rs:164-187` |

Key local type: `pub struct ShortcutsInitialized;` (`mod.rs:165`) — a zero-sized marker stored in Tauri state to gate re-init.

### 3.2 `commands/audio.rs` — devices & permissions

Types defined here:
- `CustomSounds { start: bool, stop: bool }` (`audio.rs:17-21`) — TS at `bindings.ts:841`.
- `AudioDevice { index: String, name: String, is_default: bool }` (`audio.rs:37-42`) — TS at `bindings.ts:836`.
- `enum PermissionAccess { Allowed, Denied, Unknown }` (`audio.rs:44-50`, snake_case serde) — TS at `bindings.ts:864`.
- `WindowsMicrophonePermissionStatus { supported, overall_access, device_access, app_access, desktop_app_access }` (`audio.rs:52-59`) — TS at `bindings.ts:872`.

| Command | Signature | Behavior | Line |
|---|---|---|---|
| `check_custom_sounds` | `fn(app) -> CustomSounds` | Checks existence of `custom_start.wav` / `custom_stop.wav` in app data (`custom_sound_exists`, `audio.rs:23-26`). | `audio.rs:28-35` |
| `get_windows_microphone_permission_status` | `fn() -> WindowsMicrophonePermissionStatus` | **Windows-only impl** (`audio.rs:79-111`) reads three registry keys under `CapabilityAccessManager\ConsentStore\microphone`; non-Windows returns `supported:false` + all `Unknown` (`audio.rs:121-130`). | `audio.rs:113-131` |
| `open_microphone_privacy_settings` | `fn() -> Result<(),String>` | Windows: spawns `cmd /C start ms-settings:privacy-microphone`. Non-Windows: returns `Err` "only supported on Windows". | `audio.rs:133-150` |
| `update_microphone_mode` | `fn(app, always_on: bool) -> Result<(),String>` | Persists `always_on_microphone`, then calls `AudioRecordingManager::update_mode(MicrophoneMode::AlwaysOn | OnDemand)`. **Mutates a live manager.** | `audio.rs:152-170` |
| `get_microphone_mode` | `fn(app) -> Result<bool,String>` | Reads `settings.always_on_microphone`. | `audio.rs:172-177` |
| `get_available_microphones` | `fn() -> Result<Vec<AudioDevice>,String>` | `list_input_devices()` from `audio_toolkit`, prepends a synthetic `"default"` entry. | `audio.rs:179-198` |
| `set_selected_microphone` | `fn(app, device_name) -> Result<(),String>` | Persists `selected_microphone` (`None` if `"default"`), then `AudioRecordingManager::update_selected_device()`. | `audio.rs:200-217` |
| `get_selected_microphone` | `fn(app) -> Result<String,String>` | Reads setting, defaulting to `"default"`. | `audio.rs:219-226` |
| `get_available_output_devices` | `fn() -> Result<Vec<AudioDevice>,String>` | `list_output_devices()`, prepends `"default"`. | `audio.rs:228-247` |
| `set_selected_output_device` | `fn(app, device_name) -> Result<(),String>` | Persists `selected_output_device` only (no live-manager call). | `audio.rs:249-260` |
| `get_selected_output_device` | `fn(app) -> Result<String,String>` | Reads setting. | `audio.rs:262-269` |
| `play_test_sound` | `async fn(app, sound_type: String)` | Maps `"start"/"stop"` → `SoundType`, plays via `audio_feedback::play_test_sound`; unknown logs `warn!` and no-ops. **Stringly-typed enum** (not a `Type`). | `audio.rs:271-283` |
| `set_clamshell_microphone` | `fn(app, device_name) -> Result<(),String>` | Persists `clamshell_microphone` (mic used in laptop-lid-closed mode). | `audio.rs:285-296` |
| `get_clamshell_microphone` | `fn(app) -> Result<String,String>` | Reads setting. | `audio.rs:298-305` |
| `is_recording` | `fn(app) -> bool` | `AudioRecordingManager::is_recording()`. Infallible. | `audio.rs:307-313` |

### 3.3 `commands/transcription.rs` — model lifecycle status

Type: `ModelLoadStatus { is_loaded: bool, current_model: Option<String> }` (`transcription.rs:7-11`) — TS at `bindings.ts:858`.

| Command | Signature | Behavior | Line |
|---|---|---|---|
| `set_model_unload_timeout` | `fn(app, timeout: ModelUnloadTimeout)` | Persists `model_unload_timeout`. Infallible (no Result). | `transcription.rs:13-19` |
| `get_model_load_status` | `fn(State<TranscriptionManager>) -> Result<ModelLoadStatus,String>` | Reads `is_model_loaded()` + `get_current_model()`. Uses bare `State<TranscriptionManager>` (not `Arc`-wrapped in the param). | `transcription.rs:21-30` |
| `unload_model_manually` | `fn(State<TranscriptionManager>) -> Result<(),String>` | Calls `unload_model()`; frees VRAM/RAM on demand. | `transcription.rs:32-40` |

### 3.4 `commands/models.rs` — catalog & active-model switching

Type imported: `ModelInfo` (from `managers::model`, TS at `bindings.ts:857`), `ModelStateEvent` (raw-emitted, not typed-collected — see §4).

| Command | Signature | Behavior | Line |
|---|---|---|---|
| `get_available_models` | `async fn(State<Arc<ModelManager>>) -> Result<Vec<ModelInfo>,String>` | `model_manager.get_available_models()`. | `models.rs:7-13` |
| `get_model_info` | `async fn(State<Arc<ModelManager>>, model_id) -> Result<Option<ModelInfo>,String>` | Single-model lookup. | `models.rs:15-22` |
| `download_model` | `async fn(app_handle, State<Arc<ModelManager>>, model_id) -> Result<(),String>` | `model_manager.download_model(...).await`. On error, **also raw-emits** `"model-download-failed"` with `{model_id,error}` JSON (`models.rs:37-41`) so the UI can react even if the awaiting call's rejection is missed. | `models.rs:24-44` |
| `delete_model` | `async fn(app_handle, ModelManager, TranscriptionManager, model_id) -> Result<(),String>` | If deleting the *active* model: unloads it and clears `selected_model` first (`models.rs:55-64`), then deletes from disk. | `models.rs:46-69` |
| `set_active_model` | `async fn(app_handle, _ModelManager, _TranscriptionManager, model_id) -> Result<(),String>` | Thin wrapper that delegates to the shared `switch_active_model` (params underscored — state pulled inside helper). | `models.rs:157-166` |
| `get_current_model` | `async fn(app_handle) -> Result<String,String>` | Reads `settings.selected_model`. | `models.rs:168-173` |
| `get_transcription_model_status` | `async fn(TranscriptionManager) -> Result<Option<String>,String>` | `transcription_manager.get_current_model()`. | `models.rs:175-181` |
| `is_model_loading` | `async fn(TranscriptionManager) -> Result<bool,String>` | **Semantically suspicious:** returns `current_model.is_none()` (`models.rs:190`) — i.e. "no model loaded", *not* "currently loading". Likely a latent bug / misnomer (see §5). | `models.rs:183-191` |
| `has_any_models_available` | `async fn(ModelManager) -> Result<bool,String>` | `any(is_downloaded)`. | `models.rs:193-200` |
| `has_any_models_or_downloads` | `async fn(ModelManager) -> Result<bool,String>` | Comment claims "or downloads in progress" but body only checks `is_downloaded` (`models.rs:208-209`) — comment/impl mismatch. | `models.rs:202-210` |
| `cancel_download` | `async fn(ModelManager, model_id) -> Result<(),String>` | `model_manager.cancel_download(...)`. | `models.rs:212-221` |

**Non-command shared helper:** `pub fn switch_active_model(app, model_id) -> Result<(),String>` (`models.rs:77-155`). This is the crux of model switching and is reused by both `set_active_model` and the **tray menu** (per its doc comment, `models.rs:71-76`). Notable logic:
- **Concurrency guard:** `try_start_loading()` returns a `LoadingGuard` that resets the loading flag on drop; absence ⇒ `Err("Model load already in progress")` (`models.rs:84-86`). Prevents concurrent loads from tray double-clicks.
- Validates model exists + `is_downloaded` (`models.rs:89-95`).
- **Language reconciliation:** if the new model doesn't list the currently-selected language as supported, resets `selected_language` to `"auto"` (`models.rs:109-121`) — prevents stale-language crashes (e.g. Canary + `zh-Hans`).
- Persists selection *early* (`models.rs:123`) so the frontend's event reaction sees the right model.
- If `model_unload_timeout == Immediately`: skips eager load, raw-emits `"model-state-changed"` with a `ModelStateEvent{event_type:"selection_changed",...}` and returns (`models.rs:127-144`).
- Otherwise `load_model`; **on failure reverts** the persisted selection to `old_model` (`models.rs:147-152`).

### 3.5 `commands/history.rs` — history CRUD & retry

All commands take `State<'_, Arc<HistoryManager>>`. The frontend listens for `HistoryUpdatePayload` events (emitted by the manager) to refresh.

| Command | Signature | Behavior | Line |
|---|---|---|---|
| `get_history_entries` | `async fn(_app, HistoryManager, cursor: Option<i64>, limit: Option<usize>) -> Result<PaginatedHistory,String>` | Cursor-paginated list (DESC by id). Limit is clamped to ≤100 server-side (`managers/history.rs:456`). | `history.rs:9-21` |
| `toggle_history_entry_saved` | `async fn(_app, HistoryManager, id: i64) -> Result<(),String>` | Flips `saved`; manager emits `Toggled{id}`. | `history.rs:23-34` |
| `get_audio_file_path` | `async fn(_app, HistoryManager, file_name) -> Result<String,String>` | Resolves the WAV path under recordings dir (`history.rs:43`), validates UTF-8. **No path-traversal sanitization** of `file_name` (see §5/§9). | `history.rs:36-47` |
| `delete_history_entry` | `async fn(_app, HistoryManager, id) -> Result<(),String>` | Deletes DB row (and presumably audio); manager emits `Deleted{id}`. | `history.rs:49-60` |
| `retry_history_entry_transcription` | `async fn(app, HistoryManager, TranscriptionManager, id) -> Result<(),String>` | **The heaviest command.** Loads entry, reads WAV via `audio_toolkit::read_wav_samples` (`history.rs:77`), errors if empty (`history.rs:80-82`), `initiate_model_load()`, runs `transcribe` on `spawn_blocking` (`history.rs:87-90`), post-processes via `actions::process_transcription_output` (`history.rs:96-97`), then `update_transcription`. | `history.rs:62-107` |
| `update_history_limit` | `async fn(app, HistoryManager, limit: usize) -> Result<(),String>` | Persists `history_limit`, then `cleanup_old_entries()`. | `history.rs:109-125` |
| `update_recording_retention_period` | `async fn(app, HistoryManager, period: String) -> Result<(),String>` | Parses stringly-typed period → `RecordingRetentionPeriod` enum (`history.rs:136-143`), persists, then `cleanup_old_entries()`. Unknown string ⇒ `Err`. | `history.rs:127-154` |

### 3.6 Registered elsewhere but on the same surface

- `helpers::clamshell::is_laptop` (`lib.rs:428`) → `fn() -> Result<bool,String>`; macOS uses `pmset` battery detection (`helpers/clamshell.rs:35`), non-macOS stub at `helpers/clamshell.rs:60`. TS at `bindings.ts:810-817`.
- `trigger_update_check`, `show_main_window_command` (`lib.rs:377-378`) and the entire `shortcut::*` settings family (`lib.rs:327-376`) — outside the five files but part of the contract.

---

## 4. Events: the reverse channel (Backend → Frontend)

Two distinct mechanisms coexist:

**(a) Typed, specta-collected event — exactly one:**
- `HistoryUpdatePayload` (`managers/history.rs:42-53`), a `#[serde(tag="action")]` enum with variants `Added{entry}`, `Updated{entry}`, `Deleted{id}`, `Toggled{id}`. Derives `tauri_specta::Event`. Registered at `lib.rs:430`. Emitted by `HistoryManager` methods: `save_entry` → `Added` (`managers/history.rs:271-276`), `update_transcription` → `Updated` (`:319-324`), `toggle_saved_status` → `Toggled` (`:577-578`), `delete_entry` → `Deleted` (`:634-635`). Consumed in TS via `events.historyUpdatePayload.listen(...)` (`bindings.ts:823-827`); the discriminated union type is at `bindings.ts:845`.

**(b) Raw, string-named `app.emit` events — NOT in the typed contract:**
- `"model-download-failed"` `{model_id,error}` from `download_model` (`models.rs:37-41`).
- `"model-state-changed"` carrying `ModelStateEvent` from `switch_active_model` (`models.rs:130-138`).

These raw events bypass tauri-specta, so the frontend must hand-write `listen("model-state-changed", ...)` with manually-kept-in-sync types. This is an inconsistency: history is type-safe end-to-end, model-state is not.

---

## 5. Error Handling & Edge Cases

- **Uniform error type = `String`.** Every fallible command returns `Result<T, String>`; managers return `anyhow::Result` and commands collapse via `.map_err(|e| e.to_string())` or `format!`. The frontend receives `{status:"error", error: string}` (`bindings.ts:895-897`). No structured error codes — the UI must string-match.
- **Infallible commands can still throw in JS.** Commands returning plain `T` (e.g. `isRecording`, `playTestSound`, `getWindowsMicrophonePermissionStatus`) have no try/catch in `bindings.ts`; a transport-level failure rejects the promise as a raw `Error`.
- **`retry_history_entry_transcription` edge cases** (`history.rs:62-107`): entry-not-found → `Err(format!("History entry {} not found"))`; empty samples → `Err("Recording has no audio samples")`; panicked blocking task → caught (`history.rs:89`, `"Transcription task panicked"`); empty transcription → `Err("Recording contains no speech")`. This is the most defensively-coded command.
- **`switch_active_model` revert-on-failure** (`models.rs:147-152`) prevents the persisted `selected_model` from drifting out of sync with what actually loaded.
- **`is_model_loading` misnomer** (`models.rs:183-191`): returns `current_model.is_none()`. It reports "no model loaded", which is the *opposite* of "loading". Any UI relying on this for a spinner is likely wrong. Flag for review.
- **`has_any_models_or_downloads` comment/impl mismatch** (`models.rs:202-210`): the in-progress-download branch described in the comment is not implemented.
- **`get_audio_file_path` path handling** (`history.rs:36-47`): `file_name` comes from the frontend and is joined onto the recordings dir without traversal checks (`managers/history.rs:584-586`). In practice `file_name` originates from DB rows, but the command will resolve arbitrary relative paths if called directly.
- **Stringly-typed inputs** that should be enums: `play_test_sound(sound_type: String)` (`audio.rs:273`) and `update_recording_retention_period(period: String)` (`history.rs:131`). Unknown values are handled (`warn!`/`Err`) but lose compile-time safety the rest of the surface enjoys.

---

## 6. State & Persistence Touched

| Store | Via | Commands touching it |
|---|---|---|
| **Settings** (`tauri-plugin-store`, `get_settings`/`write_settings`) | `settings.rs` | Nearly all setters: `set_log_level`, `update_microphone_mode`, `set_selected_microphone`, `set_selected_output_device`, `set_clamshell_microphone`, `set_model_unload_timeout`, `update_history_limit`, `update_recording_retention_period`, `switch_active_model` (selected_model + selected_language). |
| **SQLite** `history.db` (`rusqlite` + `rusqlite_migration`) | `HistoryManager`, schema at `managers/history.rs:20-34` | `get_history_entries`, `toggle_history_entry_saved`, `delete_history_entry`, `retry_history_entry_transcription`, `update_transcription`, `cleanup_old_entries`. **Note:** opens a *fresh* `Connection::open` per call (`managers/history.rs:195-196`) — no pool. |
| **WAV files on disk** `<app_data>/recordings/*.wav` | `HistoryManager::recordings_dir` (`managers/history.rs:78`) | `get_audio_file_path`, `retry_history_entry_transcription` (reads), `open_recordings_folder`. |
| **Model files on disk** | `ModelManager` | `download_model`, `delete_model`, `cancel_download`, `has_any_models_*`. |
| **Custom sound files** `custom_{start,stop}.wav` | `portable::resolve_app_data` | `check_custom_sounds`. |
| **Log-level atomic** `FILE_LOG_LEVEL` | `crate::FILE_LOG_LEVEL` (`mod.rs:59`) | `set_log_level`. |
| **Windows registry (read-only)** | `winreg` | `get_windows_microphone_permission_status`. |
| **Tauri managed state** (`app.manage`) | — | `initialize_enigo` (manages `EnigoState`), `initialize_shortcuts` (manages `ShortcutsInitialized`). |

---

## 7. Threading / Concurrency Model

- **Sync commands run on the webview/IPC thread**; long work must not. Handy follows this: `retry_history_entry_transcription` offloads `transcribe` to `tauri::async_runtime::spawn_blocking` (`history.rs:87-90`) and awaits it, isolating the CPU/GPU-bound Whisper/Parakeet inference from the async runtime.
- **`async fn` commands** (all of `models.rs`, `history.rs`; `play_test_sound`) run on Tauri's async runtime (tokio). The DB methods in `HistoryManager` are `async` but internally synchronous (`rusqlite` is blocking) — they open a connection inline (`managers/history.rs:455`), a minor blocking-in-async smell, mitigated by tiny query sizes.
- **`State<'_, Arc<Manager>>`** is how commands reach the long-lived singletons. The `'_` lifetime on `State<'_, ...>` is required for `async` commands (`models.rs:10`, `history.rs:13`). The managers themselves hold the locks:
  - `TranscriptionManager` (`managers/transcription.rs:53-74`): `Arc<Mutex<...>>` over `engine`, `current_model_id`, `is_loading`; `AtomicBool` shutdown signal; a background **watcher thread** (`watcher_handle`, `:73`/`:135-145`) that auto-unloads the model after the timeout. `try_start_loading()` (`:183`) is the atomic claim used by `switch_active_model`.
  - `AudioRecordingManager` — accessed for `update_mode`, `update_selected_device`, `is_recording`.
- **`switch_active_model` loading guard** (`models.rs:84-86`) is the explicit cross-entrypoint mutual exclusion (tray vs command).
- **Event emission is fire-and-forget**: emit failures are logged, not propagated (`managers/history.rs:276`).

---

## 8. Platform-Specific Branches (cfg gates)

| cfg | Location | Effect |
|---|---|---|
| `all(target_os="macos", target_arch="aarch64")` | `mod.rs:120-123` | Apple Intelligence availability; else `false` (`mod.rs:124-127`). |
| `target_os="macos"` (Enigo warning) | `mod.rs:151-156` | Tailors the accessibility-permission warning message. |
| `target_os="windows"` | `audio.rs:11-15` (winreg import), `:61-111` (registry impls), `:116-119`, `:136-144` | Mic-permission registry probe + `ms-settings:` launcher. Non-Windows stubs at `:121-130`, `:146-149`. |
| `target_os="macos"` | `helpers/clamshell.rs:1-50` | `is_laptop()` via `pmset`; non-macOS stub `:57-62`. |
| `cfg!(target_os="macos")` (runtime) | `mod.rs:151` | Branch chosen at runtime, not compile time. |
| `cfg(debug_assertions)` | `lib.rs:432` | `bindings.ts` is regenerated only in dev. |

**No iOS branches exist anywhere in this subsystem.** All `cfg` gates are macOS/Windows/Linux-desktop. This is the single biggest structural fact for a Plaud-style mobile ambition (§9).

---

## 9. PLAUD Relevance — concrete extension points

Plaud-style product = always-on / call / conversation capture → diarized multi-speaker transcript → AI summary → cloud sync → phone app. Mapping onto this surface:

**A. Long-form / continuous recording control.**
- The only recording primitives exposed are `is_recording` (`audio.rs:307`), `update_microphone_mode` (always-on vs on-demand, `audio.rs:152`), and indirect start/stop via global shortcut + `cancel_operation` (`mod.rs:11`). **There is no `start_recording`/`stop_recording`/`pause_recording`/`get_recording_duration` command.** To build Plaud-style sessions you would add `commands/recording.rs` with explicit session lifecycle commands and register them in `collect_commands!` (`lib.rs:326`). Wrap `AudioRecordingManager` (already a managed `Arc`) with a session/segment concept.
- `MicrophoneMode::AlwaysOn` (`audio.rs:162-166`) is the natural base for continuous capture; extend `AudioRecordingManager::update_mode` rather than the command.

**B. System / call audio capture.**
- Today only microphone *input* devices are enumerated (`get_available_microphones`, `audio.rs:179`) plus *output* devices for playback feedback (`get_available_output_devices`, `audio.rs:228`). Capturing the *system/call* audio (loopback) is absent. Add a `get_available_loopback_devices` / `set_capture_source` command pair and a `CaptureSource` enum; on macOS this means ScreenCaptureKit/Core Audio taps, on Windows WASAPI loopback. The command layer change is small; the heavy lift is in `audio_toolkit`.

**C. Multi-speaker / diarization.**
- `HistoryEntry` (`managers/history.rs:55-66`) has *no speaker field*; `transcription_text` is a flat string. The DB schema (`managers/history.rs:20-34`) has no segments/speakers tables. To add diarization: (1) extend the schema with a `segments(entry_id, speaker_id, start_ms, end_ms, text)` migration, (2) change `HistoryEntry`/`PaginatedHistory` (which auto-propagate to `bindings.ts:844`), (3) add a diarization step in `retry_history_entry_transcription` (`history.rs:62`) and in the live pipeline. `process_transcription_output` (`actions.rs`) is where speaker-aware post-processing would hook.

**D. AI summaries.**
- Post-processing infrastructure already exists: `AppSettings.post_process_*` (providers, API keys, models, prompts; `bindings.ts:835`), `ProcessedTranscription` (`actions.rs:343`), and `process_transcription_output` is invoked in `retry_history_entry_transcription` (`history.rs:96-97`). **Summaries are largely a prompt + a new column.** Add a `summary` column to history, a `generate_summary(id)` command wrapping `process_transcription_output` with a summary prompt, and a `summary` field on `HistoryEntry`. The provider plumbing (`set_post_process_provider`, `fetch_post_process_models`, etc., `bindings.ts:219-234`) is reusable as-is.

**E. Cloud / local sync.**
- Entirely absent. History is local SQLite + local WAV. Add a `commands/sync.rs` (`sync_now`, `get_sync_status`, `set_sync_config`) and a typed `SyncStatusEvent` to `collect_events!` (`lib.rs:430`) mirroring how `HistoryUpdatePayload` works. The `HistoryUpdatePayload` event (`managers/history.rs:42`) is the ideal trigger to enqueue sync deltas — subscribe to it server-side.

**F. Mobile (iPhone).**
- Tauri 2 supports iOS, but **this subsystem has zero iOS cfg gates** and depends on desktop-only crates (`winreg`, `pmset`, Enigo keyboard simulation, global shortcuts). For a phone companion you would *reuse* the command contract conceptually but ship a separate mobile target. The cleanest path: keep `bindings.ts` as the shared API shape (it is platform-agnostic TS) and back it with mobile-appropriate implementations; gate desktop-only commands (`initialize_enigo`, `is_laptop`, Windows perm commands) behind `cfg(desktop)`.

**G. Specific functions to modify/wrap (cheat-sheet):**
- Add session commands → register at `lib.rs:326`, wrap `AudioRecordingManager`.
- Diarization/summary fields → `HistoryEntry`/schema in `managers/history.rs:20-66` (auto-flows to `bindings.ts`).
- Summaries → reuse `actions::process_transcription_output` + `retry_history_entry_transcription` pattern (`history.rs:62`).
- Sync events → add to `collect_events!` (`lib.rs:430`); model on `HistoryUpdatePayload`.
- Make raw events typed → convert `"model-state-changed"`/`"model-download-failed"` (`models.rs:37,130`) into specta `Event`s for end-to-end type safety the sync/mobile clients will need.

---

## 10. Gaps vs a Plaud-style Product

1. **No recording-session lifecycle in the IPC surface.** No start/stop/pause/duration/segment commands; recording is shortcut-driven and binary. Plaud needs explicit, resumable, long-form sessions.
2. **No system/call/loopback audio capture commands.** Only mic input + output-feedback enumeration.
3. **No speaker model anywhere.** Flat `transcription_text`, no segments/speakers schema, no diarization step. Multi-speaker conversations cannot be represented.
4. **No summaries as first-class data.** Post-processing exists but there is no `summary` column/command/field; summaries would be conflated with `post_processed_text`.
5. **No cloud sync / multi-device.** Local-only SQLite + WAV; no sync commands, no sync status event, no conflict model, no auth/account commands.
6. **No mobile/iOS support.** Zero iOS cfg gates; hard desktop deps (winreg, pmset, Enigo, global shortcuts). The command contract is reusable but no mobile backend exists.
7. **Inconsistent event typing.** Only `HistoryUpdatePayload` is specta-typed; `model-state-changed` and `model-download-failed` are raw string events the frontend must hand-type — fragile for additional clients.
8. **Error model is unstructured `String`.** No error codes/enums; clients string-match. A networked/synced product needs typed, retryable error categories.
9. **No pagination/streaming for large transcripts.** `get_history_entries` paginates entries (`history.rs:9`) but a single long-form transcript is one unbounded string — no time-range or segment-range fetch.
10. **Latent correctness issues** to fix before extending: `is_model_loading` returns inverted/misnamed semantics (`models.rs:190`); `has_any_models_or_downloads` ignores in-progress downloads (`models.rs:208`); `get_audio_file_path` lacks path-traversal hardening (`history.rs:36`).
11. **No auth/identity/permission commands.** No account, no per-user data scoping — required for any cloud-backed Plaud product.
12. **Blocking SQLite inside `async` + per-call connections** (`managers/history.rs:195`) will not scale to continuous capture write rates; needs a pooled/WAL strategy.
