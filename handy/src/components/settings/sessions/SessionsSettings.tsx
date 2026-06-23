import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Mic, Square, Users, AudioLines } from "lucide-react";
import { commands, events } from "@/bindings";

/**
 * Sessions — the "graffetta" capture experience. One tap records a meeting (your mic + this
 * Mac's system audio as two streams) and it lands in History as one speaker-attributed
 * transcript. The same capture is reachable from the menu-bar tray; this view is its home and
 * also lets you pick a single source.
 *
 * `active` is driven by the backend `session-state-changed` event, so the indicator stays
 * correct however a session is toggled (here, the tray, or the CLI). The post-stop gap is
 * filled by the per-row `transcribing` status in History.
 */

type Mode = "meeting" | "mic" | "system";

const MODES: { id: Mode; icon: typeof Mic; labelKey: string }[] = [
  { id: "meeting", icon: Users, labelKey: "settings.sessions.modeMeeting" },
  { id: "mic", icon: Mic, labelKey: "settings.sessions.modeMic" },
  { id: "system", icon: AudioLines, labelKey: "settings.sessions.modeSystem" },
];

function formatElapsed(totalSeconds: number): string {
  const m = Math.floor(totalSeconds / 60);
  const s = totalSeconds % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

export function SessionsSettings() {
  const { t } = useTranslation();
  const [active, setActive] = useState(false);
  const [mode, setMode] = useState<Mode>("meeting");
  const [busy, setBusy] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  // The mode a running session was started with — frozen while recording so the live label
  // is truthful even if the (disabled) selector state drifts.
  const activeModeRef = useRef<Mode>("meeting");

  useEffect(() => {
    commands.isSessionActive().then(setActive);
    const unlisten = events.sessionStateChanged.listen((event) => {
      setActive(event.payload.active);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Count up while recording (wall-clock from when this view saw the session go active).
  useEffect(() => {
    if (!active) {
      setElapsed(0);
      return;
    }
    const startedAt = Date.now();
    const id = setInterval(
      () => setElapsed(Math.floor((Date.now() - startedAt) / 1000)),
      250,
    );
    return () => clearInterval(id);
  }, [active]);

  const toggle = async () => {
    setBusy(true);
    try {
      if (active) {
        const result = await commands.stopSession();
        if (result.status === "error") toast.error(result.error);
        return;
      }
      activeModeRef.current = mode;
      const result =
        mode === "meeting"
          ? await commands.startMeeting()
          : await commands.startSession(mode === "mic" ? "Mic" : "SystemAudio");
      if (result.status === "error") toast.error(result.error);
    } finally {
      setBusy(false);
    }
  };

  const liveLabelKey =
    activeModeRef.current === "meeting"
      ? "settings.sessions.capturingMeeting"
      : activeModeRef.current === "mic"
        ? "settings.sessions.capturingMic"
        : "settings.sessions.capturingSystem";

  return (
    <div className="max-w-3xl w-full mx-auto">
      <div className="flex flex-col items-center gap-7 px-6 py-12">
        {/* Hero capture control */}
        <div className="relative grid place-items-center">
          {active && (
            <span
              aria-hidden
              className="absolute h-32 w-32 rounded-full bg-red-500/20"
              style={{ animation: "session-hero-ring 1.8s ease-out infinite" }}
            />
          )}
          <button
            type="button"
            onClick={toggle}
            disabled={busy}
            aria-label={t(active ? "settings.sessions.stop" : "settings.sessions.start")}
            className={`relative grid h-28 w-28 place-items-center rounded-full text-white shadow-lg transition-all duration-200 focus:outline-none focus-visible:ring-4 focus-visible:ring-logo-primary/40 disabled:opacity-60 ${
              active
                ? "bg-red-500 hover:bg-red-600 scale-100"
                : "bg-logo-primary hover:scale-105 hover:shadow-xl"
            }`}
          >
            {active ? (
              <Square className="h-9 w-9" fill="currentColor" />
            ) : (
              <Mic className="h-11 w-11" />
            )}
          </button>
        </div>

        {/* Status line */}
        <div className="flex flex-col items-center gap-1 text-center">
          {active ? (
            <>
              <span className="text-3xl font-semibold tabular-nums tracking-tight">
                {formatElapsed(elapsed)}
              </span>
              <span
                className="flex items-center gap-2 text-sm text-text/70"
                role="status"
              >
                <span
                  className="inline-block h-2 w-2 rounded-full bg-red-500"
                  style={{ animation: "session-rec-pulse 1.5s ease-in-out infinite" }}
                />
                {t(liveLabelKey)}
              </span>
            </>
          ) : (
            <>
              <span className="text-lg font-medium">
                {t("settings.sessions.tapToStart")}
              </span>
              <span className="text-sm text-text/60">
                {t("settings.sessions.title")}
              </span>
            </>
          )}
        </div>

        {/* Mode selector — hidden while recording so the hero stays focused */}
        {!active && (
          <div className="flex gap-1 rounded-full bg-mid-gray/10 p-1">
            {MODES.map(({ id, icon: Icon, labelKey }) => (
              <button
                key={id}
                type="button"
                onClick={() => setMode(id)}
                disabled={busy}
                className={`flex items-center gap-2 rounded-full px-4 py-2 text-sm font-medium transition-colors disabled:opacity-50 ${
                  mode === id
                    ? "bg-background text-text shadow-sm"
                    : "text-text/60 hover:text-text"
                }`}
              >
                <Icon className="h-4 w-4" />
                {t(labelKey)}
              </button>
            ))}
          </div>
        )}

        {/* Calm privacy reassurance — the heart of "local-first" */}
        <p className="max-w-sm text-center text-xs leading-relaxed text-text/50">
          {t("settings.sessions.privacyNote")}
        </p>
      </div>

      <style>{`
        @keyframes session-rec-pulse {
          0%, 100% { opacity: 1; }
          50% { opacity: 0.3; }
        }
        @keyframes session-hero-ring {
          0% { transform: scale(0.9); opacity: 0.7; }
          100% { transform: scale(1.5); opacity: 0; }
        }
      `}</style>
    </div>
  );
}
