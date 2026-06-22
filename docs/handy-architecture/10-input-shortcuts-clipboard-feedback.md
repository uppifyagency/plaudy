# Handy Subsystem 10 — Global Shortcuts, Key Handling, Text Injection / Paste, Clipboard & Audio Feedback

> **Abstract.** This subsystem is the entire *input/output edge* of Handy: it owns the global keyboard hotkeys that start/stop/cancel recording, the indirection layer that lets users pick between Tauri's `global-shortcut` plugin and the custom `handy-keys` library, the shared event-dispatch logic that turns a key press into a `ShortcutAction`, and the *output* side — synthesizing keystrokes (`enigo`), driving the system clipboard, choosing among six paste strategies (incl. Linux-native `wtype`/`xdotool`/`ydotool`/`dotool`/`kwtype`/`wl-copy` tools), auto-submitting (`Enter`), and playing start/stop/feedback sounds via `rodio`. It also contains the macOS-only clamshell helper used to pick a different microphone when the lid is closed. All of these are pure *peripherals* around the recording→transcription→paste pipeline; understanding them is the prerequisite for any Plaud-style "always-listening, multi-speaker, summarize-and-sync" product, because every one of these touchpoints (keyboard trigger, clipboard write, paste injection, feedback sound) is a place a Plaud product either *removes* (background capture instead of hotkey) or *augments* (live overlay, speaker labels, summaries).

All file/line citations are relative to `handy/src-tauri/src/` unless an absolute path is given.

---

## 0. Subsystem map & request lifecycle

```
                                   USER PRESSES HOTKEY
                                          │
        ┌─────────────────────────────────┴─────────────────────────────────┐
        │                                                                     │
  KeyboardImplementation::Tauri                            KeyboardImplementation::HandyKeys
  shortcut/tauri_impl.rs                                   shortcut/handy_keys.rs
  (tauri_plugin_global_shortcut)                           (handy-keys lib, dedicated mgr thread)
        │                                                                     │
        └──────────────► handle_shortcut_event() ◄────────────────────────────┘
                          shortcut/handler.rs:29
                                    │
            ┌───────────────────────┼─────────────────────────────┐
            │                       │                              │
   is_transcribe_binding?      binding_id=="cancel"          other (e.g. "test")
            │ yes                   │  → CancelAction              │ → ACTION_MAP
            ▼                       ▼  (utils::cancel_*)           ▼
  TranscriptionCoordinator   ACTION_MAP["cancel"].start    ACTION_MAP[id].start/stop
  .send_input()                                            actions.rs:700 ACTION_MAP
  transcription_coordinator.rs
            │ (single serialized thread, debounce 30ms, PTT/toggle)
            ▼
  TranscribeAction::start / ::stop   actions.rs:389
   start → AudioRecordingManager.try_start_recording + play_feedback_sound(Start)
   stop  → stop_recording → tm.transcribe → process_transcription_output
                                            │
                                            ▼
                                  utils::paste(text, app)   ← (clipboard.rs::paste)
                                  run on MAIN THREAD (actions.rs:609)
                                            │
        ┌───────────────┬───────────────────┼───────────────────┬───────────────┐
        ▼               ▼                    ▼                   ▼               ▼
   PasteMethod::    ::Direct            ::CtrlV /            ::ExternalScript  ::None
   (input.rs)    (enigo.text or       ::CtrlShiftV /        (run script)     (skip)
                 linux native type)   ::ShiftInsert
                                      paste_via_clipboard()
                                      save clip → write text → key combo → restore clip
                                            │
                                       auto_submit? → send_return_key (Enter/Ctrl+Enter/Cmd+Enter)
                                       clipboard_handling==CopyToClipboard? → leave text on clipboard
```

The **feedback sounds** (`audio_feedback.rs`) are played from `TranscribeAction` at recording start/stop; **clamshell** (`helpers/clamshell.rs`) is consulted not by this subsystem directly but by `managers/audio.rs:193` to substitute a microphone, and it is documented here because it lives in `helpers/` and is part of the "input edge."

---

## 1. Per-file responsibility

