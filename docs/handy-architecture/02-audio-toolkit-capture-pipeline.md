# Handy Audio Toolkit — Capture Pipeline (Forensic Architecture Document)

> **Abstract.** The `audio_toolkit` subsystem is Handy's low-level real-time audio capture and conditioning layer. It enumerates input/output devices via `cpal`, opens a hardware input stream at the device's *native* sample rate on a dedicated worker thread, down-mixes any channel layout to mono, streams 30 ms frames through an FFT spectrum visualizer (16 perceptual buckets emitted to the UI) and a `rubato` FFT resampler that converts to the fixed 16 kHz Whisper rate, optionally gates frames through a pluggable Voice Activity Detector (Silero, wrapped in a hysteresis "smoothed" layer), accumulates the surviving samples, and returns the full `Vec<f32>` mono 16 kHz buffer when recording stops. Auxiliary utilities read/write/verify mono 16 kHz PCM WAV files for history persistence. This document maps every struct, function, channel, lock, thread, error path, platform gate, and data-flow edge in the subsystem, and then identifies concrete extension points and gaps relative to a Plaud-style always-on, multi-speaker, long-form, cloud-synced conversation recorder.

---

## 1. Subsystem Boundary and File Inventory

Repo root for this subsystem: `/Users/vladvrinceanu/Desktop/PROGETTI ANTYGRAVITY/Plaude Local/handy/src-tauri/src/audio_toolkit/`

The subsystem under analysis is the **`audio/` capture pipeline + module glue + WAV utils**. The sibling `vad/` and `text/` modules are touched (the recorder consumes a `VoiceActivityDetector`) but are documented here only at the interface seam.

| Path | Responsibility (1-3 lines) |
| --- | --- |
| `audio_toolkit/mod.rs` | Top-level module that declares the `audio`, `constants`, `text`, `utils`, `vad` submodules and re-exports the public surface (`AudioRecorder`, device listers, WAV helpers, `get_cpal_host`, VAD types) consumed by the rest of the app. |
| `audio_toolkit/constants.rs` | Single global constant: `WHISPER_SAMPLE_RATE = 16000`. The canonical target rate the whole pipeline resamples to. |
| `audio_toolkit/utils.rs` | `get_cpal_host()` — platform-selected `cpal::Host` (ALSA on Linux, default elsewhere). This is the one host factory used everywhere. |
| `audio_toolkit/audio/mod.rs` | Re-export hub for the `audio` submodule: device, recorder, resampler, utils (WAV), visualizer. |
| `audio_toolkit/audio/device.rs` | Device enumeration: `list_input_devices()` / `list_output_devices()` returning `CpalDeviceInfo` (index, name, is_default, owned `cpal::Device`). |
| `audio_toolkit/audio/recorder.rs` | The heart of the subsystem: `AudioRecorder` builder + worker thread + consumer loop. Owns the cpal input stream, mono down-mix, command channel, VAD gating, resampling, visualization fan-out, and stop/drain semantics. |
| `audio_toolkit/audio/resampler.rs` | `FrameResampler` — `rubato::FftFixedIn` wrapper that fixes input chunk size (1024), buffers, resamples to 16 kHz, and re-chunks output into fixed 30 ms (480-sample) frames via an `emit` callback. |
| `audio_toolkit/audio/visualizer.rs` | `AudioVisualiser` — Hann-windowed FFT (`rustfft`) producing 16 log-spaced perceptual energy buckets (vocal band 400-4000 Hz) with adaptive noise floor, gain/curve shaping, and smoothing. Feeds the recording overlay. |
| `audio_toolkit/audio/utils.rs` | WAV I/O via `hound`: `read_wav_samples` (i16 → normalized f32), `verify_wav_file` (sample-count check), `save_wav_file` (f32 → i16 mono 16 kHz). |

> Note: `audio_toolkit/text.rs` and `audio_toolkit/vad/` exist and are referenced but are out of scope for this document except at their interface to the recorder.

---

## 2. Public Surface (Re-exports)

`audio_toolkit/mod.rs:7-13` re-exports the subsystem's public API:

```rust
pub use audio::{
    is_microphone_access_denied, is_no_input_device_error, list_input_devices, list_output_devices,
    read_wav_samples, save_wav_file, verify_wav_file, AudioRecorder, CpalDeviceInfo,
};
pub use utils::get_cpal_host;
pub use vad::{SileroVad, VoiceActivityDetector};
```

`audio_toolkit/audio/mod.rs:8-12` additionally exports `FrameResampler` and `AudioVisualiser` within the crate (these are *not* re-exported at the `audio_toolkit` top level, so they are effectively internal to the audio submodule; the recorder is their only consumer).

---

## 3. Type-by-Type, Function-by-Function Reference

### 3.1 `constants.rs`

