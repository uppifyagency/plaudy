# 09 — History Manager, SQLite Persistence & Recording Retention

> **Abstract.** This subsystem is Handy's transcription "memory": every finished (or failed) recording is persisted as a row in a local SQLite database (`history.db`) plus a sibling 16 kHz mono WAV file in a `recordings/` directory, both living under the app-data directory (portable-aware). The Rust core is split into two files: `managers/history.rs` owns the `HistoryManager` struct, the `rusqlite_migration` schema, all CRUD/cleanup logic, the timestamped-title formatter, and a typed `HistoryUpdatePayload` event emitted to the frontend on every mutation; `commands/history.rs` exposes seven `#[tauri::command]` handlers (list/paginate, toggle-saved, resolve-audio-path, delete, retry-transcription, update-limit, update-retention) that wrap the manager held in Tauri managed state as `Arc<HistoryManager>`. Audio bytes are written **outside** the manager (in `actions.rs`, concurrently with transcription) and only the file *name* is recorded in the DB; retention is enforced opportunistically on every `save_entry` according to a user-selectable policy (never / count-limit / 3 days / 2 weeks / 3 months). There is **no streaming, no diarization, no speaker model, no cloud sync, no per-segment timestamps, and no mobile path** — the schema stores one flat transcript string per recording, which is the central gap versus a Plaud-style conversation product.

---

## 1. Files & per-file responsibility

| Path | Responsibility |
|---|---|
| `handy/src-tauri/src/managers/history.rs` | Owns `HistoryManager`: opens `history.db`, runs `rusqlite_migration` migrations (+ a legacy `tauri-plugin-sql` → `user_version` bridge), defines the `transcription_history` schema, and implements all DB CRUD, retention cleanup, audio-path resolution, the title formatter, and `HistoryUpdatePayload` event emission. |
| `handy/src-tauri/src/commands/history.rs` | Thin Tauri command layer: 7 async commands that borrow `State<Arc<HistoryManager>>` (and for retry also `State<Arc<TranscriptionManager>>`), translate `anyhow::Error` → `String`, and persist the limit/retention settings before triggering cleanup. |
| `handy/src-tauri/src/actions.rs` (`TranscribeAction::stop`, ~L495–660) | **Producer.** Writes the WAV file (`save_wav_file`) concurrently with transcription, verifies it, then calls `hm.save_entry(...)`. The only place rows are *created* in production. |
| `handy/src-tauri/src/audio_toolkit/audio/utils.rs` | WAV I/O primitives used by this subsystem: `save_wav_file`, `verify_wav_file`, `read_wav_samples` (16 kHz / mono / i16). |
| `handy/src-tauri/src/portable.rs` (`app_data_dir`, L60–66) | Resolves the base directory for both `recordings/` and `history.db` (portable mode → `Data/` next to exe; otherwise OS app-data dir). |
| `handy/src-tauri/src/settings.rs` (L156–164, L378–381, L490–496, L940–948) | Defines `RecordingRetentionPeriod` enum + `history_limit`/`recording_retention_period` settings and their getters consumed by cleanup. |
| `handy/src-tauri/src/tray.rs` (`copy_last_transcript`, L242) | **Consumer.** Reads the newest completed entry via `get_latest_completed_entry()` to support the "copy last transcript" tray action. |
| `handy/src-tauri/src/lib.rs` (L156–166) | Constructs `Arc::new(HistoryManager::new(...))` at startup and registers it in Tauri managed state. |
| `handy/src-tauri/src/commands/mod.rs` (`open_recordings_folder`, L71–85) | Opens the `recordings/` folder in the OS file manager (independent of the manager, recomputes the path). |
| `handy/src/components/settings/history/HistorySettings.tsx` | Frontend consumer: infinite-scroll list, audio playback (`convertFileSrc(..., "asset")`), save/delete/retry buttons, and live updates via `events.historyUpdatePayload.listen(...)`. |

---

## 2. Data model, structs & schema

### 2.1 SQLite schema (`transcription_history`)

Defined as an ordered array of `rusqlite_migration::M` migrations, `managers/history.rs:20-34`. `rusqlite_migration` records the applied version in SQLite's `user_version` pragma.