| File | Responsibility (1–3 lines) |
|---|---|
| `shortcut/mod.rs` | Public facade + dispatcher. Routes every shortcut operation (`init`/`register`/`unregister`/`register_cancel`) to the active `KeyboardImplementation`. Hosts ~40 `#[tauri::command]` setters for nearly all I/O-edge settings (paste method, clipboard, audio feedback, sound theme, auto-submit, typing tool, post-process providers/prompts/keys, accelerators). Implements runtime switching between Tauri and HandyKeys backends with validation/reset. |
| `shortcut/handler.rs` | The single shared `handle_shortcut_event()` used by *both* backends. Decides: transcribe→coordinator, cancel→only-when-recording, else→`ACTION_MAP` start/stop. |
| `shortcut/tauri_impl.rs` | Backend A: wraps `tauri_plugin_global_shortcut`. Parses/validates shortcut strings, registers `on_shortcut` closures, dynamically registers/unregisters the cancel hotkey. Rejects modifier-only and `fn` combos. |
| `shortcut/handy_keys.rs` | Backend B: wraps the `handy-keys` crate behind a dedicated manager thread (mpsc command channel). Owns `HandyKeysState` (Tauri-managed). Supports modifier-only + `fn` hotkeys, and a "recording mode" that streams raw key events to the frontend for the shortcut-capture UI. |
| `input.rs` | Low-level keystroke synthesis via `enigo`. `EnigoState` (Mutex<Enigo> in managed state); platform-specific paste keystrokes (Cmd/Ctrl+V, Ctrl+Shift+V, Shift+Insert by raw keycode for layout independence); direct text typing; cursor position read. |
| `clipboard.rs` | The `paste()` orchestrator + every paste strategy. Clipboard save/write/restore, Linux-native tool detection & invocation (`wtype`/`kwtype`/`dotool`/`ydotool`/`xdotool`/`wl-copy`), external-script paste, auto-submit `Enter`, and optional "leave on clipboard." |
| `audio_feedback.rs` | Plays start/stop/test feedback sounds through `rodio`, honoring `audio_feedback`, `audio_feedback_volume`, `sound_theme` (marimba/pop/custom WAVs), and the selected output device. Async + blocking variants. |
| `helpers/clamshell.rs` | macOS-only `ioreg`/`pmset` shellouts to detect lid-closed clamshell mode and laptop-vs-desktop; stubs return `false` elsewhere. Used by the audio manager to switch mics. |

Supporting files read to trace data flow (not primary but cited): `actions.rs`, `transcription_coordinator.rs`, `signal_handle.rs`, `utils.rs`, `settings.rs`, `commands/mod.rs`, `lib.rs`.

---

## 2. Important types, traits & public functions (with signatures & file:line)

### 2.1 `shortcut/mod.rs` — facade & commands

- `pub fn init_shortcuts(app: &AppHandle)` — `shortcut/mod.rs:34`. Reads `keyboard_implementation`; calls `tauri_impl::init_shortcuts` or `handy_keys::init_shortcuts`. On HandyKeys failure it **persists a fallback** to `Tauri` and retries (`mod.rs:43-54`). This is the *only* place that writes the fallback so it isn't retried each launch.
- `pub fn register_cancel_shortcut(app)` / `unregister_cancel_shortcut(app)` — `mod.rs:60`, `mod.rs:69`. Dispatch by impl. Called when recording starts/stops so `Escape` only intercepts while recording.
- `pub fn register_shortcut(app, binding: ShortcutBinding) -> Result<(),String>` / `unregister_shortcut(...)` — `mod.rs:78`, `mod.rs:87`. Dispatch by impl.
- `struct BindingResponse { success, binding: Option<ShortcutBinding>, error: Option<String> }` — `mod.rs:99` (`#[derive(Serialize, Type)]`).
- `#[tauri::command] pub fn change_binding(app, id: String, binding: String) -> Result<BindingResponse,String>` — `mod.rs:108`. Rejects empty bindings; clones existing-or-default; **special-cases `"cancel"`** (just store, never live-register; `mod.rs:149-160`); unregisters old, validates new for the active impl, registers new, persists. The central "rebind" command.
- `#[tauri::command] pub fn reset_binding(app, id) -> Result<BindingResponse,String>` — `mod.rs:206`. Restores `default_binding`.
- `#[tauri::command] pub fn suspend_binding(app, id) -> Result<(),String>` / `resume_binding(...)` — `mod.rs:215`, `mod.rs:228`. Temporarily unregister/re-register while the user records a new combo in the UI (avoids firing the action during capture).
- `struct ImplementationChangeResult { success: bool, reset_bindings: Vec<String> }` — `mod.rs:244`.
- `#[tauri::command] pub fn change_keyboard_implementation_setting(app, implementation: String) -> Result<ImplementationChangeResult,String>` — `mod.rs:256`. Runtime backend switch: unregister-all from old, persist new impl, (lazy-init HandyKeys with rollback), register-all under new impl resetting incompatible bindings to defaults, emit `settings-changed`.
- `#[tauri::command] pub fn get_keyboard_implementation(app) -> String` — `mod.rs:320`.
- `fn validate_shortcut_for_implementation(raw, impl) -> Result<(),String>` — `mod.rs:333`. Dispatches to per-impl validators.
- `fn parse_keyboard_implementation(s: &str) -> KeyboardImplementation` — `mod.rs:344`. `"tauri"|"handy_keys"`, defaults to Tauri on unknown.
- `fn unregister_all_shortcuts(app, impl)` — `mod.rs:359`. Skips `"cancel"`.
- `fn register_all_shortcuts_for_implementation(app, impl) -> Vec<String>` — `mod.rs:383`. Skips `"cancel"`, skips `transcribe_with_post_process` when `post_process_enabled==false`, validates each, resets invalid ones to default, returns list of reset IDs; persists if any reset.
- `fn initialize_handy_keys_with_rollback(app) -> Result<bool,String>` — `mod.rs:448`. Idempotent init; on failure reverts setting to Tauri and re-inits Tauri.
- ~30 settings commands (`mod.rs:476`–`mod.rs:1157`), the I/O-edge-relevant of which are:
  - `change_ptt_setting` (`:476`), `change_audio_feedback_setting` (`:485`), `change_audio_feedback_volume_setting` (`:494`), `change_sound_theme_setting` (`:503`, parses `marimba|pop|custom`).
  - `change_paste_delay_ms_setting` (`:674`), `change_paste_method_setting` (`:683`, parses six methods, defaults `ctrl_v`).
  - `get_available_typing_tools()` (`:704`, Linux→`clipboard::get_available_typing_tools`, else `["auto"]`), `change_typing_tool_setting` (`:717`), `change_external_script_path_setting` (`:738`).
  - `change_clipboard_handling_setting` (`:750`, `dont_modify|copy_to_clipboard`), `change_auto_submit_setting` (`:770`), `change_auto_submit_key_setting` (`:779`, `enter|ctrl_enter|cmd_enter`), `change_append_trailing_space_setting` (`:1056`).
  - `change_post_process_enabled_setting` (`:797`) — also live-registers/unregisters the `transcribe_with_post_process` hotkey.
  - Post-process provider/key/model/prompt commands (`:827`–`:1043`).

