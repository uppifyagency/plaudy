import type { HistoryEntry, TranscriptionStatus } from "@/bindings";

/**
 * Lifecycle of a history row's transcript. Alias of the generated
 * `TranscriptionStatus` binding ("transcribing" | "done" | "failed") so UI
 * code has one named type + predicates instead of raw string comparisons.
 */
export type EntryStatus = TranscriptionStatus;

export const isTranscribing = (entry: Pick<HistoryEntry, "status">): boolean =>
  entry.status === "transcribing";

export const isFailed = (entry: Pick<HistoryEntry, "status">): boolean =>
  entry.status === "failed";

/**
 * A completed row that produced no transcript = legitimate silence ("no speech detected"),
 * a terminal state — not a failure and not still transcribing. The UI reads it honestly
 * instead of showing a bare timestamp.
 */
export const isSilent = (
  entry: Pick<HistoryEntry, "status" | "transcription_text">,
): boolean => entry.status === "done" && entry.transcription_text.trim() === "";
