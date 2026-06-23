use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_specta::Event;

use crate::managers::diarization::TimedSegment;

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
    // Fase 2 diarization: speaker-attributed transcript segments. APPEND-ONLY — never edit a
    // migration above (rusqlite_migration tracks applied versions via the user_version pragma).
    // Cascades require `PRAGMA foreign_keys = ON` per connection (set in get_connection).
    M::up(
        "CREATE TABLE IF NOT EXISTS speakers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
            label TEXT NOT NULL,
            embedding BLOB
        );
        CREATE TABLE IF NOT EXISTS transcription_segments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            history_id INTEGER NOT NULL REFERENCES transcription_history(id) ON DELETE CASCADE,
            speaker_id INTEGER REFERENCES speakers(id) ON DELETE SET NULL,
            start_ms INTEGER NOT NULL,
            end_ms INTEGER NOT NULL,
            text TEXT NOT NULL,
            confidence REAL
        );
        CREATE INDEX IF NOT EXISTS idx_segments_history ON transcription_segments(history_id);",
    ),
    // Sessions UI: per-row transcript lifecycle so a long-form session shows a live
    // "transcribing…" row the moment Stop is pressed (filling the silent gap before the
    // slow transcription lands) and a finalize crash leaves a 'failed' row, not one stuck
    // on "transcribing". APPEND-ONLY. Default 'done' keeps every existing row and the
    // one-shot dictation path correct with zero code change.
    M::up("ALTER TABLE transcription_history ADD COLUMN status TEXT NOT NULL DEFAULT 'done';"),
];

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

/// Lifecycle of a row's transcript. `Done` is the default for the one-shot dictation path
/// and every pre-existing row (the migration's column default). Long-form sessions move
/// `Transcribing` → `Done`/`Failed`, so the UI can show live progress and a crashed
/// finalize leaves a `Failed` row instead of one stuck on "transcribing" forever.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionStatus {
    Transcribing,
    Done,
    Failed,
}