### 2.2 `shortcut/handler.rs` — shared dispatch

- `pub fn handle_shortcut_event(app: &AppHandle, binding_id: &str, hotkey_string: &str, is_pressed: bool)` — `handler.rs:29`. The fulcrum of the subsystem. Order:
  1. `is_transcribe_binding(binding_id)` → forward to `TranscriptionCoordinator::send_input(binding_id, hotkey, is_pressed, settings.push_to_talk)` (`handler.rs:38-45`).
  2. Else look up `ACTION_MAP[binding_id]`; warn + return if missing (`handler.rs:47-53`).
  3. `binding_id == "cancel"` → only call `action.start` if `AudioRecordingManager.is_recording() && is_pressed` (`handler.rs:56-62`).
  4. Else simple `start` on press / `stop` on release (`handler.rs:65-69`).

### 2.3 `shortcut/tauri_impl.rs` — Tauri backend

- `pub fn init_shortcuts(app)` — `tauri_impl.rs:17`. Iterates default bindings, skips `cancel` and (when disabled) `transcribe_with_post_process`, registers each (user override or default).
- `pub fn validate_shortcut(raw: &str) -> Result<(),String>` — `tauri_impl.rs:44`. Rejects empty, rejects `fn`/`function` (Tauri can't bind it), and **requires ≥1 non-modifier key** (modifier list at `:49-52`). This is why Tauri can't do modifier-only push-to-talk.
- `pub fn register_shortcut(app, binding) -> Result<(),String>` — `tauri_impl.rs:73`. Validates, parses `Shortcut`, rejects already-registered (duplicate-shadow guard `:97-101`), installs an `on_shortcut` closure that calls `handle_shortcut_event` with `is_pressed = event.state == ShortcutState::Pressed` (`:106-118`).
- `pub fn unregister_shortcut(app, binding) -> Result<(),String>` — `tauri_impl.rs:132`.
- `pub fn register_cancel_shortcut(app)` / `unregister_cancel_shortcut(app)` — `tauri_impl.rs:158`, `:180`. **Disabled on Linux** (`#[cfg(target_os="linux")]` early-return `:160-164`, `:182-186`) due to instability with dynamic registration; otherwise spawned on the async runtime to avoid deadlock.

### 2.4 `shortcut/handy_keys.rs` — HandyKeys backend

- `enum ManagerCommand { Register{binding_id, hotkey_string, response}, Unregister{binding_id, response}, Shutdown }` — `handy_keys.rs:46`. Messages to the manager thread; each carries a `Sender<Result<(),String>>` for synchronous reply.
- `pub struct HandyKeysState { command_sender: Mutex<Sender<ManagerCommand>>, thread_handle: Mutex<Option<JoinHandle<()>>>, recording_listener: Mutex<Option<KeyboardListener>>, is_recording: AtomicBool, recording_binding_id: Mutex<Option<String>>, recording_running: Arc<AtomicBool> }` — `handy_keys.rs:60`. Tauri-managed singleton.
- `#[derive(Serialize, Type)] pub struct FrontendKeyEvent { modifiers: Vec<String>, key: Option<String>, is_key_down: bool, hotkey_string: String }` — `handy_keys.rs:77`. Emitted as event `"handy-keys-event"` during capture.
- `HandyKeysState::new(app) -> Result<Self,String>` — `:90`. Creates mpsc channel, spawns `manager_thread`.
- `fn manager_thread(cmd_rx, app)` — `:110`. Creates `HotkeyManager::new_with_blocking()`, maintains `binding_to_hotkey`/`hotkey_to_binding` maps, then loops: drain `manager.try_recv()` hotkey events → `handle_shortcut_event` (`:128-137`); `cmd_rx.recv_timeout(10ms)` for register/unregister/shutdown (`:140-180`).
- `fn do_register(...)` / `fn do_unregister(...)` — `:187`, `:213`. Parse `Hotkey`, register/unregister, update both maps.
- `pub fn register(&self, &ShortcutBinding)` / `unregister(...)` — `:230`, `:247`. Send command + block on response channel (synchronous request/response).
- `pub fn start_recording(&self, app, binding_id) -> Result<(),String>` — `:263`. Creates a fresh `KeyboardListener`, sets flags, spawns `recording_loop`.
- `fn recording_loop(app, running)` — `:302`. Polls listener, converts each event to `FrontendKeyEvent` (via `modifiers_to_strings` + `as_hotkey().to_handy_string()`), emits `"handy-keys-event"`, sleeps 10ms when idle.
- `pub fn stop_recording(&self) -> Result<(),String>` — `:338`. Clears flags + listener.
- `impl Drop for HandyKeysState` — `:362`. Stops recording, sends `Shutdown`, joins the manager thread.
- `fn modifiers_to_strings(Modifiers) -> Vec<String>` — `:383`. Maps `CTRL/OPT/SHIFT/CMD/FN` → strings with macOS-specific naming (`option`/`command` vs `alt`/`super`).
- `pub fn validate_shortcut(raw) -> Result<(),String>` — `:413`. Only non-empty + parseable; **allows modifier-only and `fn`** (more permissive than Tauri).
- `pub fn init_shortcuts(app) -> Result<(),String>` — `:425`. Builds state, registers all non-cancel bindings, `app.manage(state)`.
- `register_cancel_shortcut` / `unregister_cancel_shortcut` — `:461`, `:485`. Linux-gated like Tauri's.
- `register_shortcut` / `unregister_shortcut` (module-level, `:506`, `:514`) — fetch `HandyKeysState` from managed state and delegate.
- `#[tauri::command] start_handy_keys_recording(app, binding_id)` / `stop_handy_keys_recording(app)` — `:524`, `:539`. Guard that HandyKeys is the active impl, then call the state methods. Used by the rebind UI.

### 2.5 `input.rs` — keystroke synthesis

- `pub struct EnigoState(pub Mutex<Enigo>)` — `input.rs:7`; `EnigoState::new() -> Result<Self,String>` — `:10`. Managed in Tauri state; **lazily created** by the `initialize_enigo` command after onboarding/permissions (`commands/mod.rs:134`), *not* at startup — deliberate, to avoid premature macOS Accessibility prompts (`lib.rs:141-144`).
- `pub fn get_cursor_position(app) -> Option<(i32,i32)>` — `input.rs:19`. Reads mouse location via Enigo (used by overlay positioning elsewhere).
- `pub fn send_paste_ctrl_v(enigo) -> Result<(),String>` — `:28`. Presses platform modifier + `V` **by raw virtual keycode** (`Key::Other(9)` mac, `0x56` Win, `Key::Unicode('v')` Linux) so it works under non-QWERTY layouts; 100ms hold; releases. cfg branches at `:30-35`.
- `pub fn send_paste_ctrl_shift_v(enigo)` — `:57`. Same but adds Shift (terminal "paste without formatting").
- `pub fn send_paste_shift_insert(enigo)` — `:92`. Shift+Insert (`VK_INSERT 0x2D` Win, `0x76` elsewhere). Note: macOS has no real Insert key, so this is effectively Win/Linux-oriented.
- `pub fn paste_text_direct(enigo, text) -> Result<(),String>` — `:117`. `enigo.text(text)` — uses OS text-injection where available, else per-character keystrokes.

### 2.6 `clipboard.rs` — the paste orchestrator

- `pub fn paste(text: String, app_handle: AppHandle) -> Result<(),String>` — `clipboard.rs:591`. **The single entry point** (re-exported as `utils::paste`, called from `actions.rs:610` on the main thread). Steps: read settings; optionally append trailing space (`:597`); lock `EnigoState` (`:609-615`); branch on `PasteMethod` (`:618-647`); if `should_send_auto_submit` sleep 50ms + `send_return_key` (`:649-652`); if `ClipboardHandling::CopyToClipboard` write the text to clipboard (`:655-660`).
- `fn paste_via_clipboard(enigo, text, app_handle, paste_method, paste_delay_ms) -> Result<(),String>` — `:16`. Saves current clipboard, writes new text (Wayland prefers `wl-copy`, `:29-36`), sleeps `paste_delay_ms`, sends the key combo (Linux-native first via `try_send_key_combo_linux`, else enigo fallback `:55-62`), sleeps 50ms, **restores the original clipboard** (`:66-77`). This save/restore is the clipboard-preserving paste.
- `fn try_send_key_combo_linux(paste_method) -> Result<bool,String>` — `:84` (cfg linux). Wayland: `wtype` (not KDE) → `dotool` → `ydotool`; X11: `xdotool` → `ydotool`. Returns `true` if handled.
- `fn try_direct_typing_linux(text, preferred_tool) -> Result<bool,String>` — `:123` (cfg linux). Honors explicit `TypingTool`, else auto chain: KDE-Wayland `kwtype` → Wayland `wtype`→`dotool`→`ydotool`; X11 `xdotool`→`ydotool`.
- `pub fn get_available_typing_tools() -> Vec<String>` — `:204` (cfg linux). Probes `which` for each tool; always prepends `"auto"`.
- Tool detection helpers `is_wtype_available`/`is_dotool_available`/`is_ydotool_available`/`is_xdotool_available`/`is_kwtype_available`/`is_wl_copy_available` — `:226`–`:281` (each shells out to `which`).
- Per-tool executors: `type_text_via_wtype` (`:285`), `type_text_via_xdotool` (`:302`, `--clearmodifiers`), `type_text_via_dotool` (`:321`, stdin `type <text>`), `type_text_via_ydotool` (`:350`), `type_text_via_kwtype` (`:368`); `write_clipboard_via_wl_copy` (`:387`, uses `Stdio::null()` to avoid a documented hang when `wl-copy` forks a daemon — `:383-386`); key-combo executors `send_key_combo_via_wtype` (`:406`), `_dotool` (`:429`), `_ydotool` (`:454`, raw input keycodes `ctrl=29 shift=42 v=47 insert=110`), `_xdotool` (`:479`).
- `fn paste_via_external_script(text, script_path) -> Result<(),String>` — `:504`. Runs the user script with the text as `argv[1]`; surfaces non-zero exit + stderr/stdout.
- `fn paste_direct(enigo, text, [typing_tool]) -> Result<(),String>` — `:528`. Linux tries native typing first, else `input::paste_text_direct`.
- `fn send_return_key(enigo, key_type: AutoSubmitKey) -> Result<(),String>` — `:544`. `Enter` / `Ctrl+Enter` / `Cmd(Meta)+Enter`.
- `fn should_send_auto_submit(auto_submit: bool, paste_method: PasteMethod) -> bool` — `:587`. `auto_submit && method != None`. Unit-tested (`:665-687`).

### 2.7 `audio_feedback.rs` — feedback sounds

- `pub enum SoundType { Start, Stop }` — `audio_feedback.rs:12`.
- `fn resolve_sound_path(app, settings, sound_type) -> Option<PathBuf>` — `:17`. Custom theme → `AppData` (via `portable::resolve_app_data`), else bundled `Resource`.
- `fn get_sound_path(settings, sound_type) -> String` — `:32`. `custom_start.wav`/`custom_stop.wav` for Custom; else `SoundTheme::to_start_path()/to_stop_path()` → `resources/{theme}_{start|stop}.wav` (`settings.rs:251-257`).
- `pub fn play_feedback_sound(app, sound_type)` — `:48`. Async; **no-op if `audio_feedback` disabled** (`:50-52`). Called at start/stop.
- `pub fn play_feedback_sound_blocking(app, sound_type)` — `:58`. Blocking variant (used so mic-mute can be applied right after the start sound finishes — `actions.rs:424-427`, `:449-451`).
- `pub fn play_test_sound(app, sound_type)` — `:68`. Ignores the enable flag (preview in settings); wired as command `commands::audio::play_test_sound` (`lib.rs:413`).
- `fn play_sound_async` (`:75`, spawns thread), `play_sound_blocking` (`:84`), `play_sound_at_path` (`:90`, reads volume + `selected_output_device`), `play_audio_file(path, selected_device, volume)` (`:97`) — builds a `rodio` `OutputStream` on the chosen cpal device (falls back to default if name not found, `:118-124`), plays the WAV, `sink.set_volume(volume)`, `sink.sleep_until_end()`.

### 2.8 `helpers/clamshell.rs` — macOS lid detection

- `#[cfg(target_os="macos")] pub fn is_clamshell() -> Result<bool,String>` — `helpers/clamshell.rs:9`. Runs `ioreg -r -k AppleClamshellState -d 4`, returns `true` if output contains `"AppleClamshellState" = Yes`.
- `#[cfg(target_os="macos")] #[tauri::command] pub fn is_laptop() -> Result<bool,String>` — `:35`. Runs `pmset -g batt`, returns `true` if `InternalBattery` is present.
- Non-macOS stubs `is_clamshell` (`:51`) / `is_laptop` (`:60`) return `Ok(false)`.
- Consumer: `managers/audio.rs:192-200` substitutes `settings.clamshell_microphone` for the input device when `is_clamshell() == true` and a clamshell mic is configured. `is_laptop` command is registered at `lib.rs:428`.

---

## 3. Threading / concurrency model

- **Tauri backend** — Hotkey callbacks (`on_shortcut`, `tauri_impl.rs:107`) fire on the plugin's thread and call `handle_shortcut_event` synchronously. Cancel (un)register is pushed onto `tauri::async_runtime::spawn` to *avoid deadlock* re-entering the global-shortcut plugin from within a shortcut callback (`tauri_impl.rs:169`, `:191`).
- **HandyKeys backend** — A **dedicated OS thread** owns the non-`Send` `HotkeyManager` (`handy_keys.rs:95`, `:110`). All register/unregister go through an **mpsc command channel** with a per-call reply `Sender` (synchronous request/response, `:230-244`). The thread polls hotkey events (`try_recv`) and commands (`recv_timeout(10ms)`) in one loop. A **second thread per capture session** (`recording_loop`, `:302`) polls a `KeyboardListener` and emits frontend events; lifetime controlled by `recording_running: Arc<AtomicBool>` + `is_recording: AtomicBool`. `Drop` joins the manager thread (`:362-379`).
- **TranscriptionCoordinator** (`transcription_coordinator.rs:36`) — A **single serializing thread** behind one `Sender<Command>`. It debounces rapid presses (30ms, `:10`/`:63-70`), and is the *only* place that decides start/stop given push-to-talk vs toggle and the `Idle/Recording/Processing` stage machine. This is what prevents races between keyboard events, Unix signals, and the async transcribe pipeline. Wrapped in `catch_unwind` (`:49`).
- **Feedback sounds** — `play_feedback_sound` spawns a thread per sound (`audio_feedback.rs:77`); `rodio`'s `sink.sleep_until_end()` blocks that worker only. The blocking variant is run on caller-spawned threads in `actions.rs` so mute timing can be sequenced after playback.
- **Paste** — `utils::paste` is invoked via `app.run_on_main_thread(...)` (`actions.rs:609`) because synthesizing keystrokes/clipboard ops must occur on the platform UI thread. `EnigoState` is a `Mutex<Enigo>`; the lock is held for the whole paste (`clipboard.rs:612-615`), serializing concurrent pastes.
- **Locks** — `EnigoState.0: Mutex<Enigo>`; `HandyKeysState` uses several `Mutex` (channel sender, thread handle, listener, binding id) plus two `AtomicBool`. No async locks; the subsystem is std-threads + mpsc, not Tokio tasks (except the cancel-register `spawn` and the transcribe async pipeline in `actions.rs`).

---

## 4. Data flow IN / OUT

**IN (what triggers this subsystem):**
- OS keyboard events → Tauri plugin closure (`tauri_impl.rs:107`) or HandyKeys manager thread (`handy_keys.rs:128`) → `handle_shortcut_event`.
- Unix `SIGUSR1`/`SIGUSR2` → `signal_handle.rs:25-37` → `send_transcription_input` → `TranscriptionCoordinator::send_input` (bypasses the keyboard layer entirely; `binding_id` = `transcribe_with_post_process`/`transcribe`).
- CLI flags (`--toggle-transcription`, etc.) → second instance → single-instance plugin → `send_transcription_input` (per AGENTS.md; shared `signal_handle.rs`).
- Frontend commands (Tauri IPC): all of `shortcut/mod.rs`'s `change_*`/`*_binding`/`change_keyboard_implementation_setting`, `start_handy_keys_recording`/`stop_handy_keys_recording`, `initialize_enigo`, `initialize_shortcuts`, `play_test_sound`, `is_laptop`, clamshell-mic commands.

**OUT (what this subsystem calls / emits):**
- `TranscriptionCoordinator::send_input/notify_cancel/notify_processing_finished` (`handler.rs:40`, `utils.rs:38`).
- `ACTION_MAP` actions: `TranscribeAction`, `CancelAction`, `TestAction` (`actions.rs:700`). Transcribe drives `AudioRecordingManager`, `TranscriptionManager`, `HistoryManager`, overlay, tray, and finally `utils::paste`.
- System clipboard via `tauri_plugin_clipboard_manager::ClipboardExt` (`clipboard.rs:23`, `:656`).
- OS input via `enigo` and Linux child processes (`Command::new(...)`).
- Audio out via `cpal` host + `rodio` (`audio_feedback.rs`).
- Tauri **events emitted**: `"handy-keys-event"` (capture stream, `handy_keys.rs:326`), `"settings-changed"` (`mod.rs:300`, `:567`, etc.), and from `actions.rs` `"recording-error"`/`"paste-error"`. Settings persistence via `settings::write_settings` (tauri-plugin-store).

**Message/event types:** `ManagerCommand`, `FrontendKeyEvent`, `BindingResponse`, `ImplementationChangeResult`, `Command`/`Stage` (coordinator), `SoundType`, `PasteMethod`/`ClipboardHandling`/`AutoSubmitKey`/`SoundTheme`/`TypingTool`/`KeyboardImplementation` enums.

---

## 5. Error handling & edge cases

- **Empty / invalid bindings**: `change_binding` rejects empty (`mod.rs:114`); per-impl validators reject empty, and Tauri additionally rejects `fn` and modifier-only (`tauri_impl.rs:44`). Switching impls auto-resets bindings invalid for the target and reports them in `reset_bindings` (`mod.rs:383-444`).
- **Duplicate hotkey**: Tauri returns `"Shortcut '…' is already in use"` rather than silently shadowing (`tauri_impl.rs:97-101`).
- **HandyKeys init failure**: falls back to Tauri *and persists* the fallback so it isn't retried (`mod.rs:43-54`); runtime switch has explicit rollback (`mod.rs:448-468`).
- **Cancel hotkey on Linux**: dynamic (un)registration disabled for stability on both backends (`tauri_impl.rs:160`, `handy_keys.rs:463`).
- **Enigo not initialized** (no Accessibility grant yet): `paste` returns `"Enigo state not initialized"` (`clipboard.rs:610`); `initialize_enigo` surfaces the permission error to the frontend (`commands/mod.rs:154-162`).
- **Paste failures**: each step returns `Result`; `actions.rs:615-618` emits `"paste-error"` and still hides the overlay / resets tray. Empty final text skips paste (`actions.rs:602`).
- **Wayland clipboard hang**: `wl-copy` is invoked with `Stdio::null()` to dodge a fork-daemon fd-inheritance hang (`clipboard.rs:383-401`).
- **Linux tool absence**: native tool chain falls back to enigo (`clipboard.rs:55-62`, `:538`); explicit `TypingTool` that's unavailable errors clearly (`:152-155`).
- **External script**: non-zero exit captured with code + stderr/stdout (`clipboard.rs:512-522`).
- **Audio device gone**: named output device not found → warn + default (`audio_feedback.rs:120-124`); all sound errors are logged, never fatal (`:78-88`).
- **Coordinator robustness**: debounce drops key-repeat (`transcription_coordinator.rs:63-70`); `Processing` stage ignores new presses ("pipeline busy", `:88-90`); cancel during processing is ignored to let the pipeline finish (`:98-102`); thread is panic-guarded (`:49`). `FinishGuard` (`actions.rs:32-39`) guarantees `notify_processing_finished` even on panic.
- **Clamshell**: `ioreg`/`pmset` failures return `Err` and the audio manager treats a non-`Ok` result as "not clamshell" (`managers/audio.rs:193`).

---

## 6. State & persistence touched

- **Settings store** (`tauri-plugin-store`, via `settings::{get_settings,write_settings}`): `bindings: HashMap<String,ShortcutBinding>`, `push_to_talk`, `audio_feedback`, `audio_feedback_volume`, `sound_theme`, `keyboard_implementation`, `paste_method`, `paste_delay_ms`, `clipboard_handling`, `auto_submit`, `auto_submit_key`, `append_trailing_space`, `typing_tool`, `external_script_path`, `post_process_*`, `selected_output_device`, `clamshell_microphone`. Struct & enums at `settings.rs:80-422`; defaults at `settings.rs:720-811` (default transcribe hotkey is platform-specific `alt+space`/`ctrl+space`, cancel = `escape`, `push_to_talk=true`, `audio_feedback=false`).
- **Files on disk**: bundled feedback WAVs under `resources/{marimba|pop}_{start|stop}.wav`; custom theme WAVs `custom_start.wav`/`custom_stop.wav` in AppData (`audio_feedback.rs:32-46`). External paste script path (user-provided executable).
- **System clipboard**: read/written/restored by `paste_via_clipboard`; optionally left populated when `CopyToClipboard`.
- **No SQLite or model files** are touched by this subsystem (history WAV/DB writes happen in `actions.rs`/`HistoryManager`, outside the I/O edge).
- **In-memory managed state**: `EnigoState`, `HandyKeysState`, `ShortcutsInitialized` marker, `TranscriptionCoordinator`.

---

## 7. Platform-specific branches (cfg gates)

- **macOS**: paste keycodes use `Key::Meta` + `Key::Other(9)` (`input.rs:30-31`); `CmdEnter` auto-submit uses `Key::Meta` (`clipboard.rs:568`); HandyKeys is the **default** impl (`settings.rs:177-178`); modifier names `option`/`command` (`handy_keys.rs:390-403`); Accessibility permission gating defers Enigo/shortcut init (`lib.rs:141-144`, `commands/mod.rs`); clamshell `ioreg`/`pmset` (`helpers/clamshell.rs`). Default paste = `CtrlV` (Cmd+V) (`settings.rs:193-194`).
- **Windows**: paste keycodes `Key::Control`+`0x56`, `VK_INSERT 0x2D` (`input.rs:33`, `:93`); default impl HandyKeys; default paste `CtrlV`.
- **Linux**: paste via `Key::Unicode('v')`; default impl **Tauri** and default paste **Direct** (`settings.rs:176`, `:191-192`); the entire `clipboard.rs` native-tool layer (`wtype`/`kwtype`/`dotool`/`ydotool`/`xdotool`/`wl-copy`) is `#[cfg(target_os="linux")]`; Wayland/X11/KDE detection via `utils::is_wayland/is_kde_wayland/is_kde_plasma`; cancel hotkey disabled (both backends); `get_available_typing_tools` only enumerates on Linux.
- **iOS**: **none.** No `target_os = "ios"` branches anywhere in this subsystem. `enigo`, `cpal`/`rodio`, global-shortcut, `handy-keys`, and all the CLI tools are desktop-only. This is the single biggest porting gap.
- **Fallback `#[cfg(not(any(...)))]`**: default hotkeys `alt+space`/`alt+shift+space` (`settings.rs:723`, `:743`).

---

## 8. PLAUD relevance — concrete extension points

A Plaud-style product = *always-on / call & meeting capture → diarized multi-speaker transcript → AI summary → sync → mobile.* Mapping to this subsystem:

1. **Replace hotkey-gated capture with continuous/background capture.** Today every recording is bracketed by a hotkey press/release routed through `handle_shortcut_event` (`handler.rs:29`) → `TranscriptionCoordinator` (`transcription_coordinator.rs`). For Plaud "press once to start a long session," add a new `Stage`-aware mode or a new `ShortcutAction` (`actions.rs:42`) e.g. `StartSessionAction`/`StopSessionAction` that does *not* auto-paste and instead streams to a session recorder. The coordinator's `Idle/Recording/Processing` machine (`:27-31`) is the right place to add a `LongRecording` stage that survives across utterances.
2. **System / call audio capture (the core Plaud feature).** This subsystem only *triggers* capture; the actual device is chosen in `managers/audio.rs` (with the clamshell mic substitution at `:193`). Extend the **clamshell mic-selection pattern** into a general "capture-source" selector (microphone vs system-loopback vs call audio). On macOS that means a ScreenCaptureKit/CoreAudio aggregate device; wire it in next to `clamshell::is_clamshell` so the same hotkey can capture *both* mic and system output for two-sided calls.
3. **Speaker diarization & multi-speaker conversations.** The paste path assumes a single text blob (`clipboard.rs:591`, `actions.rs:586-628`). For diarized conversations, the output of `process_transcription_output` (`actions.rs:349`) must become a structured `Vec<{speaker, text, ts}>`. Diarization itself is a transcription-layer concern, but **the paste/clipboard formatter here** is where you'd render `Speaker A: …\nSpeaker B: …`. Add a `PasteMethod`/output-format option (extend the enum at `settings.rs:132`) for "transcript with speaker labels" vs "plain."
4. **AI summaries.** The hooks already exist: `post_process_transcription` (`actions.rs:66`) + `change_post_process_*` commands (`mod.rs:827-1043`) + `transcribe_with_post_process` binding. A Plaud "summarize this meeting" is a *new prompt template* (`add_post_process_prompt`, `mod.rs:911`) over the *whole session* transcript rather than per-utterance. The structured-output path (`actions.rs:144-258`) is the place to request `{summary, action_items, decisions}` JSON instead of `{transcription}`.
5. **Don't paste — store.** For a recorder product you usually *don't* want to inject into the focused app. `PasteMethod::None` (`clipboard.rs:619`) already supports "no paste"; you'd default sessions to it and instead persist to a conversation store. `ClipboardHandling::CopyToClipboard` (`clipboard.rs:655`) is a lightweight "share" affordance to keep.
6. **Audio feedback for long sessions.** `audio_feedback.rs` Start/Stop sounds map cleanly to session begin/end chimes; add `SoundType::Pause`/`Resume` and a periodic "still recording" cue. `play_audio_file` already routes to a chosen output device (`:97`) — important so feedback doesn't bleed into captured system audio (route feedback to a *different* device than the capture loopback).
7. **Mobile (iPhone).** Nothing here is reusable on iOS as-is. The clean seam is `TranscriptionCoordinator::send_input` (`transcription_coordinator.rs:121`) and `signal_handle::send_transcription_input` (`signal_handle.rs:16`): a mobile UI would call an equivalent "start/stop session" entry instead of a hotkey. The paste/enigo/clipboard/CLI-tool layers must be *excluded* (cfg) on mobile and replaced with in-app transcript display + share-sheet. Sound feedback (`rodio`/`cpal`) needs an iOS audio-session-aware backend.
8. **Cloud / local sync.** Out of scope for this subsystem, but the persistence touchpoints to *not* break are: history WAVs (written in `actions.rs:538-543`) and settings store. A sync layer would subscribe to the same `"settings-changed"` events this module emits (`mod.rs:300`) and to a new `"session-finished"` event you'd emit from the coordinator's `ProcessingFinished` path.

**Most surgical first changes:** (a) new `ShortcutAction`s + a `LongRecording` `Stage` for sessions; (b) generalize clamshell mic-selection into capture-source selection feeding `managers/audio.rs`; (c) summary prompt over full-session transcript via existing post-process plumbing; (d) default sessions to `PasteMethod::None` and route output to a conversation store.

---

## 9. GAPS vs a Plaud-style product

- **No background/continuous capture.** Recording is strictly hotkey-bracketed (press→start, release/second-press→stop) through the coordinator; there is no notion of a long-running, resumable session, pause/resume, or auto-segmentation. Sessions longer than a single utterance aren't modeled.
- **No system/call/loopback audio.** Capture is microphone-only (plus the macOS clamshell mic swap). No ScreenCaptureKit/loopback/aggregate device, so two-sided calls and app audio can't be recorded.
- **No diarization / speaker labels.** The whole output path is a single `String`; there is no speaker model, no per-segment timestamps, no `Speaker A/B` rendering.
- **Output is "inject into focused app," not "store a conversation."** The product assumption is *paste into the cursor*. There's no conversation/notes data model, no transcript editor, no timeline. (History exists but is per-clip WAV+text, not a structured meeting object.)
- **Summaries are single-shot per utterance, not meeting-level.** Post-processing runs once on the just-recorded blob; no rolling summary, no action-item extraction over an accumulating transcript, no chapters.
- **No mobile path at all.** Zero iOS cfg; the entire I/O edge (enigo, global-shortcut, handy-keys, cpal/rodio, Linux CLI tools, clamshell shellouts) is desktop-only. iPhone is a from-scratch port for this layer.
- **No sync.** No cloud or device-to-device sync of transcripts/sessions/settings; only local settings store + local WAV/history.
- **Feedback is two static chimes.** No live waveform/level metering surfaced from this layer, no "still recording" heartbeat, no pause/resume cues — all of which long-form recorders provide.
- **Clipboard side effects are user-app-centric.** The save/restore-clipboard dance and auto-`Enter` make sense for dictation but are irrelevant (and potentially surprising) for a recorder; there's no "export/share" pipeline (email, doc, share sheet) — only `CopyToClipboard`.
- **Permissions model is desktop Accessibility only.** Enigo/shortcuts gate on macOS Accessibility (`lib.rs:141-144`); there's no microphone-while-locked, background-audio entitlement, or call-recording consent handling that a mobile/always-on recorder needs.
