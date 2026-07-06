import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Mic, Square, Users, AudioLines } from "lucide-react";
import { commands, events } from "@/bindings";
import { formatClock } from "@/utils/formatClock";
import { useSettings } from "../../../hooks/useSettings";
import { ToggleSwitch } from "../../ui/ToggleSwitch";

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

const LIVE_LABEL_KEY: Record<Mode, string> = {
  meeting: "settings.sessions.capturingMeeting",
  mic: "settings.sessions.capturingMic",
  system: "settings.sessions.capturingSystem",
};

export function SessionsSettings() {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const [active, setActive] = useState(false);
  const [mode, setMode] = useState<Mode>("meeting");
  // What the running session actually captures, for the live label. Driven by the
  // SessionStateChanged payload so tray/CLI-started sessions are labeled too.
  const [liveMode, setLiveMode] = useState<Mode>("meeting");
  const [busy, setBusy] = useState(false);
  const [elapsed, setElapsed] = useState(0);

  useEffect(() => {
    commands.isSessionActive().then(setActive);
    const unlisten = events.sessionStateChanged.listen((event) => {
      const { active: isActive, source } = event.payload;
      setActive(isActive);
      if (!isActive) {
        // Reset so a later out-of-band start isn't labeled with a stale panel mode.
        setLiveMode("meeting");
        return;
      }
      // The payload's `source` is the session's *primary* track (session.rs): SystemAudio is
      // unambiguous, but Mic-primary is either mic-only or a meeting (mic + system). The
      // out-of-band Mic-primary starters (tray, --toggle-meeting) are meetings — the default —
      // and a start from this panel already pinned the exact mode in `toggle`.
      setLiveMode((prev) => (source === "SystemAudio" ? "system" : prev));
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Count up while recording, anchored to the backend's true session start so
  // mounting this view mid-session shows the real elapsed time.
  useEffect(() => {
    if (!active) {
      setElapsed(0);
      return;
    }
    let cancelled = false;
    let startedAt = Date.now();
    commands.sessionElapsedMs().then((ms) => {
      if (cancelled) return;
      startedAt = Date.now() - (ms ?? 0);
      setElapsed(Math.floor((Date.now() - startedAt) / 1000));
    });
    const id = setInterval(
      () => setElapsed(Math.floor((Date.now() - startedAt) / 1000)),
      250,
    );
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [active]);

  const toggle = async () => {
    setBusy(true);
    try {
      if (active) {
        const result = await commands.stopSession();
        if (result.status === "error") toast.error(result.error);
        return;
      }
      // This panel knows exactly what it starts; the state event only refines it.
      setLiveMode(mode);
      const result =
        mode === "meeting"
          ? await commands.startMeeting()
          : await commands.startSession(mode === "mic" ? "Mic" : "SystemAudio");
      if (result.status === "error") toast.error(result.error);
    } catch (error) {
      console.error("Failed to toggle session:", error);
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

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
            aria-label={t(
              active ? "settings.sessions.stop" : "settings.sessions.start",
            )}
            className={`relative grid h-28 w-28 place-items-center rounded-full shadow-lg transition-all duration-200 focus:outline-none focus-visible:ring-4 focus-visible:ring-accent/40 disabled:opacity-60 ${
              active
                ? "bg-red-500 text-white hover:bg-red-600 scale-100"
                : "bg-text text-background hover:scale-105 hover:shadow-xl"
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
                {formatClock(elapsed)}
              </span>
              <span
                className="flex items-center gap-2 text-sm text-text/70"
                role="status"
              >
                <span
                  className="inline-block h-2 w-2 rounded-full bg-red-500"
                  style={{
                    animation: "session-rec-pulse 1.5s ease-in-out infinite",
                  }}
                />
                {t(LIVE_LABEL_KEY[liveMode])}
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
          <div className="glass-chip flex gap-1 p-1">
            {MODES.map(({ id, icon: Icon, labelKey }) => (
              <button
                key={id}
                type="button"
                onClick={() => setMode(id)}
                disabled={busy}
                className={`flex items-center gap-2 rounded-full px-4 py-2 text-sm font-medium transition-colors disabled:opacity-50 ${
                  mode === id
                    ? "bg-accent/90 text-white shadow-sm"
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

        {/* Seamless auto-capture — the supervisor reads this flag live, no restart needed */}
        <div className="glass-panel w-full max-w-md px-4 py-1">
          <ToggleSwitch
            checked={getSetting("auto_capture_enabled") ?? false}
            onChange={(enabled) =>
              updateSetting("auto_capture_enabled", enabled)
            }
            isUpdating={isUpdating("auto_capture_enabled")}
            label={t("settings.sessions.autoCapture.title")}
            description={t("settings.sessions.autoCapture.description")}
            grouped={true}
          />
        </div>
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
