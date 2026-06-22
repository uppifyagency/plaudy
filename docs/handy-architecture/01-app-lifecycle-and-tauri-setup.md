# Handy — Application Lifecycle, Tauri Setup, Settings, Tray, Overlay, CLI, Single-Instance

> **Abstract.** This document is a forensic, file-by-file analysis of the *application lifecycle* subsystem of Handy — the cross-platform Tauri 2.x speech-to-text app. It covers the process entry point (`main.rs`), the Tauri builder / plugin graph / managed-state bootstrap (`lib.rs`), the persisted-settings model and its store I/O (`settings.rs`), the system-tray menu and its compile-time i18n (`tray.rs`, `tray_i18n.rs`), the floating recording overlay window/panel and its platform back-ends (`overlay.rs`), the CLI argument surface (`cli.rs`), Unix signal handling plus the CLI/IPC bridge into the transcription pipeline (`signal_handle.rs`), portable-mode data redirection (`portable.rs`), and cross-cutting lifecycle utilities including the central cancel path (`utils.rs`). Throughout, every claim is anchored with `file:line` citations. The final two sections map concrete extension points and gaps for converting Handy into a Plaud-style always-on conversation/call recorder with diarization, long-form capture, AI summaries, cloud sync, and a mobile companion. All paths are absolute under `/Users/vladvrinceanu/Desktop/PROGETTI ANTYGRAVITY/Plaude Local/handy/`.

---

## 1. Per-file responsibilities

| File (absolute path) | Responsibility (1–3 lines) |
| --- | --- |
| `src-tauri/src/main.rs` | Binary entry point. Parses CLI args with `clap`, sets a Linux WebKit env workaround, then hands off to `handy_app_lib::run`. Declares `windows_subsystem = "windows"` for release builds. |
| `src-tauri/src/lib.rs` | The library crate root and true application bootstrap: declares all modules, builds the Tauri app (plugins, managed state, specta command/event registry, single-instance handler, window + tray + overlay creation, window-event handlers, run loop). Owns the global file-log-level atomic. |
| `src-tauri/src/settings.rs` | The entire persisted-settings model (`AppSettings` + ~25 enums/structs), their serde defaults, migration logic, and the read/write helpers over `tauri-plugin-store`. Single source of truth for user configuration. |
| `src-tauri/src/tray.rs` | Builds and refreshes the system-tray menu and icon, mapping theme + pipeline state to icons; implements tray actions (copy last transcript, model submenu, visibility). |
| `src-tauri/src/tray_i18n.rs` | Thin runtime lookup over a *compile-time-generated* translation table (`build.rs`) for tray menu strings; resolves locale → language → English. |
| `src-tauri/src/overlay.rs` | Creates and positions the floating "recording/transcribing/processing" overlay window; three native back-ends: NSPanel (macOS), GTK layer-shell (Linux), Win32 topmost (Windows). Emits mic levels and state to it. |
| `src-tauri/src/cli.rs` | `clap`-derive definition of the six CLI flags (`--start-hidden`, `--no-tray`, `--toggle-transcription`, `--toggle-post-process`, `--cancel`, `--debug`). |
| `src-tauri/src/signal_handle.rs` | Reusable `send_transcription_input()` bridge into the `TranscriptionCoordinator`; Unix `SIGUSR1`/`SIGUSR2` handler thread. Shared by signals and CLI remote-control. |
| `src-tauri/src/portable.rs` | Detects "portable mode" (marker file beside the exe) at startup and redirects all user data (settings, logs, webview cache) into a `Data/` directory. Pure, AppHandle-free path helpers. |
| `src-tauri/src/utils.rs` | Cross-cutting helpers: the centralized `cancel_current_operation()`, Linux display-server detection (`is_wayland`, `is_kde_plasma`, `is_kde_wayland`), and re-exports of `clipboard`/`overlay`/`tray`. |
| `src-tauri/build.rs` | (Supporting) Build script that generates `tray_translations.rs` from frontend locale JSON and compiles the macOS Apple-Intelligence Swift bridge. |

---

## 2. Startup sequence (chronological)

This is the precise order of operations from process start to "ready", with citations.

