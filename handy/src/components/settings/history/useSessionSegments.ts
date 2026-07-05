import { useEffect, useMemo, useState } from "react";
import { AudioLines, FileText, Mic, Users } from "lucide-react";
import { commands, type HistoryEntry, type PersistedSegment } from "@/bindings";

export type SessionSource = "meeting" | "mic" | "system" | "dictation";

export const SOURCE_ICON: Record<SessionSource, typeof Mic> = {
  meeting: Users,
  mic: Mic,
  system: AudioLines,
  dictation: FileText,
};

export const SOURCE_LABEL_KEY: Record<SessionSource, string> = {
  meeting: "settings.history.sourceMeeting",
  mic: "settings.history.sourceMic",
  system: "settings.history.sourceSystem",
  dictation: "settings.history.sourceDictation",
};

/** Legacy fallback for pre-migration rows (`entry.source === "unknown"`): infer the capture
 *  source from a session's speaker labels.
 *  ponytail: "Me" is the literal mic label written by finalize_session (managers/history.rs);
 *  if that label changes there, change it here too. */
export function inferSource(speakerLabels: string[]): SessionSource {
  const hasMe = speakerLabels.includes("Me");
  const hasRemote = speakerLabels.some((l) => l !== "Me");
  if (hasMe && hasRemote) return "meeting";
  if (hasMe) return "mic";
  if (hasRemote) return "system";
  return "dictation";
}

/** Card title: the first words of the transcript (non-AI placeholder, HANDOFF §12). */
export function deriveTitle(entry: HistoryEntry, fallback: string): string {
  const words = entry.transcription_text.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return fallback;
  const head = words.slice(0, 8).join(" ");
  return words.length > 8 ? `${head}…` : head;
}

/**
 * One entry's segments + the facts derived from them (speakers, source,
 * duration). The source is the entry's persisted `source` column; only
 * pre-migration rows ("unknown") fall back to speaker-label inference.
 * Used by the detail pane only (it needs the full timeline); the list rows
 * get their derived facts from the batched `get_session_overviews` command.
 * Refetches when `status` flips so a finished transcription fills in.
 */
export function useSessionSegments(entry: HistoryEntry) {
  const [segments, setSegments] = useState<PersistedSegment[]>([]);
  const { id: entryId, status } = entry;

  useEffect(() => {
    let active = true;
    setSegments([]);
    commands.getSessionSegments(entryId).then((res) => {
      if (active && res.status === "ok") setSegments(res.data);
    });
    return () => {
      active = false;
    };
  }, [entryId, status]);

  const speakers = useMemo(
    () =>
      Array.from(
        new Set(
          segments.map((s) => s.speaker_label).filter((l): l is string => !!l),
        ),
      ),
    [segments],
  );
  const source: SessionSource =
    entry.source === "unknown" ? inferSource(speakers) : entry.source;
  const durationMs = segments.length
    ? Math.max(...segments.map((s) => s.end_ms))
    : 0;

  return { segments, speakers, source, durationMs };
}
