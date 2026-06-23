# Plaude Local — Agent Handoff (authoritative briefing)

**For the next AI coding agent or developer. Read this first, top to bottom.** It is self‑contained: state, evidence, exact build/run, how to re‑verify everything, the traps that cost real time, the security posture, what's left, and the decisions only a human can make. Deep architecture lives in [CODEBASE.md](CODEBASE.md); line‑cited Fase 2 forensics in [HANDOFF-FASE2.md](HANDOFF-FASE2.md); the riffado teardown verdict in [DECISIONS.md](DECISIONS.md).

_Snapshot: 2026‑06‑23. Branch `main`. Working tree NOT committed (commit only when asked)._

---

## 0. Mission & posture (one screen)

**Mission:** Plaude Local = a **local‑first, offline, private** alternative to Plaud (AI voice recorder + "who said what") for macOS, built on the **Handy** fork (Tauri 2, Rust + React). Capture is **on‑device**; ASR + diarization run **locally** (ONNX); **nothing leaves the Mac**. Claude connects to your library through a **local MCP server**.

**Posture today:** the product thesis is **built and proven live**. One click (menu‑bar "graffetta") records a meeting — your **mic** + the Mac's **system audio** as two streams — and it lands as **one speaker‑attributed transcript** that **Claude can summarize/search locally**. Green across the board: **92 Rust unit tests · 4 MCP tests · `tsc` · ESLint**. A 36 MB optimized **release binary builds**.

**What's NOT done:** a signed/notarized `.app`/`.dmg` (needs full Xcode), true acoustic echo cancellation (only the transcript‑level bleed dup is handled), the iPhone target (needs Xcode), and visual UI polish of the History "result" view. See §10–§11.

---

## 1. First 15 minutes (orient fast)

1. **Read:** this file → [CODEBASE.md](CODEBASE.md) (architecture + file map) → skim [DECISIONS.md](DECISIONS.md) (what we adopt/drop from riffado).
2. **Build the backend + run tests** (proves your toolchain works):
   ```bash
   export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
   export CMAKE_POLICY_VERSION_MINIMUM=3.5 HANDY_FORCE_AI_STUB=1
   cd handy/src-tauri && cargo test --lib      # expect: 92 passed
   cd ../mcp && bun test                         # expect: 4 pass
   ```
3. **Run the app:** `cd handy && bun tauri dev` (leave it running; it regenerates `src/bindings.ts`). Keep an ASR model selected with `unload_timeout ≠ Immediately`.
4. **Smoke‑test the MCP** (what Claude sees), no app needed:
   ```bash
   printf '%s\n' \
     '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' \
     '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions","arguments":{"limit":5}}}' \
   | bun run handy/mcp/server.ts
   ```
5. **The mental model:** Handy's dictation path is `capture → VAD → transcribe → paste`. We added a **long‑form/meeting** path: `capture (faithful, un‑VAD’d) → stream to disk → (stop) → mix → diarize + transcribe → merge → speaker‑labelled History row → MCP exposes it`.

---

## 2. Exact build, run & test (this machine: Apple Silicon, CLT‑only, no Homebrew/Xcode)

**Every shell needs this prelude** — the toolchain is installed but not on the non‑interactive PATH:
```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
export CMAKE_POLICY_VERSION_MINIMUM=3.5   # standalone CMake 4.x rejects pre‑3.5 policy floors (whisper.cpp)
export HANDY_FORCE_AI_STUB=1              # CLT lacks the @Generable macro plugin → Apple Intelligence stub
```
| Task | Command |
| --- | --- |
| Full app (dev) | `cd handy && bun tauri dev` (regenerates `src/bindings.ts` at startup) |
| Backend tests | `cd handy/src-tauri && cargo test --lib` → **92 passed** |
| MCP tests | `cd handy/mcp && bun test` → **4 pass** |
| Type‑check (fast) | `cd handy/src-tauri && cargo check --lib` |
| Frontend type‑check | `cd handy && bunx tsc --noEmit` |
| Frontend lint (i18n enforced) | `cd handy && bun run lint` |
| Release binary | `cd handy && bun tauri build --no-bundle` → `target/release/handy` (≈36 MB, ~6 min). **Signed `.app`/`.dmg` needs full Xcode.** |