1. **`main()`** (`src-tauri/src/main.rs:7`) — `CliArgs::parse()` (`main.rs:8`). On Linux, `WEBKIT_DISABLE_DMABUF_RENDERER=1` is set before any WebView init (`main.rs:10-15`). Calls `handy_app_lib::run(cli_args)` (`main.rs:17`).
2. **`run()`** (`src-tauri/src/lib.rs:317`):
   - `portable::init()` first, before anything else, so all subsequent path resolution is portable-aware (`lib.rs:319`, impl `portable.rs:15`).
   - `build_console_filter()` parses `RUST_LOG` for stdout filtering (`lib.rs:323`, impl `lib.rs:65`).
   - Builds the `tauri-specta` `Builder` registering ~90 commands via `collect_commands!` and one event (`HistoryUpdatePayload`) via `collect_events!` (`lib.rs:325-430`).
   - In debug builds only, exports TypeScript bindings to `../src/bindings.ts` (`lib.rs:432-438`).
   - Constructs the `tauri::Builder`, installs the log plugin with a Stdout target (RUST_LOG filter) and a File target (folder under portable `Data/logs` or platform log dir; level gated by the `FILE_LOG_LEVEL` atomic) (`lib.rs:443-475`).
   - macOS only: installs `tauri_nspanel::init()` (`lib.rs:477-480`).
   - Installs the single-instance plugin with the remote-control callback (`lib.rs:483-493`), then ~13 more plugins (fs, process, updater, os, clipboard-manager, macos-permissions, opener, store, global-shortcut, autostart) (`lib.rs:494-506`).
   - `.manage(cli_args.clone())` registers `CliArgs` as managed state for later reads (`lib.rs:507`).
   - **`.setup(...)`** closure (`lib.rs:508-575`): mounts specta events; builds the main `WebviewWindow` programmatically (680×570, hidden) with portable webview `data_directory` (`lib.rs:511-526`); reads settings; applies the `--debug` runtime override (sets `debug_mode` + `LogLevel::Trace`, **not persisted**) (`lib.rs:531-534`); stores the file log level into the atomic (`lib.rs:536-539`); creates and manages the `TranscriptionCoordinator` (`lib.rs:541`); calls `initialize_core_logic()` (`lib.rs:543`); pre-warms GPU enumeration on a background thread (`lib.rs:552-554`); honors `--no-tray` (`lib.rs:557-559`); decides whether to show the main window (`lib.rs:564-572`).
   - Registers `on_window_event` (close-to-tray + theme-change) (`lib.rs:576-604`).
   - `.run(...)` with a macOS `Reopen` handler that re-shows the main window on dock-click (`lib.rs:608-614`).
3. **`initialize_core_logic()`** (`src-tauri/src/lib.rs:140`): constructs and `.manage()`s the four managers (Audio, Model, Transcription, History) (`lib.rs:147-166`); applies accelerator settings (`lib.rs:160`); installs Unix signal handlers (`lib.rs:173-177`); applies the macOS Accessory activation policy when start-hidden+tray (`lib.rs:181-187`); builds the tray icon + menu-event router (`lib.rs:194-264`); applies `show_tray_icon` (`lib.rs:270-273`); subscribes to `model-state-changed` to refresh the tray (`lib.rs:276-279`); configures autostart from settings (`lib.rs:282-291`); creates the (hidden) recording overlay (`lib.rs:294`).

> **Deliberate deferral:** Enigo (keyboard/mouse simulation) and global shortcuts are **not** initialized here. The frontend must call `initialize_enigo` and `initialize_shortcuts` after onboarding/permission grant, to avoid premature macOS permission dialogs (`lib.rs:141-144`, `lib.rs:168-171`; commands at `commands/mod.rs:134` and `commands/mod.rs:172`).

---

## 3. Important types and public functions (with signatures + citations)

### 3.1 `lib.rs`

- `pub static FILE_LOG_LEVEL: AtomicU8` (`lib.rs:51`) — runtime-mutable file-log level, stored as the `u8` of a `log::LevelFilter`. Read by the log file target filter (`lib.rs:469-472`); written by `set_log_level` (`commands/mod.rs:59`) and the `--debug`/settings path (`lib.rs:539`).
- `fn level_filter_from_u8(value: u8) -> log::LevelFilter` (`lib.rs:53`) — maps 0–5 to `Off..Trace`.
- `fn build_console_filter() -> env_filter::Filter` (`lib.rs:65`) — parses `RUST_LOG`, falling back to `Info`; warns and falls back on invalid spec (`lib.rs:70-77`).
- `fn show_main_window(app: &AppHandle)` (`lib.rs:87`) — unminimize+show+focus the `"main"` webview; macOS: sets `ActivationPolicy::Regular` to restore the dock icon (`lib.rs:98-103`). Logs the available labels if `"main"` is missing (`lib.rs:107-111`).
- `fn should_force_show_permissions_window(app: &AppHandle) -> bool` (`lib.rs:115`) — **Windows-only** body: if any model is downloaded and microphone permission is `Denied`, force the onboarding window visible (`lib.rs:116-135`); other platforms return `false`.
- `fn initialize_core_logic(app_handle: &AppHandle)` (`lib.rs:140`) — see §2 step 3.
- `#[tauri::command] fn trigger_update_check(app: AppHandle) -> Result<(), String>` (`lib.rs:299`) — emits `check-for-updates` if `update_checks_enabled` (`lib.rs:301-306`).
- `#[tauri::command] fn show_main_window_command(app: AppHandle) -> Result<(), String>` (`lib.rs:311`) — frontend-callable wrapper over `show_main_window`.
- `#[cfg_attr(mobile, tauri::mobile_entry_point)] pub fn run(cli_args: CliArgs)` (`lib.rs:316-317`) — the bootstrap; note the **mobile entry point attribute exists** so iOS/Android can call `run` directly.

### 3.2 `settings.rs`

