# Handy Architecture — Subsystem 04: Audio Recording Manager (Orchestration of Capture / VAD / State)

> **Abstract.** The Audio Recording Manager is the stateful orchestration layer that sits between Handy's global-shortcut/command surface and the low-level `audio_toolkit` capture engine. Its single public type, `AudioRecordingManager` (`src-tauri/src/managers/audio.rs:146`), owns a lazily-constructed `AudioRecorder`, a `RecordingState` finite-state machine, a `MicrophoneMode` (always-on vs on-demand), and a bundle of `Arc<Mutex<…>>` flags that coordinate stream open/close, recording start/stop, OS output muting, and idle stream teardown. It does **not** itself touch CoreAudio/WASAPI/ALSA — it delegates capture to `AudioRecorder` (a worker-thread + channel design in `audio_toolkit/audio/recorder.rs`), wiring in a Silero-based VAD (`SmoothedVad` over `SileroVad`) and a spectrum-level callback that streams `mic-level` events to the UI. Its output is a single `Vec<f32>` of 16 kHz mono, VAD-gated samples handed to the transcription pipeline. This document forensically catalogs every type, function, thread, lock, data path, error case, persisted setting, and platform gate in this subsystem, and then maps concrete extension points and gaps for a Plaud-style always-on, multi-speaker, long-form, AI-summarizing, cloud-syncing, mobile recorder.

---

## 1. Per-File Responsibilities

| File | Responsibility (1–3 lines) |
|------|----------------------------|
| `src-tauri/src/managers/mod.rs` | Module index for the manager layer. Declares `audio`, `history`, `model`, `transcription` submodules. No logic. (`managers/mod.rs:1-4`) |
| `src-tauri/src/managers/audio.rs` | **The subsystem.** Defines `AudioRecordingManager`, `RecordingState`, `MicrophoneMode`; orchestrates mic stream lifecycle, recording start/stop/cancel, mode switching, OS mute, lazy-close timer, clamshell device selection. Delegates capture to `AudioRecorder`. (`managers/audio.rs:1-517`) |
| `src-tauri/src/audio_toolkit/audio/recorder.rs` | Low-level capture engine. `AudioRecorder` spawns a worker thread that owns the cpal input stream and a consumer loop performing per-frame VAD gating, resampling to 16 kHz, and spectrum levels. Channel/command protocol. (`audio_toolkit/audio/recorder.rs:1-529`) |
| `src-tauri/src/audio_toolkit/audio/device.rs` | cpal device enumeration (`list_input_devices`, `list_output_devices`) returning `CpalDeviceInfo { index, name, is_default, device }`. (`audio_toolkit/audio/device.rs:1-53`) |
| `src-tauri/src/audio_toolkit/audio/resampler.rs` | `FrameResampler` — FFT-based (rubato `FftFixedIn`) chunked resampler from device-native rate to 16 kHz, emitting fixed 30 ms frames. (`audio_toolkit/audio/resampler.rs:1-99`) |
| `src-tauri/src/audio_toolkit/audio/visualizer.rs` | `AudioVisualiser` — Hann-windowed FFT into N vocal-frequency buckets for the UI level meter. (`audio_toolkit/audio/visualizer.rs:1-40+`) |
| `src-tauri/src/audio_toolkit/audio/utils.rs` | WAV I/O helpers: `read_wav_samples`, `verify_wav_file`, `save_wav_file` (16 kHz/mono/16-bit). (`audio_toolkit/audio/utils.rs:1-50`) |
| `src-tauri/src/audio_toolkit/vad/mod.rs` | `VoiceActivityDetector` trait + `VadFrame<'a>` enum (`Speech(&[f32])` / `Noise`). (`audio_toolkit/vad/mod.rs:1-33`) |
| `src-tauri/src/audio_toolkit/vad/silero.rs` | `SileroVad` — wraps `vad_rs::Vad` (Silero ONNX) with a probability threshold; requires exactly 30 ms (480-sample) frames at 16 kHz. (`audio_toolkit/vad/silero.rs:1-53`) |
| `src-tauri/src/audio_toolkit/vad/smoothed.rs` | `SmoothedVad` — hysteresis wrapper (prefill/pre-roll, onset debounce, hangover tail) over any inner VAD. (`audio_toolkit/vad/smoothed.rs:1-105`) |
| `src-tauri/src/audio_toolkit/utils.rs` | `get_cpal_host()` — picks ALSA on Linux, default host elsewhere. (`audio_toolkit/utils.rs:1-12`) |
| `src-tauri/src/audio_toolkit/constants.rs` | `WHISPER_SAMPLE_RATE = 16000`. (`audio_toolkit/constants.rs:1`) |
| `src-tauri/src/helpers/clamshell.rs` | macOS clamshell (lid-closed) detection via `ioreg`; stubs to `false` off-macOS. Drives clamshell-mic override. (`helpers/clamshell.rs:1-86`) |