- `WHISPER_SAMPLE_RATE: u32 = 16000` — `audio_toolkit/constants.rs:1`. The fixed output rate. Note `managers/audio.rs:102` redefines its own `const WHISPER_SAMPLE_RATE: usize = 16000;` rather than importing this one (duplicated constant — a minor smell).

### 3.2 `utils.rs` (host factory)

- `pub fn get_cpal_host() -> cpal::Host` — `audio_toolkit/utils.rs:3`.
  - **Linux** (`cfg(target_os = "linux")`, line 4): `cpal::host_from_id(cpal::HostId::Alsa)` with fallback to `cpal::default_host()` if ALSA host construction fails (line 6).
  - **All other OSes** (`cfg(not(target_os = "linux"))`, line 8): `cpal::default_host()` (line 10) — CoreAudio on macOS, WASAPI on Windows.
  - This is the single choke point for host selection; every device enumeration and stream open routes through it.

### 3.3 `device.rs` (enumeration)

- `pub struct CpalDeviceInfo` — `audio_toolkit/audio/device.rs:3-8`. Fields: `index: String` (the enumeration position stringified), `name: String`, `is_default: bool`, `device: cpal::Device` (the owned handle, later passed to `AudioRecorder::open`).
- `pub fn list_input_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>>` — `device.rs:10-30`. Gets the host (line 11), resolves the default input device *name* (line 12), iterates `host.input_devices()?` with `.enumerate()` (line 16), names unknown devices `"Unknown"` (line 17), and flags `is_default` by **name equality** against the default name (line 19). Returns the vector.
- `pub fn list_output_devices() -> Result<Vec<CpalDeviceInfo>, Box<dyn std::error::Error>>` — `device.rs:32-52`. Identical structure for `host.output_devices()`. Used for output-device selection of feedback sounds.

> Edge case: `is_default` matching by name is fragile when two devices share a name (e.g. two identical USB mics) — the first match wins and both could be flagged. Index is positional, not a stable hardware ID, so it can shift between enumerations (hot-plug). The app actually persists device **name**, not index (see `managers/audio.rs:191-216`).

### 3.4 `resampler.rs` (`FrameResampler`)

- `const RESAMPLER_CHUNK_SIZE: usize = 1024` — `resampler.rs:5`. Fixed input block fed to rubato.
- `pub struct FrameResampler` — `resampler.rs:7-13`. Fields: `resampler: Option<FftFixedIn<f32>>` (None when in/out rates are equal → passthrough), `chunk_in: usize`, `in_buf: Vec<f32>` (accumulates input to a full chunk), `frame_samples: usize` (output frame length = 480 for 16 kHz × 30 ms), `pending: Vec<f32>` (accumulates resampled output to a full frame).
- `pub fn new(in_hz, out_hz, frame_dur: Duration) -> Self` — `resampler.rs:16-35`. Computes `frame_samples = round(out_hz × frame_dur_secs)` (line 17), asserts > 0 (line 18, **panics** on too-short frame), constructs `FftFixedIn::new(in_hz, out_hz, 1024, sub_chunks=1, channels=1)` only when `in_hz != out_hz` (line 23-26); the constructor `.expect(...)` **panics** on failure.
- `pub fn push(&mut self, mut src: &[f32], mut emit: impl FnMut(&[f32]))` — `resampler.rs:37-64`. Passthrough fast-path when `resampler.is_none()` (line 38-41). Otherwise greedily fills `in_buf` to `chunk_in`, and on each full chunk calls `resampler.process(&[&in_buf], None)`, forwarding the output to `emit_frames` (line 49-62). Resampling errors are silently swallowed (`if let Ok(out) = ...`, line 51) — dropped audio, no log.
- `pub fn finish(&mut self, mut emit: impl FnMut(&[f32]))` — `resampler.rs:66-84`. Flushes the partial `in_buf` by **zero-padding** to a full chunk and resampling (line 69-75), then emits any partial `pending` frame zero-padded to `frame_samples` (line 79-83). Called once at stop to avoid losing the recording tail.
- `fn emit_frames(&mut self, mut data, emit)` — `resampler.rs:86-98`. Re-chunks resampler output into exact `frame_samples`-length frames, buffering remainders in `pending`. Guarantees the VAD always receives correctly-sized 480-sample frames (Silero requires exactly this — see §3.6).

### 3.5 `visualizer.rs` (`AudioVisualiser`)

Tuning constants — `visualizer.rs:4-7`: `DB_MIN = -55.0`, `DB_MAX = -8.0`, `GAIN = 1.3`, `CURVE_POWER = 0.7`.