Drop `HANDY_FORCE_AI_STUB` once **full Xcode** is installed (also needed for the iPhone target and a real bundle).

**Runtime data:** `~/Library/Application Support/com.pais.handy/` → `history.db` (SQLite) + `recordings/` (`*.session.pcm` live, `*.wav` finalized). **Log:** `~/Library/Logs/com.pais.handy/handy.log`.

---

## 3. What's DONE — with evidence & confidence

| Capability | Where | Evidence | Confidence |
| --- | --- | --- | --- |
| Mic long‑form sessions (Fase 0) | `managers/session.rs` | live row 11 ("Pronto, pronto…") | High |
| System/loopback audio (Fase 1) | `audio_toolkit/audio/system_audio.rs` (CoreAudio Process Tap) | live row 12 ("Ragazzi, buonasera…") | High |
| Local diarization (Fase 2) | `managers/diarization.rs` + sherpa‑onnx | live rows 9 (2 spk) / 10 (3 spk); `align` unit‑tested | High |
| Per‑row transcript **status** | migration #6, `TranscriptionStatus` | `transcribing → done/failed`; unit‑tested | High |
| Menu‑bar **"graffetta"** | `tray.rs` `toggle_session` + `lib.rs` listener | compiles; live toggle via CLI/tray | High |
| **Dual‑stream meeting capture** | `session.rs` `start_sources`/`finalize_session`, `mix_tracks` | **live row 19** (Steve Jobs talk): mic="Me" + system="Speaker 1", merged, accurate transcript | High |
| **Bleed de‑dup** (`drop_bleed`) | `diarization.rs` | **live row 23**: same speaker‑bleed scenario → collapsed to a single speaker, transcript once; 3 unit tests | High |
| Startup **self‑healing** | `history.rs` `fail_stale_transcribing()` | unit‑tested; wired before `recover_interrupted` | High |
| **Local MCP server** (Claude bridge) | `handy/mcp/`, `.mcp.json` | **verified live against the real `history.db`** (returned rows 11/12 + diarized meetings); 4 tests + JSON‑RPC smoke | High |
| Bundled diarization models | `resources/models/diarization/` | auto‑install on first run → offline | High |
| Release binary | `tauri build --no-bundle` | built clean, 36 MB | High |
| **History session‑card result view** | `HistorySettings.tsx` | source icon (meeting/mic/system/dictation) · topic title · date·duration·source meta · speaker chips · collapsible timeline + player + actions | High (live‑validated this session) |

> **The one bug the tests did NOT catch (now fixed):** a **start‑path deadlock** — `start_sources` emitted `SessionStateChanged` while holding the `active` mutex; the listener runs *inline* and re‑enters the manager (`change_tray_icon` → `update_tray_menu` → `is_active()`), re‑locking the non‑reentrant mutex. Fixed by `drop(guard)` before `emit`. See §6.1. Found by adversarial review; the unit tests never traverse the emit→listener path. **Lesson: drop the lock before emitting any event whose listener may re‑enter the manager.**

---

## 4. Control surface (the API you'll extend)

| Kind | Name | Notes |
| --- | --- | --- |
| Command | `start_session(source)` | single source (`"Mic"` / `"SystemAudio"`) |
| Command | `start_meeting()` | **dual** mic + system (the graffetta action) |
| Command | `stop_session()` · `is_session_active()` | |
| Command | `get_session_segments(id)` · `download_diarization_models` · `is_diarization_available` | |
| Event | `session-state-changed` | `{ active, source }` → drives UI + tray icon (single source of truth) |
| Event | `history-update-payload` | `added`/`updated`/`deleted`/`toggled` (binding key `historyUpdatePayload`) |
| CLI flag | `--toggle-session` / `--toggle-system-session` / `--toggle-meeting` | single mic / single system / dual. Plus upstream `--toggle-transcription` (dictation), `--cancel`, etc. |

All session routes converge on `SessionManager`. CLI flags forward to a **running** primary via the single‑instance plugin (`lib.rs`); with no primary they boot a new instance and silently ignore the flag (see §6.4). Commands/events are typed through `tauri-specta` into `src/bindings.ts` (regenerated on dev startup).

---

## 5. Data model — `history.db` (SQLite, `rusqlite_migration`, append‑only)

