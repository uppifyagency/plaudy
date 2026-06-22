# Voice Activity Detection: Silero VAD + Smoothing

> **Abstract.** This document is a forensic, line-cited analysis of Handy's Voice Activity Detection (VAD) subsystem, located under `src-tauri/src/audio_toolkit/vad/`. The subsystem defines a small trait-based abstraction (`VoiceActivityDetector`) over per-frame speech/non-speech classification, a concrete neural detector (`SileroVad`) wrapping the `vad-rs` ONNX runtime around the Silero VAD v4 model, and a stateful temporal smoother (`SmoothedVad`) that adds onset confirmation, pre-roll (prefill) buffering, and hangover/release logic to remove glitchy single-frame decisions. The VAD operates on fixed 30 ms / 480-sample mono frames at 16 kHz, is driven from the audio recorder's consumer thread behind an `Arc<Mutex<...>>`, and emits a `VadFrame` enum that the recorder uses to decide which audio to keep before handing it to the Whisper/Parakeet transcription pipeline. The model is a bundled on-disk ONNX file (`resources/models/silero_vad_v4.onnx`). This subsystem is the most directly reusable seam in the entire app for a Plaud-style product, but as written it is tuned for short push-to-talk dictation and lacks any concept of speakers, long-form segmentation, or timestamps.

---

## 1. Per-file responsibilities

| File | Responsibility |
| --- | --- |
| `src-tauri/src/audio_toolkit/vad/mod.rs` | Defines the public surface of the subsystem: the `VadFrame<'a>` enum (Speech vs Noise), the `VoiceActivityDetector` trait (streaming `push_frame` + helpers), and re-exports `SileroVad` and `SmoothedVad`. |
| `src-tauri/src/audio_toolkit/vad/silero.rs` | Concrete neural VAD. Wraps `vad_rs::Vad` (Silero ONNX model) and converts a per-frame speech probability into a thresholded keep/drop `VadFrame`. Enforces the fixed 480-sample frame size. |
| `src-tauri/src/audio_toolkit/vad/smoothed.rs` | Decorator VAD. Wraps any inner `VoiceActivityDetector` and adds temporal smoothing: onset confirmation, pre-roll prefill, and hangover/release to produce clean speech segments. |

Supporting files (outside the subsystem but on its data path):

| File | Relevance |
| --- | --- |
| `src-tauri/src/audio_toolkit/constants.rs` | `WHISPER_SAMPLE_RATE: u32 = 16000` — the sample rate the VAD and the whole pipeline assume. |
| `src-tauri/src/audio_toolkit/audio/recorder.rs` | The sole production consumer. Owns the VAD behind `Arc<Mutex<Box<dyn VoiceActivityDetector>>>`, feeds it 30 ms resampled frames, and accumulates `VadFrame::Speech` output. |
| `src-tauri/src/audio_toolkit/audio/resampler.rs` | `FrameResampler` resamples device audio to 16 kHz and chops it into exact 30 ms (480-sample) frames before they reach the VAD. |
| `src-tauri/src/managers/audio.rs` | Constructs the concrete VAD stack (`SileroVad` wrapped in `SmoothedVad`) and resolves the on-disk model path. |
| `src-tauri/src/audio_toolkit/bin/cli.rs` | A standalone test CLI (currently disabled in `Cargo.toml`) that also builds a VAD stack. Contains a **stale call signature** (see §5). |

---

## 2. Public types, traits and functions (with signatures and citations)

### 2.1 `mod.rs` — the abstraction layer

**`enum VadFrame<'a>`** — `src-tauri/src/audio_toolkit/vad/mod.rs:3-8`
```rust
pub enum VadFrame<'a> {
    Speech(&'a [f32]), // may aggregate several frames (prefill + current + hangover)
    Noise,             // non-speech; downstream code can ignore it
}
```
The borrow lifetime `'a` is significant: a `Speech` variant can borrow either the caller's input frame (`SileroVad`, ongoing-speech case of `SmoothedVad`) **or** an internal buffer owned by the detector (`SmoothedVad::temp_out` on onset). This is why `push_frame` ties `&'a mut self` and `&'a [f32]` to the same lifetime (see below).

