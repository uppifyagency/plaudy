import React, { useCallback, useState } from "react";
import { Check, Copy, RotateCcw, Star, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { HistoryEntry } from "@/bindings";
import { isTranscribing } from "@/lib/types/history";
import { formatDateTime } from "@/utils/dateFormat";
import { formatClock } from "@/utils/formatClock";
import { AudioPlayer } from "../../ui/AudioPlayer";
import { IconButton } from "./IconButton";
import { TranscriptBody } from "./TranscriptBody";
import {
  deriveTitle,
  SOURCE_ICON,
  SOURCE_LABEL_KEY,
  useSessionSegments,
} from "./useSessionSegments";

/** Detail pane: the selected recording as a result — title, meta, cast, transcript, player, actions. */
export const DetailPane: React.FC<{
  entry: HistoryEntry;
  onToggleSaved: () => void;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
}> = ({
  entry,
  onToggleSaved,
  getAudioUrl,
  deleteAudio,
  retryTranscription,
}) => {
  const { t, i18n } = useTranslation();
  const { segments, speakers, source, durationMs } = useSessionSegments(entry);
  const [showCopied, setShowCopied] = useState(false);
  const [retrying, setRetrying] = useState(false);

  const hasTranscription = entry.transcription_text.trim().length > 0;
  const hasSegments = segments.length > 0;
  const transcribing = retrying || isTranscribing(entry);
  const SourceIcon = SOURCE_ICON[source];
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
                  {formatClock(durationMs / 1000)}
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
        {/* Keyed per entry so switching recordings remounts the player: the cached
            audio src can't leak under another entry's transcript, and unmount
            revokes any blob URL. */}
        <AudioPlayer
          key={entry.id}
          onLoadRequest={handleLoadAudio}
          className="min-w-0 flex-1"
        />
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