Migrations: #1 base `transcription_history`; #2–#4 post‑process columns; #5 the diarization overlay; **#6 the `status` column**. **Never edit a shipped migration** (corrupts the `user_version` chain) — append a new one.

```sql
transcription_history(
  id, file_name, timestamp, saved, title, transcription_text,
  post_processed_text, post_process_prompt, post_process_requested,
  status TEXT NOT NULL DEFAULT 'done'        -- 'transcribing' | 'done' | 'failed'
)
speakers(id, history_id→transcription_history ON DELETE CASCADE, label, embedding)
   -- label = "Me" (mic) or "Speaker N" (diarized), per history row
transcription_segments(
  id, history_id→ ON DELETE CASCADE, speaker_id→speakers ON DELETE SET NULL,
  start_ms, end_ms, text, confidence)
CREATE INDEX idx_segments_history ON transcription_segments(history_id);
```
- The flat `transcription_text` is the canonical transcript; `speakers`+`transcription_segments` are the speaker‑attributed overlay. `ON DELETE CASCADE` needs `PRAGMA foreign_keys=ON` per connection (`get_connection` sets it).
- `write_segments` uses **two independent speaker namespaces** in the one `speakers` table: explicit `speaker_label` (e.g. `"Me"`, deduped by string) and diarizer indices (deduped by id → `"Speaker N"`), so "Me" never renumbers the remote speakers.

---

## 6. Critical engineering lessons & traps (read before touching the pipeline)

**6.1 — Drop the lock before emitting a re‑entrant event.** `SessionStateChanged::listen` (in `lib.rs`) runs **inline on the emitting thread** and re‑enters the manager — `change_tray_icon` → `update_tray_menu` → `is_active()` — which re‑locks the non‑reentrant `std::sync::Mutex`. `start_sources` therefore `drop(guard)`s *before* `emit`. `stop()` is safe because it `.take()`s the guard first. Mirror this for any new manager event.

**6.2 — Speaker bleed on laptop speakers (the doubled transcript).** A meeting played through **speakers** (not headphones) is re‑captured by the mic, so one remote person appears as both `Me` and `Speaker N`. `drop_bleed` (`diarization.rs`) drops a `"Me"` segment when an overlapping other‑speaker segment has ≥70% word overlap (= echo); genuine distinct mic speech is kept. **This is transcript‑level only — the echo is still in the mixed WAV.** The real fix is acoustic echo cancellation (AEC) on the mic input (named ceiling, §11). Headphones avoid it; **System** mode (no mic) is correct for pure listening. *(riffado offers nothing here — it has no audio capture; this is ours, see DECISIONS.md.)*

