import React from "react";
import { Star } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { HistoryEntry, SessionOverview } from "@/bindings";
import { isFailed, isTranscribing } from "@/lib/types/history";
import { formatClock } from "@/utils/formatClock";
import { deriveTitle, inferSource, SOURCE_ICON } from "./useSessionSegments";

/** One compact row in the master list. Display facts (speakers, duration)
 *  come from the container's batched overview fetch — one IPC call per page
 *  instead of one per row. `overview === undefined` means no segments yet
 *  (still transcribing, or a legacy dictation row): render without duration. */
export const ListRow: React.FC<{
  entry: HistoryEntry;
  overview: SessionOverview | undefined;
  selected: boolean;
  onSelect: () => void;
}> = ({ entry, overview, selected, onSelect }) => {
  const { t, i18n } = useTranslation();
  const speakers = overview?.speakers ?? [];
  const durationMs = overview?.duration_ms ?? 0;
  const source =
    entry.source === "unknown" ? inferSource(speakers) : entry.source;
  const SourceIcon = SOURCE_ICON[source];
  const time = new Date(Number(entry.timestamp) * 1000).toLocaleTimeString(
    i18n.language,
    { hour: "2-digit", minute: "2-digit" },
  );
  const title = deriveTitle(entry, time);
  const transcribing = isTranscribing(entry);

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
          {durationMs > 0 && ` · ${formatClock(durationMs / 1000)}`}
          {isFailed(entry) && ` · ${t("settings.history.transcriptionFailed")}`}
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