- `pub struct AudioVisualiser` — `visualizer.rs:9-18`. Fields: `fft: Arc<dyn Fft<f32>>` (planned `rustfft` forward FFT), `window: Vec<f32>` (precomputed Hann), `bucket_ranges: Vec<(usize, usize)>` (FFT bin ranges per output bucket), `fft_input: Vec<Complex32>`, `noise_floor: Vec<f32>` (per-bucket adaptive floor), `buffer: Vec<f32>` (sample accumulator), `window_size`, `buckets`.
- `pub fn new(sample_rate: u32, window_size, buckets, freq_min: f32, freq_max: f32) -> Self` — `visualizer.rs:21-78`. Plans the FFT (line 28-29), precomputes the Hann window (line 32-36), clamps freq band to Nyquist (line 39-41), and for each bucket computes **logarithmic** (squared-fraction) frequency edges → FFT bin ranges, guaranteeing ≥1 bin/bucket (line 45-66). Initializes `noise_floor` to `-40.0` dB.
- `pub fn feed(&mut self, samples: &[f32]) -> Option<Vec<f32>>` — `visualizer.rs:80-149`. Accumulates samples (line 82); returns `None` until ≥ `window_size` available (line 85-87). Then: removes DC offset via mean subtraction (line 93), applies Hann window (line 96-99), runs FFT (line 102), computes per-bucket average power → dB (line 107-126), adaptively lowers `noise_floor` only when signal is quiet (`NOISE_ALPHA = 0.001`, line 129-133), normalizes dB into 0-1 with gain + `CURVE_POWER` shaping (line 136-137), applies a 3-tap smoothing across buckets (line 141-143), clears the buffer (line 146), and returns `Some(Vec<f32>)` of length `buckets`.
- `pub fn reset(&mut self)` — `visualizer.rs:151-155`. Clears the sample buffer and resets `noise_floor` to `-40.0`. Called on each `Cmd::Start`.

> Note: the buffer is fully cleared after each window (line 146) rather than sliding — so visualization is non-overlapping windows, which is fine for a level meter but discards inter-window samples for spectral continuity. The visualizer consumes the **raw, pre-resample** audio at the device's native rate (see consumer loop §4.2), which is why `run_consumer` scales `window_size` to the input rate (`recorder.rs:420-424`).

### 3.6 `recorder.rs` (`AudioRecorder` — central component)

#### Internal message enums

- `enum Cmd { Start, Stop(mpsc::Sender<Vec<f32>>), Shutdown }` — `recorder.rs:22-26`. Control messages from the public API to the worker. `Stop` carries a reply channel for the captured samples.
- `enum AudioChunk { Samples(Vec<f32>), EndOfStream }` — `recorder.rs:28-31`. Data messages from the cpal callback (producer) to the consumer loop. `EndOfStream` is the drain sentinel.

#### Struct

- `pub struct AudioRecorder` — `recorder.rs:33-39`. Fields:
  - `device: Option<Device>` — the opened device handle.
  - `cmd_tx: Option<mpsc::Sender<Cmd>>` — control channel to the worker.
  - `worker_handle: Option<JoinHandle<()>>` — the worker thread.
  - `vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>` — optional shared, lockable VAD.
  - `level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync>>` — optional spectrum callback.

#### Builder / lifecycle methods

- `pub fn new() -> Result<Self, Box<dyn Error>>` — `recorder.rs:42-50`. All-None construction. Infallible in practice.
- `pub fn with_vad(mut self, vad: Box<dyn VoiceActivityDetector>) -> Self` — `recorder.rs:52-55`. Wraps the VAD in `Arc<Mutex<...>>`.
- `pub fn with_level_callback<F>(mut self, cb: F) -> Self where F: Fn(Vec<f32>) + Send + Sync + 'static` — `recorder.rs:57-63`. Stores the spectrum sink (used by `managers/audio.rs` to `emit_levels`).
- `pub fn open(&mut self, device: Option<Device>) -> Result<(), Box<dyn Error>>` — `recorder.rs:65-196`. The most important method. Steps:
  1. Idempotent guard: returns `Ok(())` if a worker already exists (line 66-68).
  2. Creates three channels (line 70-72): `sample_tx/rx: AudioChunk` (producer→consumer, unbounded), `cmd_tx/rx: Cmd` (API→consumer), and `init_tx/rx: Result<(),String>` (**sync_channel(1)** for synchronous worker-init handshake).
  3. Resolves the device: provided device, else `host.default_input_device()` else `NotFound` error (line 74-80).
  4. **Spawns the worker thread** (line 87-170). Inside the thread:
     - Allocates the shared `stop_flag: Arc<AtomicBool>` (line 88) cloned into the cpal callback (line 89).
     - In an inline closure: fetches the preferred config (line 91), reads `sample_rate`/`channels`/format, logs them, and builds the typed input stream via `build_stream::<T>` for each `SampleFormat` (U8/I8/I16/I32/F32; line 105-149; unsupported → `Err`), then `stream.play()` (line 152).
     - On init success: sends `Ok(())` over `init_tx` (line 160), then calls `run_consumer(...)` (line 162) which **blocks for the lifetime of the recording session**, keeping `stream` alive; drops the stream afterward (line 163).
     - On init failure: logs and sends `Err(message)` over `init_tx` (line 166-167).
  5. **Blocks on `init_rx.recv()`** (line 172) so `open()` returns only after the stream is actually playing or has failed. On `Err`, joins the worker and maps `is_microphone_access_denied` → `PermissionDenied` else `Other` (line 179-187). On channel error, joins and returns `Other` (line 188-194). On success, stores `device`, `cmd_tx`, `worker_handle` (line 173-177).
