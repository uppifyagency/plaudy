//! Seamless auto-capture — the "it just works" brain.
//!
//! Turns a stream of "is audio coming out of the speakers?" observations (from the cheap,
//! tap-free CoreAudio output sensor — `kAudioDevicePropertyDeviceIsRunningSomewhere`) into
//! start/stop decisions for a recording session, with debounce in both directions:
//!   - a momentary blip must *persist* before we start, so a notification ping doesn't spawn a
//!     junk session;
//!   - a brief gap of silence is *tolerated* before we stop, so a pause between sentences doesn't
//!     chop one conversation into many.
//!
//! This module is the pure decision core — no CoreAudio, no Tauri, no session I/O — so it is
//! unit-testable in isolation. The supervisor (the I/O shell) feeds it observations and acts on
//! the returned [`AutoAction`]. Privacy posture (see docs/HANDOFF §12 + memory): this drives the
//! *system-audio* trigger only; auto-capturing the bare microphone is a separate, opt-in path.
//!
//! STATUS — EXPERIMENTAL, OFF by default (`auto_capture_enabled` = false). The system-audio
//! trigger is now the *per-process* sensor (`audio_toolkit::audio::output_sensor`): it attributes
//! output to PIDs and excludes our own, which removes the root cause of the old false-triggers
//! (the device-level "running" flag stuck true once our tap had ever been opened; 17/17 idle
//! starts were empty). Probation/discard below stays as defense in depth. Flip the setting on
//! after a live end-to-end validation with a real meeting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{info, warn};
use tauri::AppHandle;

use crate::managers::session::{SessionManager, Source};
use crate::settings;

/// What the supervisor should do as a result of the latest observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoAction {
    /// Nothing changes this tick.
    None,
    /// Audio has persisted long enough — start a session.
    StartCapture,
    /// Silence has persisted long enough — finalize the session.
    StopCapture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Speakers quiet, no session.
    Idle,
    /// Audio just appeared; accumulating time to confirm it's real before starting.
    Arming { elapsed: Duration },
    /// A session is running and audio is present.
    Capturing,
    /// Session running but audio went quiet; accumulating silence before finalizing.
    Trailing { elapsed: Duration },
}

/// Debounced state machine that decides when a seamless session should start and stop.
#[derive(Debug, Clone)]
pub struct AutoCaptureDecider {
    phase: Phase,
    /// Audio must persist at least this long before we start (rejects blips).
    start_after: Duration,
    /// Silence must persist at least this long before we stop (tolerates pauses).
    stop_after: Duration,
}

impl AutoCaptureDecider {
    pub fn new(start_after: Duration, stop_after: Duration) -> Self {
        Self {
            phase: Phase::Idle,
            start_after,
            stop_after,
        }
    }

    /// Feed one observation: `audio_present` from the output sensor, and `dt` elapsed since the
    /// previous observation. Returns the action the supervisor should take. Every matching tick
    /// (audio while arming, silence while trailing) accrues its own `dt`, including the entering
    /// tick — so with a 0.5s poll and a 1s start window, two consecutive audio ticks start.
    pub fn observe(&mut self, audio_present: bool, dt: Duration) -> AutoAction {
        match &mut self.phase {
            Phase::Idle => {
                if audio_present {
                    if dt >= self.start_after {
                        self.phase = Phase::Capturing;
                        return AutoAction::StartCapture;
                    }
                    self.phase = Phase::Arming { elapsed: dt };
                }
                AutoAction::None
            }
            Phase::Arming { elapsed } => {
                if audio_present {
                    *elapsed += dt;
                    if *elapsed >= self.start_after {
                        self.phase = Phase::Capturing;
                        return AutoAction::StartCapture;
                    }
                    AutoAction::None
                } else {
                    // Audio vanished before it confirmed — it was a blip, never start.
                    self.phase = Phase::Idle;
                    AutoAction::None
                }
            }
            Phase::Capturing => {
                if audio_present {
                    AutoAction::None
                } else {
                    if dt >= self.stop_after {
                        self.phase = Phase::Idle;
                        return AutoAction::StopCapture;
                    }
                    self.phase = Phase::Trailing { elapsed: dt };
                    AutoAction::None
                }
            }
            Phase::Trailing { elapsed } => {
                if audio_present {
                    // Audio came back during the grace window — it was a pause, keep one session.
                    self.phase = Phase::Capturing;
                    AutoAction::None
                } else {
                    *elapsed += dt;
                    if *elapsed >= self.stop_after {
                        self.phase = Phase::Idle;
                        return AutoAction::StopCapture;
                    }
                    AutoAction::None
                }
            }
        }
    }