**`impl VadFrame::is_speech(&self) -> bool`** — `mod.rs:10-15`. Inline helper, `matches!(self, VadFrame::Speech(_))`.

**`trait VoiceActivityDetector: Send + Sync`** — `mod.rs:17-26`. The core abstraction. `Send + Sync` bounds are what allow the detector to be stored in an `Arc<Mutex<...>>` and moved into the recorder worker thread.
- `fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> Result<VadFrame<'a>>` — `mod.rs:19`. **Primary streaming API.** Feed one 30 ms frame; get a keep/drop decision plus (possibly aggregated) audio. The shared lifetime `'a` couples the returned borrow to the detector and the input — meaning the caller must consume the returned `VadFrame` before calling `push_frame` again (the borrow checker enforces single-frame-at-a-time usage).
- `fn is_voice(&mut self, frame: &[f32]) -> Result<bool>` — `mod.rs:21-23`. Default method: `Ok(self.push_frame(frame)?.is_speech())`. A convenience boolean wrapper; used by `SmoothedVad` to query its inner detector without caring about the returned audio.
- `fn reset(&mut self) {}` — `mod.rs:25`. Default no-op. Overridden by `SmoothedVad` to clear temporal state between recordings.

`mod silero; mod smoothed;` and `pub use silero::SileroVad; pub use smoothed::SmoothedVad;` — `mod.rs:28-32`.

### 2.2 `silero.rs` — the neural detector

**Constants** — `silero.rs:9-11`
```rust
const SILERO_FRAME_MS: u32 = 30;
const SILERO_FRAME_SAMPLES: usize =
    (constants::WHISPER_SAMPLE_RATE * SILERO_FRAME_MS / 1000) as usize; // = 480
```
At 16 kHz, 30 ms = **480 samples**. This is the hard contract the detector enforces on every input frame.

**`struct SileroVad`** — `silero.rs:13-16`. Fields: `engine: vad_rs::Vad` (the ONNX-backed Silero model) and `threshold: f32` (speech-probability cutoff).

**`fn SileroVad::new<P: AsRef<Path>>(model_path: P, threshold: f32) -> Result<Self>`** — `silero.rs:19-29`.
- Validates `0.0 <= threshold <= 1.0`, else `anyhow::bail!` — `silero.rs:20-22`.
- Constructs `Vad::new(&model_path, 16000)` and maps any error to `anyhow` — `silero.rs:25-26`. This is where the ONNX model file is loaded from disk and the ONNX session is created.

**`impl VoiceActivityDetector for SileroVad`** — `silero.rs:32-52`.
- `push_frame` — `silero.rs:33-51`:
  1. Hard length check: `frame.len() != 480` → `anyhow::bail!("expected 480 samples, got N")` — `silero.rs:34-39`.
  2. `self.engine.compute(frame)` runs the ONNX inference, returning a result with a `.prob` field (speech probability in `[0,1]`) — `silero.rs:41-44`.
  3. `if result.prob > self.threshold` → `VadFrame::Speech(frame)` (borrows the input), else `VadFrame::Noise` — `silero.rs:46-50`.
- `reset` is **not** overridden, so `SileroVad` keeps the default no-op. Note: the underlying Silero LSTM has internal recurrent state inside `vad_rs::Vad`; Handy has no way to reset it (see §5, §9).

### 2.3 `smoothed.rs` — the temporal smoother

**`struct SmoothedVad`** — `smoothed.rs:5-17`. Fields:
- `inner_vad: Box<dyn VoiceActivityDetector>` — the wrapped detector (in production, a `SileroVad`).
- `prefill_frames: usize` — how many past frames to prepend as pre-roll when speech starts (avoids clipping the first word).
- `hangover_frames: usize` — how many trailing non-speech frames to keep emitting after speech ends (avoids clipping word tails / bridging short pauses).
- `onset_frames: usize` — how many consecutive speech frames are required before declaring speech (debounces false positives).
- `frame_buffer: VecDeque<Vec<f32>>` — ring buffer of recent raw frames for pre-roll.
- `hangover_counter`, `onset_counter`, `in_speech: bool` — the running state machine.
- `temp_out: Vec<f32>` — scratch buffer that owns the concatenated prefill+current audio emitted on onset.