- `pub fn start(&self) -> Result<(), Box<dyn Error>>` — `recorder.rs:198-203`. Sends `Cmd::Start`. Non-blocking; the consumer clears buffers and flips `recording = true` (see §4.2).
- `pub fn stop(&self) -> Result<Vec<f32>, Box<dyn Error>>` — `recorder.rs:205-211`. Creates a reply channel, sends `Cmd::Stop(resp_tx)`, then **blocks on `resp_rx.recv()`** for the drained, resampled, VAD-gated sample buffer.
- `pub fn close(&mut self) -> Result<(), Box<dyn Error>>` — `recorder.rs:213-222`. Takes and sends `Cmd::Shutdown`, joins the worker, clears `device`. Idempotent via `Option::take`.

#### Stream construction & config

- `fn build_stream<T>(device, config, sample_tx, channels, stop_flag) -> Result<cpal::Stream, BuildStreamError>` — `recorder.rs:224-280`. Generic over sample type `T: Sample + SizedSample + Send` with `f32: FromSample<T>`. The cpal data callback (line 238-272):
  - If `stop_flag` is set, sends exactly one `AudioChunk::EndOfStream` (guarded by `eos_sent`) and returns (line 239-245). This is the drain sentinel mechanism.
  - **Mono down-mix**: if `channels == 1`, converts each sample to f32 (line 250-251); else averages every interleaved frame across channels (line 253-263). This is where any multi-channel/stereo capture is collapsed — **a key fact for multi-speaker work** (see §8).
  - Sends `AudioChunk::Samples(output_buffer.clone())` (line 266-268); logs on send failure.
  - Stream error callback just logs (line 277).
- `fn get_preferred_config(device) -> Result<SupportedStreamConfig, Box<dyn Error>>` — `recorder.rs:282-335`. **Deliberately uses the device's native default sample rate** (line 289-290) and lets `FrameResampler` down-convert, to avoid forcing hardware (Bluetooth codecs, ALSA drivers) into non-native rates (comment line 285-288). Then scans `supported_input_configs()` for a config range covering the target rate, scoring formats **F32 > I16 > I32 > others** (line 310-318), falling back to the device default config when enumeration fails or nothing matches (line 294-298, 329-334).

#### Error classifiers (with unit tests)

- `pub fn is_microphone_access_denied(error_message: &str) -> bool` — `recorder.rs:338-343`. Matches "access is denied", "permission denied", or Windows HRESULT `0x80070005`.
- `pub fn is_no_input_device_error(error_message: &str) -> bool` — `recorder.rs:345-350`. Matches "no input device found", or a CoreAudio "failed to fetch preferred config" combo (macOS surfaces a no-device situation as a config error).
- Unit tests — `recorder.rs:352-393` cover both classifiers including the Windows error code and the CoreAudio case.

#### The consumer loop

- `fn run_consumer(in_sample_rate, vad, sample_rx, cmd_rx, level_cb, stop_flag)` — `recorder.rs:395-529`. Documented in §4.2. Contains the local helper `fn handle_frame(samples, recording, vad, out_buf)` (line 433-452) which: returns early if not recording; if a VAD is present, locks it and calls `push_frame`, appending only `VadFrame::Speech` payloads (falling back to treating the frame as speech on VAD error via `unwrap_or`, line 445); else appends raw.

### 3.7 `audio/utils.rs` (WAV persistence)

- `pub fn read_wav_samples<P: AsRef<Path>>(file_path) -> Result<Vec<f32>>` — `audio/utils.rs:7-14`. Reads i16 samples and normalizes by `i16::MAX` to f32. Used to re-load a history recording for re-transcription.
- `pub fn verify_wav_file<P>(file_path, expected_samples: usize) -> Result<()>` — `audio/utils.rs:17-28`. Reopens the WAV and `anyhow::bail!`s if the frame count differs from expected — a write-integrity check.
- `pub fn save_wav_file<P>(file_path, samples: &[f32]) -> Result<()>` — `audio/utils.rs:31-50`. Hard-codes `WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, Int }` (line 32-37). Converts f32 → i16 (line 42-45) **without clipping protection** (`(sample * i16::MAX) as i16` saturates via `as` but may wrap pathologically only for NaN; normal range is safe). Finalizes the writer.

---

## 4. Threading / Concurrency Model

### 4.1 Threads & channels overview

The recorder runs a **two-actor** model inside a **single spawned OS thread**, communicating with three `std::sync::mpsc` channels plus one `AtomicBool`:

```
                         AudioRecorder (caller thread, e.g. Tauri command / action task)
                              │   open()/start()/stop()/close()
        cmd_tx (Cmd) ─────────┤  (unbounded mpsc)              init_rx (sync_channel cap=1) ◄── init handshake
                              │
                              ▼
   ┌──────────────────────────────────────────────── worker OS thread (recorder.rs:87) ───────────────┐
   │                                                                                                    │
   │  cpal audio callback thread  ──sample_tx (AudioChunk)──►  run_consumer() loop  ──level_cb──► UI    │
   │  (build_stream closure)        (unbounded mpsc)           (frame_resampler, VAD, accumulate)       │
   │     ▲ stop_flag (AtomicBool, Relaxed)                                                              │
   └────────────────────────────────────────────────────────────────────────────────────────────────-─┘
                              ▲                                              │
                              └────── Stop(reply_tx) ── reply_tx.send(Vec<f32>) ──► stop() returns samples
```

- **cpal callback thread** is owned by the audio backend (CoreAudio/WASAPI/ALSA), *not* spawned by Handy. It runs the `build_stream` closure on each buffer period. It only touches `sample_tx`, `stop_flag`, and stack-local `output_buffer`/`eos_sent`.
- **Worker thread** (`std::thread::spawn`, `recorder.rs:87`) first builds the stream, then runs `run_consumer`, which owns the `FrameResampler`, `AudioVisualiser`, `processed_samples`, and `recording` flag — all thread-local, no locking needed for them.
- **`stop_flag: Arc<AtomicBool>`** (`recorder.rs:88`) coordinates the callback and consumer with `Ordering::Relaxed` loads/stores (e.g. set on Stop line 491, cleared on Start line 481 and after drain line 520). It is the signal that triggers the `EndOfStream` sentinel.
- **VAD lock**: the only `Mutex` actually contended is `vad: Arc<Mutex<Box<dyn VoiceActivityDetector>>>`. It is locked inside `handle_frame` (line 444) and on Start/reset (line 486). Because both the consumer's frame processing and the Start-handler run on the same consumer loop, the lock is effectively uncontended *within* the recorder; the `Arc<Mutex>` exists so the VAD can be shared/owned across the builder boundary, not for cross-thread concurrency. (The `Send + Sync` bound on the trait, `vad/mod.rs:17`, is what makes this compile.)

### 4.2 The consumer loop in detail (`recorder.rs:454-528`)

Setup (line 403-431): builds the `FrameResampler(in_rate → 16 kHz, 30 ms)`, and an `AudioVisualiser` whose `window_size` is chosen from `{256,512,1024,2048}` nearest to `in_rate/30` (line 420-424) so the analysis window (~33 ms) and frequency resolution stay roughly constant across devices.

Main loop:
1. Blocking `sample_rx.recv()` (line 455); breaks on channel close (stream gone).
2. `EndOfStream` chunks outside the drain are skipped (`continue`, line 462).
3. **Visualization**: `visualizer.feed(&raw)` on the raw native-rate audio; if it returns buckets, invoke `level_cb` (line 466-470). Always runs (even when not recording) so the overlay shows live levels.
4. **Pipeline**: `frame_resampler.push(&raw, |frame| handle_frame(frame, recording, &vad, &mut processed_samples))` (line 473-475) — resamples to 16 kHz and gates each 480-sample frame through VAD into `processed_samples` only while `recording`.
5. **Command drain** (non-blocking `cmd_rx.try_recv()` loop, line 478-527):
   - `Cmd::Start` (line 480-488): clear stop_flag, clear `processed_samples`, set `recording = true`, reset visualizer + VAD.
   - `Cmd::Stop(reply_tx)` (line 489-521): set `recording = false`, set `stop_flag`. Then **drain loop** (line 497-510): `sample_rx.recv_timeout(2s)` consuming remaining `Samples` (still resampled+gated with `recording=true` forced) until the `EndOfStream` sentinel arrives (guaranteeing all captured audio precedes it because the callback goes silent after setting eos). Then `frame_resampler.finish(...)` flushes the tail (line 512-514), `reply_tx.send(take(processed_samples))` returns the buffer (line 516), and `stop_flag` is cleared again so the always-on stream keeps delivering (line 520).
   - `Cmd::Shutdown` (line 522-525): set stop_flag and `return` (ends the thread, drops the stream upstream).

**Key correctness property**: the drain-until-sentinel design (comment line 493-496) ensures *zero sample loss* at stop — every sample the hardware delivered before the stop flag was observed is in the channel ahead of the `EndOfStream` marker.

### 4.3 Concurrency at the manager layer (`managers/audio.rs`)

The recorder itself is single-stream, but `AudioRecordingManager` (`managers/audio.rs:145-156`) wraps it with several `Arc<Mutex<...>>`: `state` (Idle/Recording), `mode` (AlwaysOn/OnDemand), `recorder: Arc<Mutex<Option<AudioRecorder>>>`, `is_open`, `is_recording`, `did_mute`, and a `close_generation: Arc<AtomicU64>` used to invalidate pending lazy-close timers (`schedule_lazy_close`, line 218-240). Lazy close runs on its own spawned thread that sleeps `STREAM_IDLE_TIMEOUT = 30s` (line 11, 221-222) and only closes if the generation still matches and state is Idle — holding the `state` lock across check+close to serialize against `try_start_recording` (line 227-238).