| # | Migration (file:line) | Effect |
|---|---|---|
| 1 | `history.rs:21-30` | `CREATE TABLE transcription_history (id PK AUTOINCREMENT, file_name TEXT, timestamp INTEGER, saved BOOLEAN DEFAULT 0, title TEXT, transcription_text TEXT)` |
| 2 | `history.rs:31` | `ADD COLUMN post_processed_text TEXT` (nullable) |
| 3 | `history.rs:32` | `ADD COLUMN post_process_prompt TEXT` (nullable) |
| 4 | `history.rs:33` | `ADD COLUMN post_process_requested BOOLEAN DEFAULT 0` |

Resulting columns: `id`, `file_name`, `timestamp` (Unix seconds, UTC), `saved`, `title` (pre-formatted local-time string), `transcription_text` (raw Whisper/Parakeet output, empty string on failure), `post_processed_text` (LLM/translation output, nullable), `post_process_prompt` (the prompt used, nullable), `post_process_requested` (whether post-processing was requested).

**Notable schema properties / smells:**
- No index beyond the implicit PK. Pagination orders by `id DESC` (`history.rs:465,478,490`), cleanup-by-count orders by `timestamp DESC` (`history.rs:385`), cleanup-by-time filters `timestamp < ?` (`history.rs:426`) — all full scans, fine at the intended ~5-entry scale, but no `CREATE INDEX` on `timestamp` or `saved`.
- `timestamp` is **seconds**, not millis; the `file_name` is `handy-{unix_seconds}.wav` (`actions.rs:538`), so two recordings stopped in the same second collide on filename (last-writer-wins WAV, but distinct DB rows because `id` is autoincrement).
- No `duration`, `sample_rate`, `language`, `model`, `speaker`, or segment table — the transcript is one opaque string.

### 2.2 Rust types

| Type | File:line | Notes |
|---|---|---|
| `static MIGRATIONS: &[M]` | `history.rs:20` | The four migrations above, applied in order. |
| `struct PaginatedHistory { entries: Vec<HistoryEntry>, has_more: bool }` | `history.rs:36-40` | `Serialize + Type` (specta) → exported to TS. Return type of the list command. |
| `enum HistoryUpdatePayload` (`#[serde(tag="action")]`, derives `tauri_specta::Event`) | `history.rs:42-53` | Variants `Added{entry}`, `Updated{entry}`, `Deleted{id}`, `Toggled{id}`. This is the **only** backend→frontend signal for the subsystem; serialized with an `"action"` discriminator. |
| `struct HistoryEntry` | `history.rs:55-66` | The full row mirror; all nine columns. `saved`/`post_process_requested` as `bool`, the two post-process fields `Option<String>`. |
| `struct HistoryManager { app_handle, recordings_dir, db_path }` | `history.rs:68-72` | Holds an `AppHandle` (for event emission) and the two resolved paths. **No connection pool** — every operation opens a fresh `Connection`. |

---

## 3. Public/important functions (signatures + behavior + citations)

All methods are on `impl HistoryManager` unless noted.