**Callers / collaborators (outside the subsystem, in data flow):**

| File | Role relative to this subsystem |
|------|--------------------------------|
| `src-tauri/src/lib.rs:147` | Constructs the manager once at startup, wraps in `Arc`, registers as Tauri managed state. |
| `src-tauri/src/actions.rs:389-560` | `TranscribeAction` — the primary driver: `start()` calls `preload_vad` + `try_start_recording` + `apply_mute`; `stop()` calls `remove_mute` + `stop_recording` and forwards samples to transcription/WAV/history. |
| `src-tauri/src/commands/audio.rs` | Tauri commands: `update_microphone_mode` → `update_mode`; `set_selected_microphone` → `update_selected_device`; `is_recording` → `is_recording`; device/output enumeration. |
| `src-tauri/src/utils.rs:17-42` | `cancel_current_operation` → `cancel_recording`. |
| `src-tauri/src/shortcut/handler.rs:57-60` | Reads `is_recording()` to gate the dynamic "cancel" binding. |
| `src-tauri/src/managers/transcription.rs:119-124` | Idle-model watcher reads `is_recording()` to avoid unloading the ASR model mid-session. |
| `src-tauri/src/overlay.rs:388-395` | `emit_levels` — destination of the level callback; emits `mic-level` to main + overlay windows. |

---

## 2. Types, Traits, and Public Functions (with signatures + citations)

### 2.1 Enums

