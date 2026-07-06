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
    // Which capture path produced the row ('dictation' | 'mic' | 'system' | 'meeting').
    // Persisted at creation — the backend KNOWS the source — so the UI stops re-deriving it
    // from magic speaker labels. Empty default = pre-migration row → the UI falls back to
    // its old inference. APPEND-ONLY.
    M::up("ALTER TABLE transcription_history ADD COLUMN source TEXT NOT NULL DEFAULT '';"),
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

    /// Map the stored string back. A corrupted/unknown value must not masquerade as a
    /// successful row — degrading to `Failed` keeps the retry affordance available.
    fn from_db(s: &str) -> Self {
        match s {
            "transcribing" => Self::Transcribing,
            "failed" => Self::Failed,
            "done" => Self::Done,
            _ => Self::Failed,
        }
    }
}

/// Which capture path produced a history row. Persisted at row creation (migration #7);
/// `Unknown` covers pre-migration rows (the `''` column default), for which the UI falls back
/// to its legacy label inference.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum EntrySource {
    Dictation,
    Mic,
    System,
    Meeting,
    Unknown,
}

impl EntrySource {
    fn as_db(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Mic => "mic",
            Self::System => "system",
            Self::Meeting => "meeting",
            Self::Unknown => "",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "dictation" => Self::Dictation,
            "mic" => Self::Mic,
            "system" => Self::System,
            "meeting" => Self::Meeting,
            _ => Self::Unknown,
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
    pub source: EntrySource,
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

/// Lightweight per-entry segment summary for list views: distinct speaker labels (first-
/// appearance order) and the timeline duration. One batched query per History page instead
/// of a full-segment fetch per row (the old N+1 IPC).
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SessionOverview {
    pub history_id: i64,
    pub speakers: Vec<String>,
    pub duration_ms: i64,
}

/// Persist diarized segments for one history entry, in a single transaction. Creates one
/// `speakers` row per distinct diarizer speaker index referenced by the segments (labelled
/// "Speaker N", 1-based in first-seen order), then inserts the segments with the FK to those
/// rows. Segments with `speaker_id: None` are stored with a NULL speaker (graceful "unknown").
///
/// Free function over `&mut Connection` (not a manager method) so it is unit-testable against
/// an in-memory database without a Tauri `AppHandle`.
fn write_segments(conn: &mut Connection, history_id: i64, segments: &[TimedSegment]) -> Result<()> {
    let tx = conn.transaction()?;
    insert_segments(&tx, history_id, segments)?;
    tx.commit()?;
    Ok(())
}

/// A3 consistency invariant: REPLACE an entry's persisted speaker timeline in one transaction —
/// delete the old segments and their speakers, then insert the new ones (an empty `segments`
/// slice is the purge case). The retry path must go through this, never bare inserts: segments
/// persisted by an earlier, partially-failed finalize would otherwise sit next to — and
/// contradict — the freshly re-transcribed text. Atomic, so a crash mid-replace can never leave
/// the old and new timelines interleaved.
fn replace_segments_conn(
    conn: &mut Connection,
    history_id: i64,
    segments: &[TimedSegment],
) -> Result<()> {
    let tx = conn.transaction()?;
    // Segments first (their speaker_id FK is ON DELETE SET NULL, so the reverse order would
    // silently NULL them instead of failing — this order leaves nothing to orphan).
    tx.execute(
        "DELETE FROM transcription_segments WHERE history_id = ?1",
        params![history_id],
    )?;
    tx.execute(
        "DELETE FROM speakers WHERE history_id = ?1",
        params![history_id],
    )?;
    insert_segments(&tx, history_id, segments)?;
    tx.commit()?;
    Ok(())
}

/// The shared insert body of [`write_segments`] / [`replace_segments_conn`], running inside the
/// caller's transaction.
fn insert_segments(tx: &Connection, history_id: i64, segments: &[TimedSegment]) -> Result<()> {
    use std::collections::{BTreeMap, HashMap};
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

/// Ids the AUTOMATIC startup heal may retry: rows still `failed` with an empty transcript whose
/// WAV is still on disk. Semantic (retryable), not symptomatic (failed+empty). Three states are
/// deliberately NOT returned so the heal never re-runs full inference on them at every launch:
///  - `Done` silent rows — "no speech" is a completed, terminal outcome (bug A1);
///  - rows that failed but kept a partial transcript — left to the user's explicit retry;
///  - rows whose recording is gone — unrecoverable; auto-retrying only re-fails each boot.
/// Oldest first. Free function over a borrowed connection + dir so the retryable/terminal
/// boundary is unit-testable without an AppHandle. Manual retry via the UI still works on any row.
fn retryable_ids(conn: &Connection, recordings_dir: &std::path::Path) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id, file_name FROM transcription_history
         WHERE status = 'failed' AND transcription_text = ''
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<(i64, String)>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, file_name)| recordings_dir.join(file_name).exists())
        .map(|(id, _)| id)
        .collect())
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
    fn search_matches_transcript_and_title_case_insensitive() {
        let conn = in_memory_db(); // seeds one row: title 't', text 'full transcript'
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text)
             VALUES ('m.wav', 1, 0, 'quarterly meeting', 'we shipped the sensor')",
            [],
        )
        .unwrap();

        let by_text = HistoryManager::search_entries_conn(&conn, "TRANSCRIPT", 10).unwrap();
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].transcription_text, "full transcript");

        let by_title = HistoryManager::search_entries_conn(&conn, "quarterly", 10).unwrap();
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].title, "quarterly meeting");
    }

    #[test]
    fn search_treats_like_wildcards_as_literals() {
        let conn = in_memory_db();
        // '%' would match everything if unescaped; no row contains a literal '%'.
        assert!(HistoryManager::search_entries_conn(&conn, "%", 10)
            .unwrap()
            .is_empty());
        assert!(HistoryManager::search_entries_conn(&conn, "_", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn replace_segments_swaps_the_old_timeline_for_the_new_one() {
        // A3: a meeting row healed by retry — the old (possibly contradictory) "Me"/"Speaker 1"
        // timeline from a partially-failed finalize must be gone, replaced by the re-diarized one.
        let mut conn = in_memory_db();
        write_segments(
            &mut conn,
            1,
            &[
                labeled(0, 1000, "Me", "old mic text"),
                seg(1000, 2000, Some(0), "old system text"),
            ],
        )
        .unwrap();

        replace_segments_conn(
            &mut conn,
            1,
            &[
                seg(0, 900, Some(0), "hello"),
                seg(900, 1800, Some(1), "world"),
            ],
        )
        .unwrap();

        let got = read_segments(&conn, 1).unwrap();
        assert_eq!(got.len(), 2, "only the new timeline remains");
        assert_eq!(got[0].text, "hello");
        assert_eq!(got[0].speaker_label.as_deref(), Some("Speaker 1"));
        assert_eq!(got[1].speaker_label.as_deref(), Some("Speaker 2"));
        // The stale speaker ROWS are gone too, not just their segments — otherwise the
        // overviews query would keep listing a phantom "Me" chip forever.
        let speakers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM speakers WHERE history_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(speakers, 2);
    }

    #[test]
    fn replace_segments_with_empty_purges_the_stale_timeline() {
        // A flat retry (mic row, or diarization unavailable): honest = text only, no timeline.
        let mut conn = in_memory_db();
        write_segments(&mut conn, 1, &[labeled(0, 1000, "Me", "stale")]).unwrap();

        replace_segments_conn(&mut conn, 1, &[]).unwrap();

        assert!(read_segments(&conn, 1).unwrap().is_empty());
        let speakers: i64 = conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(speakers, 0);
    }

    #[test]
    fn replace_segments_leaves_other_entries_untouched() {
        let mut conn = in_memory_db();
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text)
             VALUES ('other.wav', 1, 0, 't2', 'x')",
            [],
        )
        .unwrap();
        write_segments(&mut conn, 1, &[labeled(0, 500, "Me", "mine")]).unwrap();
        write_segments(&mut conn, 2, &[seg(0, 500, Some(0), "theirs")]).unwrap();

        replace_segments_conn(&mut conn, 1, &[]).unwrap();

        assert!(read_segments(&conn, 1).unwrap().is_empty());
        let other = read_segments(&conn, 2).unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].text, "theirs");
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
    fn overviews_batch_distinct_speakers_and_max_duration_per_entry() {
        let mut conn = in_memory_db(); // migration seeds entry id 1
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text)
             VALUES ('s2.wav', 1, 0, 't2', 'x')",
            [],
        )
        .unwrap();

        let seg = |start_ms, end_ms, speaker_id, label: Option<&str>, text: &str| TimedSegment {
            start_ms,
            end_ms,
            speaker_id,
            speaker_label: label.map(Into::into),
            text: text.into(),
        };
        write_segments(
            &mut conn,
            1,
            &[
                seg(0, 1000, None, Some("Me"), "hi"),
                seg(1000, 2500, Some(0), None, "hello"),
                seg(2500, 4000, Some(0), None, "again"), // same speaker → no duplicate label
            ],
        )
        .unwrap();
        write_segments(&mut conn, 2, &[seg(0, 700, None, None, "unknown voice")]).unwrap();

        let overviews = HistoryManager::overviews_conn(&conn, &[1, 2, 999]).unwrap();

        assert_eq!(overviews.len(), 2, "absent ids simply produce no overview");
        assert_eq!(overviews[0].history_id, 1);
        assert_eq!(overviews[0].speakers, vec!["Me", "Speaker 1"]);
        assert_eq!(overviews[0].duration_ms, 4000);
        assert_eq!(overviews[1].history_id, 2);
        assert!(
            overviews[1].speakers.is_empty(),
            "unknown speakers add no label"
        );
        assert_eq!(overviews[1].duration_ms, 700);

        assert!(HistoryManager::overviews_conn(&conn, &[])
            .unwrap()
            .is_empty());
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
        assert_eq!(
            TranscriptionStatus::from_db(&stored),
            TranscriptionStatus::Done
        );

        for st in [
            TranscriptionStatus::Transcribing,
            TranscriptionStatus::Done,
            TranscriptionStatus::Failed,
        ] {
            assert_eq!(TranscriptionStatus::from_db(st.as_db()), st);
        }
        // Unknown / future values degrade to Failed (not Done): a corrupted status must
        // not present as a successful row, and Failed keeps the retry affordance alive.
        assert_eq!(
            TranscriptionStatus::from_db("garbage"),
            TranscriptionStatus::Failed
        );
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
            .query_row("SELECT COUNT(*) FROM transcription_segments", [], |r| {
                r.get(0)
            })
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
        // This codebase writes from several threads by design (session finalize runs
        // off-thread while dictation/cleanup/deletes hit the main path). Without a busy
        // handler a colliding write fails instantly with SQLITE_BUSY; WAL lets readers
        // proceed under a writer. journal_mode is persistent but setting it is idempotent.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        Ok(conn)
    }

    /// The columns `map_history_entry` reads — one place instead of seven copies of the
    /// SELECT list. Adding a column = this constant + the mapper + a migration.
    const HISTORY_COLUMNS: &'static str = "id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, status, source";

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
            source: EntrySource::from_db(&row.get::<_, String>("source")?),
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
            EntrySource::Dictation,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
        )
    }

    /// Save a placeholder row in `Transcribing` state for a long-form session, so it shows
    /// in History the moment the user stops — before the (slow) transcription completes.
    /// `finalize` then calls `update_transcription(..)` to fill it in and flip the status.
    pub fn save_pending_entry(
        &self,
        file_name: String,
        source: EntrySource,
    ) -> Result<HistoryEntry> {
        self.insert_entry(
            file_name,
            String::new(),
            TranscriptionStatus::Transcribing,
            source,
            false,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_entry(
        &self,
        file_name: String,
        transcription_text: String,
        status: TranscriptionStatus,
        source: EntrySource,
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
                status,
                source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                source.as_db(),
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
            source,
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

    /// Atomically replace (or, with an empty slice, purge) an entry's persisted speaker
    /// timeline — the retry path's consistency invariant. See [`replace_segments_conn`].
    pub fn replace_segments(&self, history_id: i64, segments: &[TimedSegment]) -> Result<()> {
        let mut conn = self.get_connection()?;
        replace_segments_conn(&mut conn, history_id, segments)
    }

    /// Read the speaker-attributed segments for a history entry (for the timeline UI).
    pub fn get_segments(&self, history_id: i64) -> Result<Vec<PersistedSegment>> {
        let conn = self.get_connection()?;
        read_segments(&conn, history_id)
    }

    /// Batched list-view summaries for a page of entries — see [`SessionOverview`].
    /// Entries with no segments are simply absent from the result.
    pub fn get_session_overviews(&self, ids: &[i64]) -> Result<Vec<SessionOverview>> {
        let conn = self.get_connection()?;
        Self::overviews_conn(&conn, ids)
    }

    fn overviews_conn(conn: &Connection, ids: &[i64]) -> Result<Vec<SessionOverview>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT s.history_id, sp.label, s.end_ms
             FROM transcription_segments s
             LEFT JOIN speakers sp ON sp.id = s.speaker_id
             WHERE s.history_id IN ({placeholders})
             ORDER BY s.history_id, s.id"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut map: std::collections::BTreeMap<i64, SessionOverview> =
            std::collections::BTreeMap::new();
        for row in rows {
            let (id, label, end_ms) = row?;
            let overview = map.entry(id).or_insert_with(|| SessionOverview {
                history_id: id,
                speakers: Vec::new(),
                duration_ms: 0,
            });
            if let Some(label) = label {
                if !overview.speakers.contains(&label) {
                    overview.speakers.push(label);
                }
            }
            overview.duration_ms = overview.duration_ms.max(end_ms);
        }
        Ok(map.into_values().collect())
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
        let entry = Self::update_transcription_conn(
            &conn,
            id,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            status,
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

    /// The `transcribing → done/failed` status flip on a borrowed connection
    /// (unit-testable without an AppHandle). Errors when the row doesn't exist.
    fn update_transcription_conn(
        conn: &Connection,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        status: TranscriptionStatus,
    ) -> Result<HistoryEntry> {
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

        let entry = conn.query_row(
            &format!(
                "SELECT {} FROM transcription_history WHERE id = ?1",
                Self::HISTORY_COLUMNS
            ),
            params![id],
            Self::map_history_entry,
        )?;
        Ok(entry)
    }

    /// M4 single-flight CAS on a borrowed connection (unit-testable without an AppHandle):
    /// claim a row for a retry run by flipping it to `transcribing`, but ONLY if it isn't
    /// already `transcribing`. The status column IS the mutex — a double-clicked retry, or a
    /// manual retry racing the startup heal on the same row, both hit this: exactly one wins
    /// (`Ok(true)`), the loser gets `Ok(false)` and must never load audio or start a second
    /// inference run. A `failed` row (the heal's target) and a finished `done` row (a user
    /// re-transcribing a completed recording) can both be claimed; only an in-flight
    /// `transcribing` row is refused — the guard is on the actual in-progress state rather than
    /// on a single source status, so it can't wedge legitimate re-transcription while still
    /// guaranteeing single flight. A missing row also yields `Ok(false)` (0 rows affected).
    fn begin_retry_conn(conn: &Connection, id: i64) -> Result<bool> {
        Ok(conn.execute(
            "UPDATE transcription_history SET status = 'transcribing'
             WHERE id = ?1 AND status != 'transcribing'",
            params![id],
        )? == 1)
    }

    /// Try to claim a history row for a retry run (M4 single-flight). Returns the claimed entry
    /// on success, `None` when the row is already `transcribing` (or gone). On a successful claim
    /// it emits `Updated` so EVERY view shows `transcribing` while the (slow) inference runs —
    /// no longer a stale `failed` in other panes. A crash mid-retry leaves the row
    /// `transcribing`; the startup `fail_stale_transcribing` pass collects it, so no bespoke
    /// recovery is needed here.
    pub fn try_begin_retry(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        if !Self::begin_retry_conn(&conn, id)? {
            return Ok(None);
        }
        let entry = conn.query_row(
            &format!(
                "SELECT {} FROM transcription_history WHERE id = ?1",
                Self::HISTORY_COLUMNS
            ),
            params![id],
            Self::map_history_entry,
        )?;
        self.emit_updated(&entry);
        Ok(Some(entry))
    }

    /// Return a claimed-but-failed retry to `failed` (retryable again), STATUS ONLY so the
    /// row's existing transcript/timeline is preserved. Emits `Updated`. A no-op if the row
    /// vanished mid-retry (deleted): the `WHERE id` UPDATE simply matches nothing, so the row
    /// is never resurrected.
    pub fn mark_retry_failed(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history SET status = 'failed' WHERE id = ?1",
            params![id],
        )?;
        if updated == 1 {
            let entry = conn.query_row(
                &format!(
                    "SELECT {} FROM transcription_history WHERE id = ?1",
                    Self::HISTORY_COLUMNS
                ),
                params![id],
                Self::map_history_entry,
            )?;
            self.emit_updated(&entry);
        }
        Ok(())
    }

    /// Emit the `Updated` history event; a failed emit is logged, never fatal (the DB write
    /// already succeeded — the UI just misses one live refresh).
    fn emit_updated(&self, entry: &HistoryEntry) {
        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            // Don't delete anything
            crate::settings::RecordingRetentionPeriod::Never => Ok(()),
            // Count-based logic with history_limit
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                self.cleanup_by_count(crate::settings::get_history_limit(&self.app_handle))
            }
            // Time-based logic
            _ => self.cleanup_by_time(retention_period),
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;
            // Count row deletions, not file deletions: a row whose WAV was already gone is
            // still an entry removed, and the old file-based count logged "0 cleaned" lies.
            deleted_count += 1;

            let file_path = self.recordings_dir.join(file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                }
            }

            // The list UI must hear about retention deletions too, not only manual ones —
            // otherwise every sweep silently leaves stale rows on screen.
            if let Err(e) = (HistoryUpdatePayload::Deleted { id: *id }).emit(&self.app_handle) {
                error!("Failed to emit history-updated event: {}", e);
            }
        }

        Ok(deleted_count)
    }

    /// Unsaved entries beyond the newest `limit` (count-based retention), on a borrowed
    /// connection so the boundary (`saved = 1` exemption, exactly-`limit` no-op) is testable.
    fn entries_beyond_count(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history
             WHERE saved = 0 ORDER BY timestamp DESC LIMIT -1 OFFSET ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Unsaved entries strictly older than `cutoff_timestamp` (time-based retention).
    fn entries_older_than(conn: &Connection, cutoff_timestamp: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;
        let rows = stmt
            .query_map(params![cutoff_timestamp], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;
        let entries_to_delete = Self::entries_beyond_count(&conn, limit)?;
        drop(conn);
        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;
        if deleted_count > 0 {
            debug!("Cleaned up {} old history entries by count", deleted_count);
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

        let entries_to_delete = Self::entries_older_than(&conn, cutoff_timestamp)?;
        drop(conn);
        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;
        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }
        Ok(())
    }

    /// Substring search over transcript + title, newest first. Pure SQL on a
    /// borrowed connection so it is unit-testable without an AppHandle.
    /// LIKE wildcards in the user's query are escaped → always a literal match.
    pub fn search_entries_conn(
        conn: &Connection,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{}%", escaped);
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM transcription_history
             WHERE transcription_text LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY id DESC
             LIMIT ?2",
            Self::HISTORY_COLUMNS
        ))?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], Self::map_history_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn search_history_entries(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::search_entries_conn(&conn, query, limit.unwrap_or(50).min(100))
    }

    pub fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        Self::get_entries_conn(&conn, cursor, limit)
    }

    /// Cursor pagination on a borrowed connection (unit-testable without an AppHandle).
    /// One query covers all cases: a NULL cursor/limit disables its clause. `limit: None`
    /// means "everything" — expressed as SQLite's `LIMIT -1` (no limit); a present limit is
    /// clamped to 100 and over-fetched by 1 to derive `has_more`.
    fn get_entries_conn(
        conn: &Connection,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let limit = limit.map(|l| l.min(100));
        let fetch = limit.map(|l| (l + 1) as i64).unwrap_or(-1);
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM transcription_history
             WHERE (?1 IS NULL OR id < ?1)
             ORDER BY id DESC
             LIMIT ?2",
            Self::HISTORY_COLUMNS
        ))?;
        let mut entries = stmt
            .query_map(params![cursor, fetch], Self::map_history_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }
        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM transcription_history ORDER BY timestamp DESC LIMIT 1",
            Self::HISTORY_COLUMNS
        ))?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
            Self::HISTORY_COLUMNS
        ))?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;
        let new_saved = Self::toggle_saved_conn(&conn, id)?;
        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    /// Atomic flip on a borrowed connection: one UPDATE, no read-then-write race between two
    /// rapid toggles. Returns the new value; errors when the row doesn't exist.
    fn toggle_saved_conn(conn: &Connection, id: i64) -> Result<bool> {
        conn.query_row(
            "UPDATE transcription_history SET saved = NOT saved WHERE id = ?1 RETURNING saved",
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("History entry {} not found", id))
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    pub fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM transcription_history WHERE id = ?1",
            Self::HISTORY_COLUMNS
        ))?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    /// Does a history row already point at this recording file? Recovery uses it to tell an
    /// already-persisted session (row exists → just clean up the PCM) from an orphaned archive
    /// (WAV written but the crash beat the row → adopt it). A DB error is surfaced, never
    /// swallowed as "no row" — the caller must not fabricate a duplicate on a transient failure.
    pub fn entry_exists_for_file(&self, file_name: &str) -> Result<bool> {
        let conn = self.get_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM transcription_history WHERE file_name = ?1",
            [file_name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Ids the startup heal may retry — see the [`retryable_ids`] free function for the exact
    /// retryable/terminal boundary. Wraps it with this manager's recordings directory.
    pub fn retryable_entry_ids(&self) -> Result<Vec<i64>> {
        let conn = self.get_connection()?;
        retryable_ids(&conn, &self.recordings_dir)
    }

    pub fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // One connection, row first: DELETE ... RETURNING hands back the file name, and the
        // audio file is only removed once the row is gone — a failed DB delete can never
        // orphan-delete the recording it still points to.
        let file_name: Option<String> = conn
            .query_row(
                "DELETE FROM transcription_history WHERE id = ?1 RETURNING file_name",
                params![id],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(file_name) = file_name {
            let file_path = self.get_audio_file_path(&file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", file_name, e);
                }
            }
        }

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
                status TEXT NOT NULL DEFAULT 'done',
                source TEXT NOT NULL DEFAULT ''
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

    fn set_saved(conn: &Connection, id: i64) {
        conn.execute(
            "UPDATE transcription_history SET saved = 1 WHERE id = ?1",
            params![id],
        )
        .expect("mark entry saved");
    }

    fn ids(entries: &[HistoryEntry]) -> Vec<i64> {
        entries.iter().map(|e| e.id).collect()
    }

    #[test]
    fn pagination_over_fetches_to_derive_has_more_and_trims() {
        let conn = setup_conn();
        for ts in 1..=5 {
            insert_entry(&conn, ts, "x", None); // ids 1..=5
        }

        let page1 = HistoryManager::get_entries_conn(&conn, None, Some(2)).unwrap();
        assert_eq!(ids(&page1.entries), vec![5, 4]);
        assert!(page1.has_more);

        let page2 = HistoryManager::get_entries_conn(&conn, Some(4), Some(2)).unwrap();
        assert_eq!(ids(&page2.entries), vec![3, 2]);
        assert!(page2.has_more);

        let page3 = HistoryManager::get_entries_conn(&conn, Some(2), Some(2)).unwrap();
        assert_eq!(ids(&page3.entries), vec![1]);
        assert!(!page3.has_more, "a short final page must not claim more");
    }

    #[test]
    fn pagination_exactly_limit_rows_has_no_more() {
        // The off-by-one magnet: exactly `limit` rows must NOT report has_more.
        let conn = setup_conn();
        for ts in 1..=3 {
            insert_entry(&conn, ts, "x", None);
        }
        let page = HistoryManager::get_entries_conn(&conn, None, Some(3)).unwrap();
        assert_eq!(page.entries.len(), 3);
        assert!(!page.has_more);
    }

    #[test]
    fn pagination_without_limit_returns_everything() {
        let conn = setup_conn();
        for ts in 1..=5 {
            insert_entry(&conn, ts, "x", None);
        }
        let page = HistoryManager::get_entries_conn(&conn, None, None).unwrap();
        assert_eq!(page.entries.len(), 5);
        assert!(!page.has_more);
    }

    #[test]
    fn toggle_saved_flips_atomically_and_errors_on_missing_row() {
        let conn = setup_conn();
        insert_entry(&conn, 1, "x", None);

        assert!(HistoryManager::toggle_saved_conn(&conn, 1).unwrap());
        assert!(!HistoryManager::toggle_saved_conn(&conn, 1).unwrap());
        assert!(HistoryManager::toggle_saved_conn(&conn, 999).is_err());
    }

    #[test]
    fn retention_by_count_spares_saved_and_exact_limit_is_a_noop() {
        let conn = setup_conn();
        for ts in 1..=5 {
            insert_entry(&conn, ts, "x", None); // ids 1..=5, newest = highest ts
        }
        set_saved(&conn, 1); // oldest entry is pinned by the user

        // Exactly `limit` unsaved entries → nothing to delete.
        assert!(HistoryManager::entries_beyond_count(&conn, 4)
            .unwrap()
            .is_empty());

        // limit 2 keeps the 2 newest unsaved (5, 4); 3 and 2 fall off; 1 is saved → exempt.
        let doomed = HistoryManager::entries_beyond_count(&conn, 2).unwrap();
        let doomed_ids: Vec<i64> = doomed.iter().map(|(id, _)| *id).collect();
        assert_eq!(doomed_ids, vec![3, 2]);
    }

    #[test]
    fn retention_by_time_uses_strict_cutoff_and_spares_saved() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "old", None); // id 1
        insert_entry(&conn, 200, "old-but-saved", None); // id 2
        insert_entry(&conn, 300, "at-cutoff", None); // id 3
        set_saved(&conn, 2);

        let doomed = HistoryManager::entries_older_than(&conn, 300).unwrap();
        let doomed_ids: Vec<i64> = doomed.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            doomed_ids,
            vec![1],
            "saved rows are exempt; timestamp == cutoff survives (strict <)"
        );
    }

    /// Insert a row with explicit file name, status and transcript — the axes `retryable_ids`
    /// discriminates on.
    fn insert_row(conn: &Connection, file_name: &str, status: &str, text: &str) {
        conn.execute(
            "INSERT INTO transcription_history (file_name, timestamp, saved, title, transcription_text, status)
             VALUES (?1, 0, 0, 't', ?2, ?3)",
            params![file_name, text, status],
        )
        .expect("insert row");
    }

    #[test]
    fn retryable_ids_selects_only_recoverable_failed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_conn();

        // 1: failed+empty, WAV present  → retryable (the genuine "transcription was lost" case).
        insert_row(&conn, "present.wav", "failed", "");
        std::fs::write(dir.path().join("present.wav"), b"x").unwrap();
        // 2: done+empty  → terminal silence (bug A1); never re-inferred.
        insert_row(&conn, "silent.wav", "done", "");
        std::fs::write(dir.path().join("silent.wav"), b"x").unwrap();
        // 3: failed but kept a partial transcript → left to the user's explicit retry.
        insert_row(&conn, "partial.wav", "failed", "half a sentence");
        std::fs::write(dir.path().join("partial.wav"), b"x").unwrap();
        // 4: failed+empty but recording is GONE → unrecoverable; must not loop every boot.
        insert_row(&conn, "gone.wav", "failed", "");
        // 5: still transcribing → not a failure at all.
        insert_row(&conn, "live.wav", "transcribing", "");
        std::fs::write(dir.path().join("live.wav"), b"x").unwrap();

        let ids = retryable_ids(&conn, dir.path()).unwrap();
        assert_eq!(
            ids,
            vec![1],
            "only the failed+empty row with a live WAV is retryable"
        );
    }

    #[test]
    fn silence_marked_done_drops_out_of_the_retry_selector() {
        // The A1 loop closes end-to-end: a failed+empty silent row is retryable exactly once;
        // once the retry marks it `Done` (what commands::history does for an empty transcript),
        // it is no longer selected.
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_conn();
        insert_row(&conn, "s.wav", "failed", "");
        std::fs::write(dir.path().join("s.wav"), b"x").unwrap();
        assert_eq!(retryable_ids(&conn, dir.path()).unwrap(), vec![1]);

        HistoryManager::update_transcription_conn(
            &conn,
            1,
            String::new(),
            None,
            None,
            TranscriptionStatus::Done,
        )
        .unwrap();

        assert!(
            retryable_ids(&conn, dir.path()).unwrap().is_empty(),
            "a completed silent row is terminal — never auto-retried again"
        );
    }

    #[test]
    fn begin_retry_cas_claims_a_failed_row_exactly_once() {
        // M4: the status column is the mutex. First claim wins and flips the row to
        // `transcribing`; a second claim (double-click, or heal racing a manual retry) is
        // refused — so a full inference run + full-audio Vec never happens twice.
        let conn = setup_conn();
        insert_row(&conn, "r.wav", "failed", "");

        assert!(HistoryManager::begin_retry_conn(&conn, 1).unwrap());
        let status: String = conn
            .query_row(
                "SELECT status FROM transcription_history WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "transcribing");

        assert!(
            !HistoryManager::begin_retry_conn(&conn, 1).unwrap(),
            "a second claim while transcribing must lose the CAS"
        );
    }

    #[test]
    fn a_failed_retry_returns_to_failed_and_can_be_claimed_again() {
        // M4 point 3: a transient failure reverts the row to `failed` (status only), which is
        // claimable once more — a failed retry never wedges the row out of the retry path.
        let conn = setup_conn();
        insert_row(&conn, "r.wav", "failed", "");
        assert!(HistoryManager::begin_retry_conn(&conn, 1).unwrap());

        // The `mark_retry_failed` revert path (status only).
        conn.execute(
            "UPDATE transcription_history SET status = 'failed' WHERE id = 1",
            [],
        )
        .unwrap();

        assert!(HistoryManager::begin_retry_conn(&conn, 1).unwrap());
    }

    #[test]
    fn begin_retry_allows_re_transcribing_a_done_row_but_never_an_in_flight_one() {
        // The guard is on the in-progress state, not the source status: a finished row can be
        // re-transcribed (e.g. after enabling diarization), yet an in-flight row is still
        // refused — single flight holds either way.
        let conn = setup_conn();
        insert_row(&conn, "d.wav", "done", "final text");

        assert!(HistoryManager::begin_retry_conn(&conn, 1).unwrap());
        assert!(!HistoryManager::begin_retry_conn(&conn, 1).unwrap());
    }

    #[test]
    fn begin_retry_on_a_missing_row_is_refused() {
        let conn = setup_conn();
        assert!(!HistoryManager::begin_retry_conn(&conn, 42).unwrap());
    }

    #[test]
    fn update_transcription_fills_row_flips_status_and_errors_on_missing() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "", None);
        conn.execute(
            "UPDATE transcription_history SET status = 'transcribing' WHERE id = 1",
            [],
        )
        .unwrap();

        let entry = HistoryManager::update_transcription_conn(
            &conn,
            1,
            "final text".into(),
            None,
            None,
            TranscriptionStatus::Done,
        )
        .unwrap();
        assert_eq!(entry.transcription_text, "final text");
        assert_eq!(entry.status, TranscriptionStatus::Done);

        assert!(
            HistoryManager::update_transcription_conn(
                &conn,
                999,
                String::new(),
                None,
                None,
                TranscriptionStatus::Done,
            )
            .is_err(),
            "updating a missing row must surface, not silently no-op"
        );
    }
}