- **`pub struct AppSettings`** (`settings.rs:337-433`) — the master settings record (~55 fields). Includes `bindings: HashMap<String, ShortcutBinding>`, audio/feedback flags, `start_hidden`, `autostart_enabled`, `update_checks_enabled`, `selected_model`, microphone/output device selection, `overlay_position`, `log_level`, `model_unload_timeout`, `history_limit`, `recording_retention_period`, paste/clipboard/auto-submit config, the full post-process LLM provider block, `app_language`, `show_tray_icon`, accelerator settings, and `extra_recording_buffer_ms`. Derives `Serialize, Deserialize, Debug, Clone, Type` (`Type` = specta, so it crosses to TS).
- **Enums** (all `#[serde(rename_all=...)]`, all `specta::Type`):
  - `LogLevel` (`settings.rs:13-21`) with a **custom `Deserialize`** accepting both old numeric (1–5) and new string forms (`settings.rs:24-66`) — a migration safety net.
  - `OverlayPosition { None, Top, Bottom }` (`settings.rs:109-115`).
  - `ModelUnloadTimeout` (`settings.rs:117-128`) with `to_minutes()` / `to_seconds()` (`settings.rs:210-232`).
  - `PasteMethod` (`settings.rs:130-139`), `ClipboardHandling` (`settings.rs:141-146`), `AutoSubmitKey` (`settings.rs:148-154`), `RecordingRetentionPeriod` (`settings.rs:156-164`), `KeyboardImplementation` (`settings.rs:166-171`), `SoundTheme` (`settings.rs:234-258`), `TypingTool` (`settings.rs:260-269`), `WhisperAcceleratorSetting` (`settings.rs:277-289`), `OrtAcceleratorSetting` (`settings.rs:291-306`).
- **`struct ShortcutBinding`** (`settings.rs:80-87`) — id/name/description/default/current binding strings.
- **`struct LLMPrompt`** (`settings.rs:89-94`) and **`struct PostProcessProvider`** (`settings.rs:96-107`) — the post-processing LLM config primitives.
- **`pub(crate) struct SecretMap(HashMap<String,String>)`** (`settings.rs:308-310`) — newtype with a **redacting `Debug`** impl so API keys never hit logs (`settings.rs:312-321`); `Deref`/`DerefMut` to the map (`settings.rs:323-334`). Tested at `settings.rs:962-988`.
- **Persistence helpers:**
  - `pub const SETTINGS_STORE_PATH = "settings_store.json"` (`settings.rs:713`).
  - `pub fn get_default_settings() -> AppSettings` (`settings.rs:715`) — builds platform-specific default shortcuts (`settings.rs:716-723`, `736-743`) plus the three default bindings (transcribe / transcribe_with_post_process / cancel) (`settings.rs:725-765`).
  - `fn ensure_post_process_defaults(&mut AppSettings) -> bool` (`settings.rs:657`) — idempotent migration that re-adds missing providers/keys/models and syncs `supports_structured_output`; returns `changed`.
  - `pub fn load_or_create_app_settings(app: &AppHandle) -> AppSettings` (`settings.rs:843`) — reads the store, merges in any new default *bindings*, falls back to defaults on parse failure, runs `ensure_post_process_defaults`, persists if changed.
  - `pub fn get_settings(app: &AppHandle) -> AppSettings` (`settings.rs:894`) — the hot-path reader used everywhere (tray, overlay, window events); same fallback + ensure-defaults logic.
  - `pub fn write_settings(app: &AppHandle, settings: AppSettings)` (`settings.rs:918`).
  - `pub fn get_bindings` (`settings.rs:926`), `get_stored_binding` (`settings.rs:932`), `get_history_limit` (`settings.rs:940`), `get_recording_retention_period` (`settings.rs:945`).
  - `impl AppSettings`: `active_post_process_provider()`, `post_process_provider()`, `post_process_provider_mut()` (`settings.rs:820-841`).

> **Note on store path:** every store open uses `crate::portable::store_path(SETTINGS_STORE_PATH)` (`settings.rs:846`, `896`, `920`), making settings portable-aware. The store key is the literal `"settings"` (one JSON blob, not per-field keys).

### 3.3 `tray.rs`