| Function | Signature / file:line | Behavior |
|---|---|---|
| `new` | `pub fn new(app_handle: &AppHandle) -> Result<Self>` — `history.rs:75-97` | Resolves `recordings_dir = app_data_dir/recordings` and `db_path = app_data_dir/history.db` via `portable::app_data_dir`; `create_dir_all` for recordings if missing; calls `init_database()` synchronously. Constructed once in `lib.rs:157`. |
| `init_database` | `fn init_database(&self) -> Result<()>` — `history.rs:99-136` | Opens the DB, runs `migrate_from_tauri_plugin_sql`, builds `Migrations::new(MIGRATIONS.to_vec())`, `validate()`s in debug builds (`#[cfg(debug_assertions)]`, L112-113), records version before/after, then `migrations.to_latest(&mut conn)`. |
| `migrate_from_tauri_plugin_sql` | `fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()>` — `history.rs:142-193` | One-time bridge: if a legacy `_sqlx_migrations` table exists and `user_version == 0`, copies `MAX(version) WHERE success=1` into the `user_version` pragma so rusqlite_migration won't re-run. Old table is intentionally **not** dropped (commented out, L184). |
| `get_connection` | `fn get_connection(&self) -> Result<Connection>` — `history.rs:195-197` | Opens a **new** `rusqlite::Connection` per call. No pooling, no WAL pragma, no busy_timeout. |
| `map_history_entry` | `fn map_history_entry(row) -> rusqlite::Result<HistoryEntry>` — `history.rs:199-211` | Column-name-based row→struct mapper reused by every SELECT. |
| `recordings_dir` | `pub fn recordings_dir(&self) -> &Path` — `history.rs:213-215` | Exposes the dir so `actions.rs:539` can build the WAV path before saving. |
| `save_entry` | `pub fn save_entry(&self, file_name: String, transcription_text: String, post_process_requested: bool, post_processed_text: Option<String>, post_process_prompt: Option<String>) -> Result<HistoryEntry>` — `history.rs:219-280` | Stamps `timestamp = Utc::now()`, formats `title`, INSERTs, builds `HistoryEntry` from `last_insert_rowid()`, then **calls `cleanup_old_entries()`** (L268) and emits `HistoryUpdatePayload::Added` (L271). `saved` is hard-coded `false` on insert. **Synchronous** (not async). |
| `update_transcription` | `pub fn update_transcription(&self, id, transcription_text, post_processed_text, post_process_prompt) -> Result<HistoryEntry>` — `history.rs:283-328` | UPDATE used by **retry**; errors `anyhow!("History entry {} not found")` if 0 rows changed (L305-307), re-SELECTs the row, emits `Updated`. Does **not** touch `saved`, `timestamp`, or `file_name`. |
| `cleanup_old_entries` | `pub fn cleanup_old_entries(&self) -> Result<()>` — `history.rs:330-348` | Reads the retention policy from settings and dispatches: `Never`→no-op; `PreserveLimit`→`cleanup_by_count(history_limit)`; everything else→`cleanup_by_time(period)`. |
| `delete_entries_and_files` | `fn delete_entries_and_files(&self, entries: &[(i64,String)]) -> Result<usize>` — `history.rs:350-378` | Per-entry: `DELETE` row, then `fs::remove_file` the WAV; file-delete failures are logged but not fatal; returns count of WAVs deleted. **Row delete and file delete are not transactional** and not batched. |
| `cleanup_by_count` | `fn cleanup_by_count(&self, limit: usize) -> Result<()>` — `history.rs:380-407` | Selects all `saved = 0` rows ordered `timestamp DESC`, keeps the newest `limit`, deletes the tail. **Saved entries are exempt and don't count against the limit.** |
| `cleanup_by_time` | `fn cleanup_by_time(&self, period) -> Result<()>` — `history.rs:409-448` | Computes `cutoff = now - period` (Days3 = 3·86400, Weeks2 = 14·86400, Months3 ≈ 90·86400, L418-420), deletes `saved = 0 AND timestamp < cutoff`. `_ => unreachable!()` guards non-time variants (L421). |
| `get_history_entries` | `pub async fn get_history_entries(&self, cursor: Option<i64>, limit: Option<usize>) -> Result<PaginatedHistory>` — `history.rs:450-505` | Keyset pagination on `id DESC`. Clamps `limit` to ≤100 (L456). Fetches `limit+1` rows to compute `has_more`, pops the extra (L499-502). Three SQL branches: `(cursor,limit)` → `WHERE id < cursor LIMIT limit+1`; `(none,limit)`; `(_,none)` → unbounded full table. `async` but performs **blocking** rusqlite calls on the caller's async task. |
| `get_latest_entry_with_conn` | `#[cfg(test)] fn ... -> Result<Option<HistoryEntry>>` — `history.rs:507-527` | Test-only newest-by-timestamp helper. |
| `get_latest_completed_entry` / `_with_conn` | `pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>>` — `history.rs:530-555` | Newest row `WHERE transcription_text != ''` (skips failed/empty). Used by the tray "copy last transcript" action (`tray.rs:244`). |
| `toggle_saved_status` | `pub async fn toggle_saved_status(&self, id: i64) -> Result<()>` — `history.rs:557-582` | SELECT current `saved`, write `!saved`, emit `Toggled{id}`. Read-modify-write is **not atomic** (no transaction; a SELECT-then-UPDATE race exists, though single-user UI makes it benign). Propagates a rusqlite error if `id` doesn't exist (the SELECT `query_row` fails). |
| `get_audio_file_path` | `pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf` — `history.rs:584-586` | `recordings_dir.join(file_name)`. **No path-sanitization** — a `file_name` containing `../` would escape the dir (only ever fed trusted DB values today). |
| `get_entry_by_id` | `pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>>` — `history.rs:588-608` | Single-row fetch by PK. |
| `delete_entry` | `pub async fn delete_entry(&self, id: i64) -> Result<()>` — `history.rs:610-639` | Fetches the row, deletes the WAV first (failure logged, continues), then `DELETE` the row, emits `Deleted{id}`. |
| `format_timestamp_title` | `fn format_timestamp_title(&self, timestamp: i64) -> String` — `history.rs:641-649` | Converts UTC seconds → local time, formats `"%B %e, %Y - %l:%M%p"` (e.g. "June 19, 2026 - 3:26PM"); fallback `"Recording {ts}"` if conversion fails. **Title is frozen at insert time** and never re-localized. |

