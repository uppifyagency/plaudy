# Handoff — Auto‑capture trigger unshelved (session 2026‑07‑05)

**For the next agent.** Line‑cited forensics of what this session did, how, with what evidence, and exactly what is left. Entry‑point briefing: [HANDOFF.md](HANDOFF.md) (kept current). Architecture: [CODEBASE.md §5b](CODEBASE.md). Project status header: [../CLAUDE.md](../CLAUDE.md).

_Snapshot: 2026‑07‑05. Branch `main`, working tree **not committed** (commit only when asked). Tests: **102 Rust (+2 ignored live) · 4 MCP**, all green._

---

## 1. What was done (three outcomes)

1. **The seamless auto‑capture system‑audio trigger — shelved since 2026‑06‑23 — is fixed, wired, and E2E‑validated live.** Root cause removed, not worked around.
2. **The §12 "AI provider" decision is RESOLVED (by Vlad):** the **local MCP server is the AI path** — the user's/client's own agents call `get_session`/`search_sessions` and produce summary/title **on the fly with the user's own subscription**. Explicitly **no local LLM sidecar**. Nothing to build, no keys, no encryption question.
3. **Docs brought current:** [HANDOFF.md](HANDOFF.md), [CODEBASE.md](CODEBASE.md) (new §5b + parameters), `CLAUDE.md` status, this file.

Context that led here: we studied **Meetily** (`github.com/Zackriya-Solutions/meetily`, MIT, category peer — full comparison delivered to Vlad in‑session). Two ideas were evaluated end‑to‑end; verdicts:
- **Per‑process audio attribution** (their dormant, never‑wired `frontend/src-tauri/src/audio/system_detector.rs`) → **adopted**, reimplemented clean on our stack.
- **`llama-helper` local‑LLM sidecar** (their BuiltInAI) → **rejected** by Vlad in favor of the MCP path.

## 2. The problem and the fix (precise)

**Problem (why the trigger was shelved):** the v1 sensor read the *device‑level* `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default output device. From **inside this app**, once our CoreAudio process tap had **ever** been opened, that flag reads "running" **forever** → the sensor false‑triggered on our own tap. Validated live back then: **17/17 idle auto‑starts were empty false starts**.

**Fix (root cause, not symptom):** attribute audio to **processes**, not the device. [output_sensor.rs](../handy/src-tauri/src/audio_toolkit/audio/output_sensor.rs) was rewritten:

| Piece | Where | What |
| --- | --- | --- |
| FFI snapshot | `list_process_output()` (line 76) | `kAudioHardwarePropertyProcessObjectList` on the system object (id 1) → per object `kAudioProcessPropertyPID` + `kAudioProcessPropertyIsRunningOutput` (both u32 reads via `get_u32`, line 55) |
| Pure decision | `any_external_output(procs, own_pid)` (line 40) | `procs.iter().any(|p| p.running_output && p.pid != own_pid)` — **own PID excluded: we cannot wake ourselves, by construction** |
| Public API | `external_output_active()` (line 123) | composes the two with `std::process::id()`; **false on any error** (safe default: never triggers) |

- Constants come from **`objc2_core_audio`** (already a dependency) — no hand‑rolled FourCCs, **zero new crates**. Same macOS 14.4+ floor as the tap. Plain property reads: no tap, no TCC permission, no recording indicator.
- Non‑macOS stub returns `false` ([audio/mod.rs](../handy/src-tauri/src/audio_toolkit/audio/mod.rs)).
- The old device‑level function was **deleted** (fewer elements; the lesson is recorded as HANDOFF §6.7).

**Consumer change** — one expression: [auto_capture.rs:252](../handy/src-tauri/src/managers/auto_capture.rs#L252) now calls `external_output_active()` for START. Everything else (decider, probation, cooldown, manual‑respect, RMS stop) was already built and unit‑tested on 2026‑06‑23 and is **untouched**.

**Deliberate asymmetry (do not "simplify" it away):** START = per‑process sensor; STOP = captured‑audio silence (`session.system_audio_idle() < 800ms`, line 250). A meeting app holds its output stream open even while nobody talks — "the app is outputting" can never detect the end of a call; captured silence can.

## 3. How it was done (method)

Outside‑in TDD, double‑loop (`/agile-technical-practices`):
- **Outer loop (acceptance) = the bug itself**, pinned as two `#[ignore]` live tests (lines 168, 189): `live_own_tap_open_does_not_trigger` (open our real tap on a silent machine → sensor must stay `false` — the 17/17 regression) and `live_external_afplay_triggers` (external `afplay` process playing → `true`).
- **Inner loop:** 4 pure unit tests on `any_external_output`, incl. `our_own_output_never_triggers`.
- Red→Green via Obvious Implementation (the core is one `any()`); `unsafe` confined to the FFI shell; probation kept as **defense in depth** even though the sensor no longer lies.

