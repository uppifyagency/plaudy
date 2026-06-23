import React, { useCallback, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { readFile } from "@tauri-apps/plugin-fs";
import {
  AudioLines,
  Check,
  ChevronDown,
  Copy,
  FileText,
  FolderOpen,
  Mic,
  RotateCcw,
  Star,
  Trash2,
  Users,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  events,
  type HistoryEntry,
  type HistoryUpdatePayload,
  type PersistedSegment,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { formatDateTime } from "@/utils/dateFormat";
import { AudioPlayer } from "../../ui/AudioPlayer";
import { Button } from "../../ui/Button";

const IconButton: React.FC<{
  onClick: () => void;
  title: string;
  disabled?: boolean;
  active?: boolean;
  children: React.ReactNode;
}> = ({ onClick, title, disabled, active, children }) => (
  <button
    onClick={onClick}
    disabled={disabled}
    className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${
      active
        ? "text-logo-primary hover:text-logo-primary/80"
        : "text-text/50 hover:text-logo-primary"
    }`}
    title={title}
  >
    {children}
  </button>
);

const PAGE_SIZE = 30;

interface OpenRecordingsButtonProps {
  onClick: () => void;
  label: string;
}

const OpenRecordingsButton: React.FC<OpenRecordingsButtonProps> = ({
  onClick,
  label,
}) => (
  <Button
    onClick={onClick}
    variant="secondary"
    size="sm"
    className="flex items-center gap-2"
    title={label}
  >
    <FolderOpen className="w-4 h-4" />
    <span>{label}</span>
  </Button>
);

export const HistorySettings: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<HistoryEntry[]>([]);
  const loadingRef = useRef(false);

  // Keep ref in sync for use in IntersectionObserver callback
  useEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  const loadPage = useCallback(async (cursor?: number) => {
    const isFirstPage = cursor === undefined;
    if (!isFirstPage && loadingRef.current) return;
    loadingRef.current = true;

    if (isFirstPage) setLoading(true);

    try {
      const result = await commands.getHistoryEntries(
        cursor ?? null,
        PAGE_SIZE,
      );
      if (result.status === "ok") {
        const { entries: newEntries, has_more } = result.data;
        setEntries((prev) =>
          isFirstPage ? newEntries : [...prev, ...newEntries],
        );
        setHasMore(has_more);
      }
    } catch (error) {
      console.error("Failed to load history entries:", error);
    } finally {
      setLoading(false);
      loadingRef.current = false;
    }
  }, []);

  // Initial load
  useEffect(() => {
    loadPage();
  }, [loadPage]);

  // Infinite scroll via IntersectionObserver
  useEffect(() => {
    if (loading) return;

    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;

    const observer = new IntersectionObserver(
      (observerEntries) => {
        const first = observerEntries[0];
        if (first.isIntersecting) {
          const lastEntry = entriesRef.current[entriesRef.current.length - 1];
          if (lastEntry) {
            loadPage(lastEntry.id);
          }
        }
      },
      { threshold: 0 },
    );

    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, hasMore, loadPage]);

  // Listen for new entries added from the transcription pipeline
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload: HistoryUpdatePayload = event.payload;
      if (payload.action === "added") {
        setEntries((prev) => [payload.entry, ...prev]);
      } else if (payload.action === "updated") {
        setEntries((prev) =>
          prev.map((e) => (e.id === payload.entry.id ? payload.entry : e)),
        );
      }
      // "deleted" and "toggled" are handled by optimistic updates only,
      // so we intentionally ignore them here to avoid double-mutation.
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const toggleSaved = async (id: number) => {
    // Optimistic update
    setEntries((prev) =>
      prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
    );
    try {
      const result = await commands.toggleHistoryEntrySaved(id);
      if (result.status !== "ok") {
        // Revert on failure
        setEntries((prev) =>
          prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
        );
      }
    } catch (error) {
      console.error("Failed to toggle saved status:", error);
      // Revert on failure
      setEntries((prev) =>
        prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
      );
    }
  };

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch (error) {
      console.error("Failed to copy to clipboard:", error);
    }
  };

  const getAudioUrl = useCallback(
    async (fileName: string) => {
      try {
        const result = await commands.getAudioFilePath(fileName);
        if (result.status === "ok") {
          if (osType === "linux") {
            const fileData = await readFile(result.data);
            const blob = new Blob([fileData], { type: "audio/wav" });
            return URL.createObjectURL(blob);
          }
          return convertFileSrc(result.data, "asset");
        }
        return null;
      } catch (error) {
        console.error("Failed to get audio file path:", error);
        return null;
      }
    },
    [osType],
  );

  const deleteAudioEntry = async (id: number) => {
    // Optimistically remove
    setEntries((prev) => prev.filter((e) => e.id !== id));
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") {
        // Reload on failure
        loadPage();
      }
    } catch (error) {
      console.error("Failed to delete entry:", error);
      loadPage();
    }
  };

  const retryHistoryEntry = async (id: number) => {
    const result = await commands.retryHistoryEntryTranscription(id);
    if (result.status !== "ok") {
      throw new Error(String(result.error));
    }
  };

  const openRecordingsFolder = async () => {
    try {
      const result = await commands.openRecordingsFolder();
      if (result.status !== "ok") {
        throw new Error(String(result.error));
      }
    } catch (error) {
      console.error("Failed to open recordings folder:", error);
    }
  };

  let content: React.ReactNode;

  if (loading) {
    content = (
      <div className="px-4 py-3 text-center text-text/60">
        {t("settings.history.loading")}
      </div>
    );
  } else if (entries.length === 0) {
    content = (
      <div className="px-4 py-3 text-center text-text/60">
        {t("settings.history.empty")}
      </div>
    );
  } else {
    content = (
      <>
        <div className="space-y-3">
          {entries.map((entry) => (
            <HistoryEntryComponent
              key={entry.id}
              entry={entry}
              onToggleSaved={() => toggleSaved(entry.id)}
              onCopyText={() => copyToClipboard(entry.transcription_text)}
              getAudioUrl={getAudioUrl}
              deleteAudio={deleteAudioEntry}
              retryTranscription={retryHistoryEntry}
            />
          ))}
        </div>
        {/* Sentinel for infinite scroll */}
        <div ref={sentinelRef} className="h-1" />
      </>
    );
  }

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div className="space-y-2">
        <div className="px-4 flex items-center justify-between">
          <div>
            <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
              {t("settings.history.title")}
            </h2>
          </div>
          <OpenRecordingsButton
            onClick={openRecordingsFolder}
            label={t("settings.history.openFolder")}
          />
        </div>
        <div className="overflow-visible">{content}</div>
      </div>
    </div>
  );
};

interface HistoryEntryProps {
  entry: HistoryEntry;
  onToggleSaved: () => void;
  onCopyText: () => void;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
}

/** mm:ss timecode from a millisecond offset. */
function formatTimecode(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

/** Fase 2: speaker-attributed transcript timeline (speaker · time · text per segment). */
const SpeakerTimeline: React.FC<{ segments: PersistedSegment[] }> = ({
  segments,
}) => {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-2 pb-2 select-text">
      {segments.map((seg, i) => (
        <div key={i} className="flex gap-2 text-sm">
          <span className="shrink-0 font-medium text-logo-primary/90">
            {seg.speaker_label ?? t("settings.history.unknownSpeaker")}
          </span>
          <span className="shrink-0 text-text/40 tabular-nums">
            {formatTimecode(seg.start_ms)}
          </span>
          <span className="whitespace-pre-wrap break-words text-text/90">
            {seg.text}
          </span>
        </div>
      ))}
    </div>
  );
};

type SessionSource = "meeting" | "mic" | "system" | "dictation";

const SOURCE_ICON: Record<SessionSource, typeof Mic> = {
  meeting: Users,
  mic: Mic,
  system: AudioLines,
  dictation: FileText,
};

const SOURCE_LABEL_KEY: Record<SessionSource, string> = {
  meeting: "settings.history.sourceMeeting",
  mic: "settings.history.sourceMic",
  system: "settings.history.sourceSystem",
  dictation: "settings.history.sourceDictation",
};

/** Infer the capture source from a session's speaker labels, for the card's icon + badge.
 *  ponytail: "Me" is the literal mic label written by finalize_session (managers/history.rs);
 *  if that label changes there, change it here too. */
function inferSource(speakerLabels: string[]): SessionSource {
  const hasMe = speakerLabels.includes("Me");
  const hasRemote = speakerLabels.some((l) => l !== "Me");
  if (hasMe && hasRemote) return "meeting";
  if (hasMe) return "mic";
  if (hasRemote) return "system";
  return "dictation";
}

/** h:mm:ss (or m:ss under an hour) from a millisecond duration. */
function formatDuration(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

/** Card title: the stored title if set, else the first words of the transcript.
 *  ponytail: real auto-titling is an AI step gated on the provider decision (HANDOFF §12);
 *  this non-AI placeholder ships the card now. */
function deriveTitle(entry: HistoryEntry, fallback: string): string {
  // ponytail: upstream Handy stores the capture timestamp in `title`, so it is not a real topic —
  // we headline the card with the transcript's opening words instead. Real AI topic-titling is the
  // §12 provider step; when it lands, prefer a non-timestamp entry.title here.
  const words = entry.transcription_text.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return fallback;
  const head = words.slice(0, 8).join(" ");
  return words.length > 8 ? `${head}…` : head;
}

/** The transcript region of a card. Expanded: the speaker timeline (with segments) or the flat
 *  transcript. Collapsed: only the live pulse / failed / no-speech state — a normal transcript is
 *  previewed by the card title, so it renders nothing. One place so collapsed + expanded never drift. */
const TranscriptBody: React.FC<{
  transcribing: boolean;
  hasTranscription: boolean;
  hasSegments: boolean;
  segments: PersistedSegment[];
  entry: HistoryEntry;
  collapsed?: boolean;
}> = ({
  transcribing,
  hasTranscription,
  hasSegments,
  segments,
  entry,
  collapsed,
}) => {
  const { t } = useTranslation();

  if (transcribing) {
    return (
      <p
        className="italic text-sm text-text/60"
        style={{ animation: "transcribe-pulse 3s ease-in-out infinite" }}
      >
        <style>{`
          @keyframes transcribe-pulse {
            0%, 100% { color: color-mix(in srgb, var(--color-text) 40%, transparent); }
            50% { color: color-mix(in srgb, var(--color-text) 90%, transparent); }
          }
        `}</style>
        {t("settings.history.transcribing")}
      </p>
    );
  }

  // Collapsed: the title already previews a normal transcript, so only surface why a row is empty.
  if (collapsed) {
    if (hasTranscription) return null;
    return (
      <p className="italic text-sm text-text/40">
        {entry.status === "failed"
          ? t("settings.history.transcriptionFailed")
          : t("settings.history.noSpeech")}
      </p>
    );
  }

  if (hasSegments) {
    return <SpeakerTimeline segments={segments} />;
  }

  if (hasTranscription) {
    return (
      <p className="text-sm whitespace-pre-wrap break-words select-text text-text/90">
        {entry.transcription_text}
      </p>
    );
  }

  return (
    <p className="italic text-sm text-text/40">
      {entry.status === "failed"
        ? t("settings.history.transcriptionFailed")
        : t("settings.history.noSpeech")}
    </p>
  );
};

const HistoryEntryComponent: React.FC<HistoryEntryProps> = ({
  entry,
  onToggleSaved,
  onCopyText,
  getAudioUrl,
  deleteAudio,
  retryTranscription,
}) => {
  const { t, i18n } = useTranslation();
  const [showCopied, setShowCopied] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [segments, setSegments] = useState<PersistedSegment[]>([]);

  // ponytail: one cheap indexed query per entry — empty for non-diarized (dictation) entries.
  // If this ever shows up in a profile, add a `has_segments` flag to HistoryEntry to skip it.
  useEffect(() => {
    let active = true;
    commands.getSessionSegments(entry.id).then((res) => {
      if (active && res.status === "ok") {
        setSegments(res.data);
      }
    });
    return () => {
      active = false;
    };
  }, [entry.id]);

  const hasTranscription = entry.transcription_text.trim().length > 0;
  const hasSegments = segments.length > 0;
  // A long-form session row is created in `transcribing` state before its (slow) transcript
  // lands; the manual retry path also pulses. Both share the same "Transcribing…" affordance.
  const transcribing = retrying || entry.status === "transcribing";
  // The cast of the conversation, in first-seen order — shows "Me" + the diarized speakers
  // of a meeting at a glance, so a session reads as a rich result, not just a wall of text.
  const speakers = Array.from(
    new Set(segments.map((s) => s.speaker_label).filter((l): l is string => !!l)),
  );
  const source = inferSource(speakers);
  const SourceIcon = SOURCE_ICON[source];
  const durationMs = segments.length
    ? Math.max(...segments.map((s) => s.end_ms))
    : 0;

  const handleLoadAudio = useCallback(
    () => getAudioUrl(entry.file_name),
    [getAudioUrl, entry.file_name],
  );

  const handleCopyText = () => {
    if (!hasTranscription) {
      return;
    }

    onCopyText();
    setShowCopied(true);
    setTimeout(() => setShowCopied(false), 2000);
  };

  const handleDeleteEntry = async () => {
    try {
      await deleteAudio(entry.id);
    } catch (error) {
      console.error("Failed to delete entry:", error);
      toast.error(t("settings.history.deleteError"));
    }
  };

  const handleRetranscribe = async () => {
    try {
      setRetrying(true);
      await retryTranscription(entry.id);
    } catch (error) {
      console.error("Failed to re-transcribe:", error);
      toast.error(t("settings.history.retranscribeError"));
    } finally {
      setRetrying(false);
    }
  };

  const formattedDate = formatDateTime(String(entry.timestamp), i18n.language);
  const title = deriveTitle(entry, formattedDate);

  return (
    <div
      className={`overflow-hidden rounded-xl border border-mid-gray/20 bg-background ${
        expanded ? "" : "pb-3"
      }`}
    >
      <div className="flex items-start gap-3 px-4 pt-3">
        <span className="mt-0.5 grid h-9 w-9 shrink-0 place-items-center rounded-lg bg-logo-primary/10 text-logo-primary">
          <SourceIcon className="h-4 w-4" />
        </span>

        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          className="min-w-0 flex-1 cursor-pointer text-left"
        >
          <p className="truncate text-sm font-medium">{title}</p>
          <p className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-text/50">
            <span>{formattedDate}</span>
            {durationMs > 0 && (
              <>
                <span aria-hidden>·</span>
                <span className="tabular-nums">{formatDuration(durationMs)}</span>
              </>
            )}
            <span aria-hidden>·</span>
            <span>{t(SOURCE_LABEL_KEY[source])}</span>
          </p>
        </button>

        <IconButton
          onClick={onToggleSaved}
          disabled={retrying}
          active={entry.saved}
          title={
            entry.saved
              ? t("settings.history.unsave")
              : t("settings.history.save")
          }
        >
          <Star
            width={16}
            height={16}
            fill={entry.saved ? "currentColor" : "none"}
          />
        </IconButton>
        <IconButton
          onClick={() => setExpanded((v) => !v)}
          title={
            expanded
              ? t("settings.history.collapse")
              : t("settings.history.expand")
          }
        >
          <ChevronDown
            width={16}
            height={16}
            className={`transition-transform ${expanded ? "rotate-180" : ""}`}
          />
        </IconButton>
      </div>

      {speakers.length > 0 && !transcribing && (
        <div className="flex flex-wrap gap-1.5 px-4 pt-2">
          {speakers.map((s) => (
            <span
              key={s}
              className="rounded-full bg-logo-primary/10 px-2 py-0.5 text-xs font-medium text-logo-primary/90"
            >
              {s}
            </span>
          ))}
        </div>
      )}

      {!expanded && (transcribing || !hasTranscription) && (
        <div className="px-4 pt-2">
          <TranscriptBody
            transcribing={transcribing}
            hasTranscription={hasTranscription}
            hasSegments={hasSegments}
            segments={segments}
            entry={entry}
            collapsed
          />
        </div>
      )}

      {expanded && (
        <div className="mt-2 flex flex-col gap-3 border-t border-mid-gray/10 px-4 py-3">
          <TranscriptBody
            transcribing={transcribing}
            hasTranscription={hasTranscription}
            hasSegments={hasSegments}
            segments={segments}
            entry={entry}
          />
          <AudioPlayer onLoadRequest={handleLoadAudio} className="w-full" />
          <div className="flex items-center justify-end">
            <IconButton
              onClick={handleCopyText}
              disabled={!hasTranscription || retrying}
              title={t("settings.history.copyToClipboard")}
            >
              {showCopied ? (
                <Check width={16} height={16} />
              ) : (
                <Copy width={16} height={16} />
              )}
            </IconButton>
            <IconButton
              onClick={handleRetranscribe}
              disabled={retrying}
              title={t("settings.history.retranscribe")}
            >
              <RotateCcw
                width={16}
                height={16}
                style={
                  retrying
                    ? { animation: "spin 1s linear infinite reverse" }
                    : undefined
                }
              />
            </IconButton>
            <IconButton
              onClick={handleDeleteEntry}
              disabled={retrying}
              title={t("settings.history.delete")}
            >
              <Trash2 width={16} height={16} />
            </IconButton>
          </div>
        </div>
      )}
    </div>
  );
};