### 3.1 Tauri commands (`commands/history.rs`)

| Command | Signature / file:line | Behavior |
|---|---|---|
| `get_history_entries` | `async fn(_app, history_manager: State<Arc<HistoryManager>>, cursor: Option<i64>, limit: Option<usize>) -> Result<PaginatedHistory,String>` — `commands/history.rs:9-21` | Delegates to manager; maps error to string. |
| `toggle_history_entry_saved` | `async fn(_app, State, id: i64) -> Result<(),String>` — `:23-34` | Toggles `saved`. |
| `get_audio_file_path` | `async fn(_app, State, file_name: String) -> Result<String,String>` — `:36-47` | Resolves to absolute path string; errors `"Invalid file path"` if non-UTF8. Frontend feeds this to `convertFileSrc(path, "asset")` for playback. |
| `delete_history_entry` | `async fn(_app, State, id: i64) -> Result<(),String>` — `:49-60` | Deletes row + WAV. |
| `retry_history_entry_transcription` | `async fn(app, history_manager, transcription_manager: State<Arc<TranscriptionManager>>, id: i64) -> Result<(),String>` — `:62-107` | **Cross-subsystem.** Loads entry → `read_wav_samples(audio_path)` → guards empty samples → `initiate_model_load()` → `spawn_blocking(move || tm.transcribe(samples))` → guards empty transcript → `process_transcription_output(&app, &transcription, entry.post_process_requested)` → `update_transcription(...)`. Re-uses the stored WAV as the source of truth. |
| `update_history_limit` | `async fn(app, State, limit: usize) -> Result<(),String>` — `:109-125` | Writes `settings.history_limit`, then `cleanup_old_entries()`. |
| `update_recording_retention_period` | `async fn(app, State, period: String) -> Result<(),String>` — `:127-154` | Parses the string into `RecordingRetentionPeriod` (`"never"|"preserve_limit"|"days3"|"weeks2"|"months3"`, else error), persists it, then `cleanup_old_entries()`. |

---

## 4. Threading / concurrency model