**`fn SmoothedVad::new(inner_vad, prefill_frames, hangover_frames, onset_frames) -> Self`** — `smoothed.rs:20-37`. A **4-argument** constructor (this matters — see §5). Initializes all counters to zero / `in_speech=false`.

**`impl VoiceActivityDetector for SmoothedVad`** — `smoothed.rs:40-105`.
- `push_frame` — `smoothed.rs:41-96`. The smoothing state machine:
  1. **Buffer for pre-roll** — `smoothed.rs:43-46`: push a clone (`frame.to_vec()`) into `frame_buffer`, then trim so it never exceeds `prefill_frames + 1` entries.
  2. **Delegate** to the inner VAD via `is_voice(frame)?` — `smoothed.rs:49`.
  3. **Four-state transition** on `(self.in_speech, is_voice)` — `smoothed.rs:51-95`:
     - `(false, true)` *potential onset* — `smoothed.rs:53-71`: increment `onset_counter`; once it reaches `onset_frames`, flip `in_speech=true`, arm `hangover_counter=hangover_frames`, reset `onset_counter`, concatenate the whole `frame_buffer` (prefill + current) into `temp_out`, and return `VadFrame::Speech(&self.temp_out)`. Otherwise return `Noise` (still suppressing audio).
     - `(true, true)` *ongoing speech* — `smoothed.rs:74-77`: re-arm `hangover_counter`, return `VadFrame::Speech(frame)`.
     - `(true, false)` *end / release* — `smoothed.rs:80-88`: if `hangover_counter > 0`, decrement and still return `Speech(frame)` (keep the tail); else flip `in_speech=false`, return `Noise`.
     - `(false, false)` *silence* — `smoothed.rs:91-94`: reset `onset_counter` (broken onset streak), return `Noise`.
- `reset` — `smoothed.rs:98-104`: clears `frame_buffer`, zeroes both counters, sets `in_speech=false`, clears `temp_out`. **Does not** reset `inner_vad` (the inner default `reset` is a no-op anyway).

### 2.4 Production parameterization

In `managers/audio.rs:124-126` (`create_audio_recorder`):
```rust
let silero = SileroVad::new(vad_path, 0.3)?;             // threshold 0.3
let smoothed_vad = SmoothedVad::new(Box::new(silero), 15, 15, 2);
//                                  inner, prefill=15, hangover=15, onset=2
```
At 30 ms/frame: **prefill = 15 frames = 450 ms** of pre-roll, **hangover = 15 frames = 450 ms** of release tail, **onset = 2 frames = 60 ms** of confirmation. Threshold `0.3` is fairly permissive (favors recall — keep speech — over precision), appropriate for dictation where dropping a word is worse than keeping a little noise.

---

## 3. Threading & concurrency model

The VAD types themselves contain **no threads, channels, async tasks, or locks** — they are plain synchronous state machines. Concurrency is imposed entirely by the consumer, the `AudioRecorder`:

- **Storage:** `AudioRecorder.vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>` — `recorder.rs:37`. Installed via `with_vad` — `recorder.rs:52-55`.
- **Thread handoff:** in `open()`, `let vad = self.vad.clone();` clones the `Arc` and moves it into the recorder worker thread (`std::thread::spawn`) — `recorder.rs:83, 87`. The `Send + Sync` bound on the trait (`mod.rs:17`) is what makes this legal.
- **Lock acquisition per frame:** the consumer's nested `handle_frame` does `let mut det = vad_arc.lock().unwrap();` then `det.push_frame(samples)` — `recorder.rs:443-448`. So **exactly one frame is classified at a time, under the mutex, on the consumer thread.** There is no parallelism across frames (and there cannot be: Silero is stateful/recurrent, and `push_frame`'s `&mut self` + shared lifetime forbid it).
- **Reset under lock:** on `Cmd::Start`, the consumer calls `v.lock().unwrap().reset()` — `recorder.rs:485-487` — to clear smoothing state at the start of each recording.
- **Producer/consumer split:** the cpal audio callback (producer) sends `AudioChunk::Samples` over an `mpsc::channel` — `recorder.rs:70`. The consumer (`run_consumer`, `recorder.rs:395`) receives them, runs the `FrameResampler` to emit exact 30 ms frames, and only those frames reach the VAD. The VAD never sees raw device audio.

**Lock-contention note:** `push_frame` performs synchronous ONNX inference while holding the mutex. In the current design only the consumer thread ever locks it, so contention is nil; but any future "always-on" parallel transcription or a second consumer would serialize on this single mutex.

---

## 4. Data flow IN and OUT