---

## 5. Data Flow IN and OUT

### 5.1 Inbound (who drives the subsystem)

- **Construction**: `managers/audio.rs::create_audio_recorder` (line 120-141) builds the `AudioRecorder` via `.with_vad(SmoothedVad(SileroVad))` (line 124-132) and `.with_level_callback(move |levels| utils::emit_levels(&app_handle, &levels))` (line 133-138). `preload_vad` (line 266-283) resolves `resources/models/silero_vad_v4.onnx` from the Tauri Resource dir and instantiates the recorder.
- **Device selection**: `get_effective_microphone_device` (line 191-216) calls `list_input_devices()` and matches by **name** (settings `selected_microphone` or `clamshell_microphone`). Tauri commands `get_available_microphones` / `set_selected_microphone` (`commands/audio.rs:179-217`) drive this from the UI.
- **Open/Start/Stop**: `start_microphone_stream` → `rec.open(selected_device)` (line 319); `try_start_recording` → `rec.start()` (line 402); `stop_recording`/`cancel_recording` → `rec.stop()` (line 448/501). These are invoked from the global-shortcut action layer (`actions.rs`) and CLI/signal handlers.
- **Output-device enumeration**: `list_output_devices()` is consumed by `commands/audio.rs:228-247` and by `audio_feedback.rs:107` to route feedback sounds.

### 5.2 Outbound (what the subsystem produces)

- **Sample buffer**: `AudioRecorder::stop()` returns `Vec<f32>` (mono, 16 kHz, VAD-gated). In `managers/audio.rs::stop_recording` (line 427-484) it is optionally padded if shorter than 1 s (line 474-477) and handed to the transcription pipeline.
- **Persistence**: `actions.rs:542-543` calls `save_wav_file(wav_path, samples)` on a `spawn_blocking` task and `verify_wav_file` (line 553) — writing `handy-<unix_ts>.wav` into the history `recordings_dir()`. Re-transcription reloads via `read_wav_samples` (`commands/history.rs:77`).
- **Live spectrum events**: the `level_cb` → `overlay::emit_levels` (`overlay.rs:388-396`) emits a `"mic-level"` Tauri event with `Vec<f32>` (16 buckets) to both the main window and the `recording_overlay` window. The frontend listens in `src/overlay/RecordingOverlay.tsx:41` (`listen<number[]>("mic-level", ...)`, default 16 bars at line 20).

### 5.3 Message/event type catalog

| Type | Direction | Defined at |
| --- | --- | --- |
| `Cmd::{Start, Stop(Sender<Vec<f32>>), Shutdown}` | API → consumer | `recorder.rs:22` |
| `AudioChunk::{Samples(Vec<f32>), EndOfStream}` | cpal callback → consumer | `recorder.rs:28` |
| `Result<(), String>` (init handshake) | worker → `open()` | `recorder.rs:72` |
| `VadFrame::{Speech(&[f32]), Noise}` | VAD → consumer | `vad/mod.rs:3` |
| `Vec<f32>` (mono 16 kHz samples) | `stop()` → manager | `recorder.rs:205` |
| `Vec<f32>` (16 spectrum buckets) | `level_cb` → `emit_levels` → `"mic-level"` event | `recorder.rs:38`, `overlay.rs:390` |

---

## 6. Error Handling & Edge Cases

- **Init handshake** prevents `open()` from returning before the stream actually plays; errors are surfaced with the correct `io::ErrorKind` (`PermissionDenied` vs `Other`) based on `is_microphone_access_denied` (`recorder.rs:179-187`).
- **No input device**: `open()` returns `NotFound` (line 78-79); the manager pre-flights with `list_input_devices().is_empty()` and returns a clean `"No input device found"` (`managers/audio.rs:305-312`), which `is_no_input_device_error` later classifies for UI messaging.
- **Resampler panics**: `FftFixedIn::new(...).expect(...)` (`resampler.rs:24-25`) and the `frame_samples > 0` assert (line 18) will **panic the worker thread** rather than return an error — an upstream invariant (rates are valid, frame_dur=30 ms) keeps this from firing in practice, but it is an unhandled failure mode.
- **Silent audio drops**: resampler `process` errors are swallowed (`resampler.rs:51, 72`) with no logging; visualizer power=0 maps to a `-80 dB` floor (`visualizer.rs:125`).
- **VAD failure tolerance**: `handle_frame` uses `det.push_frame(samples).unwrap_or(VadFrame::Speech(samples))` (`recorder.rs:445`) — on any VAD error it *keeps* the frame (fail-open), preventing audio loss but also bypassing gating. Note Silero requires exactly 480 samples (`vad/silero.rs:34-39` bails otherwise) — the `FrameResampler` 30 ms framing is what satisfies this; a mismatch would make every frame error and (because of fail-open) effectively disable VAD.
- **Stop drain timeout**: `recv_timeout(2s)` (`recorder.rs:498`) logs a warning and breaks if the `EndOfStream` never arrives (line 505-508) — bounded blocking, no deadlock.
- **Lock poisoning**: every `.lock().unwrap()` (recorder VAD line 444, 486; manager throughout) will **panic on poison**. There is no recovery path.
- **WAV mismatch**: `verify_wav_file` catches truncated/partial writes; `actions.rs:557-562` logs and marks `wav_saved=false`, gating history persistence.
- **Channel closure**: consumer `sample_rx.recv()` error → loop break (line 457) → thread exits cleanly when the stream is dropped.