    /// Whether the decider currently believes a session should be running (capturing or in the
    /// silence grace period). Lets the supervisor reconcile with manual toggles.
    pub fn wants_capture(&self) -> bool {
        matches!(self.phase, Phase::Capturing | Phase::Trailing { .. })
    }

    /// Reset to dormant — used when a session is stopped out from under us (e.g. manually), so the
    /// decider doesn't think it's still capturing.
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
    }
}

// --- Supervisor (I/O shell) -------------------------------------------------------------------
//
// The pure decider above is fed by this background loop, which is the only part that touches the
// OS: it polls the tap-free output sensor, runs the decider, and drives the SessionManager. Kept
// thin and side-effect-only so the testable brain stays pure.

/// How often the supervisor samples the output sensor.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Audio must play this long before a session auto-starts (rejects notification blips).
const START_AFTER: Duration = Duration::from_millis(1200);
/// Silence must persist this long before a session auto-finalizes (tolerates pauses in a call).
const STOP_AFTER: Duration = Duration::from_secs(4);
/// After auto-starting, real audio must be captured within this window — else it's a false start
/// (the OS sensor reads "running" because of our own tap) and the session is discarded.
const PROBATION: Duration = Duration::from_millis(2000);
/// After any auto-session ends (finalized or discarded), stand down this long before sensing again,
/// so a stale "device running" reading right after tap teardown can't immediately re-trigger.
const COOLDOWN: Duration = Duration::from_secs(8);

