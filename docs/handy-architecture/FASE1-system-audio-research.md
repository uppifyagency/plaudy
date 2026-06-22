# FASE 1 — macOS System / Loopback Audio Capture

Lead-architect decision record for the FASE 1 walking skeleton: capture what is playing
out of the speakers (the far side of a Zoom/Meet/FaceTime call, a YouTube video) and feed
it into the existing Fase 0 session pipeline (`managers/session.rs` →
`audio_toolkit/audio/recorder.rs` `run_consumer` → WAV + transcript).

Target host: macOS 26 (Tahoe), Apple Silicon (M1 Pro), **Command Line Tools only** (no full
Xcode), Rust 1.95, standalone `swiftc` via CLT. Dev build is ad-hoc signed.

---

## 1. TL;DR — Recommendation

**Use the CoreAudio Process Tap API (`AudioHardwareCreateProcessTap` + `CATapDescription` +
a private aggregate device + IOProc), driven from pure Rust via the `objc2-core-audio` crate.**
No Swift bridge, no `build.rs` change for the demo.

Three reasons it wins for a CLT-only macOS 26 dev build:

1. **Right permission, no purple banner.** The tap is gated by the dedicated *Audio Recording*
   TCC permission (`NSAudioCaptureUsageDescription`), **not** Screen Recording. For a Plaud-style
   call recorder this is the decisive product fact: no scary "Screen Recording" grant and no
   persistent purple "screen is being recorded" Control Center indicator that ScreenCaptureKit
   forces even for audio-only capture.
2. **No video plane, no Swift step, builds on CLT.** The full tap API is present in the installed
   CLT SDK (`AudioHardwareTapping.h`, `CATapDescription.h`, `API_AVAILABLE(macos(14.2))`) and
   `objc2-core-audio 0.3.2` exposes every needed symbol as callable Rust FFI. So Fase 1 needs
   **no new `build.rs` swiftc step** and **no dummy video output** (ScreenCaptureKit refuses an
   audio-only stream and you must register a tiny video plane to work around it). This is the
   YAGNI/Ponytail-aligned path: fewer moving parts than the `apple_intelligence.swift` C-FFI
   precedent.
3. **1:1 fit with the existing seam.** The tap delivers **Float32 PCM at the output device's
   native rate** (typically 48 kHz). That maps verbatim onto the producer-agnostic
   `run_consumer(in_sample_rate=48000, …)` → `FrameResampler(48000 → 16000, 30 ms)` path the
   mic already uses. No new DSP, no integer→float conversion. `session.rs`'s
   chunk_sink → PCM-to-disk → WAV → transcribe tail is reused **unchanged**.

> Honest caveat carried forward: the one shared friction with every approach here is
> ad-hoc-signing TCC churn (the grant can reset on each rebuild). Mitigation is a stable
> self-signed / Apple Development cert for the dev loop — `codesign` ships with CLT. See §5.

---

## 2. Approach comparison

