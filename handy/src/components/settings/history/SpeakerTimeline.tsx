import React from "react";
import { useTranslation } from "react-i18next";
import type { PersistedSegment } from "@/bindings";
import { formatClock } from "@/utils/formatClock";

/** Fase 2: speaker-attributed transcript timeline (speaker · time · text per segment). */
export const SpeakerTimeline: React.FC<{ segments: PersistedSegment[] }> = ({
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
            {formatClock(seg.start_ms / 1000)}
          </span>
          <span className="whitespace-pre-wrap break-words text-text/90">
            {seg.text}
          </span>
        </div>
      ))}
    </div>
  );
};