---

## 7. State & Persistence Touched

- **Files on disk**: `save_wav_file` writes `handy-<ts>.wav` (mono/16 kHz/16-bit) under the history `recordings_dir()` (`actions.rs:538-543`); `read_wav_samples` reloads them for retry (`commands/history.rs:77`). These are the only artifacts the *audio* subsystem writes.
- **Model files**: the recorder indirectly depends on `resources/models/silero_vad_v4.onnx`, resolved by the manager (`managers/audio.rs:269-276`) and loaded by `SileroVad::new` (`vad/silero.rs:18-30`). The audio subsystem itself reads no model.
- **Settings store** (tauri-plugin-store, via `get_settings`/`write_settings`): not read inside `audio_toolkit` directly — instead the manager/commands layer reads `selected_microphone`, `clamshell_microphone`, `selected_output_device`, `always_on_microphone`, `mute_while_recording`, `lazy_stream_close`, `extra_recording_buffer_ms` and passes resolved devices into the recorder. The subsystem is intentionally settings-agnostic.
- **SQLite**: not touched by the audio subsystem (history DB lives in `managers/history.rs`); only the produced WAV path is shared.

---

## 8. Platform-Specific Branches (cfg gates)

Within `audio_toolkit/audio/*` and `audio_toolkit/utils.rs`, **the only OS cfg gate is the host selection** in `get_cpal_host` (`utils.rs:4-11`): ALSA on Linux, default host (CoreAudio/WASAPI) elsewhere. The recorder, resampler, visualizer, device enumeration, and WAV utils are **fully platform-agnostic** — they rely on cpal's abstraction. The format-scoring and "use native rate" logic in `get_preferred_config` (`recorder.rs:282-335`) exists specifically to be robust across CoreAudio/WASAPI/ALSA and Bluetooth/USB codecs without per-OS code.

Platform specifics live *outside* the subsystem: Windows mic-permission registry probing and `ms-settings:privacy-microphone` (`commands/audio.rs:61-150`), mute via WASAPI/`wpctl`/`pactl`/`amixer`/`osascript` (`managers/audio.rs:13-100`), and clamshell detection (`helpers::clamshell`). **There is no iOS/Android/mobile branch anywhere in the subsystem** — `cfg(target_os = "ios")` does not appear; the only cfgs are `linux` / `not(linux)` (`utils.rs`) and `cfg(test)` (`recorder.rs:352`, `text.rs:322`). The toolkit is desktop-only.

---

## 9. PLAUD Relevance — Concrete Extension Points

A Plaud-style product needs: continuous/long-form capture, system+call audio (not just mic), multi-speaker diarization, AI summaries, cloud/local sync, and an iPhone client. Mapping each to specific functions to modify or wrap:

1. **Long-form, always-on capture** — the recorder already supports an always-on stream (`managers/audio.rs:182-184`, `MicrophoneMode::AlwaysOn`) and a non-loss drain. *But* `processed_samples` (`recorder.rs:409`) accumulates the **entire** recording in RAM and is only flushed on `stop()`. For hour-long sessions, **wrap the consumer's `handle_frame` accumulation** (`recorder.rs:433-452, 516`) to stream fixed-size chunks out a new `mpsc::Sender<Vec<f32>>` (e.g. add a `with_chunk_sink(cb)` builder alongside `with_level_callback`) so audio is persisted incrementally rather than held in one growing `Vec`.

2. **Raw, ungated audio retention** — VAD gating (`handle_frame` line 443-451) *discards* non-speech, so the saved WAV is speech-only. Plaud keeps full audio for replayable timelines. **Add a parallel ungated sink**: in the consumer, push every resampled frame to a "full audio" buffer/file independent of the VAD branch, and keep the gated buffer only for transcription. The split point is exactly `handle_frame`.

3. **System / call audio capture** — `get_cpal_host` + `build_stream` capture *input* devices only. For call/meeting audio you need loopback. **Modify `get_cpal_host`/`device.rs`** to expose WASAPI loopback (Windows) and route a macOS virtual device (BlackHole/CoreAudio taps); on macOS 13+, wrap `ScreenCaptureKit` system-audio as an alternative producer feeding the same `sample_tx`/consumer. The consumer loop is producer-agnostic, so a new producer just needs to emit `AudioChunk::Samples`.