**6.3 — The ASR model must be RESIDENT at `finalize`, and `finalize` will not load it.** Diarization+transcription run only when `is_model_loaded()`. The model **unloads on its idle timer**; a long gap before capturing yields an empty (but `done`) row. To warm it you must run a transcription **with audio actually playing** (silence is VAD‑discarded and won't load the model). Keep `unload_timeout ≠ Immediately`, or warm with one dictation while audio plays. *(This cost real time during testing — empty rows looked like a bug but were a cold model.)*

**6.4 — The CLI toggle needs a running primary.** `handy --toggle-meeting` (etc.) only work as a *second* instance forwarding to a live `bun tauri dev`; with no primary they boot their own instance and ignore the flag (symptom: no `*.session.pcm`).

**6.5 — Capture taps the default output at session start.** System audio routed to headphones/Bluetooth or muted → the tap records silence → empty transcript (graceful, not a bug). Verify you can hear the captured output.

**6.6 — Bindings export is dev‑only & non‑fatal** (logs and continues on a read‑only CWD). **Migrations are append‑only.** **i18n is build‑blocking** (ESLint errors on literal JSX strings; add keys to `src/i18n/locales/en/translation.json`, others fall back to English; the tray's `TrayStrings` are auto‑generated by `build.rs` from the English `tray` block).

---

## 7. Security posture (the local‑first contract)

- **No network egress for data.** ASR + diarization run on‑device (ONNX). The MCP server speaks **stdio only — no listener, no socket, no fetch** (grep‑verified). The "nothing leaves the Mac" promise holds end‑to‑end.
- **MCP is read‑only.** `new Database(path, { readonly: true })` — it physically cannot alter a recording. Every query is **parameterized** (`?` placeholders); table/column names are static literals; tool args become bound params or integer limits only → **no SQL injection**, no path traversal (DB path is fixed or `PLAUDE_DB` env, never a tool arg).
- **macOS permissions:** system‑audio capture uses the **Audio‑Recording TCC permission** (`NSAudioCaptureUsageDescription`), *not* Screen Recording → no purple banner. Mic uses the standard mic permission.
- **No secrets in the repo or DB today.** If a cloud LLM key is ever added, use the macOS Keychain or AES‑GCM at rest (see DECISIONS.md §Encryption) — do **not** store plaintext.
- **Audio files** live unencrypted in `recordings/`; the DB is unencrypted (single‑user, on‑device). At‑rest DB encryption is an *optional* future, not a multi‑tenant requirement.

---

## 8. File inventory (our delta on upstream Handy)

**New:** `managers/session.rs` (long‑form + dual capture), `audio_toolkit/audio/system_audio.rs` (CoreAudio tap), `managers/diarization.rs` (`align`/`label_segments`/`merge_segments`/`drop_bleed` + `DiarizationManager`), `commands/session.rs`, `resources/models/diarization/*.onnx`, **`handy/mcp/{db,server}.ts,db.test.ts,package.json,README.md}`**, **`.mcp.json`** (repo root), `src/components/settings/sessions/SessionsSettings.tsx`.

**Modified (key):** `managers/history.rs` (migrations #5/#6, status, dual‑namespace `write_segments`, `save_pending_entry`, `fail_stale_transcribing`), `managers/transcription.rs` (`transcribe_with_segments`), `managers/model.rs` (diarization download + bundle), `tray.rs` (graffetta), `lib.rs` (wiring, listener, `--toggle-meeting`, `start_meeting`, startup self‑heal), `cli.rs`, `audio_toolkit/audio/recorder.rs` (`with_chunk_sink`), `commands/history.rs`, `components/settings/history/HistorySettings.tsx` (timeline + speaker chips + status states), `Sidebar.tsx`, `i18n/locales/{en,it}/translation.json`, `Cargo.toml`/`.cargo/config.toml`/`build.rs`/`Info.plist`, `src/bindings.ts` (regenerated). Full table in [CODEBASE.md §11](CODEBASE.md).

---

## 9. Verification procedures (re‑prove any claim)

```bash
# Backend + MCP correctness
cd handy/src-tauri && cargo check --lib && cargo test --lib   # 92 passed
cd ../mcp && bun test                                          # 4 pass
cd ../ && bunx tsc --noEmit && bun run lint                    # clean

# LIVE dual meeting capture (deterministic, no headphones needed — uses macOS `say`):
#   1) bun tauri dev running; an ASR model resident.
#   2) WARM the model WITH audio playing (silence won't load it):
#      ./target/debug/handy --toggle-transcription; say "warming the model"; ./target/debug/handy --toggle-transcription ; sleep 22
#   3) CAPTURE:
#      ./target/debug/handy --toggle-meeting; say "the quarterly numbers look strong"; ./target/debug/handy --toggle-meeting ; sleep 30
#   4) INSPECT (expect ONE speaker "Speaker 1", text once — bleed de-duped):
#      sqlite3 -readonly ~/Library/Application\ Support/com.pais.handy/history.db \
#        "SELECT label FROM speakers WHERE history_id=(SELECT max(id) FROM transcription_history);"
#   With real human speech on a real call + headphones: expect "Me" (you) AND the remote "Speaker N", cleanly separated.

# LIVE MCP (what Claude sees) — see §1.4.
```
> Cleaning up test rows: the app's History trash icon, or delete `id > <your last real id>` from `transcription_history` (+ their `transcription_segments`/`speakers` and the `recordings/*.wav`).

---

## 10. Repo / git state

- **Remote:** `github.com/uppifyagency/plaude-local` (private), branch `main`. The upstream `handy/.git` was **flattened** into this single repo (upstream = `cjpais/Handy`).
- **Working tree is NOT committed** — this session's work (dual capture, graffetta, MCP, status column, bleed de‑dup, self‑heal, docs) is staged in the tree only. **Commit/push only when the user asks.** End commit messages with the `Co-Authored-By: Claude …` trailer; branch off `main` first if needed.
- **Models are committed** (`resources/models/diarization/*.onnx`, ~46 MB); `git config http.postBuffer 524288000` is set locally for the large push. `.gitignore` excludes `target/`, `node_modules/`, `dist/`, and stray `*.onnx` loose in `src-tauri/`.
- `handy/mcp/` has **no `node_modules`** (dependency‑free) — nothing to install.

---

## 11. What's PENDING / DEFERRED (prioritized)

1. **Acoustic echo cancellation (AEC).** `drop_bleed` removes the transcript *duplicate*; the echo is still in the mixed WAV and the mic still hears the speakers. Real AEC (subtract the system reference from the mic in `recorder.rs`/`session.rs`) is the proper fix for clean speaker use. Headphones sidestep it today.
2. **AI topic‑title & summary for History cards.** The session‑card result view **shipped** (`HistorySettings.tsx`, live‑validated): source icon, date·duration·source meta, speaker chips, and a collapsible speaker timeline + player + actions. The card title is currently the transcript's opening words — a **non‑AI placeholder**. The remaining piece — a clean AI‑generated title/summary instead of that placeholder — is **gated on the AI‑provider decision (§12)**.
3. **"Enable diarization" download button** in the Sessions view (command exists; bundling already covers fresh clones, so it's a fallback).
4. **Signed/notarized `.app`/`.dmg`** — release *binary* builds; the bundle needs **full Xcode**.
5. **iPhone target (needs full Xcode).** No iOS upstream. Recommended: **iPhone‑as‑capture + Mac‑as‑brain** over Apple's nearby transfer — a SwiftUI app records locally and, on proximity, pushes files (MultipeerConnectivity, or Network.framework peer‑to‑peer Bonjour) into the Mac's `recordings/` dir, where the existing `recover_interrupted`/finalize pipeline ingests them chronologically.
6. **Clustering threshold tuning** — only if a rapid‑alternation recording over‑merges speakers (defaults are good for long‑turn audio). Lever: `OfflineSpeakerDiarizationConfig` in `diarization.rs`.

---

## 12. Open decisions (need a human call — do NOT guess)

- **AI provider stance:** local‑only by default? Allow opt‑in Ollama/LM Studio "cloud boost"? Any non‑local path at all? (Drives a `ProviderPreset`/`TranscriptionStyle` abstraction + an `ai_enhancements` table — see DECISIONS.md.)
- **Encryption key management** (only once a secret exists): macOS Keychain (recommended) vs passphrase‑derived (Argon2), or defer.
- **Bleed strategy:** ship AEC, or treat "use headphones / System mode" as the documented answer and keep `drop_bleed` as the safety net?
- **Webhooks / local automation surface** (fire `session.ended` to n8n/Obsidian) — build, or YAGNI until asked?
- **Commit cadence** — the tree is uncommitted by design; ask before committing.

---

## 13. Conventions (enforced — respect them)

- **Ponytail is active (level: full).** Lazy‑senior‑dev: YAGNI → stdlib → native → existing dep → one line → minimum. Never cut validation, error handling, security, accessibility. Mark intentional shortcuts with a `ponytail:` comment naming the ceiling + upgrade path. Toggle: `/ponytail lite|full|ultra|off`.
- **Agile technical practices** are the working method: thin vertical slices, **outside‑in/TDD** (pure domain logic — `align`/`merge_segments`/`drop_bleed`/`mix_tracks` — is tested in isolation), stay green, refactor under green, **adversarial review before declaring done** (it caught the deadlock).
- **Extend Handy's managers/pipeline; cite the real `handy/src-tauri/...` file** when proposing changes. Prefer the producer‑agnostic `chunk_sink` seam over net‑new capture paths.
- **Single source of truth for diarization filenames:** `DiarizationManager::{SUBDIR,SEG_FILE,EMB_FILE}`.
- **nWave** waves/agents available for backend/architecture work.

---

*Created 2026‑06‑22, elevated to a full briefing 2026‑06‑23. Keep this current as the agent entry‑point. Deep technical reference: [CODEBASE.md](CODEBASE.md). riffado teardown verdict: [DECISIONS.md](DECISIONS.md).*