- `enum TrayIconState { Idle, Recording, Transcribing }` (`tray.rs:14-19`) — pipeline visual state (re-exported via `utils::*`).
- `enum AppTheme { Dark, Light, Colored }` (`tray.rs:21-26`) — `Colored` is the Linux pink theme.
- `pub fn get_current_theme(app) -> AppTheme` (`tray.rs:29`) — Linux ⇒ always `Colored`; else maps the main window's system theme (`tray.rs:30-44`).
- `pub fn get_icon_path(theme, state) -> &'static str` (`tray.rs:48`) — 9-way match returning a `resources/...png` path (`tray.rs:49-63`).
- `pub fn change_tray_icon(app, icon: TrayIconState)` (`tray.rs:65`) — resolves+sets the icon and rebuilds the menu (`tray.rs:65-82`).
- `pub fn tray_tooltip() -> String` / `fn version_label()` (`tray.rs:84-94`) — "Handy v{CARGO_PKG_VERSION}", with "(Dev)" in debug.
- `pub fn update_tray_menu(app, state: &TrayIconState, locale: Option<&str>)` (`tray.rs:96`) — rebuilds the whole menu: version (disabled), settings, check-updates (enabled per `update_checks_enabled`), copy-last-transcript, a **model submenu** of downloaded models with the active one checked (`tray.rs:142-169`), unload-model (enabled iff a model is loaded), and quit; the Recording/Transcribing variant inserts a `cancel` item (`tray.rs:180-218`). Uses platform accelerators (`Cmd+,`/`Cmd+Q` vs `Ctrl+,`/`Ctrl+Q`) (`tray.rs:103-106`).
- `fn last_transcript_text(entry: &HistoryEntry) -> &str` (`tray.rs:226`) — prefers `post_processed_text`, falls back to raw (`tray.rs:226-231`; tested `tray.rs:292-302`).
- `pub fn set_tray_visibility(app, visible: bool)` (`tray.rs:233`).
- `pub fn copy_last_transcript(app)` (`tray.rs:242`) — fetches the latest completed history entry and writes it to the clipboard (`tray.rs:242-271`).

### 3.4 `tray_i18n.rs` + `build.rs`

- `include!(concat!(env!("OUT_DIR"), "/tray_translations.rs"))` (`tray_i18n.rs:19`) — pulls in the build-time-generated `struct TrayStrings` and `static TRANSLATIONS: Lazy<HashMap<&str, TrayStrings>>`.
- `pub fn get_tray_translations(locale: Option<String>) -> TrayStrings` (`tray_i18n.rs:24`) — lookup order full-locale → language-code → `"en"` (`tray_i18n.rs:28-33`).
- The generator `fn generate_tray_translations()` (`build.rs:14`) walks `../src/i18n/locales/*/translation.json`, takes the `"tray"` object, derives struct fields from the English keys (camelCase→snake_case via `build.rs:94`), and emits Rust source (`build.rs:54-91`). `cargo:rerun-if-changed` is set on the locales dir (`build.rs:22`, `36`).

### 3.5 `overlay.rs`

- Constants: `OVERLAY_WIDTH=172.0`, `OVERLAY_HEIGHT=36.0` (`overlay.rs:34-35`); per-platform top/bottom offsets (`overlay.rs:37-46`).
- macOS panel type declared via `tauri_panel! { panel!(RecordingOverlayPanel { can_become_key_window:false, is_floating_panel:true }) }` (`overlay.rs:25-32`).
- `fn get_monitor_with_cursor(app) -> Option<Monitor>` (`overlay.rs:142`) — finds the monitor under the cursor via `input::get_cursor_position` (Enigo), normalizing physical→logical by `scale_factor` (`overlay.rs:142-170`); falls back to primary monitor.
- `fn is_mouse_within_monitor(...)` (`overlay.rs:172`) — bounds check.
- `fn calculate_overlay_position(app) -> Option<(f64,f64)>` (`overlay.rs:203`) — horizontally centered, vertically anchored top/bottom per `overlay_position` (`overlay.rs:213-219`). Uses logical coords deliberately to survive cross-monitor moves (`overlay.rs:193-202`).
- `pub fn create_recording_overlay(app)` — **two cfg-gated impls**: non-macOS (`overlay.rs:225-283`) builds a transparent, always-on-top, skip-taskbar, non-focusable `WebviewWindow` loading `src/overlay/index.html`, with portable webview dir, and (Linux) initializes GTK layer shell; macOS (`overlay.rs:286-320`) builds an `NSPanel` via `PanelBuilder` at `PanelLevel::Status` with `can_join_all_spaces` + `full_screen_auxiliary` collection behavior, then hides it.
- Linux-only: `update_gtk_layer_shell_anchors` (`overlay.rs:48-67`), `env_flag_enabled` (`overlay.rs:73-82`), `init_gtk_layer_shell` (`overlay.rs:86-110`) — togglable via `HANDY_NO_GTK_LAYER_SHELL` (`overlay.rs:88`).
- Windows-only: `force_overlay_topmost` (`overlay.rs:114-140`) — raw Win32 `SetWindowPos(HWND_TOPMOST, …)` on the UI thread.
- `fn show_overlay_state(app, state: &str)` (`overlay.rs:322`) — early-returns if `overlay_position == None` (`overlay.rs:324-327`), repositions, shows, re-asserts topmost on Windows, emits `show-overlay` with the state string.
- Public state shows: `show_recording_overlay` (`overlay.rs:343`), `show_transcribing_overlay` (`overlay.rs:348`), `show_processing_overlay` (`overlay.rs:353`).
- `pub fn update_overlay_position(app)` (`overlay.rs:358`).
- `pub fn hide_recording_overlay(app)` (`overlay.rs:373`) — emits `hide-overlay` for a fade-out, then **spawns a thread that sleeps 300 ms and hides** the window (`overlay.rs:380-385`).
- `pub fn emit_levels(app, levels: &Vec<f32>)` (`overlay.rs:388`) — emits `mic-level` to both the main app and the overlay window (`overlay.rs:388-396`).

