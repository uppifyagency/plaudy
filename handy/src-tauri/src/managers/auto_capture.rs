//! Seamless auto-capture — the "it just works" brain.
//!
//! Turns a stream of "is an external process playing audio?" observations (from the tap-free
//! per-process CoreAudio output sensor, `audio_toolkit::audio::output_sensor`) into start/stop
//! decisions for a recording session, with debounce in both directions:
//!   - a momentary blip must *persist* before we start, so a notification ping doesn't spawn a
//!     junk session;
//!   - a brief gap of silence is *tolerated* before we stop, so a pause between sentences doesn't
//!     chop one conversation into many.
//!
//! Two layers, both pure and unit-testable without CoreAudio/Tauri/session I/O:
//!   - [`AutoCaptureDecider`] — the debounce state machine (present/absent → start/stop edges);
//!   - [`Supervisor`] — the product state machine around it (ownership of the session it
//!     started, silent-start failsafe, cooldown, manual-session non-interference). Its
//!     [`Supervisor::tick`] consumes a [`TickView`] snapshot and returns an [`Effect`]; the
//!     I/O loop ([`run_supervisor`]) only gathers snapshots and applies effects.
//!
//! Signal design (the churn lesson): a meeting app holds its output stream open from the moment
//! you join, even while everyone is muted. So while an auto-started session has not yet captured
//! any real audio, presence keeps coming from the *external sensor* (the trigger condition still
//! holds — do NOT discard and re-trigger every few seconds); only once real audio was heard does
//! presence switch to the captured loudness level, which is what can actually detect the end of
//! a call. A session that never heard audio is *cancelled* (discarded), never finalized — no
//! junk silence rows. The old 2 s probation existed to defend against the device-level sensor
//! lying because of our own tap; the per-process sensor excludes our own PID, so probation is
//! now a long failsafe against unknown liars, not a hair-trigger.
//!
//! Privacy posture (see docs/HANDOFF §12 + memory): this drives the *system-audio* trigger only;
//! auto-capturing the bare microphone is a separate, opt-in path.
//!
//! STATUS — EXPERIMENTAL, OFF by default (`auto_capture_enabled` = false). Flip the setting on
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

// --- Supervisor (pure product state machine) ---------------------------------------------------

/// How often the I/O loop samples the output sensor.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// One observation's `dt` is clamped to this: after system sleep (or a scheduler stall) the
/// first tick would otherwise carry hours of `dt` and blow straight through both debounce
/// windows — a single observation must never count as more than two poll intervals of evidence.
const MAX_TICK_DT: Duration = Duration::from_millis(500);
/// Audio must play this long before a session auto-starts (rejects notification blips).
const START_AFTER: Duration = Duration::from_millis(1200);
/// Silence must persist this long before a session auto-finalizes (tolerates pauses in a call).
const STOP_AFTER: Duration = Duration::from_secs(4);
/// Captured system audio idle for less than this counts as "present" for the STOP decision.
const CAPTURED_IDLE_AS_SILENCE: Duration = Duration::from_millis(800);
/// Silent-start failsafe: an auto-started session that has captured NO real audio at all for
/// this long is discarded. Generous on purpose — a muted meeting join is a *valid* start whose
/// audio arrives when someone speaks; this only defends against a sensor that lies indefinitely.
const PROBATION: Duration = Duration::from_secs(60);
/// After any auto-session ends (finalized or discarded), stand down this long before sensing
/// again, so a stale reading right after tap teardown can't immediately re-trigger.
const COOLDOWN: Duration = Duration::from_secs(8);

/// One tick's world snapshot, gathered by the I/O loop. Pure data — this is what makes the
/// supervisor's decisions unit-testable without CoreAudio or a Tauri AppHandle.
#[derive(Debug, Clone, Copy)]
pub struct TickView {
    pub enabled: bool,
    pub session_active: bool,
    /// Any real (loud) system audio captured in the current session.
    pub system_audio_heard: bool,
    /// How long the captured system audio has been silent.
    pub system_audio_idle: Duration,
    /// The tap-free per-process sensor: some external process holds a running output stream.
    pub external_output_active: bool,
}

/// What the I/O loop must do after a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Start a meeting session (`[Mic, SystemAudio]`); report back via `after_start`.
    Start,
    /// Finalize the auto-started session (real audio was captured); report via `after_stop`.
    Stop,
    /// Discard the auto-started session (no real audio ever arrived); report via `after_cancel`.
    Cancel,
}

