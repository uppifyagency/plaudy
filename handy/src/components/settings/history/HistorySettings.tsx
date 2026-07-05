import React, { useCallback, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { readFile } from "@tauri-apps/plugin-fs";
import {
  AudioLines,
  Check,
  Copy,
  FileText,
  FolderOpen,
  Mic,
  RotateCcw,
  Search,
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
        ? "text-accent hover:text-accent/80"
        : "text-text/50 hover:text-accent"
    }`}
    title={title}
  >
    {children}
  </button>
);

const PAGE_SIZE = 30;

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
          <span className="shrink-0 font-medium text-accent/90">
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

/** Card title: the first words of the transcript (non-AI placeholder, HANDOFF §12). */
function deriveTitle(entry: HistoryEntry, fallback: string): string {
  const words = entry.transcription_text.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return fallback;
  const head = words.slice(0, 8).join(" ");
  return words.length > 8 ? `${head}…` : head;
}

/** The transcript region of the detail pane. */
const TranscriptBody: React.FC<{
  transcribing: boolean;
  hasTranscription: boolean;
  hasSegments: boolean;
  segments: PersistedSegment[];
  entry: HistoryEntry;
}> = ({ transcribing, hasTranscription, hasSegments, segments, entry }) => {
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

/** Day bucket label for the master list: Today / Yesterday / long date. */
function dayLabel(
  tsSeconds: number,
  lang: string,
  t: (k: string) => string,
): string {
  const d = new Date(tsSeconds * 1000);
  const today = new Date();
  const yesterday = new Date(Date.now() - 86400000);
  const sameDay = (a: Date, b: Date) => a.toDateString() === b.toDateString();
  if (sameDay(d, today)) return t("settings.history.today");
  if (sameDay(d, yesterday)) return t("settings.history.yesterday");
  return d.toLocaleDateString(lang, {
    day: "numeric",
    month: "long",
    year: "numeric",
  });
}

/** One compact row in the master list. Fetches its own segments (cheap, indexed)
 *  to infer the source icon — same pattern the old cards used. */
const ListRow: React.FC<{
  entry: HistoryEntry;
  selected: boolean;
  onSelect: () => void;
}> = ({ entry, selected, onSelect }) => {
  const { t, i18n } = useTranslation();
  const [segments, setSegments] = useState<PersistedSegment[]>([]);

  useEffect(() => {
    let active = true;
    commands.getSessionSegments(entry.id).then((res) => {
      if (active && res.status === "ok") setSegments(res.data);
    });
    return () => {
      active = false;
    };
  }, [entry.id]);

  const speakers = Array.from(
    new Set(
      segments.map((s) => s.speaker_label).filter((l): l is string => !!l),
    ),
  );
  const source = inferSource(speakers);
  const SourceIcon = SOURCE_ICON[source];
  const durationMs = segments.length
    ? Math.max(...segments.map((s) => s.end_ms))
    : 0;
  const time = new Date(Number(entry.timestamp) * 1000).toLocaleTimeString(
    i18n.language,
    { hour: "2-digit", minute: "2-digit" },
  );
  const title = deriveTitle(entry, time);
  const transcribing = entry.status === "transcribing";

  return (
    <button
      type="button"
      onClick={onSelect}
      className={`w-full text-left flex items-center gap-2.5 rounded-xl px-2.5 py-2 transition-colors cursor-pointer ${
        selected ? "bg-accent/90 text-white shadow-sm" : "hover:bg-mid-gray/10"
      }`}
    >
      <span
        className={`grid h-8 w-8 shrink-0 place-items-center rounded-lg ${
          selected ? "bg-white/20 text-white" : "bg-accent/10 text-accent"
        }`}
      >
        <SourceIcon className="h-4 w-4" />
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">{title}</span>
        <span
          className={`block truncate text-xs tabular-nums ${
            selected ? "text-white/70" : "text-text/50"
          }`}
        >
          {time}
          {durationMs > 0 && ` · ${formatDuration(durationMs)}`}
          {entry.status === "failed" &&
            ` · ${t("settings.history.transcriptionFailed")}`}
        </span>
      </span>
      {transcribing && (
        <span
          className={`h-2 w-2 shrink-0 rounded-full ${
            selected ? "bg-white" : "bg-accent"
          } animate-pulse`}
        />
      )}
      {entry.saved && (
        <Star
          className={`h-3.5 w-3.5 shrink-0 ${
            selected ? "text-white" : "text-accent"
          }`}
          fill="currentColor"
        />
      )}
    </button>
  );
};

/** Detail pane: the selected recording as a result — title, meta, cast, transcript, player, actions. */
const DetailPane: React.FC<{
  entry: HistoryEntry;
  onToggleSaved: () => void;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
}> = ({ entry, onToggleSaved, getAudioUrl, deleteAudio, retryTranscription }) => {
  const { t, i18n } = useTranslation();
  const [segments, setSegments] = useState<PersistedSegment[]>([]);
  const [showCopied, setShowCopied] = useState(false);
  const [retrying, setRetrying] = useState(false);

  useEffect(() => {
    let active = true;
    setSegments([]);
    commands.getSessionSegments(entry.id).then((res) => {
      if (active && res.status === "ok") setSegments(res.data);
    });
    return () => {
      active = false;
    };
  }, [entry.id, entry.status]);

  const hasTranscription = entry.transcription_text.trim().length > 0;
  const hasSegments = segments.length > 0;
  const transcribing = retrying || entry.status === "transcribing";
  const speakers = Array.from(
    new Set(
      segments.map((s) => s.speaker_label).filter((l): l is string => !!l),
    ),
  );
  const source = inferSource(speakers);
  const SourceIcon = SOURCE_ICON[source];
  const durationMs = segments.length
    ? Math.max(...segments.map((s) => s.end_ms))
    : 0;
  const formattedDate = formatDateTime(String(entry.timestamp), i18n.language);
  const title = deriveTitle(entry, formattedDate);

  const handleLoadAudio = useCallback(
    () => getAudioUrl(entry.file_name),
    [getAudioUrl, entry.file_name],
  );

  const handleCopyText = async () => {
    if (!hasTranscription) return;
    try {
      await navigator.clipboard.writeText(entry.transcription_text);
      setShowCopied(true);
      setTimeout(() => setShowCopied(false), 2000);
    } catch (error) {
      console.error("Failed to copy to clipboard:", error);
    }
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

  return (
    <div className="glass-panel flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
      <div className="flex items-start gap-3 px-5 pt-4">
        <span className="mt-0.5 grid h-10 w-10 shrink-0 place-items-center rounded-xl bg-accent/10 text-accent">
          <SourceIcon className="h-5 w-5" />
        </span>
        <div className="min-w-0 flex-1">
          <p className="truncate text-base font-semibold">{title}</p>
          <p className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-text/50">
            <span>{formattedDate}</span>
            {durationMs > 0 && (
              <>
                <span aria-hidden>·</span>
                <span className="tabular-nums">
                  {formatDuration(durationMs)}
                </span>
              </>
            )}
            <span aria-hidden>·</span>
            <span>{t(SOURCE_LABEL_KEY[source])}</span>
          </p>
        </div>
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
      </div>

      {speakers.length > 0 && !transcribing && (
        <div className="flex flex-wrap gap-1.5 px-5 pt-2.5">
          {speakers.map((s) => (
            <span
              key={s}
              className="glass-chip px-2.5 py-0.5 text-xs font-medium text-accent"
            >
              {s}
            </span>
          ))}
        </div>
      )}

      <div className="mt-3 min-h-0 flex-1 overflow-y-auto border-t border-mid-gray/10 px-5 py-3">
        <TranscriptBody
          transcribing={transcribing}
          hasTranscription={hasTranscription}
          hasSegments={hasSegments}
          segments={segments}
          entry={entry}
        />
      </div>

      <div className="flex items-center gap-3 border-t border-mid-gray/10 px-5 py-3">
        <AudioPlayer onLoadRequest={handleLoadAudio} className="min-w-0 flex-1" />
        <div className="flex items-center">
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
    </div>
  );
};

/** The Workstation: searchable date-grouped master list + rich detail pane. */
export const HistorySettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const osType = useOsType();
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<HistoryEntry[] | null>(
    null,
  );
  const sentinelRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<HistoryEntry[]>([]);
  const loadingRef = useRef(false);

  useEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  const loadPage = useCallback(async (cursor?: number) => {
    const isFirstPage = cursor === undefined;
    if (!isFirstPage && loadingRef.current) return;
    loadingRef.current = true;
    if (isFirstPage) setLoading(true);
    try {
      const result = await commands.getHistoryEntries(cursor ?? null, PAGE_SIZE);
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

  useEffect(() => {
    loadPage();
  }, [loadPage]);

  // Debounced search; empty query returns to the paged list.
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setSearchResults(null);
      return;
    }
    const handle = setTimeout(async () => {
      try {
        const result = await commands.searchHistoryEntries(q, 100);
        if (result.status === "ok") setSearchResults(result.data);
      } catch (error) {
        console.error("Search failed:", error);
      }
    }, 250);
    return () => clearTimeout(handle);
  }, [query]);

  // Infinite scroll via IntersectionObserver (paged list only).
  useEffect(() => {
    if (loading || searchResults !== null) return;
    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;
    const observer = new IntersectionObserver(
      (observerEntries) => {
        if (observerEntries[0].isIntersecting) {
          const lastEntry = entriesRef.current[entriesRef.current.length - 1];
          if (lastEntry) loadPage(lastEntry.id);
        }
      },
      { threshold: 0 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, hasMore, loadPage, searchResults]);

  // Live updates from the transcription pipeline.
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload: HistoryUpdatePayload = event.payload;
      if (payload.action === "added") {
        setEntries((prev) => [payload.entry, ...prev]);
      } else if (payload.action === "updated") {
        setEntries((prev) =>
          prev.map((e) => (e.id === payload.entry.id ? payload.entry : e)),
        );
        setSearchResults((prev) =>
          prev
            ? prev.map((e) => (e.id === payload.entry.id ? payload.entry : e))
            : prev,
        );
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const displayed = searchResults ?? entries;

  // Keep a valid selection: default to the newest visible entry.
  useEffect(() => {
    if (displayed.length === 0) {
      setSelectedId(null);
      return;
    }
    if (!displayed.some((e) => e.id === selectedId)) {
      setSelectedId(displayed[0].id);
    }
  }, [displayed, selectedId]);

  const toggleSaved = async (id: number) => {
    const flip = (list: HistoryEntry[]) =>
      list.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e));
    setEntries(flip);
    setSearchResults((prev) => (prev ? flip(prev) : prev));
    try {
      const result = await commands.toggleHistoryEntrySaved(id);
      if (result.status !== "ok") {
        setEntries(flip);
        setSearchResults((prev) => (prev ? flip(prev) : prev));
      }
    } catch (error) {
      console.error("Failed to toggle saved status:", error);
      setEntries(flip);
      setSearchResults((prev) => (prev ? flip(prev) : prev));
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
    setEntries((prev) => prev.filter((e) => e.id !== id));
    setSearchResults((prev) =>
      prev ? prev.filter((e) => e.id !== id) : prev,
    );
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") loadPage();
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
      if (result.status !== "ok") throw new Error(String(result.error));
    } catch (error) {
      console.error("Failed to open recordings folder:", error);
    }
  };

  // Group the visible entries by day, preserving order (newest first).
  const groups: { label: string; items: HistoryEntry[] }[] = [];
  for (const entry of displayed) {
    const label = dayLabel(Number(entry.timestamp), i18n.language, t);
    const last = groups[groups.length - 1];
    if (last && last.label === label) last.items.push(entry);
    else groups.push({ label, items: [entry] });
  }

  const selectedEntry = displayed.find((e) => e.id === selectedId) ?? null;

  return (
    <div className="flex h-full min-h-0 w-full gap-4">
      {/* Master list */}
      <div className="flex w-72 shrink-0 flex-col gap-3">
        <div className="glass-chip flex items-center gap-2 px-3 py-1.5">
          <Search className="h-4 w-4 shrink-0 text-text/40" />
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("settings.history.searchPlaceholder")}
            className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-text/35"
          />
          <IconButton
            onClick={openRecordingsFolder}
            title={t("settings.history.openFolder")}
          >
            <FolderOpen className="h-4 w-4" />
          </IconButton>
        </div>

        <div className="glass-panel min-h-0 flex-1 overflow-y-auto p-2">
          {loading ? (
            <p className="px-2 py-3 text-center text-sm text-text/60">
              {t("settings.history.loading")}
            </p>
          ) : displayed.length === 0 ? (
            <p className="px-2 py-3 text-center text-sm text-text/60">
              {searchResults !== null
                ? t("settings.history.noResults")
                : t("settings.history.empty")}
            </p>
          ) : (
            <>
              {groups.map((group) => (
                <div key={group.label} className="mb-1">
                  <p className="px-2.5 pb-1 pt-2 text-xs font-medium uppercase tracking-wide text-text/40">
                    {group.label}
                  </p>
                  <div className="flex flex-col gap-0.5">
                    {group.items.map((entry) => (
                      <ListRow
                        key={entry.id}
                        entry={entry}
                        selected={entry.id === selectedId}
                        onSelect={() => setSelectedId(entry.id)}
                      />
                    ))}
                  </div>
                </div>
              ))}
              {searchResults === null && <div ref={sentinelRef} className="h-1" />}
            </>
          )}
        </div>
      </div>

      {/* Detail pane */}
      {selectedEntry ? (
        <DetailPane
          entry={selectedEntry}
          onToggleSaved={() => toggleSaved(selectedEntry.id)}
          getAudioUrl={getAudioUrl}
          deleteAudio={deleteAudioEntry}
          retryTranscription={retryHistoryEntry}
        />
      ) : (
        <div className="glass-panel flex min-w-0 flex-1 items-center justify-center">
          <p className="text-sm text-text/40">
            {t("settings.history.selectEntry")}
          </p>
        </div>
      )}
    </div>
  );
};