**`enum RecordingState`** — `managers/audio.rs:106-110`
```rust
pub enum RecordingState { Idle, Recording { binding_id: String } }
```
The orchestration FSM. `Recording` carries the originating shortcut binding id so that `stop_recording(binding_id)` only stops the recording it started (prevents a different binding from stopping someone else's session). `Clone + Debug`.

**`enum MicrophoneMode`** — `managers/audio.rs:112-116`
```rust
pub enum MicrophoneMode { AlwaysOn, OnDemand }
```
`AlwaysOn`: stream is opened at construction and kept hot. `OnDemand`: stream opened on first `try_start_recording`, closed (eagerly or lazily) on stop/cancel.

### 2.2 The Manager Struct

**`struct AudioRecordingManager`** — `managers/audio.rs:145-156`
```rust
pub struct AudioRecordingManager {
    state: Arc<Mutex<RecordingState>>,           // FSM
    mode: Arc<Mutex<MicrophoneMode>>,            // always-on vs on-demand
    app_handle: tauri::AppHandle,                // for settings, events, resource paths
    recorder: Arc<Mutex<Option<AudioRecorder>>>, // lazily created capture engine
    is_open: Arc<Mutex<bool>>,                   // is cpal stream open?
    is_recording: Arc<Mutex<bool>>,              // low-level recording flag (mirrors recorder)
    did_mute: Arc<Mutex<bool>>,                  // did WE mute system output?
    close_generation: Arc<AtomicU64>,            // monotonic token for lazy-close cancellation
}
```
`#[derive(Clone)]` — cloning shares all `Arc`s (cheap handle). Registered as `Arc<AudioRecordingManager>` in Tauri state (`lib.rs:147,163`), so callers retrieve via `app.state::<Arc<AudioRecordingManager>>()`.

### 2.3 Public / pub(crate) Methods

| Signature | File:line | What it does |
|-----------|-----------|--------------|
| `pub fn new(app: &tauri::AppHandle) -> Result<Self, anyhow::Error>` | `audio.rs:161-187` | Reads `always_on_microphone` setting → initial `MicrophoneMode`. Initializes all `Arc<Mutex>` flags to idle/false, `close_generation=0`. If always-on, immediately calls `start_microphone_stream()`. |
| `pub fn apply_mute(&self)` | `audio.rs:245-254` | If `mute_while_recording` setting is on **and** stream is open, calls `set_mute(true)` and sets `did_mute=true`. Called by `TranscribeAction` after the start feedback sound. |
| `pub fn remove_mute(&self)` | `audio.rs:257-264` | If `did_mute`, calls `set_mute(false)` and clears the flag. Called in `TranscribeAction::stop` before the stop sound. |
| `pub fn preload_vad(&self) -> Result<(), anyhow::Error>` | `audio.rs:266-283` | If `recorder` is `None`, resolves bundled `resources/models/silero_vad_v4.onnx` via Tauri `BaseDirectory::Resource` and builds the `AudioRecorder` (VAD + level callback). Idempotent. |
| `pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error>` | `audio.rs:285-334` | Opens the cpal stream. Early-returns if already open. Resolves effective device (clamshell-aware), pre-flight checks for "no input device", calls `preload_vad()`, then `recorder.open(device)`. Sets `is_open=true`. |
| `pub fn stop_microphone_stream(&self)` | `audio.rs:336-359` | Closes the stream. Unmutes if needed, stops recorder if mid-recording, calls `recorder.close()` (joins worker thread), sets `is_open=false`. **Does not take the `state` lock** (deliberate, see §3). |
| `pub fn update_mode(&self, new_mode: MicrophoneMode) -> Result<(), anyhow::Error>` | `audio.rs:363-382` | Transitions between modes. AlwaysOn→OnDemand closes stream only if `Idle`. OnDemand→AlwaysOn opens stream. Bumps `close_generation` to cancel pending lazy-closes. Persists new mode in `self.mode`. |
| `pub fn try_start_recording(&self, binding_id: &str) -> Result<(), String>` | `audio.rs:386-415` | Idempotent start. Only from `Idle`. In on-demand mode, cancels pending lazy-close and opens the stream. Calls `recorder.start()`, sets `is_recording=true`, transitions FSM to `Recording { binding_id }`. Returns `Err("Already recording")` etc. |
| `pub fn update_selected_device(&self) -> Result<(), anyhow::Error>` | `audio.rs:417-425` | If stream currently open, bumps `close_generation`, stop+restart the stream to bind the newly selected device. |
| `pub fn stop_recording(&self, binding_id: &str) -> Option<Vec<f32>>` | `audio.rs:427-484` | Stops only if `Recording { binding_id }` matches. Optional `extra_recording_buffer_ms` sleep (trailing audio), calls `recorder.stop()` to drain samples, sets `is_recording=false`, schedules lazy-close or hard-close in on-demand mode, **pads** very short clips (<1 s) to 1.25 s. Returns the sample vector. |
| `pub fn is_recording(&self) -> bool` | `audio.rs:485-490` | Returns whether FSM is in `Recording`. Read by shortcut handler, transcription idle watcher, `is_recording` command. |
| `pub fn cancel_recording(&self)` | `audio.rs:493-515` | Discards the in-flight recording: stops recorder ignoring samples, transitions to `Idle`, closes/lazy-closes in on-demand mode. Called by `cancel_current_operation`. |

### 2.4 Private functions in the subsystem

| Signature | File:line | What it does |
|-----------|-----------|--------------|
| `fn set_mute(mute: bool)` | `audio.rs:13-100` | OS-level output mute toggle. Platform-branched (see §7). Fails silently. |
| `fn create_audio_recorder(vad_path: &str, app_handle: &tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error>` | `audio.rs:120-141` | Builds `SileroVad(threshold 0.3)` → `SmoothedVad(prefill 15, hangover 15, onset 2)` → `AudioRecorder::new().with_vad(...).with_level_callback(emit_levels)`. |
| `fn get_effective_microphone_device(&self, settings: &AppSettings) -> Option<cpal::Device>` | `audio.rs:191-216` | Resolves which cpal device to use: clamshell mic if `is_clamshell()` and configured, else `selected_microphone`. `None` ⇒ cpal default. Looks up device by name via `list_input_devices`. |
| `fn schedule_lazy_close(&self)` | `audio.rs:218-240` | Spawns a detached thread, captures a `close_generation` token, sleeps `STREAM_IDLE_TIMEOUT` (30 s), then — holding the `state` lock — closes the stream iff the token is still current and FSM is `Idle`. |

### 2.5 Capture-engine types (delegated; `audio_toolkit/audio/recorder.rs`)

| Item | File:line | Notes |
|------|-----------|-------|
| `enum Cmd { Start, Stop(mpsc::Sender<Vec<f32>>), Shutdown }` | `recorder.rs:22-26` | Control messages to the worker. `Stop` carries a reply channel for the captured samples. |
| `enum AudioChunk { Samples(Vec<f32>), EndOfStream }` | `recorder.rs:28-31` | Data messages from the cpal callback to the consumer loop. `EndOfStream` is a drain sentinel. |
| `struct AudioRecorder { device, cmd_tx, worker_handle, vad, level_cb }` | `recorder.rs:33-39` | Handle holding the worker `JoinHandle`, command `Sender`, the shared VAD (`Arc<Mutex<Box<dyn VAD>>>`), and the level callback. |
| `fn new() -> Result<Self, …>` | `recorder.rs:42-50` | Empty handle, no thread yet. |
| `fn with_vad(self, vad: Box<dyn VoiceActivityDetector>) -> Self` | `recorder.rs:52-55` | Builder: install VAD. |
| `fn with_level_callback<F>(self, cb: F) -> Self` | `recorder.rs:57-63` | Builder: install spectrum-level callback. |
| `fn open(&mut self, device: Option<Device>) -> Result<(), …>` | `recorder.rs:65-196` | Spawns the worker thread: builds the cpal stream for the device's native format, `stream.play()`, then runs `run_consumer`. Uses a sync init handshake channel so `open()` returns only after the stream is confirmed running (or errors). Classifies access-denied vs other errors. |
| `fn start(&self) -> Result<(), …>` | `recorder.rs:198-203` | Sends `Cmd::Start`. |
| `fn stop(&self) -> Result<Vec<f32>, …>` | `recorder.rs:205-211` | Sends `Cmd::Stop(reply_tx)`, blocks on `reply_rx.recv()` for the drained samples. |
| `fn close(&mut self) -> Result<(), …>` | `recorder.rs:213-222` | Sends `Cmd::Shutdown`, joins the worker thread. |
| `fn build_stream::<T>(…) -> Result<cpal::Stream, …>` | `recorder.rs:224-280` | Generic over sample type (U8/I8/I16/I32/F32). Downmixes interleaved multi-channel to mono by averaging, forwards `AudioChunk::Samples`. Honors `stop_flag` to emit `EndOfStream`. |
| `fn get_preferred_config(device) -> Result<SupportedStreamConfig, …>` | `recorder.rs:282-335` | Picks the device's **native** sample rate (avoids forcing hardware rates), prefers F32>I16>I32 formats. |
| `fn is_microphone_access_denied(msg: &str) -> bool` | `recorder.rs:338-343` | Error classifier (matches "access is denied", "permission denied", Windows `0x80070005`). |
| `fn is_no_input_device_error(msg: &str) -> bool` | `recorder.rs:345-350` | Error classifier (no device / CoreAudio config failure). |
| `fn run_consumer(in_sample_rate, vad, sample_rx, cmd_rx, level_cb, stop_flag)` | `recorder.rs:395-529` | The consumer loop: per chunk runs the FFT visualizer, pushes through `FrameResampler` → 30 ms frames → `handle_frame` (VAD gating). Handles `Start`/`Stop`/`Shutdown` commands; on `Stop`, drains until `EndOfStream`, flushes the resampler, and replies with `processed_samples`. |

### 2.6 VAD types

| Item | File:line | Notes |
|------|-----------|-------|
| `trait VoiceActivityDetector: Send + Sync` | `vad/mod.rs:17-26` | `push_frame(&mut, &[f32]) -> Result<VadFrame>`, default `is_voice`, default no-op `reset`. |
| `enum VadFrame<'a> { Speech(&'a [f32]), Noise }` | `vad/mod.rs:3-15` | Borrowed-slice result; `Speech` may aggregate prefill+current+hangover frames. |
| `struct SileroVad { engine: vad_rs::Vad, threshold: f32 }` | `vad/silero.rs:13-52` | Requires exactly `SILERO_FRAME_SAMPLES` (480 = 30 ms @ 16 kHz). `prob > threshold` ⇒ Speech. |
| `struct SmoothedVad { inner_vad, prefill_frames, hangover_frames, onset_frames, frame_buffer, … }` | `vad/smoothed.rs:5-105` | Hysteresis: buffers `prefill+1` frames for pre-roll; needs `onset_frames` consecutive voice frames to enter speech; emits `hangover_frames` of tail after voice stops. |

---

## 3. Threading / Concurrency Model

This subsystem is heavily concurrent. There are **three classes of threads** and **two channel systems**.

### 3.1 Threads

1. **Caller threads** — Tauri command threads and `actions.rs` worker threads call manager methods. `TranscribeAction::start` itself spawns short-lived `std::thread::spawn` workers for VAD preload (`actions.rs:401-405`) and for the feedback-sound-then-mute sequence (`actions.rs:424-427`, `444-451`). `TranscribeAction::stop` runs the transcription on `tauri::async_runtime::spawn` (`actions.rs:516`).

2. **The capture worker thread** — created in `AudioRecorder::open` (`recorder.rs:87`). It owns the non-`Send` cpal `Stream` (so the stream never crosses threads) and runs `run_consumer` in a blocking loop. It lives from `open()` to `close()`/`Shutdown`. The cpal input callback (`build_stream`, `recorder.rs:238-272`) runs on cpal's own realtime audio thread and only does cheap downmix + channel send.

3. **The lazy-close timer thread** — `schedule_lazy_close` (`audio.rs:221-239`) spawns a detached thread that sleeps 30 s then conditionally closes the stream.

### 3.2 Channels (in `AudioRecorder`)

- `sample_tx/sample_rx: mpsc::channel::<AudioChunk>` (`recorder.rs:70`) — cpal callback → consumer (audio data).
- `cmd_tx/cmd_rx: mpsc::channel::<Cmd>` (`recorder.rs:71`) — manager → consumer (control).
- `init_tx/init_rx: mpsc::sync_channel::<Result<(),String>>(1)` (`recorder.rs:72`) — worker → `open()` handshake so `open()` returns only after the stream is confirmed running.
- Per-`stop` reply channel inside `Cmd::Stop(mpsc::Sender<Vec<f32>>)` (`recorder.rs:24`, `205-211`) — consumer → `stop()` to return drained samples.
- `stop_flag: Arc<AtomicBool>` (`recorder.rs:88`) — shared between cpal callback and consumer to coordinate end-of-stream drain.

### 3.3 Locks in `AudioRecordingManager`

All synchronous `std::sync::Mutex`. Lock-ordering discipline matters:

- `state` is the **serialization point**. `try_start_recording` holds `state` across the start. `schedule_lazy_close` holds `state` across the close-check **and** the close, deliberately preventing a race where the idle timer closes the stream under a recording that just started (`audio.rs:226-238`). The code comments note `stop_microphone_stream` does **not** take `state`, so holding `state` while calling it from the lazy-close thread cannot deadlock.
- `is_open`, `is_recording`, `did_mute` — short critical sections, each typically locked independently. Note `start_microphone_stream` holds `is_open`, `did_mute`, and `recorder` simultaneously (`audio.rs:286-323`).
- `close_generation: AtomicU64` — lock-free monotonic counter. Every operation that should cancel a pending lazy-close (`try_start_recording`, `update_mode`, `update_selected_device`) does `fetch_add(1, SeqCst)`; the timer compares its captured token to the current value (`audio.rs:228`). This is a generation/epoch cancellation pattern.

**Edge:** `stop_recording`/`cancel_recording` drop the `state` lock (`drop(state)`, `audio.rs:435`, `498`) **before** the (potentially long, `extra_recording_buffer_ms`) sleep and the recorder stop, to avoid holding the FSM lock during blocking work.

---

## 4. Data Flow IN and OUT

### 4.1 IN (who calls this subsystem, with what)

```
Global shortcut (rdev) ──> shortcut/handler.rs ──> actions.rs TranscribeAction
                                                       │
   start(): preload_vad() ──┐                          │
            try_start_recording(binding_id) ───────────┤
            apply_mute() ────────────────────────────► AudioRecordingManager
   stop():  remove_mute()                               │
            stop_recording(binding_id) -> Vec<f32> ─────┘
   cancel:  cancel_current_operation() -> cancel_recording()

Frontend (React/Zustand) ──> Tauri commands (commands/audio.rs):
   update_microphone_mode  -> update_mode(AlwaysOn|OnDemand)
   set_selected_microphone -> update_selected_device()
   is_recording            -> is_recording()
```

- **Inputs are method args:** a `binding_id: &str` and a `MicrophoneMode`. Settings are pulled internally via `get_settings(&self.app_handle)` (not passed in).
- **Construction input:** `tauri::AppHandle` (`new`, `lib.rs:147`).

### 4.2 OUT (what this subsystem calls / emits)

- **Down to capture:** `AudioRecorder::{new, with_vad, with_level_callback, open, start, stop, close}` and `list_input_devices` (`audio_toolkit`).
- **OS side effects:** `set_mute` → AppleScript / WASAPI `IAudioEndpointVolume` / `wpctl|pactl|amixer`.
- **Settings reads:** `get_settings` for `always_on_microphone`, `selected_microphone`, `clamshell_microphone`, `mute_while_recording`, `lazy_stream_close`, `extra_recording_buffer_ms` (`settings.rs:354-432`).
- **Events emitted (indirectly):** the level callback → `utils::emit_levels` → `app.emit("mic-level", levels)` to main + overlay windows (`overlay.rs:388-395`). Recording errors are emitted by the **caller** (`actions.rs:476-482`, `"recording-error"`), not the manager.
- **Primary output:** `stop_recording` returns `Option<Vec<f32>>` (16 kHz, mono, VAD-gated). The caller (`actions.rs:524-548`) then: (a) `spawn_blocking(save_wav_file)` into `HistoryManager::recordings_dir()/handy-<ts>.wav`, and (b) `TranscriptionManager::transcribe(samples)`.

**Message/event type inventory:** `Cmd` (internal), `AudioChunk` (internal), `VadFrame` (internal), `"mic-level"` Tauri event (out, payload `Vec<f32>`), `"recording-error"` (out, by caller), `RecordingState`/`MicrophoneMode` (internal state).

---

## 5. Error Handling and Edge Cases

- **No input device:** `start_microphone_stream` pre-flight: if the effective device is `None` **and** `list_input_devices()` is empty, returns `Err("No input device found")` early (`audio.rs:305-312`), avoiding cryptic cpal backend errors. Classified by caller via `is_no_input_device_error` into a `"no_input_device"` error event (`actions.rs:471`).
- **Permission denied:** detected in `AudioRecorder::open` (`recorder.rs:181-186`) → `ErrorKind::PermissionDenied`; surfaced as `"microphone_permission_denied"` event (`actions.rs:469`). The caller then reverts UI (hides overlay, idle tray) so the app never gets stuck in the recording overlay.
- **VAD model missing:** `preload_vad` returns an error if the bundled ONNX path cannot be resolved (`audio.rs:274-276`); propagated from `start_microphone_stream`. `SileroVad::new` also validates threshold ∈ [0,1] (`vad/silero.rs:20-21`).
- **`recorder.stop()` failure:** in `stop_recording`, a failed `stop()` logs and returns `Vec::new()` (empty), not an error (`audio.rs:449-453`). Caller treats empty samples as "no audio; skip persistence" (`actions.rs:531-534`).
- **Recorder not available / wrong state:** `try_start_recording` returns `Err("Recorder not available")` or `Err("Already recording")` (`audio.rs:411-414`); `stop_recording`/`cancel_recording` no-op (return `None`/nothing) if the FSM isn't recording the matching binding (`audio.rs:482`, `496`).
- **Very short recordings:** `stop_recording` pads clips shorter than 1 s (`< WHISPER_SAMPLE_RATE`, but `>0`) up to 1.25 s of trailing zeros so Whisper has enough context (`audio.rs:474-477`).
- **Drain correctness:** on `Cmd::Stop`, the consumer sets `stop_flag`, the cpal callback emits one `EndOfStream`, and the consumer drains all remaining `Samples` until the sentinel, with a 2 s `recv_timeout` guard that logs and breaks on stall (`recorder.rs:489-510`). After replying it resets `stop_flag` so always-on mode keeps receiving.
- **Idle-close race:** mitigated by holding `state` across check+close and by the `close_generation` token (§3.3).
- **Silent failures:** `set_mute` swallows all OS errors by design (`audio.rs:14-19`).

---

## 6. State and Persistence Touched

- **In-memory state:** `RecordingState`, `MicrophoneMode`, `is_open`, `is_recording`, `did_mute`, `close_generation` (all in the struct, not persisted).
- **Settings store (tauri-plugin-store, via `settings.rs`):** reads `always_on_microphone`, `selected_microphone`, `clamshell_microphone`, `mute_while_recording`, `lazy_stream_close`, `extra_recording_buffer_ms`, `selected_output_device` (fields at `settings.rs:354-432`). The manager **reads** these; the `commands/audio.rs` handlers **write** them via `write_settings` then call the manager to apply.
- **Model files on disk:** the Silero VAD ONNX is loaded from bundled resources `resources/models/silero_vad_v4.onnx` via `BaseDirectory::Resource` (`audio.rs:271-276`). (Per `AGENTS.md`, fetched from `https://blob.handy.computer/silero_vad_v4.onnx` during dev setup.)
- **Audio files on disk:** the manager produces samples; the **caller** writes `handy-<unix_ts>.wav` (16 kHz/mono/16-bit) into `HistoryManager::recordings_dir()` (`actions.rs:538-543`, `history.rs:213`).
- **No SQLite directly in this subsystem.** History/DB lives in `managers/history.rs` and is fed by the caller, not by the audio manager.

---

## 7. Platform-Specific Branches

| Concern | macOS | Windows | Linux | iOS/Android |
|---------|-------|---------|-------|-------------|
| **Output mute** (`set_mute`, `audio.rs:13-100`) | `osascript -e "set volume output muted true/false"` (`audio.rs:91-99`) | COM `IMMDeviceEnumerator`→`IAudioEndpointVolume.SetMute` under `#[cfg(target_os="windows")]` (`audio.rs:21-55`) | tries `wpctl` (PipeWire) → `pactl` (PulseAudio) → `amixer` (ALSA) (`audio.rs:57-89`) | **None.** No branch; no-op. |
| **cpal host** (`get_cpal_host`, `audio_toolkit/utils.rs:1-12`) | default host | default host | forces `HostId::Alsa`, falls back to default | default |
| **Clamshell mic** (`helpers/clamshell.rs`) | `ioreg … AppleClamshellState` (`clamshell.rs:8-26`) | stub `false` (`clamshell.rs:50-53`) | stub `false` | stub `false` |
| **Windows mic permission** (`commands/audio.rs:61-150`) | n/a | registry-based `ConsentStore\microphone` read | n/a | n/a |
| **Mobile entry** | — | — | — | `#[cfg_attr(mobile, tauri::mobile_entry_point)]` exists (`lib.rs:316`) but no iOS audio capture path. |

**Key gap:** there is **no `#[cfg(target_os = "ios")]` or `#[cfg(mobile)]` audio capture code anywhere in this subsystem.** The only mobile gate in the codebase is the Tauri mobile entry point macro (`lib.rs:316`). cpal on iOS is technically possible but unexercised here.

---

## 8. PLAUD Relevance — Concrete Extension Points

Plaud-style features (always-on capture, system/call audio, multi-speaker diarization, long-form recording, AI summaries, cloud/local sync, iPhone) map onto this subsystem as follows. Each item names the **exact function/struct to modify or wrap**.

### 8.1 Capturing system/call audio (loopback), not just the mic
- **Where:** `AudioRecorder::open` / `get_preferred_config` / `build_stream` (`recorder.rs:65-280`) currently call `device.build_input_stream`. To capture system output, you need a **loopback/render-side capture**:
  - macOS: integrate a CoreAudio aggregate/ScreenCaptureKit or a virtual device (BlackHole/Loopback) — selectable through `get_effective_microphone_device` (`audio.rs:191-216`) which already resolves devices by name; add a "system audio" pseudo-device.
  - Windows: WASAPI loopback via cpal's render device in loopback mode — add a branch in `build_stream`/`get_preferred_config`.
- **Mixing mic + system:** wrap `AudioRecorder` to run **two** cpal streams (mic + loopback) and sum/interleave in `run_consumer`. The cleanest seam is to generalize `AudioRecorder` to hold a `Vec<DeviceStream>` and merge their `AudioChunk`s before `FrameResampler`.

### 8.2 Multi-speaker conversations & speaker diarization
- **Where to hook:** today `build_stream` (`recorder.rs:250-263`) **downmixes to mono immediately** and `handle_frame` (`recorder.rs:433-452`) collapses everything into one `processed_samples` buffer. For diarization you must:
  1. **Preserve channels** — stop the mono averaging in `build_stream`; carry per-channel or stereo through a new `AudioChunk` variant. (Stereo lets you separate near/far party on a call.)
  2. **Insert a diarization stage** parallel to the VAD in `run_consumer` (`recorder.rs:454-528`) — e.g. an embedding/clustering pass (pyannote-style) producing speaker-labeled segments alongside the 16 kHz frames.
  3. **Change the output type** from `Vec<f32>` (`stop_recording`, `audio.rs:427`) to a richer `RecordingSegments { samples, Vec<SpeakerSegment{ start, end, speaker_id }> }`, then thread that through `TranscribeAction::stop` (`actions.rs:524-548`) into transcription.
- `SmoothedVad` (`vad/smoothed.rs`) already produces speech/silence boundaries — reuse its onset/hangover boundaries as turn-segmentation seeds.

### 8.3 Long-form recording (hours, not seconds)
- **Critical change:** `run_consumer` accumulates the entire recording in a single in-RAM `processed_samples: Vec<f32>` and only returns it at `Stop` (`recorder.rs:409`, `516`). For long-form you must **stream to disk incrementally** instead of buffering. Add a streaming sink in `run_consumer` (rolling WAV/Opus writer), and change `stop_recording` (`audio.rs:447-458`) to return a file handle/path rather than samples.
- **VAD gating is lossy for long-form:** `handle_frame` drops `Noise` frames (`recorder.rs:449`), so silences are removed — bad for verbatim meeting records and timestamp alignment. Add a "raw/verbatim" mode that bypasses VAD (the `vad: Option<...>` is already optional in `AudioRecorder`, so a `with_vad(None)` path exists — expose it as a setting).
- **Lazy-close / idle timeout** (`STREAM_IDLE_TIMEOUT = 30s`, `audio.rs:11`) and `extra_recording_buffer_ms` are tuned for dictation; for long-form, `AlwaysOn` mode + disabling lazy-close is the right base (`update_mode`, `audio.rs:363`).
- **Chunked transcription:** drive transcription on rolling N-second windows rather than at `Stop`, by emitting partial buffers from `run_consumer`.

### 8.4 AI summaries
- **Where:** purely downstream of `stop_recording`. The caller `TranscribeAction::stop` (`actions.rs:516-548`) already runs transcription async and writes WAV+history. Add a post-transcription summarization step there (or in `managers/transcription.rs`) consuming the transcript (+ diarized speaker labels from §8.2). No change needed inside `audio.rs` itself beyond providing speaker/timestamp metadata.

### 8.5 Cloud / local sync
- **Where:** the WAV write happens at `actions.rs:538-543` into `HistoryManager::recordings_dir()`. Wrap/extend the `save_wav_file` call site (or `HistoryManager`) with an upload/sync hook. The audio manager produces the bytes; sync belongs to history/storage, but the **trigger point** is the `stop_recording` → save path.
- Consider encoding to a compressed format (Opus) before sync — `save_wav_file` (`audio_toolkit/audio/utils.rs:31`) is the place to swap the encoder.

### 8.6 Mobile (iPhone)
- **Biggest gap.** The entire capture stack is desktop-cpal. For iOS:
  - `set_mute` (`audio.rs:13`) has no iOS branch — would need AVAudioSession handling instead.
  - `get_cpal_host` / `AudioRecorder::open` would need an iOS AVAudioEngine backend (cpal iOS support is limited).
  - Background long-form recording requires iOS background-audio entitlements and AVAudioSession category management — none present.
  - The `mobile` entry exists (`lib.rs:316`) but no audio path is wired.
- **Recommended wrap:** define a trait `CaptureBackend` and make `AudioRecordingManager` depend on it (instead of concretely on `AudioRecorder`), with a cpal impl for desktop and an AVAudioEngine impl for iOS. The manager's FSM/mode logic (`audio.rs`) is largely platform-agnostic and could be reused as-is.

### 8.7 Smallest-diff hooks (summary)
- Multi-source/loopback: generalize `AudioRecorder` (`recorder.rs:33-196`).
- Keep channels / diarization: `build_stream` mono-downmix (`recorder.rs:250-263`) + `run_consumer` (`recorder.rs:454-528`).
- Streaming to disk: `run_consumer` `processed_samples` (`recorder.rs:409,516`) + `stop_recording` return type (`audio.rs:427`).
- Verbatim mode: expose optional-VAD path (`create_audio_recorder`, `audio.rs:120-141`).
- Summaries/sync: `TranscribeAction::stop` save path (`actions.rs:516-548`).
- Mobile: new `CaptureBackend` behind `AudioRecordingManager`.

---

## 9. Gaps vs a Plaud-Style Product

1. **No system/call/loopback audio.** Mic-only via `device.build_input_stream` (`recorder.rs:274`). No render-side/loopback capture, no app-audio capture, no "capture this Zoom/phone call" path.
2. **No diarization / speaker separation.** Audio is force-downmixed to mono at the callback (`recorder.rs:250-263`) and merged into one buffer; no per-speaker channels, embeddings, or turn labels. `RecordingState` has no notion of speakers.
3. **No long-form architecture.** Whole recording held in RAM (`processed_samples`, `recorder.rs:409`); returned only at stop. No incremental disk streaming, no chunked/partial transcription, no resumable/segmented sessions. Tuned constants (30 s idle close, short-clip padding) are dictation-oriented.
4. **VAD is lossy & non-verbatim.** Silences are dropped (`handle_frame`, `recorder.rs:449`), destroying timing/pauses needed for meeting transcripts and accurate timestamps. No timestamp track at all.
5. **No on-device summaries / no metadata model.** Output is a flat `Vec<f32>`; there is no transcript-segment structure, no titles, no action items, no speaker metadata flowing out of the subsystem.
6. **No cloud sync / sharing / encryption.** WAV is written locally only (`actions.rs:538-543`); no upload, no E2E encryption, no multi-device library, no conflict handling.
7. **No mobile capture.** No iOS/Android audio backend, no background-recording entitlements, no AVAudioSession management; only a dangling `mobile` entry-point macro (`lib.rs:316`).
8. **No mid-recording resilience / persistence.** A crash mid-session loses everything (RAM-only). No journaling, no auto-save checkpoints.
9. **Single concurrent session.** `RecordingState` is a single global FSM (`audio.rs:106`); cannot record two sources/sessions simultaneously (e.g. mic + call on separate tracks).
10. **No bandwidth/format options.** Fixed 16 kHz/mono/16-bit WAV (`audio_toolkit/audio/utils.rs:31-37`) — fine for ASR, poor for archival/playback quality and large for sync (no Opus/AAC).
11. **No calendar/context enrichment, no live captions, no real-time streaming** to a server — all of which Plaud-style products offer.
