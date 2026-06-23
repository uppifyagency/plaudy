import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { listSessions, getSession, searchSessions } from "./db";

// Mirror the app's history.db shape (migrations #1/#5/#6) so the query layer is exercised
// against the real schema without depending on a populated ~/Library DB.
function seed(): Database {
  const db = new Database(":memory:");
  db.run(`
    CREATE TABLE transcription_history (
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
    );
    CREATE TABLE speakers (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      history_id INTEGER NOT NULL,
      label TEXT NOT NULL,
      embedding BLOB
    );
    CREATE TABLE transcription_segments (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      history_id INTEGER NOT NULL,
      speaker_id INTEGER,
      start_ms INTEGER NOT NULL,
      end_ms INTEGER NOT NULL,
      text TEXT NOT NULL,
      confidence REAL
    );
  `);

  // Session 1: a dual-stream meeting with "Me" + one diarized remote speaker.
  db.run(
    "INSERT INTO transcription_history (id, file_name, timestamp, title, transcription_text, status) VALUES (1, 'a.wav', 1000, 'Standup', 'Parliamo del budget per il prossimo trimestre', 'done')",
  );
  db.run("INSERT INTO speakers (id, history_id, label) VALUES (10, 1, 'Me'), (11, 1, 'Speaker 1')");
  db.run(
    "INSERT INTO transcription_segments (history_id, speaker_id, start_ms, end_ms, text) VALUES (1, 10, 0, 1000, 'Parliamo del budget'), (1, 11, 1000, 2000, 'per il prossimo trimestre')",
  );

  // Session 2: a solo note, still transcribing.
  db.run(
    "INSERT INTO transcription_history (id, file_name, timestamp, title, transcription_text, status) VALUES (2, 'b.wav', 2000, 'Nota', '', 'transcribing')",
  );
  return db;
}

test("listSessions returns newest first with snippet and speaker labels", () => {
  const db = seed();
  const rows = listSessions(db, 10, 0);
  expect(rows.map((r) => r.id)).toEqual([2, 1]);
  expect(rows[1].speakers).toEqual(["Me", "Speaker 1"]);
  expect(rows[1].snippet).toContain("budget");
  expect(rows[0].status).toBe("transcribing");
});

test("getSession returns full transcript and ordered speaker segments", () => {
  const db = seed();
  const s = getSession(db, 1)!;
  expect(s.title).toBe("Standup");
  expect(s.segments).toHaveLength(2);
  expect(s.segments[0]).toMatchObject({ speaker: "Me", text: "Parliamo del budget" });
  expect(s.segments[1].speaker).toBe("Speaker 1");
});

test("getSession returns null for an unknown id", () => {
  expect(getSession(seed(), 999)).toBeNull();
});

test("searchSessions matches transcript and segment text, centering the snippet on the hit", () => {
  const db = seed();
  const hits = searchSessions(db, "budget", 10);
  expect(hits.map((h) => h.id)).toEqual([1]);
  expect(hits[0].snippet.toLowerCase()).toContain("budget");

  // A term that only appears in a segment still finds the session.
  expect(searchSessions(db, "trimestre", 10).map((h) => h.id)).toEqual([1]);
  // No false positives.
  expect(searchSessions(db, "zzzznope", 10)).toHaveLength(0);
});