### 3.6 `cli.rs`

- `#[derive(Parser, Debug, Clone, Default)] pub struct CliArgs` (`cli.rs:3-29`): `start_hidden`, `no_tray`, `toggle_transcription`, `toggle_post_process`, `cancel`, `debug` — all `--long` bool flags. `Default` lets the mobile/library path construct empty args.

### 3.7 `signal_handle.rs`

- `pub fn send_transcription_input(app: &AppHandle, binding_id: &str, source: &str)` (`signal_handle.rs:16`) — looks up the `TranscriptionCoordinator` via `try_state`, calls `c.send_input(binding_id, source, true, false)` (i.e. **press=true, push_to_talk=false** → toggle semantics) (`signal_handle.rs:17-21`). Warns if the coordinator is absent.
- `#[cfg(unix)] pub fn setup_signal_handler(app_handle: AppHandle, mut signals: Signals)` (`signal_handle.rs:25`) — spawns a thread iterating `signals.forever()`, mapping `SIGUSR1 → transcribe_with_post_process` and `SIGUSR2 → transcribe` (`signal_handle.rs:27-37`).

### 3.8 `portable.rs`

- `static PORTABLE_DATA_DIR: OnceLock<Option<PathBuf>>` (`portable.rs:11`).
- `pub fn init()` (`portable.rs:15`) — one-time detection: portable if a `portable` marker file next to the exe contains the magic string `"Handy Portable Mode"`, **or** (legacy migration) an empty marker exists alongside an existing `Data/` dir, in which case it upgrades the marker in place (`portable.rs:23-34`). Creates `Data/` if needed (`portable.rs:36-44`).
- `pub fn is_portable() -> bool` (`portable.rs:49`); `pub fn data_dir() -> Option<&'static PathBuf>` (`portable.rs:55`).
- Portable-aware path helpers that wrap Tauri's path API: `app_data_dir` (`portable.rs:60`), `app_log_dir` (`portable.rs:69`), `resolve_app_data` (`portable.rs:79`), `store_path` (`portable.rs:86`).
- `fn is_valid_portable_marker(path) -> bool` (`portable.rs:96`) — extracted for unit tests (`portable.rs:102-166`).

### 3.9 `utils.rs`

- `pub fn cancel_current_operation(app: &AppHandle)` (`utils.rs:17`) — the centralized cancel: unregister the cancel shortcut (`utils.rs:21`), cancel any recording (`utils.rs:24-26`), reset tray icon to Idle + hide overlay (`utils.rs:29-30`), immediate model-unload if configured (`utils.rs:33-34`), and notify the coordinator with `notify_cancel(recording_was_active)` (`utils.rs:37-39`).
- Linux display-server detection: `is_wayland` (`utils.rs:46`), `is_kde_plasma` (`utils.rs:55`), `is_kde_wayland` (`utils.rs:64`).
- Re-exports `crate::clipboard::*`, `crate::overlay::*`, `crate::tray::*` (`utils.rs:11-13`) — this is why `utils::update_tray_menu` and `utils::create_recording_overlay` are callable from `lib.rs` (`lib.rs:267`, `294`).

---

## 4. Threading / concurrency model

The lifecycle subsystem is mostly main-thread/event-driven, but spawns several long-lived and one-shot threads:

- **Single-instance plugin callback** (`lib.rs:483-493`) runs on the *primary* (already-running) instance when a *second* instance launches; it dispatches into the coordinator or `cancel_current_operation`. The second process exits immediately after forwarding its argv.
- **`TranscriptionCoordinator`** (managed at `lib.rs:541`; impl `transcription_coordinator.rs:36-159`) owns a dedicated worker thread that drains an `mpsc::channel::<Command>` (`transcription_coordinator.rs:46-114`). It is the **single serialization point** for all transcription lifecycle events (keyboard, signal, CLI, cancel), with a 30 ms debounce on presses (`transcription_coordinator.rs:10`, `63-70`) and a `Stage { Idle, Recording, Processing }` state machine (`transcription_coordinator.rs:27-31`). The worker is wrapped in `catch_unwind` so a panic is logged rather than killing the process (`transcription_coordinator.rs:49`, `111-113`).
- **Unix signal thread** (`signal_handle.rs:27-37`) — blocks on `signals.forever()`, forwarding to the coordinator.
- **GPU pre-warm thread** (`lib.rs:552-554`) — one-shot, calls `get_available_accelerators()` so the first Advanced-settings open doesn't freeze the UI.
- **Model-switch-from-tray thread** (`lib.rs:247-259`) — one-shot per tray model selection, so the menu-event handler stays responsive.
- **Overlay fade-out thread** (`overlay.rs:381-384`) — one-shot 300 ms sleep then `window.hide()`.
- **Locks:** `EnigoState(Mutex<Enigo>)` (`input.rs:7`) is the only mutex in this subsystem; `get_cursor_position` (`input.rs:19`, used by overlay positioning) takes it briefly. The file-log level is a lock-free `AtomicU8` (`lib.rs:51`). Portable state is a write-once `OnceLock` (`portable.rs:11`).
- **macOS UI-thread marshaling:** Windows topmost (`overlay.rs:124`) and Linux GTK anchor updates (`overlay.rs:51`) use `run_on_main_thread` because the native calls must happen on the UI thread.