4. **Multi-speaker / diarization** — the **down-mix to mono is the blocker**: `build_stream` averages all channels into one (`recorder.rs:250-263`). To diarize, you must **preserve multi-channel** (or capture mic + loopback as separate tracks) before mono collapse. Concretely: parameterize `build_stream` to emit per-channel (or a stereo mic + system pair) `Vec<Vec<f32>>`, run resampling per channel, and feed a diarization model (pyannote/Silero-based embedding clustering) in a new stage between the resampler and `processed_samples`. The `FrameResampler` is already single-channel (`channels=1` hard-coded at `resampler.rs:24`) so a per-channel resampler array is the cleanest wrap.

5. **Speaker labels in the transcript** — the subsystem outputs a flat `Vec<f32>`; diarization metadata has no carrier. **Introduce a timestamped frame type** (e.g. `TimedFrame { t_ms, samples, speaker: Option<SpeakerId> }`) replacing the bare `Vec<f32>` returned by `stop()`, and thread it through `managers/audio.rs::stop_recording` → transcription.

6. **AI summaries** — purely downstream of `stop()`; no audio-subsystem change. The hook is the `Vec<f32>` (or future streamed chunks) handed to `tm.transcribe` in `actions.rs:548`; summaries consume the resulting text. The audio toolkit's job is just to deliver clean 16 kHz mono, which it does.

7. **Cloud / local sync** — the WAV writer (`save_wav_file`, `audio/utils.rs:31-50`) is the persistence seam. Wrap or replace it with a sink that (a) writes a local canonical file and (b) enqueues an upload (chunked/resumable). Because `save_wav_file` already runs on `spawn_blocking` (`actions.rs:542`), adding an async upload after `verify_wav_file` is low-risk. For incremental sync, combine with extension point #1's chunk sink.

8. **Mobile (iPhone)** — none of this compiles for iOS today (no `cfg(target_os="ios")`, cpal/ALSA assumptions). The portable, reusable core is `FrameResampler`, `AudioVisualiser`, and the VAD trait — all pure DSP with no OS deps. **Strategy**: extract `resampler.rs` + `visualizer.rs` + the `VoiceActivityDetector` trait into a platform-neutral crate, and provide an iOS audio producer (AVAudioEngine/CoreAudio via Swift FFI or `cpal`'s iOS backend if viable) that feeds the same `AudioChunk` channel into `run_consumer`. The consumer logic (drain, gate, accumulate) is OS-agnostic and would port directly.

9. **Format/quality** — `save_wav_file` is locked to 16 kHz/16-bit mono (`audio/utils.rs:32-37`), matching Whisper but lossy for archival. For Plaud-grade archives, add a higher-fidelity native-rate writer (e.g. capture the pre-resample `raw` already available in the consumer at `recorder.rs:460-463`) in parallel with the 16 kHz transcription path.

---

## 10. Gaps vs a Plaud-Style Product

- **Mono-only capture** — multi-channel is averaged away at `recorder.rs:250-263`; no stereo, no per-speaker channels, no diarization substrate.
- **No system/loopback/call audio** — input devices only; no WASAPI loopback, CoreAudio tap, or ScreenCaptureKit. Cannot record the other side of a call or meeting playback.
- **No diarization / speaker identification** — nothing in the pipeline emits or carries speaker IDs; output is a flat sample buffer.
- **Full recording buffered in RAM** — `processed_samples` grows unbounded until `stop()` (`recorder.rs:409, 516`); no streaming/segmenting, risky for long-form (hours) sessions.
- **Speech-only persistence** — VAD discards non-speech (`handle_frame`), so saved audio is not a faithful, replayable recording of the session.
- **No timestamps / segment boundaries** — no per-frame timing, so no word/segment alignment carrier from the audio layer.
- **No mobile/iOS support** — entirely desktop; no `ios` cfg; cpal/ALSA-bound.
- **No live transcription streaming** — transcription only happens post-`stop()`; there is no incremental/partial-result path from the capture loop.
- **Lossy archival format** — fixed 16 kHz/16-bit mono WAV; no native-rate or compressed (FLAC/Opus) archive, no cloud upload, no sync/versioning.
- **No encryption at rest / privacy controls** — WAV files are written in cleartext to the history dir; a Plaud product handling sensitive conversations would need encryption + retention policy.
- **Panic-prone failure modes** — `expect`/`assert` in the resampler (`resampler.rs:18, 24`) and `lock().unwrap()` throughout can crash the worker thread with no recovery; resampler errors are silently dropped.
- **Single concurrent recording** — `AudioRecorder::open` is idempotent-single and the manager enforces one `RecordingState::Recording`; no support for capturing multiple simultaneous sources/sessions.
