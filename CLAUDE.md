# Plaude Local

Open-source, **local-first alternative to Plaud** (AI voice recorder / note-taker) for **macOS + iPhone**,
built on top of [Handy](https://github.com/cjpais/Handy) as scaffolding.

## Layout
- `handy/` — upstream Handy clone (Tauri 2.x: Rust backend in `src-tauri/`, React/TS frontend in `src/`). Our scaffolding; extend, don't treat as read-only vendor.
- `docs/HANDOFF.md` — **agent handoff / entry-point** (current state, build, what's done, next steps, gotchas, conventions). **Read this first.**
- `docs/CODEBASE.md` — extensive technical reference (architecture, what we built & how, file-by-file map, data model).
- `docs/HANDOFF-FASE2.md` — line-cited forensic detail of the Fase 2 diarization work + the sessions-UI plan.
- `docs/HANDOFF-AUTOCAPTURE.md` — line-cited forensics of the 2026-07-05 session: per-process auto-capture trigger unshelved (what/how/evidence/remaining) + the resolved MCP-as-AI-path decision.
- `README.md` — project landing page / index.
- `docs/handy-architecture/` — forensic architecture docs of Handy + the Plaud gap analysis & roadmap.
- `docs/RIFFADO-CODEWIKI.md` — teardown of the AGPL `riffado` app (a category peer); `docs/DECISIONS.md` — the Keep/Spec/Drop verdict distilled from it (what to adopt vs drop, with the AGPL clean-room boundary).
- `.claude/ponytail/` — project-local Ponytail install (see its README). Not global.
- `.nwave/` — nWave methodology config (used for backend/architecture work).
- **Our code lives inside `handy/` (we extend the fork in place):** `handy/src-tauri/src/managers/session.rs` (long-form + dual-stream meeting sessions: `Track`/`mix_tracks`/`finalize_session`), `.../managers/diarization.rs` (`merge_segments`/`label_segments`), `.../audio_toolkit/audio/system_audio.rs` (CoreAudio system-audio tap), `.../audio_toolkit/audio/recorder.rs` (`with_chunk_sink` tap), `.../commands/session.rs`, `.../tray.rs` (the menu-bar "graffetta").
- **`handy/mcp/`** — dependency-free local MCP server (Bun + `bun:sqlite`, read-only over `history.db`) that lets Claude summarize/search recordings; registered in repo-root `.mcp.json`.

## Conventions
- **Ponytail is active in this folder** ("lazy senior dev": YAGNI → stdlib → native → one line → minimum). Toggle with `/ponytail lite|full|ultra|off`. Never cut validation, error handling, security, accessibility.
- Use **nWave** waves/agents for backend & architectural work.
- Prefer extending Handy's existing managers/pipeline over net-new subsystems where possible — cite the real `handy/src-tauri/...` file when proposing changes.

## Build & run (toolchain installed — Apple Silicon, CLT only, no Homebrew)
Rust 1.95, Bun 1.3.14, standalone CMake 4.x are installed but **not on the non-interactive PATH**. Always prefix:
```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # CMake 4 dropped pre-3.5 policy compat (whisper.cpp won't configure)
export HANDY_FORCE_AI_STUB=1              # build the Apple Intelligence stub without full Xcode
cd handy && bun tauri dev                 # or: cd handy/src-tauri && cargo build / cargo test --lib session
```
- `HANDY_FORCE_AI_STUB=1` is an escape hatch added to `handy/src-tauri/build.rs`: the CLT SDK ships `FoundationModels.framework` but not the `@Generable` macro plugin. Drop it once **full Xcode** is installed (also needed later for the iPhone target).
- App data: `~/Library/Application Support/com.pais.handy/` (`history.db` + `recordings/`).
- Long-form sessions: `./handy --toggle-session` (mic) · `./handy --toggle-system-session` (system audio). macOS Intel would need ONNX Runtime via Homebrew (see `handy/BUILD.md`); we're on Apple Silicon.

## Status (as of 2026-06-23)
- **Fase 0 (mic sessions) + Fase 1 (macOS system-audio capture) + Fase 2 (local speaker diarization)** are **built and validated live** — 1/2/3-speaker recordings each separated correctly, silence handled, the two ONNX runtimes (diarizer + ASR) coexist with zero crashes. Diarization models are **bundled** (`handy/src-tauri/resources/models/diarization/`) and auto-installed on first run → offline-ready clone.
- **Sessions UI + per-row status (2026-06-23, live-validated):** start/stop + source selector + live indicator (`SessionsSettings.tsx`), driven by `start_session(source)` + a `SessionStateChanged` event (Mic → row 11, SystemAudio → row 12). A `status` column (`transcribing → done/failed`, migration #6) fills the silent gap after Stop.
- **"Graffetta" + meeting capture + local Claude MCP (2026-06-23, this session):**
  - **Menu-bar tray toggle** ("graffetta") → `SessionManager::toggle_sources([Mic, System])`; tray icon tracks state via a `SessionStateChanged` listener.
  - **Dual-stream meeting capture** (`session.rs`): a session = N `Track`s → `mix_tracks` → one playable WAV; `finalize_session` labels the mic "Me", diarizes system, `merge_segments` → one speaker-attributed transcript; **`drop_bleed`** removes the mic's speaker-echo of the system audio (so one person isn't split into Me+Speaker on laptop speakers). System audio is best-effort/self-healing. Triggers: tray · `start_meeting` cmd (redesigned hero UI) · `--toggle-meeting` flag.
  - **Local MCP server** `handy/mcp/` (Bun + `bun:sqlite`, dependency-free, read-only) + `.mcp.json` → Claude connects locally to `list_sessions`/`get_session`/`search_sessions`. **Verified live against the real `history.db`.**
  - **Self-healing** `HistoryManager::fail_stale_transcribing()` at startup; **92 Rust + 4 MCP tests green.** Dual meeting capture **live-validated with real speech** (mic "Me" + system "Speaker 1", merged, bleed de-duped) on 2026-06-23 (recipe in `docs/HANDOFF.md` §7).
- **History session-card result view (2026-06-23, this session, live-validated):** the Cronologia list now reads as a *result*, not a dump — each row is a session card (`HistorySettings.tsx`): source icon (meeting/mic/system/dictation, inferred from speaker labels), a **topic title** (transcript's opening words — a non-AI placeholder), `date · duration · source` meta, speaker chips, and a **collapsible** speaker timeline + player + actions. Closes the old "History-as-result polish" gap. A clean AI title/summary remains gated on the provider decision (§12).
- **Menu-bar "ear" + seamless auto-capture R&D (2026-06-23):** the tray icon now becomes an **ear** (`TrayIconState::Listening`, `resources/tray_listening.png` from the SF Symbol) whenever a session is recording — the honest "I'm listening" signal; dictation keeps the dot. **Auto-capture engine** built (`managers/auto_capture.rs`: unit-tested debounce brain + supervisor with probation/discard/cooldown; `SessionManager::cancel`/`system_audio_idle`/`system_audio_heard`).
- **Per-process auto-capture trigger (2026-07-05, this session, live-validated E2E):** the shelved system-audio trigger is **unshelved**. `output_sensor.rs` rewritten from the device-level `DeviceIsRunningSomewhere` read (stuck "running" once our tap ever opened → 17/17 false starts) to **per-process CoreAudio attribution**: `kAudioHardwarePropertyProcessObjectList` + `kAudioProcessPropertyIsRunningOutput` with **own PID excluded** — our tap can no longer wake ourselves (approach spotted dormant in Meetily's `system_detector.rs`, reimplemented on raw `objc2_core_audio`, zero new deps). Validated live: tap open + silent machine → no trigger; external `afplay` → auto-start ≈1.4s → probation confirmed → inter-sound gaps absorbed by the grace window → auto-finalized (`history.db` row 79, `done`). **102 Rust tests green** (4 new unit + 2 ignored live-acceptance: `cargo test --lib output_sensor -- --ignored`). Still **opt-in** (`auto_capture_enabled`=false) pending a real-meeting validation; the mic-VAD trigger idea is demoted to optional fallback. **§12 provider decision: resolved — MCP is the AI title/summary path** (client agents summarize on demand using the user's own subscription); no local LLM sidecar.
- **Next (for Gianni / incoming dev):** read `docs/HANDOFF-GIANNI.md`. Then: the **"Enable diarization" download button**; a **real-meeting validation** of auto-capture (then consider flipping the default); optional app-allowlist refinement of the trigger. See `docs/HANDOFF.md` §11.
- **Now a git repository:** pushed to `github.com/uppifyagency/plaude-local` (private), branch `main`. The upstream `handy/.git` was flattened into this single repo. Commit/push only when asked.
- iPhone target remains an open decision — Handy has **no iOS support** today (recommended path: iPhone-as-capture + Mac-as-brain; needs full Xcode, deferred).