---

## 5. Data flow IN and OUT

### IN (what drives this subsystem)
- **OS/process:** argv → `clap` → `CliArgs` (`main.rs:8`); environment (`RUST_LOG`, `WAYLAND_DISPLAY`, `HANDY_NO_GTK_LAYER_SHELL`, `SDKROOT`/`SWIFTC` at build, `WEBKIT_DISABLE_DMABUF_RENDERER` set at `main.rs:14`).
- **Second instance argv** → single-instance plugin callback (`lib.rs:483-493`).
- **Unix signals** `SIGUSR1`/`SIGUSR2` (`signal_handle.rs:29-33`).
- **Frontend → backend commands** (specta-registered, `lib.rs:325-430`): e.g. `show_main_window_command`, `trigger_update_check`, `cancel_operation`, `get_app_settings`, `set_log_level`, `initialize_enigo`, `initialize_shortcuts`, plus all the `shortcut::change_*_setting` mutators.
- **Tray menu events** (`lib.rs:207-261`) and **window events** (close/theme, `lib.rs:576-604`).
- **Internal event `model-state-changed`** (subscribed at `lib.rs:277`) → tray refresh.

### OUT (what this subsystem calls / emits)
- **Managed state writes:** `.manage(...)` for the four managers, `TranscriptionCoordinator`, `TrayIcon`, `CliArgs`, `EnigoState`, `ShortcutsInitialized` (`lib.rs:163-166`, `264`, `507`, `541`; `commands/mod.rs:146`, `183`).
- **`TranscriptionCoordinator::send_input / notify_cancel / notify_processing_finished`** (`transcription_coordinator.rs:121-158`) — into which the coordinator calls `ACTION_MAP` actions (`transcription_coordinator.rs:161-184`), which in turn drive overlay/tray (`actions.rs:36`, `409`).
- **Tauri events emitted to the frontend:** `check-for-updates` (`lib.rs:215`, `304`); to overlay window `show-overlay` / `hide-overlay` / `mic-level` (`overlay.rs:338`, `378`, `394`); `mic-level` to main app (`overlay.rs:390`); `HistoryUpdatePayload` (registered `lib.rs:430`).
- **Store I/O** to `settings_store.json` via `tauri-plugin-store` (`settings.rs:843-924`).
- **Clipboard writes** for copy-last-transcript (`tray.rs:265`).
- **OS shell:** `tauri-plugin-opener` opens recordings/log/data dirs (`commands/mod.rs:73-113`); `tauri-plugin-autostart` enable/disable (`lib.rs:282-291`).

---

## 6. Error handling & edge cases

- **Settings parse failure** (`settings.rs:873-879`, `900-908`): logs a warning, overwrites the store with defaults rather than crashing. `LogLevel` accepts both legacy numeric and string forms (`settings.rs:24-66`).
- **Manager init failure** is *fatal by design*: `initialize_core_logic` uses `.expect()` on each manager (`lib.rs:148-157`) — a hard guarantee that the app never runs half-initialized. Same with the tray build `.unwrap()` (`lib.rs:262-263`).
- **Missing main window:** `show_main_window` logs the available labels instead of panicking (`lib.rs:107-111`).
- **Overlay creation is best-effort:** if the position can't be computed (no monitor) on non-Linux it skips creation entirely (`overlay.rs:229-236`); macOS/Windows/Linux build failures are logged, not fatal (`overlay.rs:279-281`, `315-317`).
- **Coordinator robustness:** debounce drops key-repeat (`transcription_coordinator.rs:63-70`); `Cancel` is ignored during `Processing` to avoid corrupting an in-flight pipeline (`transcription_coordinator.rs:97-103`); the worker is panic-isolated (`transcription_coordinator.rs:49`).
- **`unwrap()` hot-spots to note:** `get_stored_binding` unwraps the binding lookup (`settings.rs:935`) and `serde_json::to_value(&settings).unwrap()` is used on writes (`settings.rs:868`, `877`, `883`, `888`, `903`, `907`, `913`, `923`) — these assume well-formed in-memory state.
- **macOS startup crash avoidance:** Apple-Intelligence availability is deliberately *not* probed at startup (would `SIGABRT` on macOS 26 beta); the provider is always listed and checked lazily on use (`settings.rs:576-590`).
- **Single-instance unknown-arg fallthrough:** any argv not matching a known flag just shows the window (`lib.rs:490-492`).

---

## 7. State & persistence touched