impl TranscriptionStatus {
    fn as_db(self) -> &'static str {
        match self {
            Self::Transcribing => "transcribing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Map the stored string back. Unknown values degrade to `Done` (the column default)
    /// so a hand-edited or future-schema DB never panics the read path.
    fn from_db(s: &str) -> Self {
        match s {
            "transcribing" => Self::Transcribing,
            "failed" => Self::Failed,
            _ => Self::Done,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    pub status: TranscriptionStatus,
}

/// A persisted, speaker-attributed transcript segment — the read shape for the timeline UI.
/// `speaker_label` is `None` for segments no diarizer turn covered ("unknown speaker").
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PersistedSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker_label: Option<String>,
    pub text: String,
}

/// Persist diarized segments for one history entry, in a single transaction. Creates one
/// `speakers` row per distinct diarizer speaker index referenced by the segments (labelled
/// "Speaker N", 1-based in first-seen order), then inserts the segments with the FK to those
/// rows. Segments with `speaker_id: None` are stored with a NULL speaker (graceful "unknown").
///
/// Free function over `&mut Connection` (not a manager method) so it is unit-testable against
/// an in-memory database without a Tauri `AppHandle`.
fn write_segments(
    conn: &mut Connection,
    history_id: i64,
    segments: &[TimedSegment],
) -> Result<()> {
    use std::collections::{BTreeMap, HashMap};
    let tx = conn.transaction()?;
    // Two independent speaker namespaces sharing one `speakers` table:
    //  - explicit names (e.g. the dual-stream mic track "Me") deduped by the label string;
    //  - diarizer indices deduped by local id and surfaced as "Speaker N" in first-seen order.
    // They number independently, so "Me" never shifts the remote "Speaker 1/2…" sequence.
    let mut local_to_db: BTreeMap<i64, i64> = BTreeMap::new();
    let mut label_to_db: HashMap<String, i64> = HashMap::new();
    for seg in segments {
        let db_speaker: Option<i64> = if let Some(label) = &seg.speaker_label {
            if let Some(&id) = label_to_db.get(label) {
                Some(id)
            } else {
                tx.execute(
                    "INSERT INTO speakers (history_id, label, embedding) VALUES (?1, ?2, NULL)",
                    params![history_id, label],
                )?;
                let id = tx.last_insert_rowid();
                label_to_db.insert(label.clone(), id);
                Some(id)
            }
        } else if let Some(local) = seg.speaker_id {
            if let Some(&id) = local_to_db.get(&local) {
                Some(id)
            } else {
                let generated = format!("Speaker {}", local_to_db.len() + 1);
                tx.execute(
                    "INSERT INTO speakers (history_id, label, embedding) VALUES (?1, ?2, NULL)",
                    params![history_id, generated],
                )?;
                let id = tx.last_insert_rowid();
                local_to_db.insert(local, id);
                Some(id)
            }
        } else {
            None
        };
        // ponytail: `confidence` is NULL until the ASR pass threads it through (Phase D).
        tx.execute(
            "INSERT INTO transcription_segments
                (history_id, speaker_id, start_ms, end_ms, text, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![history_id, db_speaker, seg.start_ms, seg.end_ms, &seg.text],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Read a history entry's speaker-attributed segments, ordered by start time.
fn read_segments(conn: &Connection, history_id: i64) -> Result<Vec<PersistedSegment>> {
    let mut stmt = conn.prepare(
        "SELECT s.start_ms, s.end_ms, sp.label, s.text
           FROM transcription_segments s
           LEFT JOIN speakers sp ON sp.id = s.speaker_id
          WHERE s.history_id = ?1
          ORDER BY s.start_ms, s.id",
    )?;
    let rows = stmt
        .query_map(params![history_id], |row| {
            Ok(PersistedSegment {
                start_ms: row.get(0)?,
                end_ms: row.get(1)?,
                speaker_label: row.get(2)?,
                text: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Flip every row still marked `transcribing` to `failed`. Used once at startup: such a row
/// means a `finalize` died with the process and will never resume, so without this it would
/// pulse "transcribing" forever. Returns how many rows were healed.
fn fail_stale_transcribing(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE transcription_history SET status = 'failed' WHERE status = 'transcribing'",
        [],
    )?)
}

#[cfg(test)]
mod segment_persistence_tests {
    use super::*;

    fn in_memory_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .unwrap();
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text)
             VALUES ('s.wav', 0, 0, 't', 'full transcript')",
            [],
        )
        .unwrap();
        conn
    }

    fn seg(start_ms: i64, end_ms: i64, speaker_id: Option<i64>, text: &str) -> TimedSegment {
        TimedSegment {
            start_ms,
            end_ms,
            speaker_id,
            speaker_label: None,
            text: text.into(),
        }
    }

    fn labeled(start_ms: i64, end_ms: i64, label: &str, text: &str) -> TimedSegment {
        TimedSegment {
            start_ms,
            end_ms,
            speaker_id: None,
            speaker_label: Some(label.into()),
            text: text.into(),
        }
    }

    #[test]
    fn round_trips_segments_and_reuses_a_speaker_label() {
        let mut conn = in_memory_db();
        let segments = vec![
            seg(0, 1000, Some(0), "ciao"),
            seg(1000, 2000, Some(1), "come va"),
            seg(2000, 3000, Some(0), "bene"),
        ];
        write_segments(&mut conn, 1, &segments).unwrap();

        let got = read_segments(&conn, 1).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].speaker_label.as_deref(), Some("Speaker 1"));
        assert_eq!(got[1].speaker_label.as_deref(), Some("Speaker 2"));
        assert_eq!(got[2].speaker_label.as_deref(), Some("Speaker 1")); // local 0 reused
        assert_eq!(got[0].text, "ciao");

        let speakers: i64 = conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(speakers, 2); // exactly two distinct speakers persisted
    }

    #[test]
    fn status_column_defaults_to_done_and_round_trips_every_variant() {
        // in_memory_db() inserts one row via migration #6's column default (no `status` listed).
        let conn = in_memory_db();
        let stored: String = conn
            .query_row(
                "SELECT status FROM transcription_history WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "done");
        assert_eq!(TranscriptionStatus::from_db(&stored), TranscriptionStatus::Done);

        for st in [
            TranscriptionStatus::Transcribing,
            TranscriptionStatus::Done,
            TranscriptionStatus::Failed,
        ] {
            assert_eq!(TranscriptionStatus::from_db(st.as_db()), st);
        }
        // Unknown / future values degrade to Done rather than panicking the read path.
        assert_eq!(TranscriptionStatus::from_db("garbage"), TranscriptionStatus::Done);
    }

    #[test]
    fn stale_transcribing_rows_are_healed_to_failed_at_startup() {
        // in_memory_db() seeds one 'done' row; add a stuck 'transcribing' one.
        let conn = in_memory_db();
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text, status)
             VALUES ('stuck.wav', 0, 0, 't', '', 'transcribing')",
            [],
        )
        .unwrap();

        let healed = fail_stale_transcribing(&conn).unwrap();
        assert_eq!(healed, 1);

        let still_stuck: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transcription_history WHERE status = 'transcribing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_stuck, 0);
        // The pre-existing 'done' row is left alone — only stuck rows are touched.
        let done: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transcription_history WHERE status = 'done'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(done, 1);
    }

    #[test]
    fn explicit_label_is_its_own_speaker_independent_of_diarizer_numbering() {
        // A dual-stream session: "Me" (mic) interleaved with two diarized remote speakers.
        let mut conn = in_memory_db();
        let segments = vec![
            labeled(0, 1000, "Me", "ciao a tutti"),
            seg(1000, 2000, Some(0), "buongiorno"),
            labeled(2000, 3000, "Me", "iniziamo"),
            seg(3000, 4000, Some(1), "perfetto"),
        ];
        write_segments(&mut conn, 1, &segments).unwrap();

        let got = read_segments(&conn, 1).unwrap();
        let labels: Vec<_> = got.iter().map(|s| s.speaker_label.as_deref()).collect();
        // "Me" is reused (one row), remote speakers number independently as Speaker 1 / 2.
        assert_eq!(
            labels,
            vec![Some("Me"), Some("Speaker 1"), Some("Me"), Some("Speaker 2")]
        );
        let speakers: i64 = conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(speakers, 3); // Me + Speaker 1 + Speaker 2
    }

    #[test]
    fn unknown_speaker_is_stored_as_null_and_creates_no_speaker_row() {
        let mut conn = in_memory_db();
        write_segments(&mut conn, 1, &[seg(0, 1000, None, "boh")]).unwrap();

        let got = read_segments(&conn, 1).unwrap();
        assert_eq!(got[0].speaker_label, None);
        assert_eq!(got[0].text, "boh");

        let speakers: i64 = conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(speakers, 0);
    }

    #[test]
    fn deleting_the_history_entry_cascades_to_segments_and_speakers() {
        let mut conn = in_memory_db();
        write_segments(&mut conn, 1, &[seg(0, 1000, Some(0), "x")]).unwrap();

        conn.execute("DELETE FROM transcription_history WHERE id = 1", [])
            .unwrap();

        let segs: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcription_segments", [], |r| r.get(0))
            .unwrap();
        let spk: i64 = conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))
            .unwrap();
        assert_eq!((segs, spk), (0, 0)); // ON DELETE CASCADE actually fires
    }
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        // Fase 2: enforce foreign keys so deleting a history entry cascades to its
        // speakers/segments. SQLite defaults this OFF per connection. No-op for the
        // pre-existing FK-free tables.
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(conn)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            status: TranscriptionStatus::from_db(&row.get::<_, String>("status")?),
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Save a new, fully-transcribed history entry (the one-shot dictation path).
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        self.insert_entry(
            file_name,
            transcription_text,
            TranscriptionStatus::Done,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
        )
    }