/// Run the seamless auto-capture supervisor forever (spawn on a dedicated thread at startup).
///
/// Posture (see memory `seamless-autocapture-direction`): triggers on **system-audio presence**
/// only. When it fires it records a *meeting* (`[Mic, SystemAudio]`) — the mic only ever joins a
/// session that system audio already triggered, never on its own. Gated behind the
/// `auto_capture_enabled` setting (opt-in). It never fights manual control: it only stops sessions
/// it started itself, and stands down whenever a manual session is active.
pub fn run_supervisor(app: AppHandle, session: Arc<SessionManager>) {
    let mut decider = AutoCaptureDecider::new(START_AFTER, STOP_AFTER);
    // Did *we* start the active session? Only then may we auto-stop/cancel it.
    let mut auto_started = false;
    // While Some, we are confirming the session captured real audio (else it's a false start).
    let mut probation_deadline: Option<Instant> = None;
    // While Some(t) and now < t, the supervisor stands down (post-session settle).
    let mut cooldown_until: Option<Instant> = None;
    let mut last = Instant::now();
    // Diagnostic: log only when the sensor flips, so we can see start/stop edges without spam.
    let mut last_present: Option<bool> = None;

    info!("Auto-capture supervisor started (system-audio trigger, opt-in).");
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        let dt = now.duration_since(last);
        last = now;

        if !settings::get_settings(&app).auto_capture_enabled {
            if auto_started && session.is_active() {
                let _ = session.cancel();
                info!("Auto-capture disabled mid-session; discarded our session.");
            }
            auto_started = false;
            probation_deadline = None;
            cooldown_until = None;
            decider.reset();
            continue;
        }

        // Our session ended out from under us (manual stop / finalize) → resync + cool down.
        if auto_started && !session.is_active() {
            auto_started = false;
            probation_deadline = None;
            cooldown_until = Some(now + COOLDOWN);
            decider.reset();
        }

        // A *manual* session is running (we didn't start it) → don't interfere at all.
        if session.is_active() && !auto_started {
            decider.reset();
            continue;
        }

        // Probation: a session we started must capture real audio quickly. The OS sensor reads
        // "running" once our tap is open, so without this a silent false start would never end.
        if auto_started {
            if let Some(deadline) = probation_deadline {
                if session.system_audio_heard() {
                    probation_deadline = None;
                    info!("Auto-capture: real audio confirmed → keeping session.");
                } else if now >= deadline {
                    info!("Auto-capture: no audio within probation → discarding false start.");
                    let _ = session.cancel();
                    auto_started = false;
                    probation_deadline = None;
                    cooldown_until = Some(now + COOLDOWN);
                    decider.reset();
                    continue;
                }
            }
        }

        // Cooldown after a session ends — skip sensing so a stale post-teardown reading can't
        // immediately re-trigger.
        if let Some(until) = cooldown_until {
            if now < until {
                continue;
            }
            cooldown_until = None;
        }

        // START uses the tap-free per-process sensor (own PID excluded, so our tap can't wake us).
        // For STOP we still prefer the captured system-audio level: a meeting app holds its output
        // stream open even while nobody talks, so "is the app outputting" can't detect the end of
        // a call — captured silence for STOP_AFTER can.
        let present = if session.is_active() {
            session.system_audio_idle() < Duration::from_millis(800)
        } else {
            crate::audio_toolkit::audio::external_output_active()
        };
        if last_present != Some(present) {
            info!("Auto-capture sensor: audio_present = {present} (capturing={auto_started})");
            last_present = Some(present);
        }

        match decider.observe(present, dt) {
            AutoAction::StartCapture => {
                if !session.is_active() {
                    match session.start_sources(&[Source::Mic, Source::SystemAudio]) {
                        Ok(()) => {
                            auto_started = true;
                            probation_deadline = Some(now + PROBATION);
                            info!("Auto-capture: system audio detected → session started (probation).");
                        }
                        Err(e) => {
                            warn!("Auto-capture: start failed ({e}); will retry on next trigger.");
                            decider.reset();
                        }
                    }
                }
            }
            AutoAction::StopCapture => {
                if auto_started && session.is_active() {
                    if let Err(e) = session.stop() {
                        warn!("Auto-capture: stop failed: {e}");
                    } else {
                        info!("Auto-capture: speakers quiet → session finalized.");
                    }
                }
                auto_started = false;
                probation_deadline = None;
                cooldown_until = Some(now + COOLDOWN);
            }
            AutoAction::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(500);
    // Start after 1s of audio, stop after 3s of silence.
    fn decider() -> AutoCaptureDecider {
        AutoCaptureDecider::new(Duration::from_secs(1), Duration::from_secs(3))
    }

    /// Feed `n` identical observations and collect the actions (helper for readable tests).
    fn feed(d: &mut AutoCaptureDecider, present: bool, n: usize) -> Vec<AutoAction> {
        (0..n).map(|_| d.observe(present, TICK)).collect()
    }

    #[test]
    fn a_brief_blip_never_starts_a_session() {
        let mut d = decider();
        // One 500ms tick of audio (< 1s start threshold), then silence.
        assert_eq!(d.observe(true, TICK), AutoAction::None);
        assert_eq!(d.observe(false, TICK), AutoAction::None);
        assert!(!d.wants_capture());
    }

    #[test]
    fn sustained_audio_starts_exactly_once() {
        let mut d = decider();
        // 1.0s of audio reaches the start threshold on the second tick.
        assert_eq!(d.observe(true, TICK), AutoAction::None); // 0.5s
        assert_eq!(d.observe(true, TICK), AutoAction::StartCapture); // 1.0s → start
                                                                     // Further audio does not re-trigger.
        assert_eq!(feed(&mut d, true, 5), vec![AutoAction::None; 5]);
        assert!(d.wants_capture());
    }

    #[test]
    fn a_short_pause_does_not_chop_the_session() {
        let mut d = decider();
        feed(&mut d, true, 2); // start (1.0s)
        assert!(d.wants_capture());
        // 2s of silence (< 3s stop threshold) then audio again → still one session, no stop.
        let acts = feed(&mut d, false, 4); // 2.0s of silence
        assert!(acts.iter().all(|a| *a == AutoAction::None));
        assert_eq!(d.observe(true, TICK), AutoAction::None); // pause over, resume
        assert!(d.wants_capture());
    }

    #[test]
    fn sustained_silence_stops_exactly_once() {
        let mut d = decider();
        feed(&mut d, true, 2); // start
                               // 3s of silence reaches the stop threshold on the 6th tick.
        let acts = feed(&mut d, false, 6);
        assert_eq!(
            acts.iter().filter(|a| **a == AutoAction::StopCapture).count(),
            1
        );
        assert_eq!(acts.last(), Some(&AutoAction::StopCapture));
        assert!(!d.wants_capture());
        // Once stopped, more silence does nothing.
        assert_eq!(feed(&mut d, false, 3), vec![AutoAction::None; 3]);
    }

    #[test]
    fn it_can_start_again_after_stopping() {
        let mut d = decider();
        feed(&mut d, true, 2); // start
        feed(&mut d, false, 6); // stop
        assert!(!d.wants_capture());
        // A fresh burst of audio starts a new session.
        assert_eq!(d.observe(true, TICK), AutoAction::None);
        assert_eq!(d.observe(true, TICK), AutoAction::StartCapture);
        assert!(d.wants_capture());
    }

    #[test]
    fn reset_returns_to_dormant() {
        let mut d = decider();
        feed(&mut d, true, 2); // start
        assert!(d.wants_capture());
        d.reset();
        assert!(!d.wants_capture());
        // After reset it behaves as fresh: needs the full start window again.
        assert_eq!(d.observe(true, TICK), AutoAction::None);
        assert_eq!(d.observe(true, TICK), AutoAction::StartCapture);
    }
}