- **`settings_store.json`** (key `"settings"`) via `tauri-plugin-store` — the only persistence this subsystem owns directly (`settings.rs:713`, `846`). Path is portable-aware (`portable::store_path`).
- **Log files** — `handy*.log` under either `Data/logs` (portable) or the platform log dir, max 500 KB, `KeepOne` rotation (`lib.rs:447-468`).
- **WebView cache** — redirected to `Data/webview` in portable mode for both the main window (`lib.rs:522-523`) and the overlay (`overlay.rs:260-261`).
- **Autostart registration** — OS-level (LaunchAgent on macOS) toggled from `autostart_enabled` (`lib.rs:282-291`).
- **Indirectly referenced (owned by other subsystems):** model files (via `ModelManager`), SQLite history DB + recordings WAVs (via `HistoryManager`), all under the same app-data/portable root (`commands/mod.rs:77`).
- **Build-time artifacts:** `OUT_DIR/tray_translations.rs` (`build.rs:85`) and the Apple-Intelligence static lib (`build.rs:128-251`).

---

## 8. Platform-specific branches (cfg gates)

- **macOS (`target_os="macos"`):** `windows_subsystem` n/a; activation-policy juggling Regular↔Accessory (`lib.rs:98-103`, `181-187`, `581-596`); `Reopen` dock handler (`lib.rs:609-612`); `tauri_nspanel` plugin + `NSPanel` overlay (`lib.rs:477-480`, `overlay.rs:25-32`, `286-320`); tray accelerators `Cmd+,`/`Cmd+Q` (`tray.rs:104`); Apple-Intelligence provider gated on `aarch64` (`settings.rs:580-590`); paste virtual-keys (`input.rs:30-31`).
- **Windows (`target_os="windows"`):** `windows_subsystem="windows"` in release (`main.rs:2`); permission-onboarding force-show (`lib.rs:116-135`); Win32 `force_overlay_topmost` (`overlay.rs:114-140`); NSIS/signing config (`tauri.conf.json:60-65`); paste VK codes (`input.rs:33`, `62`, `93`).
- **Linux (`target_os="linux"`):** WebKit DMABUF workaround (`main.rs:10-15`); GTK layer-shell overlay + `HANDY_NO_GTK_LAYER_SHELL` escape hatch (`overlay.rs:18-110`); `OverlayPosition::None` default (`settings.rs:464-465`); `Colored` tray theme (`tray.rs:30-32`); `KeyboardImplementation::Tauri` default (`settings.rs:175-176`); `PasteMethod::Direct` default (`settings.rs:191-192`); Wayland/KDE detection (`utils.rs:45-66`).
- **Unix (`cfg(unix)`):** signal-hook handlers (`lib.rs:33-36`, `173-177`; `signal_handle.rs:24-38`).
- **Mobile (`cfg(mobile)`):** `#[cfg_attr(mobile, tauri::mobile_entry_point)]` on `run` (`lib.rs:316`) — the *only* mobile gate in this subsystem; **no iOS/Android-specific code exists** beyond this attribute.
- **Catch-all fallback shortcuts** (`alt+space`) for non-mac/win/linux targets (`settings.rs:722-723`, `742-743`).

---

## 9. PLAUD relevance — concrete extension points

A Plaud-style product = always-on / call & system-audio capture, long-form recording, multi-speaker diarization, AI summaries, cloud + local sync, and an iPhone companion. Map onto this subsystem as follows:

1. **Capture trigger surface — reuse the coordinator bridge.** `send_transcription_input()` (`signal_handle.rs:16`) and `TranscriptionCoordinator::send_input` (`transcription_coordinator.rs:121`) are the clean choke point. Add a `--start-recording` / `--stop-recording` (long-form) pair to `CliArgs` (`cli.rs:5-29`) and route them in the single-instance callback (`lib.rs:483-493`), plus a new `SIGRTMIN`-style signal in `setup_signal_handler` (`signal_handle.rs:25-38`). This gives you headless, scriptable, phone-triggered (over SSH/shortcuts) capture with zero pipeline rework.
2. **Long-form / continuous recording — extend the state machine.** The coordinator's `Stage` enum (`transcription_coordinator.rs:27-31`) and the toggle semantics in `signal_handle.rs:18` assume short dictation. Add a `Stage::LongRecording { session_id }` and a `Command::SegmentFlush` so the worker can chunk audio into rolling segments without leaving `Recording`. `ModelUnloadTimeout` (`settings.rs:117-128`) and `extra_recording_buffer_ms` (`settings.rs:432`) are the existing knobs to extend with a "keep mic hot / no idle unload during session" mode.
3. **System / call audio capture.** This is *not* in the lifecycle layer — it lives in `managers/audio.rs` / `audio_toolkit/`. But the lifecycle wiring you must touch: add device-mode settings beside `selected_microphone` / `selected_output_device` / `always_on_microphone` (`settings.rs:354-361`) — e.g. `capture_system_audio: bool`, `loopback_device: Option<String>`. macOS needs a ScreenCaptureKit/CoreAudio-tap entitlement; thread it through the macOS bundle config (`tauri.conf.json:39-45`, `Entitlements.plist`).
4. **Speaker diarization & multi-speaker conversations.** No diarization exists anywhere. The settings model is where speaker config belongs: add a `diarization_enabled`, `max_speakers`, and a `speaker_labels: HashMap<String,String>` to `AppSettings` (`settings.rs:337-433`). The history entry struct (`HistoryEntry`, referenced in `tray.rs:279-289`) would need a `segments: Vec<Segment{ speaker, start_ms, end_ms, text }>` field; `last_transcript_text` (`tray.rs:226`) and `copy_last_transcript` (`tray.rs:242`) then become speaker-aware exports.
5. **AI summaries — reuse the post-process provider stack.** The full LLM provider/key/model/prompt config already exists: `PostProcessProvider` (`settings.rs:96-107`), `default_post_process_providers` (`settings.rs:524-613` — OpenAI, Anthropic, Groq, Cerebras, Bedrock, OpenRouter, Apple Intelligence, custom/Ollama), `SecretMap` for keys (`settings.rs:308-321`), and `LLMPrompt` (`settings.rs:89-94`). A Plaud "summary/action-items/mind-map" feature is a new set of `LLMPrompt` templates plus a `post_process_mode` enum — **no new provider plumbing needed**. The `default_post_process_prompts` template at `settings.rs:641-647` shows the `${output}` substitution convention to follow.
6. **Cloud / local sync.** Today everything is local: portable `Data/` (`portable.rs`), `settings_store.json`, the SQLite history DB. Hook points: (a) wrap `write_settings` (`settings.rs:918`) to push a sync delta; (b) add a `sync` module managed at `lib.rs:541`-style alongside the coordinator; (c) add `cloud_sync_enabled` + auth tokens (store them in a `SecretMap` to inherit redaction, `settings.rs:308-321`). The single-instance callback (`lib.rs:483-493`) is also where a `--sync-now` flag would land.
7. **Mobile (iPhone).** `run(cli_args)` already carries the `mobile_entry_point` attribute (`lib.rs:316`) and `CliArgs: Default` (`cli.rs:3`) so a mobile shell can call `run(CliArgs::default())`. The blockers are all the desktop-only cfg branches: tray (`lib.rs:194-264`), NSPanel/GTK/Win32 overlay (`overlay.rs`), Unix signals (`signal_handle.rs`), and Enigo paste (`input.rs`). Strategy: gate the entire `initialize_core_logic` tray+overlay block behind `#[cfg(desktop)]` and provide a mobile lifecycle that keeps only the managers + coordinator + settings, with a native iOS recording UI replacing the overlay.
8. **Background reliability.** `autostart_enabled` (`lib.rs:282-291`) and `start_hidden` (`lib.rs:564-572`) already give "launch on login, run in tray." For a recorder you'd add a watchdog/relaunch and a "recording in progress" tray state — extend `TrayIconState` (`tray.rs:14-19`) with a `LongRecording` variant and a matching icon set in `get_icon_path` (`tray.rs:48-63`).

---

## 10. Gaps vs a Plaud-style product

- **No long-form session concept.** Coordinator/state machine is dictation-shaped (`transcription_coordinator.rs:27-31`); no session id, no rolling segmentation, no pause/resume of a long recording, no crash-recovery of an in-progress capture.
- **No system/loopback or call-audio capture.** Settings only model a single input mic + output device (`settings.rs:354-361`); there is no loopback/system-audio/meeting-bot path and no related macOS tap entitlement wiring.
- **No diarization / speaker model.** Nothing in settings, history, or tray represents speakers; transcripts are flat text (`tray.rs:226-231`).
- **No structured conversation/meeting object.** History is per-utterance (`HistoryEntry` in `tray.rs:279-289`); there's no notion of a multi-turn conversation, participants, or timeline.
- **Summaries exist only as ad-hoc post-processing.** The LLM stack (`settings.rs:96-107`, `524-647`) cleans a single transcript; there are no summary/action-item/chapter artifacts persisted or surfaced.
- **No cloud sync, accounts, or sharing.** Storage is 100% local/portable (`portable.rs`); `update_checks_enabled` (`settings.rs:350-351`) is the only network-facing lifecycle feature, and the updater endpoint is a static GitHub release URL (`tauri.conf.json:68-73`).
- **No real mobile app.** Only the `mobile_entry_point` attribute (`lib.rs:316`) exists; the entire tray/overlay/signal/paste surface is desktop-only and would be dead weight on iOS.
- **No telemetry/analytics or usage events** beyond log files (`lib.rs:447-468`), so a product dashboard ("X minutes recorded this week") has no data source.
- **Secrets stored in plaintext store with only Debug-redaction.** `SecretMap` redacts logs (`settings.rs:312-321`) but the underlying `settings_store.json` holds API keys in clear text — insufficient for cloud-account tokens; a Plaud product needs OS keychain integration.
- **Single-blob settings persistence** (one `"settings"` JSON value, `settings.rs:846-924`) means every write rewrites the whole record — fine now, but a poor fit for high-frequency sync deltas or multi-device merge.
- **Overlay is fixed-size status chrome** (172×36, `overlay.rs:34-35`) with no live transcript / waveform / speaker view — a Plaud capture UI would need a richer, resizable surface or a separate window.