### IN (what calls the VAD, and with what)
```
cpal input callback  ──mpsc(AudioChunk)──▶  run_consumer (recorder.rs:395)
   raw device samples (any rate/format, downmixed to mono)
        │
        ▼
FrameResampler.push  (resampler.rs:37)  ──▶ exact 30 ms / 480-sample @16 kHz frames
        │  (only while `recording == true`)
        ▼
handle_frame (recorder.rs:433) ──▶ vad.lock().push_frame(frame)   ◀── ENTRY POINT
```
- Input message type: a borrowed `&[f32]` slice of exactly 480 mono f32 samples at 16 kHz.
- Construction inputs: model path (`resources/models/silero_vad_v4.onnx`, resolved via Tauri's resource dir — `audio.rs:269-280`) and tuning scalars (`0.3`, `15`, `15`, `2`).

### OUT (what the VAD returns, and where it goes)
```
push_frame ──▶ VadFrame::Speech(buf)  ──▶ out_buf.extend_from_slice(buf)   (recorder.rs:445-446)
            └▶ VadFrame::Noise        ──▶ dropped                          (recorder.rs:447)
```
- The accumulated `out_buf` / `processed_samples` (`recorder.rs:409`) is the VAD-filtered speech audio.
- On `Cmd::Stop`, the consumer drains remaining audio and sends the final `Vec<f32>` back over an `mpsc::Sender<Vec<f32>>` (`recorder.rs:489+`) to the caller in `managers/audio.rs`, which hands it to the transcription manager (Whisper/Parakeet via `transcribe-rs`).
- **Event types:** the VAD emits no Tauri events itself. (The *spectrum visualiser*, a sibling in `run_consumer`, emits level events via `level_cb` → `utils::emit_levels` — `recorder.rs:466-469`, `audio.rs:133-137` — but that path is independent of the VAD decision.)

So the subsystem is a pure in-process function on the audio thread: **frames in, kept-speech-audio out.** It does not touch the database, the settings store, or the frontend directly.

---

## 5. Error handling & edge cases

- **Threshold validation:** `SileroVad::new` rejects thresholds outside `[0,1]` with `anyhow::bail!` — `silero.rs:20-22`.
- **Frame-size contract:** `SileroVad::push_frame` hard-fails if `frame.len() != 480` — `silero.rs:34-39`. The `FrameResampler` guarantees this size, but a misconfigured caller would get an `Err`.
- **Model-load failure:** `Vad::new` errors are wrapped (`"Failed to create VAD: {e}"`) and propagate as `anyhow::Error` out of `SileroVad::new` and up through `create_audio_recorder` / `preload_vad` — `silero.rs:25-26`, `audio.rs:124-125`. A missing/corrupt `silero_vad_v4.onnx` therefore fails recorder construction, not the whole app.
- **Inference failure → fail-open:** the critical resilience decision is in the consumer, not the VAD. `det.push_frame(samples).unwrap_or(VadFrame::Speech(samples))` — `recorder.rs:445`. **If the VAD errors, the frame is treated as speech and kept.** This biases toward never losing audio, but it also means a persistently failing VAD silently degrades into a pass-through (no filtering) with only the dropped `Err` as evidence — there is no logging on this path.
- **Onset interruption:** a single non-speech frame during the onset window resets `onset_counter` to 0 — `smoothed.rs:91-93` — so brief blips can't trigger a false speech segment, but a noisy environment that never gets `onset_frames` consecutive hits will never start a segment.
- **Hangover bridging:** because `hangover_counter` is re-armed on every speech frame (`smoothed.rs:75`) and counts down only during silence, gaps shorter than 450 ms (15 frames) are bridged into one continuous segment. Pauses longer than that split the audio — but since the recorder simply concatenates all `Speech` output into one buffer, this is invisible downstream (no boundaries are recorded).
- **Stateful Silero across recordings:** `SmoothedVad::reset` clears *its* state, but the inner `SileroVad`/`vad_rs::Vad` recurrent state is never reset (no override of `reset` in `silero.rs`). Across many start/stop cycles the LSTM carries residual state. In practice the first `onset_frames` confirmation masks this, but it is a latent correctness gap.
- **Stale CLI signature (dead code):** `src-tauri/src/audio_toolkit/bin/cli.rs:179` calls `SmoothedVad::new(Box::new(silero), 15, 15)` — **only 3 args**, against the current **4-arg** constructor (`smoothed.rs:20`). This would not compile. It is harmless today only because the `[[bin]]` target is commented out in `Cargo.toml` (lines 22-24). Any attempt to re-enable the CLI must fix this call.
- **Lifetime aliasing:** the shared `'a` in `push_frame` (`mod.rs:19`) prevents holding a `VadFrame` across the next call; the consumer respects this by consuming the result immediately (`recorder.rs:445-448`).

---

## 6. State & persistence touched

- **Model file (on disk):** `resources/models/silero_vad_v4.onnx`. Bundled as a Tauri *resource* and resolved at runtime via `app_handle.path().resolve("resources/models/silero_vad_v4.onnx", BaseDirectory::Resource)` — `audio.rs:269-280`. It must be downloaded into `src-tauri/resources/models/` during dev setup (`curl ... https://blob.handy.computer/silero_vad_v4.onnx`, per `AGENTS.md`). Source URL referenced again as a literal in `audio.rs:273`.
- **In-memory state only:** all runtime VAD state (smoothing counters, frame ring buffer, Silero recurrent state, ONNX session) lives in RAM inside the `Arc<Mutex<Box<dyn VoiceActivityDetector>>>` held by the recorder. **Nothing is persisted** to the settings store, SQLite history DB, or any file.
- **No settings binding:** the tuning parameters (`0.3`, `15/15/2`) are **hard-coded** at the construction site in `audio.rs:124-126`. They are *not* in the Zustand/`tauri-plugin-store` settings system and are not exposed to the frontend. (Contrast with mic device / model selection, which are persisted settings.) Changing VAD sensitivity today requires a code change + rebuild.

---

## 7. Platform-specific branches

- **No `cfg` gates inside the VAD subsystem.** `mod.rs`, `silero.rs`, and `smoothed.rs` contain zero `#[cfg(...)]` attributes; the code is identical on macOS, Windows, Linux, iOS, and Android.
- **Platform variance is delegated to `vad-rs`/ONNX backend selection in `Cargo.toml`:** `vad-rs = { git = "https://github.com/cjpais/vad-rs", default-features = false }` (`Cargo.toml:58`). With default features off, the ONNX runtime backend (e.g. CPU `ort`) is whatever `vad-rs` resolves per target. The VAD is CPU-only here; the GPU acceleration mentioned in `AGENTS.md` (Metal/Vulkan/DirectML) applies to the **transcription** crate (`transcribe-rs`, `Cargo.toml:72,91,103,108`), **not** to VAD.
- **No iOS/Android VAD code path.** The only mobile-aware line in the backend is `#[cfg_attr(mobile, tauri::mobile_entry_point)]` on `run()` in `lib.rs:316`. There is no mobile audio capture or mobile-specific VAD. (See §8/§9 for the implications.)
- **Sample rate is universal:** `WHISPER_SAMPLE_RATE = 16000` (`constants.rs`) is assumed on every platform; per-device rates are normalized by `FrameResampler` before reaching the VAD, so the VAD never sees platform mic differences.

---

## 8. PLAUD relevance — concrete extension points

A Plaud-style product (capture system/call audio, multi-speaker conversations, diarization, long-form recording, AI summaries, cloud/local sync, iPhone) maps onto this subsystem as follows. The trait `VoiceActivityDetector` and the `VadFrame` enum are the cleanest seams; almost everything can be added by *wrapping* rather than rewriting.

1. **Timestamped, bounded segments (the #1 prerequisite for everything Plaud).** Today `VadFrame::Speech` carries only `&[f32]` and the recorder concatenates everything into one blob. Introduce a richer output, e.g. `VadFrame::Speech { samples, start_ms, end_ms, segment_id }`, by:
   - extending the enum in `mod.rs:3-8`,
   - having `SmoothedVad::push_frame` (`smoothed.rs:41`) stamp segment boundaries when `in_speech` flips on (onset, `smoothed.rs:57`) and off (release, `smoothed.rs:85`) using a running frame counter (each frame = 30 ms),
   - and changing the recorder accumulator (`recorder.rs:445-446`) to emit a list of `(start_ms, end_ms, Vec<f32>)` utterances instead of one buffer. This single change unlocks per-utterance summaries, transcript alignment, and seek/replay.

2. **Speaker diarization.** Diarization is naturally a *second decorator* in the same `Box<dyn VoiceActivityDetector>` chain or a parallel consumer. Concretely: wrap the production stack built in `create_audio_recorder` (`audio.rs:124-126`) so that each confirmed speech segment from `SmoothedVad` is additionally passed to a speaker-embedding model (e.g. an ECAPA/pyannote ONNX model loaded the same way `SileroVad` loads Silero in `silero.rs:25`), and attach a `speaker_id` to the new timestamped `VadFrame`. The 450 ms prefill buffer (`frame_buffer`, `smoothed.rs:11`) is exactly the kind of per-segment context an embedding model wants. Diarization needs the segment boundaries from extension #1 to exist first.

3. **System / call / loopback audio capture.** The VAD is source-agnostic — it only requires 480-sample/16 kHz mono frames. To capture system or call audio, change the *producer* (the cpal stream in `recorder.rs:65-120`) to a loopback/aggregate device (macOS CoreAudio aggregate or ScreenCaptureKit, Windows WASAPI loopback). The VAD and smoother need no changes. For two-party calls, run **two** `FrameResampler`+`SmoothedVad` pipelines (mic + system) and tag each segment with a channel/speaker — again leaning on the timestamped-segment extension.

4. **Long-form recording (hours).** The smoother already bridges sub-450 ms pauses (`hangover`, `smoothed.rs:74-88`); for long recordings you want it to *split* on longer silences and stream each completed utterance out rather than buffer the whole session. The hook is the release transition `(true, false)` with `hangover_counter == 0` (`smoothed.rs:84-86`): emit a "segment complete" signal there. Also raise `hangover_frames`/add a configurable "max segment length" so meetings split into digestible chunks. Memory is currently unbounded (the recorder keeps `processed_samples` in RAM) — long-form needs the per-segment streaming from extension #1 to avoid OOM.

5. **AI summaries.** Summaries consume the transcript, which consumes the VAD-segmented audio. The leverage point is purely that the VAD must produce *speaker-attributed, timestamped utterances* (extensions #1-#2). With those, summary generation lives entirely downstream of `transcribe-rs` and needs nothing from this subsystem beyond clean boundaries.

6. **Configurable sensitivity / persisted VAD settings.** Move the hard-coded `0.3 / 15 / 15 / 2` (`audio.rs:124-126`) into the settings store so users can trade aggressiveness for completeness (a meeting recorder wants different tuning than a dictation tool). `SileroVad::new` already validates the threshold (`silero.rs:20-22`); expose `threshold`, `prefill_frames`, `hangover_frames`, `onset_frames` as settings and thread them through `create_audio_recorder`.

7. **Cloud/local sync.** Sync is orthogonal to VAD, but the *unit of sync* should be the timestamped segment from extension #1. Persist `(segment_id, start_ms, end_ms, speaker_id, audio_blob, transcript)` to SQLite (the app already uses `rusqlite`, `Cargo.toml:68`) at the recorder accumulation point (`recorder.rs:445`) and sync those rows.

8. **Mobile (iPhone).** The VAD code is already platform-agnostic and would run under `tauri::mobile_entry_point` (`lib.rs:316`). The blockers are *not* in this subsystem: (a) the cpal-based capture in `recorder.rs` is desktop-oriented and would need an iOS AVAudioEngine/AudioSession producer feeding the same 480-sample frames; (b) the model file resolution (`audio.rs:269-280`) and the CPU ONNX backend (`Cargo.toml:58`) must be validated for iOS. If those are solved, `SileroVad` + `SmoothedVad` can run unchanged on-device for private, offline VAD.

**Smallest high-leverage change:** add timestamps/segment IDs to `VadFrame` (`mod.rs:3-8`) and stamp them in `SmoothedVad` at the onset/release transitions (`smoothed.rs:57, 85`). Almost every Plaud feature depends on that one seam.

---

## 9. Gaps vs a Plaud-style product

- **No timestamps and no segment boundaries.** `VadFrame::Speech` is just `&[f32]` (`mod.rs:4`); the recorder concatenates everything into one buffer (`recorder.rs:445-446`). There is no notion of "utterance N from 00:12.3 to 00:18.7." This is the foundational gap.
- **No speaker awareness / diarization.** Nothing in the subsystem models *who* is speaking; it is single-channel speech-vs-noise only.
- **Single-stream, mic-only by construction.** The pipeline assumes one downmixed mono input. No multi-channel, no system/loopback, no per-speaker channels.
- **Tuned for short push-to-talk, not meetings.** `onset=2` (60 ms) and `hangover=15` (450 ms) with permissive `threshold=0.3` (`audio.rs:124-126`) suit quick dictation; long meetings need longer onset debouncing, max-segment limits, and silence-based splitting that don't exist.
- **No persistence of VAD output.** Filtered audio lives only in RAM in the recorder; nothing is written to SQLite or files at the VAD stage (§6). No replay, no seek, no resumable long-form capture.
- **Hard-coded, non-user-configurable parameters.** Threshold and smoothing windows are compile-time constants at the call site (`audio.rs:124-126`), absent from the settings/store system.
- **Fail-open with no telemetry.** VAD errors silently degrade to pass-through (`recorder.rs:445`, `unwrap_or(Speech)`), with no logging, metric, or user signal — bad for a product that promises reliable capture.
- **Inner recurrent state never reset.** `SileroVad` doesn't override `reset` (`silero.rs`), so the Silero LSTM carries state across recordings; only the smoother resets (`smoothed.rs:98-104`).
- **CPU-only VAD.** No GPU/NPU acceleration for VAD (`Cargo.toml:58`); fine for one stream, but multi-stream call capture would multiply CPU cost.
- **No mobile capture path.** The model and detector are mobile-portable, but there is no iOS/Android audio producer or model-loading validation (§7), so on-device iPhone VAD is not actually wired up.
- **No VAD-emitted events.** The frontend has no insight into speech/silence state from the VAD (only the unrelated spectrum visualiser emits events), so there's no live "speaking now / silence" UI primitive to build a recorder UX on.
- **Dead/stale test CLI.** `bin/cli.rs:179` uses the obsolete 3-arg `SmoothedVad::new`; the standalone harness can't build until fixed (§5).