## 4. Evidence (re‑runnable)

| Claim | Evidence | Re‑verify |
| --- | --- | --- |
| Pure logic correct | 4 unit tests | `cargo test --lib output_sensor` |
| Regression dead | live test: tap open, 29 process objects enumerated, **all `running_output:false`**, sensor `false` | `cargo test --lib output_sensor -- --ignored --nocapture --test-threads=1` (quiet machine) |
| Positive path | live test: `afplay` → sensor `true` | same command |
| No collateral damage | **102 passed, 0 failed** full suite | `cargo test --lib` |
| **Full E2E** | app run 2026‑07‑05 ≈09:10: quiet → no trigger; `afplay` ≈8 s → log `session started (probation)` (+≈1.4 s) → `real audio confirmed` → gaps absorbed by `Trailing` → `speakers quiet → session finalized`; **`history.db` row 79**, status `done`, 2×172 800 samples (10.8 s @16 kHz), empty transcript (system *sound*, not speech — correct) | recipe in [HANDOFF.md §9](HANDOFF.md); log `~/Library/Logs/com.pais.handy/handy.log` |

E2E hygiene notes: the run temporarily set `auto_capture_enabled=true` and **restored it to `false`** (backup/restore of `settings_store.json`; kill the app **before** restoring — the store flushes on exit). Test row 79 was left in Cronologia; delete from the UI if unwanted.

## 5. Parameters (current values — single place: consts atop [auto_capture.rs](../handy/src-tauri/src/managers/auto_capture.rs), lines 153–163)

`POLL_INTERVAL` 250 ms · `START_AFTER` 1200 ms · `STOP_AFTER` 4 s · `PROBATION` 2000 ms · `COOLDOWN` 8 s · in‑session presence `system_audio_idle() < 800 ms` · gate `auto_capture_enabled` **false** (settings.rs — remember the two‑edit rule, HANDOFF §6.8). Rationale per value in [CODEBASE.md §5b](CODEBASE.md).

## 6. What remains (in priority order)

1. **Real‑meeting validation** (the only gap between "works" and "on by default"): one Zoom/Meet call with `auto_capture_enabled=true` — expect auto‑start ≤ ~1.5 s after the call's audio starts, probation confirmed, "Me"+"Speaker N" merged transcript, auto‑finalize ~4 s after hangup. Then bring Vlad the **flip‑the‑default** decision (HANDOFF §12).
2. **Optional app allowlist** (YAGNI until the validation says otherwise): `kAudioProcessPropertyBundleID` is one more `get_u32`‑style read in `output_sensor.rs`; would make the trigger semantic ("a meeting app is playing") instead of "anything is playing". Evaluate only if real‑world use shows spurious triggers from music/video apps — note Spotify/YouTube *will* trigger today by design.
3. **"Enable diarization" download button** (pre‑existing, unchanged, HANDOFF §11.3).
4. **Known ceilings, unchanged:** AEC (§11.2), signed `.app`/`.dmg` + iPhone (need full Xcode), mic‑VAD trigger now **demoted to optional fallback** (not needed as primary).
5. **Nothing to do for AI title/summary** beyond MCP already shipped — unless Vlad later wants agent‑produced titles *persisted*, which requires a deliberate tiny write surface (MCP is read‑only by contract, HANDOFF §7 — do not casually break that).

## 7. Traps for you specifically

- **Don't reintroduce the device‑level flag** anywhere in the trigger path (HANDOFF §6.7). Don't remove probation "because the sensor is reliable now" — it is the second net and it costs nothing.
- The two live tests are `#[ignore]` because they need real CoreAudio + a quiet machine; they are **part of the definition of done** for any sensor change — run them.
- The Meetily clone used for the study lives in this session's scratchpad (temporary); the repo is `github.com/Zackriya-Solutions/meetily` if you need to re‑consult it. We reimplemented ideas, **no code was copied** (their tree is MIT, but ours is a clean‑room on `objc2_core_audio` regardless).
- Everything is **uncommitted** — one coherent commit (sensor rewrite + supervisor wiring + docs) when Vlad asks. Suggested: `feat(auto-capture): per-process system-audio trigger (own PID excluded) — unshelves seamless capture`.

---

*Written 2026‑07‑05 by the session that unshelved the trigger. Companion forensics: [HANDOFF-FASE2.md](HANDOFF-FASE2.md) (diarization). Keep [HANDOFF.md](HANDOFF.md) as the single entry‑point.*
