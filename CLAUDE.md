# Plaude Local

Open-source, **local-first alternative to Plaud** (AI voice recorder / note-taker) for **macOS + iPhone**,
built on top of [Handy](https://github.com/cjpais/Handy) as scaffolding.

## Layout
- `handy/` — upstream Handy clone (Tauri 2.x: Rust backend in `src-tauri/`, React/TS frontend in `src/`). Our scaffolding; extend, don't treat as read-only vendor.
- `docs/HANDOFF.md` — **agent handoff / entry-point** (current state, build, what's done, next steps, gotchas, conventions). **Read this first.**
- `docs/CODEBASE.md` — extensive technical reference (architecture, what we built & how, file-by-file map, data model).
- `docs/HANDOFF-FASE2.md` — line-cited forensic detail of the Fase 2 diarization work + the sessions-UI plan.
- `README.md` — project landing page / index.
- `docs/handy-architecture/` — forensic architecture docs of Handy + the Plaud gap analysis & roadmap.
- `.claude/ponytail/` — project-local Ponytail install (see its README). Not global.
- `.nwave/` — nWave methodology config (used for backend/architecture work).
- **Our code lives inside `handy/` (we extend the fork in place):** `handy/src-tauri/src/managers/session.rs` (long-form sessions), `.../audio_toolkit/audio/system_audio.rs` (CoreAudio system-audio tap), `.../audio_toolkit/audio/recorder.rs` (`with_chunk_sink` tap), `.../commands/session.rs`.

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

## Status (as of 2026-06-22)
- **Fase 0 (mic sessions) + Fase 1 (macOS system-audio capture) + Fase 2 (local speaker diarization)** are **built and validated live** — 1/2/3-speaker recordings each separated correctly, silence handled, the two ONNX runtimes (diarizer + ASR) coexist with zero crashes. Build green; **79 unit tests pass**. Diarization models are **bundled** (`handy/src-tauri/resources/models/diarization/`) and auto-installed on first run → offline-ready clone.
- **Next (for the incoming agent):** the **Sessions UI** (start/stop button + Mic/System selector + live indicator) to replace the CLI flags — the top remaining gap. Then the diarization download button in that view. See `docs/HANDOFF.md` §3 and `docs/HANDOFF-FASE2.md` §7.
- **Now a git repository:** pushed to `github.com/uppifyagency/plaude-local` (private), branch `main`. The upstream `handy/.git` was flattened into this single repo. Commit/push only when asked.
- iPhone target remains an open decision — Handy has **no iOS support** today (recommended path: iPhone-as-capture + Mac-as-brain; needs full Xcode, deferred).