/// Timing knobs, separated so tests can shrink the windows without touching production values.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorConfig {
    pub start_after: Duration,
    pub stop_after: Duration,
    pub probation: Duration,
    pub cooldown: Duration,
    pub max_tick_dt: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            start_after: START_AFTER,
            stop_after: STOP_AFTER,
            probation: PROBATION,
            cooldown: COOLDOWN,
            max_tick_dt: MAX_TICK_DT,
        }
    }
}

/// The product state machine around the decider: ownership of the session it started, the
/// silent-start failsafe, post-session cooldown, and manual-session non-interference. All the
/// parts that previously lived as loose mutable locals inside the I/O loop — and had zero tests.
pub struct Supervisor {
    cfg: SupervisorConfig,
    decider: AutoCaptureDecider,
    /// Did *we* start the active session? Only then may we auto-stop/cancel it.
    auto_started: bool,
    /// When our session started — drives the silent-start failsafe.
    started_at: Option<Instant>,
    /// While Some(t) and now < t, stand down (post-session settle).
    cooldown_until: Option<Instant>,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            decider: AutoCaptureDecider::new(cfg.start_after, cfg.stop_after),
            cfg,
            auto_started: false,
            started_at: None,
            cooldown_until: None,
        }
    }

    /// Whether the supervisor currently owns the active session (used by the panic guard to
    /// know it may safely stop it).
    pub fn owns_session(&self) -> bool {
        self.auto_started
    }

    /// Consume one snapshot and decide. `raw_dt` is wall-clock since the previous tick and is
    /// clamped (see `MAX_TICK_DT`) so sleep-wake can't bypass the debounce.
    pub fn tick(&mut self, view: &TickView, now: Instant, raw_dt: Duration) -> Option<Effect> {
        let dt = raw_dt.min(self.cfg.max_tick_dt);

        if !view.enabled {
            let effect = (self.auto_started && view.session_active).then_some(Effect::Cancel);
            self.auto_started = false;
            self.started_at = None;
            self.cooldown_until = None;
            self.decider.reset();
            return effect;
        }

        // Our session ended out from under us (manual stop / finalize) → resync + cool down.
        if self.auto_started && !view.session_active {
            self.auto_started = false;
            self.started_at = None;
            self.cooldown_until = Some(now + self.cfg.cooldown);
            self.decider.reset();
        }

        // A *manual* session is running (we didn't start it) → don't interfere at all.
        if view.session_active && !self.auto_started {
            self.decider.reset();
            return None;
        }

        // Silent-start failsafe: our session has captured no real audio at all for the whole
        // probation window → the trigger was a lie; discard. (State clears in `after_cancel`,
        // so a failed cancel retries here next tick instead of orphaning the session.)
        if self.auto_started && !view.system_audio_heard {
            if let Some(started) = self.started_at {
                if now.duration_since(started) >= self.cfg.probation {
                    warn!("Auto-capture: no audio for the whole probation window → discarding.");
                    return Some(Effect::Cancel);
                }
            }
        }

        // Cooldown after a session ends — skip sensing so a stale post-teardown reading can't
        // immediately re-trigger.
        if let Some(until) = self.cooldown_until {
            if now < until {
                return None;
            }
            self.cooldown_until = None;
        }

        // Presence signal (the churn fix): while our auto-started session has NOT yet heard
        // real audio, keep trusting the external sensor — a meeting app holds its stream open
        // while everyone is muted, and that is a *valid* start, not one to discard-and-retrigger
        // every few seconds. Only once audio was actually captured does the captured loudness
        // take over for STOP (an app's open-but-quiet stream can't detect the end of a call).
        let present = if view.session_active && self.auto_started && view.system_audio_heard {
            view.system_audio_idle < CAPTURED_IDLE_AS_SILENCE
        } else {
            // Idle, or our session hasn't captured real audio yet: the external sensor rules.
            view.external_output_active
        };

        match self.decider.observe(present, dt) {
            AutoAction::StartCapture => (!view.session_active).then_some(Effect::Start),
            AutoAction::StopCapture => {
                if self.auto_started && view.session_active {
                    // Never finalize a session in which no real audio was ever captured —
                    // that would be a junk silence row in History.
                    if view.system_audio_heard {
                        Some(Effect::Stop)
                    } else {
                        Some(Effect::Cancel)
                    }
                } else {
                    self.auto_started = false;
                    self.started_at = None;
                    self.cooldown_until = Some(now + self.cfg.cooldown);
                    None
                }
            }
            AutoAction::None => None,
        }
    }

    /// Report the outcome of an `Effect::Start`. A failure resets the decider so the next
    /// trigger retries cleanly.
    pub fn after_start(&mut self, ok: bool, now: Instant) {
        if ok {
            self.auto_started = true;
            self.started_at = Some(now);
        } else {
            self.decider.reset();
        }
    }

    /// Report the outcome of an `Effect::Stop` (finalize is fire-and-forget: the session is
    /// gone from our ownership either way).
    pub fn after_stop(&mut self, now: Instant) {
        self.auto_started = false;
        self.started_at = None;
        self.cooldown_until = Some(now + self.cfg.cooldown);
        self.decider.reset();
    }

    /// Report the outcome of an `Effect::Cancel`. On failure, ownership state is KEPT so the
    /// next tick retries (or the resync branch reconciles if the session is in fact gone) —
    /// the old code cleared `auto_started` unconditionally and could orphan a recording
    /// session forever.
    pub fn after_cancel(&mut self, ok: bool, now: Instant) {
        if ok {
            self.auto_started = false;
            self.started_at = None;
            self.cooldown_until = Some(now + self.cfg.cooldown);
            self.decider.reset();
        }
    }
}

