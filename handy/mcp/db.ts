// Read-only query layer over Plaude Local's history.db (the Tauri app owns all writes).
// Pure functions taking a bun:sqlite Database so they unit-test against a temp DB.
import { Database } from "bun:sqlite";
import { homedir } from "os";
import { join } from "path";

/** Default location of the app's SQLite DB; override with PLAUDE_DB for tests / portability. */
export function defaultDbPath(): string {
  return (
    process.env.PLAUDE_DB ??
    join(homedir(), "Library", "Application Support", "com.pais.handy", "history.db")
  );
}

/** Open the DB read-only — this process never writes, so it can never corrupt a recording. */
export function openDb(path: string): Database {
  return new Database(path, { readonly: true });
}

const SNIPPET_LEN = 160;
const CONTEXT = 60;

function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

function headSnippet(text: string): string {
  const t = oneLine(text);
  return t.length > SNIPPET_LEN ? t.slice(0, SNIPPET_LEN) + "…" : t;
}

/** A snippet centered on the first occurrence of `query`, else the head of the text. */
function matchSnippet(text: string, query: string): string {
  const t = oneLine(text);
  const i = t.toLowerCase().indexOf(query.toLowerCase());
  if (i < 0) return headSnippet(t);
  const start = Math.max(0, i - CONTEXT);
  const end = Math.min(t.length, i + query.length + CONTEXT);
  return (start > 0 ? "…" : "") + t.slice(start, end) + (end < t.length ? "…" : "");
}

function speakerLabels(db: Database, historyId: number): string[] {
  const rows = db
    .query("SELECT label FROM speakers WHERE history_id = ? ORDER BY id")
    .all(historyId) as { label: string }[];
  return rows.map((r) => r.label);
}

export interface SessionSummary {
  id: number;
  title: string;
  timestamp: number;
  status: string;
  snippet: string;
  speakers: string[];
}

export function listSessions(db: Database, limit = 20, offset = 0): SessionSummary[] {
  const rows = db
    .query(
      `SELECT id, title, timestamp, status, transcription_text
         FROM transcription_history
        ORDER BY id DESC
        LIMIT ? OFFSET ?`,
    )
    .all(limit, offset) as Row[];
  return rows.map((r) => ({
    id: r.id,
    title: r.title,
    timestamp: r.timestamp,
    status: r.status,
    snippet: headSnippet(r.transcription_text ?? ""),
    speakers: speakerLabels(db, r.id),
  }));
}

export interface Segment {
  start_ms: number;
  end_ms: number;
  speaker: string | null;
  text: string;
}

export interface SessionDetail extends SessionSummary {
  text: string;
  segments: Segment[];
}

export function getSession(db: Database, id: number): SessionDetail | null {
  const r = db
    .query(
      `SELECT id, title, timestamp, status, transcription_text
         FROM transcription_history WHERE id = ?`,
    )
    .get(id) as Row | null;
  if (!r) return null;
  const segments = db
    .query(
      `SELECT s.start_ms AS start_ms, s.end_ms AS end_ms, sp.label AS speaker, s.text AS text
         FROM transcription_segments s
         LEFT JOIN speakers sp ON sp.id = s.speaker_id
        WHERE s.history_id = ?
        ORDER BY s.start_ms, s.id`,
    )
    .all(id) as Segment[];
  return {
    id: r.id,
    title: r.title,
    timestamp: r.timestamp,
    status: r.status,
    snippet: headSnippet(r.transcription_text ?? ""),
    speakers: speakerLabels(db, id),
    text: r.transcription_text ?? "",
    segments,
  };
}

export interface SearchHit {
  id: number;
  title: string;
  timestamp: number;
  snippet: string;
}

/** Find sessions whose flat transcript OR any speaker segment contains `query` (case-insensitive). */
export function searchSessions(db: Database, query: string, limit = 20): SearchHit[] {
  // Escape LIKE metacharacters so "100%" or "snake_case" mean the literal text, not wildcards.
  const like = `%${query.replace(/[\\%_]/g, "\\$&")}%`;
  const rows = db
    .query(
      `SELECT DISTINCT h.id AS id, h.title AS title, h.timestamp AS timestamp,
                       h.transcription_text AS transcription_text
         FROM transcription_history h
         LEFT JOIN transcription_segments s ON s.history_id = h.id
        WHERE h.transcription_text LIKE ? ESCAPE '\\' OR s.text LIKE ? ESCAPE '\\'
        ORDER BY h.id DESC
        LIMIT ?`,
    )
    .all(like, like, limit) as Row[];
  return rows.map((r) => ({
    id: r.id,
    title: r.title,
    timestamp: r.timestamp,
    snippet: matchSnippet(r.transcription_text ?? "", query),
  }));
}

interface Row {
  id: number;
  title: string;
  timestamp: number;
  status: string;
  transcription_text: string | null;
}