- **No internal threads or channels.** The subsystem has no background worker — `HistoryManager` is a passive façade over a path + an `AppHandle`.
- **Connection-per-call.** `get_connection()` (`history.rs:195`) opens a fresh `rusqlite::Connection` for every operation. There is no shared connection, no `Mutex`, and no pool. Concurrency safety relies entirely on SQLite's own file locking. With the default journal mode and **no `busy_timeout` set**, a concurrent write could surface `SQLITE_BUSY` — unlikely today because writes are serialized through the single transcription pipeline.
- **`async` is cosmetic for DB work.** Methods like `get_history_entries`, `toggle_saved_status`, `delete_entry`, `get_entry_by_id` are declared `async` but contain only synchronous rusqlite calls, so they block the executor thread for the (tiny) duration of the query. `save_entry`/`update_transcription`/`get_latest_completed_entry` are plain sync fns.
- **The producer offloads correctly.** WAV encoding in `actions.rs:542` uses `tauri::async_runtime::spawn_blocking`, and retry's `tm.transcribe` runs under `spawn_blocking` (`commands/history.rs:87`) — so the CPU-heavy work is off the async pool, but the DB row write itself runs inline.
- **Event emission** (`HistoryUpdatePayload::...emit(&self.app_handle)`) is fire-and-forget; failures are logged via `error!` and never propagate (`history.rs:276, 324, 578, 635`).

---

## 5. Data flow IN and OUT

### IN (who writes / triggers)
1. **Primary producer — `actions.rs` `TranscribeAction::stop` (L516-654):** after recording stops, it (a) builds `file_name = handy-{unix_seconds}.wav`, (b) `spawn_blocking(save_wav_file)`, (c) transcribes concurrently, (d) `verify_wav_file` (sample-count check), and only if `wav_saved` calls `hm.save_entry(...)` — both on success (with transcript + post-processed fields, L591) and on transcription failure (empty transcript so the user can retry, L634). **If the WAV fails to save/verify, no DB row is created at all.**
2. **Retry — `commands/history.rs:62` `retry_history_entry_transcription`:** reads the stored WAV back and calls `update_transcription` (no new row, no new file).
3. **Settings commands — `update_history_limit`, `update_recording_retention_period`:** mutate settings then call `cleanup_old_entries()` (which can DELETE rows + WAVs).
4. **Opportunistic cleanup:** every `save_entry` runs `cleanup_old_entries()` (`history.rs:268`).

### OUT (who reads / receives)
1. **Frontend list UI — `HistorySettings.tsx`:** `commands.getHistoryEntries(cursor, limit)` for infinite scroll (`has_more` drives the IntersectionObserver sentinel), plus `toggleHistoryEntrySaved`, `deleteHistoryEntry`, `retryHistoryEntryTranscription`, and `getAudioFilePath` → `convertFileSrc(path,"asset")` for inline WAV playback.
2. **Live updates:** `events.historyUpdatePayload.listen(...)` (`HistorySettings.tsx:135`) receives `Added/Updated/Deleted/Toggled` and patches local React state without a refetch.
3. **Tray — `tray.rs:242` `copy_last_transcript`:** `get_latest_completed_entry()` → clipboard.
4. **Recordings folder — `commands/mod.rs:73` `open_recordings_folder`:** opens the dir in Finder/Explorer (recomputes path independently of the manager).

### Message/event types crossing the boundary
- **Command results:** `PaginatedHistory`, `HistoryEntry`, `String` (audio path), `()`.
- **Event:** `HistoryUpdatePayload` (tagged enum, `tauri-specta` generated TS binding in `src/bindings.ts`).
- **File-channel:** the WAV file itself, addressed by `file_name`, is the implicit contract between the producer (`actions.rs`) and the consumer (frontend playback / retry).

---

## 6. Error handling & edge cases

- **Errors as `anyhow::Result` internally**, flattened to `String` at the command boundary (`map_err(|e| e.to_string())`).
- **Failed transcription is persisted, not dropped:** empty `transcription_text` row is created so the UI can offer "retry" (`actions.rs:633-643`). `get_latest_completed_entry` filters these out with `transcription_text != ''`.
- **WAV-delete failures are non-fatal:** both `delete_entries_and_files` (L368) and `delete_entry` (L618) log and continue, so the DB row is removed even if the file is locked/missing — but a failed *row* delete inside the cleanup loop (`history.rs:360`) **bubbles up via `?` and aborts the whole cleanup batch**, potentially leaving orphan WAVs for the earlier-deleted rows (rows deleted, files not yet processed). Conversely an orphaned WAV (file exists, no row) is never garbage-collected.
- **Non-transactional multi-step ops:** `toggle_saved_status` (SELECT+UPDATE) and the cleanup loops have no surrounding transaction. No `BEGIN/COMMIT` anywhere in the file.
- **`update_transcription` missing-id** → explicit `anyhow!` error (L306). `toggle_saved_status` missing-id → rusqlite `QueryReturnedNoRows` surfaced as a generic error.
- **Pagination clamp:** `limit.min(100)` (L456) caps page size; `limit = None` triggers an **unbounded** full-table SELECT (L486) — a latent memory risk if history grows large under `Never` retention.
- **Filename collisions:** second-granularity timestamps mean rapid consecutive recordings can overwrite each other's WAV while keeping distinct rows.
- **No path traversal guard** in `get_audio_file_path` (trusted input today).
- **Migration safety:** `validate()` runs only in debug builds (`#[cfg(debug_assertions)]`, L112); a broken migration would not be caught in a release build until `to_latest` fails at runtime.