// --- I/O shell ----------------------------------------------------------------------------------

/// Run the seamless auto-capture supervisor forever (spawn on a dedicated thread at startup).
///
/// Posture (see memory `seamless-autocapture-direction`): triggers on **system-audio presence**
/// only. When it fires it records a *meeting* (`[Mic, SystemAudio]`) — the mic only ever joins a
/// session that system audio already triggered, never on its own. Gated behind the
/// `auto_capture_enabled` setting (opt-in). It never fights manual control: it only stops
/// sessions it started itself, and stands down whenever a manual session is active.
///
/// Panic containment: a panic anywhere in a tick (poisoned lock, settings hiccup) must neither
/// silently kill the feature until restart nor orphan a session we started. On panic: if we
/// owned the active session, STOP it (finalize — never throw the user's audio away), then
/// restart supervision after a pause.
pub fn run_supervisor(app: AppHandle, session: Arc<SessionManager>) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, Ordering};

    info!("Auto-capture supervisor started (system-audio trigger, opt-in).");
    let owns = Arc::new(AtomicBool::new(false));
    loop {
        let owns_flag = owns.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            supervise(&app, &session, &owns_flag);
        }));
        if result.is_err() {
            log::error!(
                "Auto-capture supervisor panicked; recovering (session preserved if ours)."
            );
            if owns.load(Ordering::SeqCst) && session.is_active() {
                let _ = session.stop();
            }
            owns.store(false, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

/// The actual supervision loop: gather a [`TickView`], tick the [`Supervisor`], apply the
/// [`Effect`], report the outcome back. `owns` mirrors the supervisor's session ownership for
/// the panic guard above.
fn supervise(app: &AppHandle, session: &Arc<SessionManager>, owns: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;

    let mut sup = Supervisor::new(SupervisorConfig::default());
    let mut last = Instant::now();
    // Diagnostic: log only when the sensor flips, so we can see edges without spam.
    let mut last_present: Option<bool> = None;

    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        let dt = now.duration_since(last);
        last = now;

        let view = TickView {
            enabled: settings::get_settings(app).auto_capture_enabled,
            session_active: session.is_active(),
            system_audio_heard: session.system_audio_heard(),
            system_audio_idle: session.system_audio_idle(),
            external_output_active: crate::audio_toolkit::audio::external_output_active(),
        };
        if view.enabled && last_present != Some(view.external_output_active) {
            info!(
                "Auto-capture sensor: external_output = {} (owned session={})",
                view.external_output_active,
                sup.owns_session()
            );
            last_present = Some(view.external_output_active);
        }

        match sup.tick(&view, now, dt) {
            Some(Effect::Start) => match session.start_sources(&[Source::Mic, Source::SystemAudio])
            {
                Ok(()) => {
                    sup.after_start(true, now);
                    info!("Auto-capture: system audio detected → session started.");
                }
                Err(e) => {
                    warn!("Auto-capture: start failed ({e}); will retry on next trigger.");
                    sup.after_start(false, now);
                }
            },
            Some(Effect::Stop) => {
                match session.stop() {
                    Ok(()) => info!("Auto-capture: speakers quiet → session finalized."),
                    Err(e) => warn!("Auto-capture: stop failed: {e}"),
                }
                sup.after_stop(now);
            }
            Some(Effect::Cancel) => match session.cancel() {
                Ok(()) => {
                    sup.after_cancel(true, now);
                    info!("Auto-capture: discarded silent session.");
                }
                Err(e) => {
                    warn!("Auto-capture: cancel failed ({e}); will retry.");
                    sup.after_cancel(false, now);
                }
            },
            None => {}
        }
        owns.store(sup.owns_session(), Ordering::SeqCst);
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
            acts.iter()
                .filter(|a| **a == AutoAction::StopCapture)
                .count(),
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

    #[test]
    fn re_entering_trailing_restarts_the_silence_clock() {
        let mut d = decider();
        feed(&mut d, true, 2); // start
        feed(&mut d, false, 5); // 2.5 s of silence — one tick shy of the 3 s stop
        assert_eq!(d.observe(true, TICK), AutoAction::None); // audio returns → pause, not end
                                                             // Silence again: the previous 2.5 s must NOT carry over.
        let acts = feed(&mut d, false, 5);
        assert!(acts.iter().all(|a| *a == AutoAction::None));
        assert_eq!(d.observe(false, TICK), AutoAction::StopCapture);
    }

    // --- Supervisor (the product state machine) --------------------------------------------

    /// Small windows so tests stay readable: start 1 s, stop 3 s, probation 10 s, cooldown 8 s.
    fn sup_cfg() -> SupervisorConfig {
        SupervisorConfig {
            start_after: Duration::from_secs(1),
            stop_after: Duration::from_secs(3),
            probation: Duration::from_secs(10),
            cooldown: Duration::from_secs(8),
            max_tick_dt: Duration::from_millis(500),
        }
    }

    fn view(
        enabled: bool,
        session_active: bool,
        heard: bool,
        idle: Duration,
        external: bool,
    ) -> TickView {
        TickView {
            enabled,
            session_active,
            system_audio_heard: heard,
            system_audio_idle: idle,
            external_output_active: external,
        }
    }

    const IDLE_LONG: Duration = Duration::from_secs(30);
    const IDLE_NONE: Duration = Duration::from_millis(0);

    /// Drive a fresh supervisor through the trigger into an owned session, returning
    /// (supervisor, time-of-start).
    fn started_supervisor() -> (Supervisor, Instant) {
        let mut sup = Supervisor::new(sup_cfg());
        let t0 = Instant::now();
        let idle_view = view(true, false, false, IDLE_LONG, true);
        assert_eq!(sup.tick(&idle_view, t0, TICK), None); // arming: 0.5 s
        let t1 = t0 + TICK;
        assert_eq!(
            sup.tick(&idle_view, t1, TICK),
            Some(Effect::Start),
            "1 s of external audio must trigger a start"
        );
        sup.after_start(true, t1);
        assert!(sup.owns_session());
        (sup, t1)
    }

    #[test]
    fn muted_meeting_join_does_not_churn() {
        // THE churn regression: a meeting app holds its output stream open while everyone is
        // muted. The old 2 s probation discarded and re-triggered every ~11 s, losing the
        // first words of the call. Now: the open stream keeps the session alive until either
        // audio arrives or the stream closes.
        let (mut sup, t_start) = started_supervisor();
        let muted = view(true, true, false, IDLE_LONG, true); // stream open, nothing heard yet
        for i in 1..=16 {
            let now = t_start + TICK * i; // 8 s of muted meeting — well past the old 2 s
            assert_eq!(
                sup.tick(&muted, now, TICK),
                None,
                "tick {i} must not discard"
            );
        }
        assert!(sup.owns_session());
    }

    #[test]
    fn transient_ping_is_discarded_never_finalized() {
        // The trigger stream vanished and no real audio was ever captured: this session is
        // a false start — it must be CANCELLED (no junk silence row), not finalized.
        let (mut sup, t_start) = started_supervisor();
        let gone = view(true, true, false, IDLE_LONG, false);
        let mut effect = None;
        for i in 1..=8 {
            effect = sup.tick(&gone, t_start + TICK * i, TICK);
            if effect.is_some() {
                break;
            }
        }
        assert_eq!(effect, Some(Effect::Cancel));
        sup.after_cancel(true, t_start + TICK * 8);
        assert!(!sup.owns_session());
    }

    #[test]
    fn real_meeting_end_finalizes_the_session() {
        let (mut sup, t_start) = started_supervisor();
        // Real audio arrives → presence switches to the captured level.
        let talking = view(true, true, true, IDLE_NONE, true);
        assert_eq!(sup.tick(&talking, t_start + TICK, TICK), None);
        // Call ends: captured audio idle far beyond the 800 ms threshold.
        let quiet = view(true, true, true, IDLE_LONG, true);
        let mut effect = None;
        for i in 2..=9 {
            effect = sup.tick(&quiet, t_start + TICK * i, TICK);
            if effect.is_some() {
                break;
            }
        }
        assert_eq!(
            effect,
            Some(Effect::Stop),
            "a session with real audio is finalized, not discarded"
        );
    }

    #[test]
    fn silent_start_failsafe_discards_after_probation_and_retries_failed_cancels() {
        let (mut sup, t_start) = started_supervisor();
        let muted = view(true, true, false, IDLE_LONG, true);
        // Just before probation: still holding.
        assert_eq!(
            sup.tick(&muted, t_start + Duration::from_secs(9), TICK),
            None
        );
        // Past probation with zero audio ever: discard.
        let late = t_start + Duration::from_secs(11);
        assert_eq!(sup.tick(&muted, late, TICK), Some(Effect::Cancel));
        // Cancel failed → ownership kept → next tick retries instead of orphaning.
        sup.after_cancel(false, late);
        assert!(sup.owns_session());
        assert_eq!(sup.tick(&muted, late + TICK, TICK), Some(Effect::Cancel));
        sup.after_cancel(true, late + TICK);
        assert!(!sup.owns_session());
    }

    #[test]
    fn cooldown_suppresses_immediate_retrigger() {
        let (mut sup, t_start) = started_supervisor();
        sup.after_stop(t_start + TICK);
        let noisy_idle = view(true, false, false, IDLE_LONG, true);
        // Within the 8 s cooldown: the still-true sensor must not re-arm.
        for i in 2..=14 {
            assert_eq!(sup.tick(&noisy_idle, t_start + TICK * i, TICK), None);
        }
        // Past cooldown: normal debounced start applies again.
        let t = t_start + Duration::from_secs(10);
        assert_eq!(sup.tick(&noisy_idle, t, TICK), None); // arming
        assert_eq!(sup.tick(&noisy_idle, t + TICK, TICK), Some(Effect::Start));
    }

    #[test]
    fn a_manual_session_is_never_touched() {
        let mut sup = Supervisor::new(sup_cfg());
        let t0 = Instant::now();
        let manual = view(true, true, true, IDLE_LONG, true);
        for i in 0..20 {
            assert_eq!(sup.tick(&manual, t0 + TICK * i, TICK), None);
        }
        // Even disabling the feature must not cancel a session we do not own.
        let disabled = view(false, true, true, IDLE_LONG, true);
        assert_eq!(sup.tick(&disabled, t0 + TICK * 21, TICK), None);
    }

    #[test]
    fn disabling_mid_session_cancels_our_session() {
        let (mut sup, t_start) = started_supervisor();
        let disabled = view(false, true, false, IDLE_LONG, true);
        assert_eq!(
            sup.tick(&disabled, t_start + TICK, TICK),
            Some(Effect::Cancel)
        );
        assert!(!sup.owns_session());
    }

    #[test]
    fn sleep_wake_cannot_bypass_the_debounce() {
        // One tick returning from a 1 h sleep must count as ≤ max_tick_dt of evidence, not
        // instantly satisfy the start window from a single observation.
        let mut sup = Supervisor::new(sup_cfg());
        let t0 = Instant::now();
        let noisy_idle = view(true, false, false, IDLE_LONG, true);
        assert_eq!(
            sup.tick(&noisy_idle, t0, Duration::from_secs(3600)),
            None,
            "a huge dt is clamped — no instant start after sleep-wake"
        );
    }

    #[test]
    fn failed_start_resets_and_allows_retry() {
        let mut sup = Supervisor::new(sup_cfg());
        let t0 = Instant::now();
        let noisy_idle = view(true, false, false, IDLE_LONG, true);
        sup.tick(&noisy_idle, t0, TICK);
        assert_eq!(sup.tick(&noisy_idle, t0 + TICK, TICK), Some(Effect::Start));
        sup.after_start(false, t0 + TICK); // permission denied, device gone, …
        assert!(!sup.owns_session());
        // The decider was reset: a fresh full window re-triggers.
        assert_eq!(sup.tick(&noisy_idle, t0 + TICK * 2, TICK), None);
        assert_eq!(
            sup.tick(&noisy_idle, t0 + TICK * 3, TICK),
            Some(Effect::Start)
        );
    }
}