| Approach | Viability (macOS 26 / CLT) | Permission cost | Code complexity | Prod-signing cost |
|---|---|---|---|---|
| **CoreAudio Process Tap, pure Rust (`objc2-core-audio`)** ★ chosen | Works (API in CLT SDK, `objc2-core-audio 0.3.2` exposes all symbols; macOS 14.2+, we're on 26.4.1) | **Audio Recording** TCC only; one new Info.plist key `NSAudioCaptureUsageDescription`; **no** Screen Recording, **no** purple banner | ~150–250 lines unsafe Rust (tap desc + aggregate device + IOProc + format read); refactor `run_consumer` to a non-cpal feeder | Developer ID + notarization for distribution; **no special entitlement**; keep app **un-sandboxed** (as today) |
| **CoreAudio Process Tap via Swift bridge** | Works (same API; mirrors `apple_intelligence.swift` build.rs pattern, verified CLT-only) | Same as above (Audio Recording TCC, no banner) | ~120 lines Swift + C-FFI header + `build.rs` step + Rust shim; more files but less unsafe Rust | Same as above |
| **ScreenCaptureKit, pure Rust (`screencapturekit` crate v8)** | Works-with-caveats (fast-moving crate, API churn across majors; must keep a tiny video plane) | **Screen & System Audio Recording** TCC; `NSScreenCaptureUsageDescription`; **purple banner shown** | Drops into the seam cleanly, but new crate + objc2/CF/Metal transitive deps; audio-only video-plane workaround | Developer ID + notarization; no entitlement (TCC-only) |
| **ScreenCaptureKit via Swift bridge** | Works-with-caveats (compile+link **proven on this machine** CLT-only; live capture unverified) | Same SCK TCC + purple banner | ~120 lines async/delegate Swift + C-FFI + build.rs; queue/stream lifetime, deinterleaved copy trap | Developer ID + notarization |

Why not ScreenCaptureKit: it is Apple's first-party loopback path and would work, but for a
**call recorder** the Screen-Recording grant + permanent purple indicator is a real UX tax, and
the audio-only stream requires a dummy video plane. The tap is lighter, uses the correct
permission, and has a pure-Rust path. (SCK remains the documented **fallback** — see §5.)

Why pure Rust over the Swift bridge for the tap: the Swift bridge is a valid, repo-precedented
fallback, but now that `objc2-core-audio 0.3.2` is confirmed to expose
`AudioHardwareCreateProcessTap`, `CATapDescription`, `AudioHardwareCreateAggregateDevice`,
`AudioDeviceCreateIOProcIDWithBlock`, `AudioDeviceStart`, and `kAudioTapPropertyFormat`, the
pure-Rust route avoids a new `build.rs` swiftc stage and an extra language boundary. Ponytail:
fewer files, one toolchain. If `CATapDescription` initializer ergonomics in `objc2` become a
wall, fall back to the Swift bridge (§5) — the seam below is identical either way.

---

## 3. Thin-slice definition (smallest end-to-end demo)

**Goal:** Start a session that records **system audio** instead of the mic, and produce the same
artifacts Fase 0 already produces: a `session-*.wav` (mono 16 kHz) + a history row with a
transcript. Demoable by playing a YouTube clip / taking a call, toggling a session, stopping, and
seeing the WAV + transcript in history.

**In scope (the slice):**

- A new **system-audio producer** (`SystemAudioRecorder`) that:
  1. builds a **mono global** `CATapDescription` (`initMonoGlobalTapButExcludeProcesses: []`,
     `privateTap = true`, `muteBehavior = unmuted` so the user still hears the call);
  2. creates the tap + a private aggregate device, reads `kAudioTapPropertyFormat` for the real
     rate, starts an IOProc;
  3. inside the IOProc copies buffer 0 (mono f32) into a `Vec<f32>` and `send`s it to an
     `mpsc::Sender<AudioChunk>` (non-blocking, allocation-light — same discipline as the cpal
     callback);
  4. spawns the **existing** `run_consumer(native_rate, vad=None, sample_rx, cmd_rx, level_cb,
     chunk_sink, stop_flag)` worker — reused verbatim;
  5. exposes the same `open / start / stop / close` surface as `AudioRecorder` and accepts
     `with_chunk_sink(tx)`.
- **`session.rs` reuse:** `SessionManager::start` picks `Source::SystemAudio`, constructs the
  `SystemAudioRecorder` with the **same** `with_chunk_sink(tx)` it already hands `AudioRecorder`,
  and the **entire** PCM-writer → WAV → transcribe → history tail (lines ~92–148, 209–245) is
  **untouched**. The system path differs from the mic path only in *which struct fills the sink*.
- **One Info.plist key** added: `NSAudioCaptureUsageDescription`.
- **First-run permission UX:** the OS prompt fires lazily on first `AudioDeviceStart`; surface the
  same `PermissionDenied` handling shape `recorder.rs` already has (`is_microphone_access_denied`)
  and, on denial, deep-link the user to *Privacy & Security → Screen & System Audio Recording*.

**Deferred (explicitly NOT in the slice):**

- **Mic + system two-track mux / mixdown** (drift compensation, aligned tracks). Capture them as
  separate sessions for now; mixing in one aggregate clock invites drift.
- **Source-selector UI** (mic vs system toggle in settings + bindings). The slice can hard-select
  system audio behind a flag / CLI for the demo; a polished toggle is Fase 1.1.
- **Per-app (per-PID) tapping** (tap only Zoom/Meet via
  `kAudioHardwarePropertyTranslatePIDToProcessObject`). Global mono tap is enough to prove the
  capability; per-PID is a later refinement.
- **Live level/visualiser parity** for the system source (the `level_cb` still works through
  `run_consumer`, but tuning the spectrum window for loopback is not load-bearing for the slice).
- **Stereo / multi-track preservation** — the slice mixes to mono for Whisper.

---

## 4. File-by-file implementation plan

### 4a. Refactor: make the consumer feedable by a non-cpal producer (load-bearing)

`run_consumer` is already documented as producer-agnostic, **but** the `AudioChunk` and `Cmd`
enums are private to `recorder.rs` and the only producer today is the cpal stream built inside
`AudioRecorder::open` / `build_stream`. The tap producer must push the **same `AudioChunk`** into
a `run_consumer` instance.

- **Touch `handy/src-tauri/src/audio_toolkit/audio/recorder.rs`:**
  - Make `AudioChunk` and `Cmd` `pub(crate)` (or move both + `run_consumer` into a small sibling
    module `consumer.rs` and re-export). Keep `run_consumer`'s signature exactly as-is — it
    already takes `in_sample_rate`, `sample_rx: Receiver<AudioChunk>`, `cmd_rx`, `level_cb`,
    `chunk_sink`, `stop_flag`. **No logic change** to `run_consumer`, `FrameResampler`, VAD, or the
    `chunk_sink` drain. This is the whole point of the Fase 0 seam.
  - No change to `build_stream` / `get_preferred_config` / the cpal path.

### 4b. New file: the system-audio producer (pure Rust)

- **New `handy/src-tauri/src/audio_toolkit/audio/system_audio.rs`:**
  - `pub struct SystemAudioRecorder` mirroring `AudioRecorder`'s public surface:
    `new()`, `with_chunk_sink(sink)`, `with_vad(_)` (accepted, typically `None` for loopback),
    `with_level_callback(_)`, `open()`, `start()`, `stop() -> Vec<f32>`, `close()`.
  - `open()`:
    1. Build `CATapDescription` via `objc2-core-audio` + `objc2-foundation`
       (`initMonoGlobalTapButExcludeProcesses:` with empty `NSArray`, set `privateTap`,
       `muteBehavior` = unmuted).
    2. `AudioHardwareCreateProcessTap(&desc, &mut tap_id)`.
    3. Build aggregate-device CFDictionary (`objc2-core-foundation`): `kAudioAggregateDeviceIsPrivateKey = true`,
       `kAudioAggregateDeviceTapListKey = [{ kAudioSubTapUIDKey: tap.UUID, kAudioSubTapDriftCompensationKey: true }]`;
       `AudioHardwareCreateAggregateDevice(dict, &mut agg_id)`.
    4. **Read the real format**: `AudioObjectGetPropertyData` for `kAudioTapPropertyFormat` →
       `AudioStreamBasicDescription`. Capture `mSampleRate` (do **not** hardcode 48000) and assert
       `mFormatFlags` are float; record `mChannelsPerFrame`.
    5. `AudioDeviceCreateIOProcIDWithBlock(&mut proc_id, agg_id, queue, block)` where `block`
       (via `block2`) reads `inInputData: *const AudioBufferList`, takes `mBuffers[0]`
       (mono mixdown → one f32 buffer of `mDataByteSize/4` samples), copies into `Vec<f32>`, and
       does `let _ = sample_tx.send(AudioChunk::Samples(v));`. On stop, send `AudioChunk::EndOfStream`.
    6. `AudioDeviceStart(agg_id, proc_id)`.
    7. Spawn the worker thread that calls the **reused** `run_consumer(asbd.mSampleRate as u32,
       vad, sample_rx, cmd_rx, level_cb, chunk_sink, stop_flag)`.
  - `stop()` / `close()`: set stop flag → emit `EndOfStream` → `AudioDeviceStop`,
    `AudioDeviceDestroyIOProcID`, `AudioHardwareDestroyAggregateDevice`,
    `AudioHardwareDestroyProcessTap` **in that order** (leak-safe; a leftover private aggregate
    device on crash is the main hygiene risk — destroy on every exit path).
  - Guard the whole module behind `#[cfg(all(target_os = "macos", target_arch = "aarch64"))]`.
  - **Resample mapping (delivered → 16 kHz mono f32):** tap delivers `f32`, **planar/non-interleaved**,
    1 channel (mono tap), at `mSampleRate` (e.g. 48000). Producer copies channel 0 straight through
    → `run_consumer`'s `FrameResampler::new(mSampleRate, 16000, 30 ms)` does the downsample, exactly
    as the mic path. If a stereo tap is ever used, average L+R per frame (the same fold
    `build_stream` already does for multi-channel cpal). **Never** let CoreAudio's own SRC resample
    (it drops f32→16-bit); keep f32 native and resample in Rust with the existing rubato-backed
    `FrameResampler`.

- **Touch `handy/src-tauri/src/audio_toolkit/audio/mod.rs`** (and `audio_toolkit/mod.rs` if it
  re-exports): add `mod system_audio; pub use system_audio::SystemAudioRecorder;`.

- **Touch `handy/src-tauri/Cargo.toml`** — add, gated to macOS aarch64:
  ```toml
  [target.'cfg(all(target_os = "macos", target_arch = "aarch64"))'.dependencies]
  objc2-core-audio = "0.3"
  objc2-core-audio-types = "0.3"   # AudioStreamBasicDescription / AudioBufferList
  objc2-core-foundation = "0.3"    # CFDictionary for the aggregate device
  objc2-foundation = "0.3"         # NSArray / NSNumber / NSUUID
  objc2 = "0.6"                    # msg_send for the NS_REFINED_FOR_SWIFT initializers
  block2 = "0.6"                   # IOProc block
  ```
  Pin exact patch versions in `Cargo.lock` and **commit the lock** for the demo (these are pre-1.0;
  guard against churn). Keep the integration behind a thin adapter so an upgrade touches one file.

### 4c. Wire the source into the session manager

- **Touch `handy/src-tauri/src/managers/session.rs`:**
  - Add a `Source { Mic, SystemAudio }` selector (default `Mic`; read from a settings flag or a CLI
    override for the demo).
  - In `start()`: keep the `mpsc::channel::<Vec<f32>>()` + `spawn_pcm_writer` exactly as-is. Branch
    only on which recorder to build:
    ```text
    Source::Mic         → AudioRecorder::new().with_chunk_sink(tx); open(None); start();
    Source::SystemAudio → SystemAudioRecorder::new().with_chunk_sink(tx); open(); start();
    ```
    Store the active recorder behind a small enum or `Box<dyn>` so `stop()/close()` dispatch
    uniformly. **Everything after the sink** (PCM writer, finalize, WAV, transcribe, history row,
    `recover_interrupted`) is unchanged.

### 4d. Permissions / Info.plist / entitlements

- **Touch `handy/src-tauri/Info.plist`** — add (keep the existing mic key):
  ```xml
  <key>NSAudioCaptureUsageDescription</key>
  <string>Record system audio so Plaude can transcribe the other side of a call.</string>
  ```
  This key is intentionally absent from Xcode's dropdown — type it manually. Mic key stays for the
  mic path; `NSScreenCaptureUsageDescription` is **not** needed (that's the SCK path).
- **Entitlements:** none required for the tap — it is purely TCC-gated. Leave `Entitlements.plist`
  as-is (`device.microphone` / `device.audio-input` are harmless). **Do not enable App Sandbox**
  (taps are heavily restricted under sandbox; Handy is un-sandboxed today — keep it that way).
  `hardenedRuntime = true` in `tauri.conf.json` is fine and does not block the tap.
- **Permission UX:** no public API to pre-check/pre-prompt. The OS prompt fires on first
  `AudioDeviceStart`. On a denied/late grant, capture returns silence — detect and surface a
  "grant System Audio Recording in Privacy & Security" message (reuse the `PermissionDenied`
  error-kind shape from `recorder.rs`).

### 4e. (Fallback only) Swift-bridge variant — files if pure Rust hits a wall

If `objc2` `CATapDescription` ergonomics block progress, switch to the repo-precedented Swift bridge
(no seam change). Files: `swift/system_audio_tap.swift` (the 7-step tap flow + an `@_cdecl`
`start_system_tap(rate_out, cb, ctx)` / `stop_system_tap(handle)`), `swift/system_audio_bridge.h`
(C-FFI), a `build_system_audio_bridge()` cloned from `build_apple_intelligence_bridge()` in
`build.rs` (same `swiftc -parse-as-library -import-objc-header … -c … -o`, `libtool -static`, then
`cargo:rustc-link-lib=static=system_audio` + `=framework=CoreAudio` + `=framework=AVFoundation`),
and the Rust shim in `system_audio.rs`:

```c
// system_audio_bridge.h
typedef void (*SystemAudioCallback)(const float* samples, size_t frame_count, void* ctx);
int32_t system_audio_start(SystemAudioCallback cb, void* ctx);   // 1=ok, 0/neg=err
void    system_audio_stop(void);
```

```rust
// Rust trampoline — ctx is Box::into_raw(Box::new(Sender<Vec<f32>>)), freed in stop()
extern "C" fn on_frame(ptr: *const f32, n: usize, ctx: *mut c_void) {
    let tx = unsafe { &*(ctx as *const std::sync::mpsc::Sender<AudioChunk>) };
    let slice = unsafe { std::slice::from_raw_parts(ptr, n) }; // copy before returning
    let _ = tx.send(AudioChunk::Samples(slice.to_vec()));
}
```

The downstream seam (`run_consumer` → `FrameResampler` → `chunk_sink` → `session.rs`) is identical.

---

## 5. Risks, "what needs full Xcode later", and fallback

### Risks (and mitigations)

- **Ad-hoc-signing TCC churn (biggest dev-loop risk).** TCC binds the Audio-Recording grant to the
  code-signing identity; ad-hoc signatures change every rebuild, so macOS may re-prompt or lose the
  grant each `tauri dev`. *Mitigation:* sign dev builds with a stable self-signed / "Apple
  Development" identity (`codesign -s <stable-identity>` — ships with CLT), keep a stable bundle id
  and install path, or accept occasional re-granting. This is a signing artifact, **not** a crate
  bug — document it so it isn't mis-debugged.
- **Don't hardcode the format.** Read `kAudioTapPropertyFormat` at runtime; Bluetooth/AirPlay/DAC
  outputs report 44.1/24/16 kHz and ASBD flags can differ. Assert float + channel/buffer counts
  before downmixing; pass the real rate to `FrameResampler`.
- **Realtime IOProc discipline.** The block runs on a realtime thread — only `memcpy` + a
  non-blocking `mpsc` send (or a `ringbuf::HeapRb<f32>` drained off-thread). No locks, no
  allocation-heavy work, exactly like the cpal callback.
- **Aggregate-device lifecycle.** Create/start/stop/destroy ordering is fiddly and leaks a private
  aggregate device if not torn down. Destroy tap + aggregate on **every** exit path including panic.
- **`objc2-core-audio` is pre-1.0 (0.3.2).** API churn risk. Pin exact versions, commit
  `Cargo.lock`, and keep all FFI behind the single `system_audio.rs` adapter.
- **muteBehavior.** Use the default **un-muted** tap so the user keeps hearing the call while you
  record; a misconfigured tap can silence playback.
- **DRM / app routing.** Validate against a real Zoom/Meet/FaceTime call, not just YouTube — some
  protected audio paths can be muted by macOS regardless of capture method.

### What needs full Xcode / Developer ID / notarization later

- **Dev demo runs now:** CLT-only build, ad-hoc signing, grant the prompt once → capture works. The
  full tap API is in the installed CLT SDK; no full Xcode for Fase 1.
- **Production distribution** (outside the App Store) needs a **Developer ID Application** cert +
  **hardened runtime** + **notarization** (`xcrun notarytool`) so the TCC grant persists across
  updates and Gatekeeper passes. All available via **CLT + a paid Apple Developer account** —
  **full Xcode.app is not required** for the macOS build/sign/notarize path.
- **Full Xcode becomes mandatory only** for the separate **iPhone** target (a later phase) and for
  Apple-Intelligence `@Generable` (unrelated to this feature).

### Fallback if the primary hits a wall

1. **Tap, Swift bridge instead of pure Rust** (§4e) — same API, same seam, mirrors the proven
   `apple_intelligence.swift` build.rs pattern; use if `objc2` `CATapDescription` ergonomics block.
2. **ScreenCaptureKit** (pure-Rust `screencapturekit` v8 *or* Swift bridge — compile+link proven
   CLT-only on this machine) — use only if the tap API regresses on the macOS 26 target. Accept the
   Screen-Recording TCC grant + purple indicator + dummy video plane as the cost. Both SCK variants
   drop into the **same** `chunk_sink`/`run_consumer` seam, so switching is a producer swap, not a
   pipeline rewrite.
