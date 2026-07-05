import React from "react";
import { useTranslation } from "react-i18next";
import type { HistoryEntry, PersistedSegment } from "@/bindings";
import { isFailed } from "@/lib/types/history";
import { SpeakerTimeline } from "./SpeakerTimeline";

/** The transcript region of the detail pane. */
export const TranscriptBody: React.FC<{
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
      {isFailed(entry)
        ? t("settings.history.transcriptionFailed")
        : t("settings.history.noSpeech")}
    </p>
  );
};