    /// Save a placeholder row in `Transcribing` state for a long-form session, so it shows
    /// in History the moment the user stops — before the (slow) transcription completes.
    /// `finalize` then calls `update_transcription(..)` to fill it in and flip the status.
    pub fn save_pending_entry(&self, file_name: String) -> Result<HistoryEntry> {
        self.insert_entry(
            file_name,
            String::new(),
            TranscriptionStatus::Transcribing,
            false,
            None,
            None,
        )
    }

    fn insert_entry(
        &self,
        file_name: String,
        transcription_text: String,
        status: TranscriptionStatus,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &file_name,
                timestamp,
                false,
                &title,
                &transcription_text,
                &post_processed_text,
                &post_process_prompt,
                post_process_requested,
                status.as_db(),
            ],
        )?;

        let entry = HistoryEntry {
            id: conn.last_insert_rowid(),
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
            status,
        };

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// Persist diarized, speaker-attributed segments for an existing history entry.
    /// `history_id` is the id returned by `save_entry`. See `write_segments`.
    pub fn save_segments(&self, history_id: i64, segments: &[TimedSegment]) -> Result<()> {
        let mut conn = self.get_connection()?;
        write_segments(&mut conn, history_id, segments)
    }

    /// Read the speaker-attributed segments for a history entry (for the timeline UI).
    pub fn get_segments(&self, history_id: i64) -> Result<Vec<PersistedSegment>> {
        let conn = self.get_connection()?;
        read_segments(&conn, history_id)
    }

    /// Self-healing: at startup, mark any session left mid-`transcribing` (its finalize died
    /// with a previous process) as `failed`, so it never pulses "transcribing" forever and the
    /// History retry icon can re-run it. Call once before `recover_interrupted`.
    pub fn fail_stale_transcribing(&self) -> Result<usize> {
        let conn = self.get_connection()?;
        let healed = fail_stale_transcribing(&conn)?;
        if healed > 0 {
            info!("Healed {healed} stale transcribing session(s) at startup");
        }
        Ok(healed)
    }

    /// Update an existing history entry with new transcription results and flip its status.
    /// Used by the retry command (→ `Done`) and by long-form session finalize, which fills
    /// in the placeholder row created by `save_pending_entry` (→ `Done`/`Failed`).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        status: TranscriptionStatus,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3,
                 status = ?4
             WHERE id = ?5",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                status.as_db(),
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn
            .query_row(
                "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, status
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                return Ok(());
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                return self.cleanup_by_count(limit);
            }
            _ => {
                // Use time-based logic
                return self.cleanup_by_time(retention_period);
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            // Delete database entry
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;

            // Delete WAV file
            let file_path = self.recordings_dir.join(file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, status
                     FROM transcription_history
                     WHERE id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, status
                     FROM transcription_history
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, status
                     FROM transcription_history
                     ORDER BY id DESC",
                )?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                status
             FROM transcription_history
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                status
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                status
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first
            let file_path = self.get_audio_file_path(&entry.file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", entry.file_name, e);
                    // Continue with database deletion even if file deletion fails
                }
            }
        }

        // Delete from database
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'done'
            );",
        )
        .expect("create transcription_history table");
        conn
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("handy-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }
}