---

## 7. State & persistence touched

| Store | What | Where |
|---|---|---|
| **SQLite** | `history.db` → `transcription_history` table (+ legacy `_sqlx_migrations` left in place), version tracked in `user_version` pragma. | `app_data_dir/history.db` (`history.rs:79`). |
| **Files on disk** | One `handy-{ts}.wav` per recording, 16 kHz / mono / 16-bit PCM. | `app_data_dir/recordings/` (`history.rs:78`, written `actions.rs:543`). |
| **Settings store (tauri-plugin-store)** | `history_limit: usize` (default 5) and `recording_retention_period: RecordingRetentionPeriod` (default `PreserveLimit`). | `settings.rs:378-381, 490-496, 940-948`; written by the two settings commands. |
| **Model files** | Not touched directly; retry uses `TranscriptionManager` which loads model files. | — |

The base directory is **portable-aware**: `portable::app_data_dir` (`portable.rs:60`) returns `Data/` next to the executable in portable mode, else the OS app-data dir, so both the DB and recordings move together.

---

## 8. Platform-specific branches

- **None inside this subsystem.** `history.rs` and `commands/history.rs` contain **no `#[cfg(target_os = ...)]` gates**. The only conditional compilation is `#[cfg(debug_assertions)]` for migration validation (`history.rs:112`) and `#[cfg(test)]` helpers/tests (`history.rs:507, 652`).
- Platform divergence is fully delegated:
  - **Path resolution** to `portable.rs` (which itself has no per-OS branches — it relies on Tauri's `app_data_dir`).
  - **File-manager opening** to `commands/mod.rs:open_recordings_folder` via `opener` plugin.
  - **Asset serving** for playback to Tauri's `asset:` protocol + the `fs:scope`/`fs:read-files` capability (`capabilities/default.json:18-23`, scoped to `$APPDATA/**/*`).
- **iOS:** No iOS path exists. `rusqlite` with `features=["bundled"]` (`Cargo.toml:68`) compiles SQLite into the binary and would work on iOS, but the entire producer chain (cpal recording, whisper-rs, global shortcuts) has no iOS support, so the history subsystem is currently desktop-only by transitive dependency.

---

## 9. PLAUD relevance — concrete extension points

The subsystem is the natural persistence backbone for a Plaud-style recorder, but its single-string transcript model must be extended. Concrete hooks:

1. **Schema: add a 5th migration for conversation metadata.** Append `M::up(...)` entries at `history.rs:34` to add `duration_secs`, `sample_rate`, `channels`, `source` (`mic`/`system`/`call`/`import`), `language`, `model`, and `device_label`. `rusqlite_migration` makes this additive and safe. Update `HistoryEntry` (`history.rs:55-66`), `map_history_entry` (`history.rs:199`), and every SELECT column list.

2. **Speaker diarization → new tables.** Add `transcription_segments(history_id FK, start_ms, end_ms, speaker_id, text)` and `speakers(id, history_id, label, embedding BLOB)`. Keep `transcription_text` as the flattened cache. Diarization output (e.g. pyannote/sherpa) would be written alongside `save_entry` — wrap/extend `save_entry` (`history.rs:219`) to accept an optional `Vec<Segment>` and insert in the same transaction. This is the single highest-leverage change for Plaud parity.

3. **Long-form / system / call audio capture.** The producer is `actions.rs:TranscribeAction::stop` (L516); for long recordings, replace the "encode-whole-buffer-then-`save_entry`" model with **chunked streaming**: create the DB row *first* (status `recording`), append-write to a growing WAV/Opus file, and `update_transcription`/segment-insert incrementally. `save_wav_file` (`utils.rs:31`) currently buffers the entire `&[f32]` — switch to a streaming `WavWriter`/Opus encoder for hour-long sessions. System/call audio is captured upstream (the `audio_toolkit`), but this subsystem only needs the new `source` column and a longer-capable encoder.

4. **AI summaries.** `post_processed_text`/`post_process_prompt` already model "derived text". For Plaud-style summaries/action-items, either reuse these columns or add `summary TEXT`, `action_items TEXT (JSON)`, `key_topics TEXT (JSON)`. The retry command (`commands/history.rs:62`) is the template for a new `summarize_history_entry` command that feeds `transcription_text` to the post-processing LLM and calls `update_transcription`-style UPDATE.

5. **Cloud / local sync.** Add a `sync_state` column (`local`/`pending`/`synced`) and an `updated_at`/`uuid` column (replace integer `id` exposure with a stable UUID for cross-device identity). A sync worker would watch `HistoryUpdatePayload` emissions (`history.rs:42`) — they already fire on every Added/Updated/Deleted/Toggled, making them a ready-made change feed. The retention cleanup (`cleanup_old_entries`, `history.rs:330`) must become sync-aware so it doesn't delete un-synced rows.

6. **Mobile (iPhone).** The cleanest reuse is to lift the **schema + `HistoryEntry` contract** as the sync wire format and have a mobile client own its own SQLite mirror. `HistoryManager` itself is desktop-bound only through its dependencies, not its logic; the DDL and the `HistoryUpdatePayload` enum are portable.

7. **Search.** Add a `transcription_history_fts` FTS5 virtual table (another `M::up`) and a `search_history(query)` command modeled on `get_history_entries` — essential for a notes product, absent today.

8. **Retention by speaker/importance.** Extend `RecordingRetentionPeriod` (`settings.rs:158`) and `cleanup_by_time`/`cleanup_by_count` (`history.rs:409, 380`) so "saved", summarized, or shared conversations are never auto-pruned (currently only the boolean `saved` exempts a row).

---

## 10. Gaps vs a Plaud-style product

- **No conversation/segment model.** One flat `transcription_text` string per recording; no per-utterance timestamps, no segment table, no word-level timing.
- **No speaker diarization or speaker identity.** No `speaker` column, embedding storage, or multi-speaker concept anywhere.
- **No long-form / streaming capture.** `save_wav_file` buffers the entire sample vector in memory and is written once after the recording ends; `save_entry` is post-hoc. No resumable, append-only, or chunked recording. Hour-long meetings would hold the whole PCM buffer in RAM.
- **No system-audio / call-audio source tagging.** No `source` field; capture path is mic-only (upstream limitation reflected by the absent schema field).
- **No AI summaries / action items / topics.** Only a generic `post_processed_text` (rewrite/translate), not structured meeting intelligence.
- **No cloud or cross-device sync, no UUIDs, no `updated_at`, no soft-delete/tombstones.** Integer autoincrement `id` is device-local; deletes are hard.
- **No full-text search, tags, folders, or favourites beyond a single `saved` boolean.**
- **No encryption at rest.** `history.db` and WAVs are plaintext on disk; a privacy-focused recorder typically needs at-rest encryption.
- **No transaction boundaries** around multi-step mutations; cleanup can orphan WAVs on partial failure, and orphaned files are never garbage-collected.
- **No connection pooling / `busy_timeout` / WAL** — fine at 5 entries, fragile under concurrent or high-volume long-form use.
- **No export** (Markdown/JSON/SRT/VTT) beyond opening the raw recordings folder and clipboard copy of the last transcript.
- **No mobile (iOS) target** for the producer chain.
- **Title is frozen at insert and second-granularity filenames can collide** — minor but user-visible data-integrity rough edges.
