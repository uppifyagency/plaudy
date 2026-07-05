import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { listSessions, getSession, searchSessions } from "./db";
import { createDb, seedSessions } from "./fixture";

function seed(): Database {
  const db = createDb();
  seedSessions(db);
  return db;
}

function insertSession(db: Database, id: number, text: string, timestamp = id * 1000): void {
  db.run(
    "INSERT INTO transcription_history (id, file_name, timestamp, title, transcription_text, status) VALUES (?, ?, ?, ?, ?, 'done')",
    [id, `${id}.wav`, timestamp, `Session ${id}`, text],
  );
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

test("searchSessions treats LIKE metacharacters as literal text", () => {
  const db = seed();
  insertSession(db, 3, "sconto del 100% applicato subito");
  insertSession(db, 4, "100 giorni di prova gratuita");
  insertSession(db, 5, "variabile snake_case rinominata");
  insertSession(db, 6, "la meccanica del motore"); // "ecc" would match an unescaped 'e_c'
  insertSession(db, 7, "cartella C:\\temp aperta");

  // "%" is not a wildcard: must not match "100 giorni…".
  expect(searchSessions(db, "100%", 10).map((h) => h.id)).toEqual([3]);
  // "_" is not any-single-char: only the literal underscore matches.
  expect(searchSessions(db, "_", 10).map((h) => h.id)).toEqual([5]);
  expect(searchSessions(db, "e_c", 10).map((h) => h.id)).toEqual([5]);
  // A lone backslash is safe (no dangling-escape error) and matches only a literal backslash.
  expect(searchSessions(db, "\\", 10).map((h) => h.id)).toEqual([7]);
});

test("empty database returns [] / null without throwing", () => {
  const db = createDb();
  expect(listSessions(db, 20, 0)).toEqual([]);
  expect(searchSessions(db, "anything", 20)).toEqual([]);
  expect(getSession(db, 1)).toBeNull();
});

test("matchSnippet falls back to the head of the transcript when the hit is only in a segment", () => {
  const db = seed();
  db.run(
    "INSERT INTO transcription_segments (history_id, speaker_id, start_ms, end_ms, text) VALUES (1, 11, 2000, 3000, 'fatturato xyzzy confermato')",
  );
  const hits = searchSessions(db, "xyzzy", 10);
  expect(hits.map((h) => h.id)).toEqual([1]);
  // Term absent from the flat transcript → snippet is the transcript head, not empty.
  expect(hits[0].snippet).toBe("Parliamo del budget per il prossimo trimestre");
});

test("matchSnippet ellipsis logic: match at position 0, in the middle, and at the end", () => {
  const db = createDb();
  insertSession(db, 1, `alpha ${"x".repeat(200)} omega`);
  insertSession(db, 2, `${"y".repeat(100)}needle${"y".repeat(100)}`);

  const head = searchSessions(db, "alpha", 10)[0].snippet;
  expect(head.startsWith("alpha")).toBe(true); // no leading ellipsis at position 0
  expect(head.endsWith("…")).toBe(true);

  const tail = searchSessions(db, "omega", 10)[0].snippet;
  expect(tail.startsWith("…")).toBe(true);
  expect(tail.endsWith("omega")).toBe(true); // no trailing ellipsis at the end

  const mid = searchSessions(db, "needle", 10)[0].snippet;
  expect(mid.startsWith("…")).toBe(true);
  expect(mid.endsWith("…")).toBe(true);
  expect(mid).toContain("needle");
});

test("headSnippet truncates transcripts longer than 160 chars", () => {
  const db = createDb();
  insertSession(db, 1, "z".repeat(300));
  const [row] = listSessions(db, 10, 0);
  expect(row.snippet).toHaveLength(161); // 160 chars + ellipsis
  expect(row.snippet.endsWith("…")).toBe(true);
});

test("a term appearing in several segments of one session yields one deduped hit", () => {
  const db = seed();
  db.run(
    "INSERT INTO transcription_segments (history_id, speaker_id, start_ms, end_ms, text) VALUES (1, 10, 3000, 4000, 'budget di nuovo'), (1, 11, 4000, 5000, 'ancora il budget')",
  );
  expect(searchSessions(db, "budget", 10).map((h) => h.id)).toEqual([1]);
});

test("timestamp is passed through unconverted, in SECONDS", () => {
  const db = createDb();
  const epochSeconds = 1751700000; // 2025-07-05, in seconds
  insertSession(db, 1, "ciao", epochSeconds);
  const [row] = listSessions(db, 1, 0);
  expect(row.timestamp).toBe(epochSeconds);
  expect(getSession(db, 1)!.timestamp).toBe(epochSeconds);
  // Pin the unit: a milliseconds migration (~1.7e12) must break this assert loudly.
  expect(row.timestamp).toBeLessThan(1e11);
});
