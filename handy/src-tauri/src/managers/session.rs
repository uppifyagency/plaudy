//! Long-form recording sessions — Plaude Local, Fase 0 walking skeleton.
//!
//! Handy's push-to-talk dictation buffers everything in RAM, VAD-gates it (silence
//! discarded), transcribes once on stop, and pastes into the focused app. A Plaud-
//! style *session* is different on every axis, so it lives beside that flow instead
//! of inside it:
//!   * **faithful** — captures every frame (silence included) via the recorder's
//!     un-VAD-gated [`AudioRecorder::with_chunk_sink`] tap, so the saved file is a
//!     replayable recording, not just the spoken bits;
//!   * **streamed to disk** — frames are appended to a raw PCM file as they arrive
//!     (bounded RAM for multi-hour meetings), each frame flushed so a `kill -9`
//!     loses < 30 ms;
//!   * **crash-safe** — a raw PCM file has no header to repair, so a half-written
//!     one is still readable; a leftover `*.session.pcm` on startup means the
//!     process died mid-recording and is finalized by [`SessionManager::recover_interrupted`].
//!
//! On stop the PCM is finalized to a mono 16 kHz WAV (the same archive format the
//! dictation path uses), transcribed in full (best-effort — only if a model is
//! resident), and persisted as a history row. The new row reaches the UI for free
//! via the existing `HistoryUpdatePayload::Added` event.
//!
//! The flat transcript is the canonical `transcription_history` row; when diarization
//! models are present (Fase 2), `finalize` additionally persists per-speaker segments
//! via [`crate::managers::diarization`] + `HistoryManager::save_segments`.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::audio_toolkit::audio::SystemAudioRecorder;
use crate::audio_toolkit::AudioRecorder;
use crate::managers::diarization::{
    align, drop_bleed, label_segments, merge_segments, AsrSegment, DiarizationManager, TimedSegment,
};
use crate::managers::history::{EntrySource, HistoryEntry, HistoryManager, TranscriptionStatus};
use crate::managers::transcription::TranscriptionManager;
use anyhow::{anyhow, Result};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

/// Marks an in-progress raw capture. A file with this suffix that outlives its
/// process is an interrupted session to be recovered.
const PCM_SUFFIX: &str = ".session.pcm";

/// The speaker label given to the microphone track of a dual-stream session — the user's own
/// voice. Persisted on segment rows (the UI and MCP read it back) and used by `drop_bleed` to
/// tell which side of an echo pair is the mic copy. One constant instead of a magic string
/// spread across three files.
pub(crate) const MIC_LABEL: &str = "Me";

/// Which audio source a session captures.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, specta::Type)]
pub enum Source {
    /// The microphone (Fase 0).
    Mic,
    /// macOS system / loopback audio — the other side of a call/meeting (Fase 1).
    SystemAudio,
}

impl Source {
    /// Short on-disk tag embedded in a track's PCM name, e.g. `session-<id>.mic.session.pcm`.
    fn suffix(self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::SystemAudio => "system",
        }
    }

    /// Recover the source from an orphan PCM file name — the inverse of [`Source::suffix`],
    /// kept adjacent so the writer and the parser can't silently drift apart (a drift would
    /// make recovery diarize mic audio / mislabel system audio). Round-trip pinned by a test.
    fn from_pcm_name(name: &str) -> Source {
        if name.contains(".system.") {
            Source::SystemAudio
        } else {
            Source::Mic
        }
    }
}

/// Emitted on every session start/stop so the UI's live indicator stays correct
/// even when the state changes out-of-band — the `--toggle-session` CLI path,
/// startup recovery, and the UI command all flow through `start`/`stop`, so this
/// is the single source of truth for "is a session recording right now".
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, tauri_specta::Event)]
pub struct SessionStateChanged {
    pub active: bool,
    pub source: Option<Source>,
}

/// Tap-immune loudness signal for seamless auto-capture's STOP decision. While a session holds the
/// system-audio tap, the OS "is the output device running" property reads true *because of our own
/// capture*, so it can't tell when the call goes quiet. Instead each track's PCM writer marks
/// the wall-clock of the last loud frame here; the supervisor finalizes once it has been idle long
/// enough. Cheap (one RMS per 30 ms frame) and immune to our own tap.
struct AudioActivity {
    /// `(last loud frame this session if any, session start)` under ONE mutex — a single
    /// invariant behind a single lock, so there is no cross-lock ordering to get wrong.
    state: Mutex<(Option<Instant>, Instant)>,
}

impl AudioActivity {
    fn new() -> Self {
        Self {
            state: Mutex::new((None, Instant::now())),
        }
    }
    /// Self-healing lock: a panic while holding it degrades to "state as it was",
    /// not a poison-panic on every later read.
    fn lock(&self) -> std::sync::MutexGuard<'_, (Option<Instant>, Instant)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Reset at session start AND end: no loud frame heard, stamp the clock. Resetting on
    /// stop/cancel too means `heard_audio()` can never leak a previous session's `true`
    /// into the auto-capture supervisor's next decision.
    fn begin(&self) {
        *self.lock() = (None, Instant::now());
    }
    /// Mark "system audio heard now" (called by the system track's writer on a loud frame).
    fn mark(&self) {
        self.lock().0 = Some(Instant::now());
    }
    /// True once any loud system-audio frame has been captured this session (probation gate).
    fn heard_audio(&self) -> bool {
        self.lock().0.is_some()
    }
    /// How long since the last loud frame; if none yet, since the session started.
    fn idle(&self) -> Duration {
        let (last_loud, started) = *self.lock();
        match last_loud {
            Some(t) => t.elapsed(),
            None => started.elapsed(),
        }
    }
}

/// Judges a track's frames before marking the shared loudness clock — the seam that keeps two
/// invariants apart:
///   - **system audio → loudness (RMS)**: any played sound counts (a call, a video);
///   - **microphone → VAD speech only**: room noise (fan, keyboard) must NOT count, or a false
///     auto-start would pass probation and *finalize* a junk row instead of being discarded,
///     and steady noise above the RMS floor would keep a session alive for hours.
pub(crate) struct ActivityTap {
    clock: Arc<AudioActivity>,
    judge: Judge,
}

enum Judge {
    Loudness,
    Speech(Box<dyn crate::audio_toolkit::vad::VoiceActivityDetector>),
}

impl ActivityTap {
    fn loudness(clock: Arc<AudioActivity>) -> Self {
        Self {
            clock,
            judge: Judge::Loudness,
        }
    }

    fn speech(
        clock: Arc<AudioActivity>,
        vad: Box<dyn crate::audio_toolkit::vad::VoiceActivityDetector>,
    ) -> Self {
        Self {
            clock,
            judge: Judge::Speech(vad),
        }
    }

    /// RMS above this (on [-1,1] samples) counts as "audio playing" vs silence / noise floor.
    const LOUD_RMS: f32 = 0.005;

    fn observe(&mut self, frame: &[f32]) {
        if frame.is_empty() {
            return;
        }
        let real = match &mut self.judge {
            Judge::Loudness => {
                let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
                (sum_sq / frame.len() as f32).sqrt() > Self::LOUD_RMS
            }
            // Wrong-size frames (device hiccup) are just skipped, not errors.
            Judge::Speech(vad) => vad.is_voice(frame).unwrap_or(false),
        };
        if real {
            self.clock.mark();
        }
    }
}

/// The active capture backend. Both expose the same start/stop/close surface and
/// feed the same on-disk PCM sink; they differ only in the sample source.
enum ActiveRecorder {
    Mic(AudioRecorder),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    System(SystemAudioRecorder),
}

impl ActiveRecorder {
    fn stop(&self) {
        match self {
            ActiveRecorder::Mic(r) => {
                let _ = r.stop();
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            ActiveRecorder::System(r) => {
                let _ = r.stop();
            }
        }
    }

    fn close(&mut self) {
        match self {
            ActiveRecorder::Mic(r) => {
                let _ = r.close();
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            ActiveRecorder::System(r) => {
                let _ = r.close();
            }
        }
    }
}

/// One capture track within a session: a recorder feeding its own on-disk PCM sink.
struct Track {
    recorder: ActiveRecorder,
    writer: JoinHandle<Result<u64>>,
    pcm_path: PathBuf,
    source: Source,
    /// When this track's recorder actually started capturing. Tracks are opened sequentially
    /// (the CoreAudio tap can take hundreds of ms), so finalize uses the deltas to re-align
    /// them on one timeline instead of pretending they all began at sample 0.
    started: Instant,
}

/// A finished track handed to finalize: its PCM on disk, its source, and how much later than
/// the session's first track it began capturing (prepended as silence so the mixed WAV and the
/// merged speaker timeline agree across tracks).
#[derive(Debug)]
struct CapturedTrack {
    pcm: PathBuf,
    source: Source,
    lead_silence_ms: u64,
}

/// An active session is one or more capture tracks that finalize into a single history entry:
/// their audio is mixed into one playable WAV and their transcripts merged into one
/// speaker-attributed timeline. A solo track behaves exactly as before; two tracks (mic +
/// system audio) are the dual-stream "meeting" capture.
struct ActiveSession {
    tracks: Vec<Track>,
    wav_path: PathBuf,
    /// When capture actually began (the first track's start) — lets a UI mounting
    /// mid-session show the true elapsed time instead of restarting from zero.
    started: Instant,
}

pub struct SessionManager {
    app: AppHandle,
    recordings_dir: PathBuf,
    active: Mutex<Option<ActiveSession>>,
    /// Serializes `toggle_sources`' check-then-act so two concurrent toggles (tray + CLI)
    /// queue instead of both reading "active" and racing into `stop()`.
    toggle_lock: Mutex<()>,
    /// Shared system-audio loudness clock; the auto-capture supervisor reads it to decide STOP.
    activity: Arc<AudioActivity>,
}

impl SessionManager {
    pub fn new(app: &AppHandle) -> Result<Self> {
        let recordings_dir = crate::portable::app_data_dir(app)?.join("recordings");
        fs::create_dir_all(&recordings_dir)?;
        Ok(Self {
            app: app.clone(),
            recordings_dir,
            active: Mutex::new(None),
            toggle_lock: Mutex::new(()),
            activity: Arc::new(AudioActivity::new()),
        })
    }

    /// VAD judge for the mic track's activity marking. `None` (model unresolved/unloadable)
    /// degrades to "mic never marks" — the pre-feature behavior, safe on both invariants.
    fn mic_speech_vad(&self) -> Option<Box<dyn crate::audio_toolkit::vad::VoiceActivityDetector>> {
        use tauri::Manager;
        let path = self
            .app
            .path()
            .resolve(
                "resources/models/silero_vad_v4.onnx",
                tauri::path::BaseDirectory::Resource,
            )
            .ok()?;
        match crate::audio_toolkit::vad::SileroVad::new(&path, 0.5) {
            Ok(vad) => Some(Box::new(vad)),
            Err(e) => {
                warn!("Mic activity VAD unavailable ({e}); mic won't mark the loudness clock.");
                None
            }
        }
    }

    /// Self-healing lock on the active session: a panic while holding it (poison) degrades to
    /// "state as it was" instead of turning every later session call into a panic.
    fn active_guard(&self) -> std::sync::MutexGuard<'_, Option<ActiveSession>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn is_active(&self) -> bool {
        self.active_guard().is_some()
    }

    /// Milliseconds since the active session began capturing, if one is running.
    pub fn elapsed_ms(&self) -> Option<u32> {
        self.active_guard()
            .as_ref()
            .map(|s| s.started.elapsed().as_millis().min(u32::MAX as u128) as u32)
    }

    /// How long ALL captured tracks (mic and system audio) have been silent — the auto-capture
    /// supervisor's tap-immune STOP signal (the OS device sensor is useless once our own tap
    /// is open).
    pub fn audio_idle(&self) -> Duration {
        self.activity.idle()
    }

    /// Whether any real (loud) audio has been captured on ANY track this session — the
    /// auto-capture supervisor's probation gate to discard false starts.
    pub fn audio_heard(&self) -> bool {
        self.activity.heard_audio()
    }

    /// Start if idle, stop if active. Returns whether a session is active afterwards.
    pub fn toggle(&self, source: Source) -> Result<bool> {
        self.toggle_sources(&[source])
    }

    /// Toggle a multi-source session — the menu-bar "graffetta" uses `[Mic, SystemAudio]` so one
    /// click captures both sides of a call. Returns whether a session is active afterwards.
    pub fn toggle_sources(&self, sources: &[Source]) -> Result<bool> {
        // Hold the toggle lock across check-then-act: two toggles racing (tray click + CLI
        // flag) execute in order, the second acting on the first's outcome instead of both
        // observing "active" and racing into stop().
        let _toggling = self
            .toggle_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_active() {
            self.stop()?;
            Ok(false)
        } else {
            self.start_sources(sources)?;
            Ok(true)
        }
    }

    pub fn start(&self, source: Source) -> Result<()> {
        self.start_sources(&[source])
    }

    /// Start a session capturing each source as its own track. Sources are **best-effort**: one
    /// that fails to start (system-audio permission denied, nothing playing, unsupported target)
    /// is skipped with a warning so the session still records whatever it can — it only errors
    /// when *no* source starts. This is the seamless, self-healing capture the product wants:
    /// one click records the mic, and the system audio too whenever it is available.
    pub fn start_sources(&self, sources: &[Source]) -> Result<()> {
        let mut guard = self.active_guard();
        if guard.is_some() {
            return Err(anyhow!("A session is already active"));
        }

        let id = new_session_id();
        let wav_path = self.recordings_dir.join(format!("{id}.wav"));

        // Fresh session: no real audio heard yet (probation starts now).
        self.activity.begin();
        let mut tracks: Vec<Track> = Vec::new();
        for &source in sources {
            // Only the system track drives the auto-capture STOP clock (it's the "other side"
            // of the call going quiet that should end the recording, not the mic).
            // EVERY track feeds the shared loudness clock (the trigger is source-agnostic,
            // so probation/STOP must hear the mic too — a voice-only session would otherwise
            // be discarded as silent) — but through per-source judges: see [`ActivityTap`].
            let activity = match source {
                Source::Mic => self
                    .mic_speech_vad()
                    .map(|vad| ActivityTap::speech(self.activity.clone(), vad)),
                Source::SystemAudio => Some(ActivityTap::loudness(self.activity.clone())),
            };
            // If this track's PCM writer ever dies (disk full), stop the whole session so the
            // audio flushed so far is finalized instead of the UI claiming "recording" for
            // hours while nothing is captured. Stop from a fresh thread: stop() joins the
            // writer threads (including the one invoking this callback) and may have to wait
            // for this very function to release the session lock.
            let on_writer_fail = {
                let app = self.app.clone();
                move || {
                    error!(
                        "Session track {source:?} writer failed; stopping session to preserve captured audio"
                    );
                    std::thread::spawn(move || {
                        if let Some(sm) = app.try_state::<Arc<SessionManager>>() {
                            if let Err(e) = sm.stop() {
                                warn!("Auto-stop after writer failure: {e}");
                            }
                        }
                    });
                }
            };
            match build_track(source, &id, &self.recordings_dir, activity, on_writer_fail) {
                Ok(track) => {
                    info!(
                        "Session track started ({source:?}) → {}",
                        track.pcm_path.display()
                    );
                    tracks.push(track);
                }
                Err(e) => warn!("Session source {source:?} unavailable, skipping: {e}"),
            }
        }
        if tracks.is_empty() {
            return Err(anyhow!("No audio source could be started for the session"));
        }

        let primary = tracks.first().map(|t| t.source);
        let started = tracks
            .iter()
            .map(|t| t.started)
            .min()
            .unwrap_or_else(Instant::now);
        *guard = Some(ActiveSession {
            tracks,
            wav_path,
            started,
        });
        self.unlock_and_emit(guard, primary);
        Ok(())
    }

    /// The ONE way to announce a session state change. Takes the lock guard BY VALUE and
    /// drops it before emitting: the `SessionStateChanged` listener (lib.rs) runs INLINE on
    /// this thread and re-enters `is_active()`, so emitting while holding the non-reentrant
    /// lock deadlocks — a bug this codebase has already shipped once. Owning the guard here
    /// makes the unlock-before-emit order a compile-time property instead of a comment: a
    /// caller cannot still hold the lock after this call, because it gave the guard away.
    ///
    /// The state is re-read at emit time so the LAST event on the wire always reflects
    /// reality — a concurrent start/stop that slipped into the post-unlock window wins.
    fn unlock_and_emit(
        &self,
        guard: std::sync::MutexGuard<'_, Option<ActiveSession>>,
        source: Option<Source>,
    ) {
        drop(guard);
        let _ = SessionStateChanged {
            active: self.is_active(),
            source,
        }
        .emit(&self.app);
    }

    pub fn stop(&self) -> Result<()> {
        let mut guard = self.active_guard();
        let session = guard.take().ok_or_else(|| anyhow!("No active session"))?;

        let ActiveSession {
            tracks,
            wav_path,
            started: _,
        } = session;

        // Flip the live indicator to idle as soon as the user stops — before the (possibly
        // slow) frame drain — so the UI feels responsive. Finalization and the row that
        // follows happen off-thread.
        self.unlock_and_emit(guard, None);

        // Tear down every track, then compute each one's offset from the session's first
        // capture instant — that delta becomes leading silence in finalize, so the mixed WAV
        // and the merged speaker timeline share one clock.
        let torn: Vec<(PathBuf, Source, Instant)> =
            tracks.into_iter().map(teardown_track).collect();
        let first_start = torn
            .iter()
            .map(|(_, _, s)| *s)
            .min()
            .expect("tracks is non-empty");
        let captured: Vec<CapturedTrack> = torn
            .into_iter()
            .map(|(pcm, source, started)| CapturedTrack {
                pcm,
                source,
                lead_silence_ms: started.duration_since(first_start).as_millis() as u64,
            })
            .collect();

        // Reset the loudness clock so the auto-capture supervisor can never read a previous
        // session's "heard audio" as this idle period's state.
        self.activity.begin();

        // Finalize off-thread: transcribing a long file is slow and must not block the caller.
        let app = self.app.clone();
        std::thread::spawn(move || {
            if let Err(e) = finalize_session(&app, &captured, &wav_path) {
                error!("Failed to finalize session {}: {e}", wav_path.display());
            }
        });
        Ok(())
    }

    /// Stop capture and DISCARD the recording — no WAV, no history row. Used by seamless
    /// auto-capture to abandon a *false start* (triggered by the OS sensor, but no real audio
    /// arrived within probation) so it never pollutes History.
    pub fn cancel(&self) -> Result<()> {
        let mut guard = self.active_guard();
        let session = guard.take().ok_or_else(|| anyhow!("No active session"))?;
        let ActiveSession {
            tracks,
            wav_path: _,
            started: _,
        } = session;

        self.unlock_and_emit(guard, None);

        for track in tracks {
            let (pcm_path, source, _) = teardown_track(track);
            let _ = fs::remove_file(&pcm_path);
            info!(
                "Auto-capture: discarded false-start {source:?} track ({})",
                pcm_path.display()
            );
        }
        self.activity.begin();
        Ok(())
    }

    /// Recover any session whose process died mid-finalize or mid-recording. Safe to call
    /// once at startup, after the history and transcription managers are in managed state.
    /// Orphan PCMs are grouped by session id so a dual-stream crash comes back as ONE
    /// session (mic labelled "Me"), and a PCM whose WAV already exists is only cleaned up —
    /// re-finalizing it would duplicate the history row (see [`plan_recovery`]).
    ///
    /// BLOCKING and SEQUENTIAL by design: it finalizes crashed sessions one at a time on the
    /// caller's thread, never a thread per session. A parallel recovery at boot would hold
    /// (N+1)× full-audio buffers (~350 MB each) and N diarization runtimes at once — the worst
    /// possible moment. Serial recovery peaks at 1×; total latency is unchanged because the
    /// inference gate already serializes the ASR runs. Call it off the setup thread (see
    /// `lib.rs`) so startup itself is not blocked.
    pub fn recover_interrupted(&self) {
        let Ok(entries) = fs::read_dir(&self.recordings_dir) else {
            return;
        };
        let pcms: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(PCM_SUFFIX))
            .collect();

        let hm = self.app.state::<Arc<HistoryManager>>();
        // A DB error while probing for a row must NOT be read as "no row" — that would fabricate
        // a duplicate. Treat an errored probe as "row exists" so we fall back to the safe
        // cleanup-only path (never an adopt) and leave the audio for a later, healthy run.
        let row_exists = |wav: &Path| {
            wav.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .map(|name| hm.entry_exists_for_file(&name).unwrap_or(true))
                .unwrap_or(true)
        };

        for plan in plan_recovery(pcms, |wav| wav.exists(), row_exists) {
            match plan {
                RecoveryPlan::CleanupArchived(pcm) => {
                    info!(
                        "Recovery: {} already archived (WAV exists) → removing PCM",
                        pcm.display()
                    );
                    let _ = fs::remove_file(&pcm);
                }
                RecoveryPlan::AdoptOrphanArchive {
                    pcms,
                    wav_path,
                    source,
                } => {
                    warn!(
                        "Recovery: {} archived but no history row → adopting as retryable",
                        wav_path.display()
                    );
                    match wav_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                    {
                        Some(file_name) => {
                            match hm.save_pending_entry(file_name, source) {
                                Ok(entry) => {
                                    // Failed + empty transcript = the standard retryable state; the
                                    // audio is in the WAV, so the retry rebuilds the rest on demand.
                                    if let Err(e) = hm.update_transcription(
                                        entry.id,
                                        String::new(),
                                        None,
                                        None,
                                        TranscriptionStatus::Failed,
                                    ) {
                                        error!("Recovery: failed to mark adopted orphan retryable: {e}");
                                    }
                                    for pcm in &pcms {
                                        let _ = fs::remove_file(pcm);
                                    }
                                }
                                // Leave the PCMs on disk so a future healthy run retries the adopt.
                                Err(e) => error!(
                                    "Recovery: failed to adopt orphan {}: {e}",
                                    wav_path.display()
                                ),
                            }
                        }
                        None => error!(
                            "Recovery: orphan WAV path has no file name: {}",
                            wav_path.display()
                        ),
                    }
                }
                RecoveryPlan::Refinalize { tracks, wav_path } => {
                    warn!("Recovering interrupted session → {}", wav_path.display());
                    // Serial, in-line: one finalize at a time so RAM peaks at 1× (see the
                    // method doc). Each carries its own error — a failed row heals via
                    // `fail_stale_transcribing` + the History retry affordance.
                    if let Err(e) = finalize_session(&self.app, &tracks, &wav_path) {
                        error!("Recovery failed for {}: {e}", wav_path.display());
                    }
                }
            }
        }
    }
}

/// One recovery decision for a crashed session's leftovers.
#[derive(Debug)]
enum RecoveryPlan {
    /// The PCM's WAV exists AND a history row points at it: the session was fully persisted
    /// before the crash. Only remove the PCM — re-finalizing would insert a second row for the
    /// same recording. The row itself is healed by `fail_stale_transcribing` + the retry.
    CleanupArchived(PathBuf),
    /// The WAV exists but NO history row points at it: the crash landed in the finalize window
    /// between archiving the audio (`stream_mix_to_wav`) and writing the row (`save_pending_entry`),
    /// leaving audio no one can see. Adopt it: create a retryable `Failed` row for the WAV, then
    /// remove the PCMs (the retry rebuilds transcript + segments from the WAV on demand). `source`
    /// is inferred from the PCM name(s) — dual ⇒ Meeting, else mic/system.
    AdoptOrphanArchive {
        pcms: Vec<PathBuf>,
        wav_path: PathBuf,
        source: EntrySource,
    },
    /// No WAV yet: finalize these tracks as one session (a dual-stream crash yields two
    /// PCMs with the same session id — they must come back as ONE session, mic first so
    /// the "Me" labelling applies, not as two unrelated recordings).
    Refinalize {
        tracks: Vec<CapturedTrack>,
        wav_path: PathBuf,
    },
}

/// Which capture path an orphaned archive came from, inferred from its PCM name(s) — the
/// recovery-side twin of [`entry_source_for`] (which reads `ArchivedTrack`s); dual ⇒ Meeting.
fn orphan_source(pcms: &[PathBuf]) -> EntrySource {
    if pcms.len() > 1 {
        EntrySource::Meeting
    } else {
        match pcms
            .first()
            .map(|p| Source::from_pcm_name(&p.to_string_lossy()))
        {
            Some(Source::SystemAudio) => EntrySource::System,
            _ => EntrySource::Mic,
        }
    }
}

/// Pure recovery planning over the orphan PCM list — unit-testable without a filesystem
/// scan (`wav_exists` and `row_exists` are injected). Groups by session id: the filename stem
/// up to the first `.` (`session-<millis>-<seq>`), which both `.mic.`/`.system.` dual names and
/// legacy single-track names share. Lead offsets are unknown across a crash → 0. `row_exists`
/// is keyed on the session's WAV path so an archived-but-rowless session (a crash in the
/// finalize window) is adopted rather than silently cleaned up into an invisible orphan.
fn plan_recovery(
    pcms: Vec<PathBuf>,
    wav_exists: impl Fn(&Path) -> bool,
    row_exists: impl Fn(&Path) -> bool,
) -> Vec<RecoveryPlan> {
    use std::collections::BTreeMap;

    // BTreeMap for deterministic plan order (stable logs and tests).
    let mut sessions: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for pcm in pcms {
        let id = pcm
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        sessions.entry(id).or_default().push(pcm);
    }

    let mut plans = Vec::new();
    for (id, mut group) in sessions {
        let wav_path = group[0]
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(format!("{id}.wav"));
        if wav_exists(&wav_path) {
            if row_exists(&wav_path) {
                plans.extend(group.into_iter().map(RecoveryPlan::CleanupArchived));
            } else {
                // Audio archived but the row never landed — adopt it as one retryable session.
                let source = orphan_source(&group);
                plans.push(RecoveryPlan::AdoptOrphanArchive {
                    pcms: group,
                    wav_path,
                    source,
                });
            }
            continue;
        }
        // Mic first: track order is the merge tie-break, and dual labelling expects it.
        group.sort_by_key(|p| {
            matches!(
                Source::from_pcm_name(&p.to_string_lossy()),
                Source::SystemAudio
            )
        });
        let tracks = group
            .into_iter()
            .map(|pcm| {
                let source = Source::from_pcm_name(&pcm.to_string_lossy());
                CapturedTrack {
                    pcm,
                    source,
                    lead_silence_ms: 0,
                }
            })
            .collect();
        plans.push(RecoveryPlan::Refinalize { tracks, wav_path });
    }
    plans
}

/// Stop, close and join one track's capture pipeline, logging the writer's outcome.
/// Shared by `stop()` (which keeps the PCM for finalize) and `cancel()` (which discards it).
fn teardown_track(track: Track) -> (PathBuf, Source, Instant) {
    let Track {
        mut recorder,
        writer,
        pcm_path,
        source,
        started,
    } = track;
    recorder.stop();
    recorder.close();
    drop(recorder);
    match writer.join() {
        Ok(Ok(samples)) => info!("Session track {source:?} captured {samples} samples"),
        Ok(Err(e)) => error!("Session track {source:?} writer error: {e}"),
        Err(_) => error!("Session track {source:?} writer thread panicked"),
    }
    (pcm_path, source, started)
}

/// Collision-resistant session id: millisecond timestamp plus a per-process
/// monotonic counter, so two sessions started in the same millisecond (e.g. a
/// scripted double `--toggle-session`) can never derive the same file path.
fn new_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = chrono::Utc::now().timestamp_millis();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("session-{millis}-{seq}")
}

/// Build + start one capture track: open the recorder for `source` and spawn its PCM writer.
/// The recorder is started FIRST so a failure (e.g. denied permission) leaves no orphan file.
fn build_track(
    source: Source,
    id: &str,
    dir: &Path,
    activity: Option<ActivityTap>,
    on_writer_fail: impl FnOnce() + Send + 'static,
) -> Result<Track> {
    let pcm_path = dir.join(format!("{id}.{}{PCM_SUFFIX}", source.suffix()));
    // Unbounded consumer → PCM-writer channel. Backpressure limitation (same as the recorder's
    // sample channel): a stalled writer thread queues audio silently. std::mpsc has no depth
    // probe; the writer is a simple disk append that keeps up, so we accept it rather than switch
    // to a bounded channel (which would risk dropping capture — the semantics are live-validated).
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let recorder = build_recorder(source, tx)?;
    // Stamp the capture start as close to the recorder's first frame as we can observe —
    // finalize aligns the tracks on these deltas (approximate, but hundreds of ms better
    // than pretending sequentially-opened tracks began together).
    let started = Instant::now();
    let writer = spawn_pcm_writer(pcm_path.clone(), rx, activity, on_writer_fail);
    Ok(Track {
        recorder,
        writer,
        pcm_path,
        source,
        started,
    })
}

/// Construct and start the capture backend for `source`, feeding `tx` (the
/// on-disk PCM sink). The downstream PCM-writer → WAV → transcribe tail is
/// identical regardless of source.
fn build_recorder(source: Source, tx: mpsc::Sender<Vec<f32>>) -> Result<ActiveRecorder> {
    match source {
        Source::Mic => {
            let mut r = AudioRecorder::new()
                .map_err(|e| anyhow!("create mic recorder: {e}"))?
                .with_chunk_sink(tx);
            r.open(None).map_err(|e| anyhow!("open microphone: {e}"))?;
            r.start().map_err(|e| anyhow!("start mic capture: {e}"))?;
            Ok(ActiveRecorder::Mic(r))
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        Source::SystemAudio => {
            let mut r = SystemAudioRecorder::new()
                .map_err(|e| anyhow!("create system-audio recorder: {e}"))?
                .with_chunk_sink(tx);
            r.open().map_err(|e| {
                anyhow!("open system audio (grant Audio Recording permission?): {e}")
            })?;
            r.start()
                .map_err(|e| anyhow!("start system-audio capture: {e}"))?;
            Ok(ActiveRecorder::System(r))
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        Source::SystemAudio => {
            drop(tx);
            Err(anyhow!(
                "System audio capture is only available on Apple Silicon macOS"
            ))
        }
    }
}

/// Append captured frames to `pcm_path` as little-endian i16, flushing each frame
/// so an abrupt kill loses at most one ~30 ms frame. Ends when the sink closes.
///
/// A writer that dies mid-session (disk full, file error) must never fail silently — the UI
/// would keep saying "recording" for hours while nothing is captured. Any write error invokes
/// `on_fail` (once, from the writer thread) so the session can stop itself and preserve what
/// was already flushed to disk.
fn spawn_pcm_writer(
    pcm_path: PathBuf,
    rx: mpsc::Receiver<Vec<f32>>,
    activity: Option<ActivityTap>,
    on_fail: impl FnOnce() + Send + 'static,
) -> JoinHandle<Result<u64>> {
    std::thread::spawn(move || {
        let result = write_pcm_stream(&pcm_path, rx, activity);
        if result.is_err() {
            on_fail();
        }
        result
    })
}

/// The writer-thread body: create the file and append frames until the sink closes.
/// Synchronous and handle-free so tests can drive it without a thread.
fn write_pcm_stream(
    pcm_path: &Path,
    rx: mpsc::Receiver<Vec<f32>>,
    mut activity: Option<ActivityTap>,
) -> Result<u64> {
    let mut out = BufWriter::new(File::create(pcm_path)?);
    let mut written: u64 = 0;
    while let Ok(frame) = rx.recv() {
        if let Some(tap) = &mut activity {
            tap.observe(&frame);
        }
        for &s in &frame {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            out.write_all(&v.to_le_bytes())?;
        }
        written += frame.len() as u64;
        out.flush()?;
    }
    out.flush()?;
    Ok(written)
}

/// The slice of the transcription manager finalize needs — a seam (Stubs for Queries) so the
/// persist stage is unit-testable without loading a real ASR model.
pub(crate) trait Transcriber {
    /// Get a model resident (loading one on demand if needed) and report whether
    /// transcription can proceed. Finalize must never fail a row just because the idle
    /// unloader happened to have evicted the model a moment earlier.
    fn ensure_model_ready(&self) -> bool;
    fn transcribe_with_segments(&self, samples: Vec<f32>) -> Result<(String, Vec<AsrSegment>)>;
}

impl Transcriber for TranscriptionManager {
    fn ensure_model_ready(&self) -> bool {
        TranscriptionManager::ensure_model_ready(self)
    }
    fn transcribe_with_segments(&self, samples: Vec<f32>) -> Result<(String, Vec<AsrSegment>)> {
        TranscriptionManager::transcribe_with_segments(self, samples)
    }
}

/// The history operations finalize performs — the matching seam on the persistence side.
pub(crate) trait SessionSink {
    fn save_pending_entry(&self, file_name: String, source: EntrySource) -> Result<HistoryEntry>;
    fn save_segments(&self, history_id: i64, segments: &[TimedSegment]) -> Result<()>;
    fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        status: TranscriptionStatus,
    ) -> Result<()>;
    /// Whether the row still exists — finalize uses this to treat a mid-flight deletion as a
    /// benign discard rather than a hard failure.
    fn entry_exists(&self, id: i64) -> Result<bool>;
}

impl SessionSink for HistoryManager {
    fn save_pending_entry(&self, file_name: String, source: EntrySource) -> Result<HistoryEntry> {
        HistoryManager::save_pending_entry(self, file_name, source)
    }
    fn save_segments(&self, history_id: i64, segments: &[TimedSegment]) -> Result<()> {
        HistoryManager::save_segments(self, history_id, segments)
    }
    fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        status: TranscriptionStatus,
    ) -> Result<()> {
        HistoryManager::update_transcription(self, id, transcription_text, None, None, status)
            .map(|_| ())
    }
    fn entry_exists(&self, id: i64) -> Result<bool> {
        HistoryManager::entry_exists(self, id)
    }
}

/// Finalize a session from its captured tracks: stream-mix the audio into one playable WAV,
/// then (best-effort, only if a model is resident) transcribe each track, attribute speakers,
/// and merge into one chronological timeline persisted as a single history entry.
///
/// This is a thin shell: it resolves the managers and owns the PCM end-of-life; the logic
/// lives in the handle-free [`archive_tracks`] + [`persist_session`] (both under test).
fn finalize_session(app: &AppHandle, tracks: &[CapturedTrack], wav_path: &Path) -> Result<()> {
    let Some(archived) = archive_tracks(tracks, wav_path)? else {
        return Ok(()); // every track was empty — silence, nothing to keep
    };

    let tm = app.state::<Arc<TranscriptionManager>>();
    let hm = app.state::<Arc<HistoryManager>>();
    let diarizer = DiarizationManager::new(&crate::portable::app_data_dir(app)?.join("models"));

    let outcome = persist_session(&**tm, &**hm, &diarizer, &archived, wav_path);
    if outcome.is_ok() {
        // The PCMs have served both the mix and the per-track transcription; the WAV holds
        // the audio and the row exists — remove them. On error they stay on disk, and
        // recovery's "WAV already exists" rule cleans them up WITHOUT re-finalizing (which
        // would duplicate the history row).
        for t in &archived {
            let _ = fs::remove_file(&t.pcm);
        }
    }
    outcome
}

/// A track that made it into the mixed WAV: its PCM (still on disk — it is also the per-track
/// transcription source), its source, and its lead offset on the shared timeline in samples.
struct ArchivedTrack {
    pcm: PathBuf,
    source: Source,
    lead_samples: usize,
}

/// Which capture path produced this session — persisted on the row so the UI reads it
/// directly instead of re-deriving it from magic speaker labels.
fn entry_source_for(archived: &[ArchivedTrack]) -> EntrySource {
    if archived.len() > 1 {
        EntrySource::Meeting
    } else {
        match archived.first().map(|t| t.source) {
            Some(Source::SystemAudio) => EntrySource::System,
            _ => EntrySource::Mic,
        }
    }
}

/// The manager-facing half of finalize, behind seams so it is unit-testable: create the
/// pending row immediately (the session must show in History the moment the user stops),
/// transcribe each track — ONE track resident in RAM at a time — attribute speakers, merge,
/// and flip the row's status.
fn persist_session(
    tm: &dyn Transcriber,
    hm: &dyn SessionSink,
    diarizer: &DiarizationManager,
    archived: &[ArchivedTrack],
    wav_path: &Path,
) -> Result<()> {
    let file_name = wav_path
        .file_name()
        .ok_or_else(|| anyhow!("session WAV path has no file name"))?
        .to_string_lossy()
        .to_string();
    let entry = hm.save_pending_entry(file_name, entry_source_for(archived))?;

    // Get a model resident, loading it on demand — the idle unloader means "not loaded right
    // now" is the NORMAL state when a session finalizes, not an error (every auto-captured
    // session used to fail here). Only when no model can load (none selected/downloaded) does
    // the row go Failed — a missing transcript never costs the recording, the audio is already
    // in the WAV, and Failed (not Done) keeps the History retry affordance visible.
    if !tm.ensure_model_ready() {
        warn!("No transcription model available; session audio saved, transcript pending retry");
        hm.update_transcription(entry.id, String::new(), TranscriptionStatus::Failed)?;
        return Ok(());
    }

    let (track_segments, full_texts, any_error) = transcribe_tracks(tm, diarizer, archived);

    // The row can vanish while we transcribed (the user discarded the recording from History, or
    // retention trimmed it — transcription of a long dual session takes minutes). That is a
    // benign discard, not a failure: return Ok so finalize_session removes the PCMs and startup
    // recovery can't resurrect the deleted recording. Writing segments/status to the gone row
    // would otherwise FK-fail (WARN) then hard-error, orphaning the PCMs.
    if !hm.entry_exists(entry.id)? {
        info!(
            "Session row {} deleted before finalize completed — discarding.",
            entry.id
        );
        return Ok(());
    }

    // Merge the tracks chronologically, then drop microphone "Me" segments that are just the
    // system audio echoing back through the speakers (acoustic bleed when not on headphones) —
    // one person must never appear as two speakers. Persist before flipping status so the UI
    // finds the segments when it re-fetches on completion.
    let merged = drop_bleed(merge_segments(track_segments), MIC_LABEL);
    if !merged.is_empty() {
        if let Err(e) = hm.save_segments(entry.id, &merged) {
            warn!("Failed to persist merged segments: {e}");
        }
    }
    let transcript = flat_transcript(&merged, &full_texts);
    let status = resolve_status(any_error);
    hm.update_transcription(entry.id, transcript, status)?;

    info!("Session finalized → {}", wav_path.display());
    Ok(())
}

/// Transcribe each archived track, reading its PCM from disk one at a time — the finalize
/// counterpart of the capture path's bounded-RAM design (a 3 h dual session peaks at one
/// decoded track, not all of them plus a mix buffer). Per-track failures degrade to a partial
/// transcript (`any_error` reports them); the audio is already safe in the WAV either way.
fn transcribe_tracks(
    tm: &dyn Transcriber,
    diarizer: &DiarizationManager,
    archived: &[ArchivedTrack],
) -> (Vec<Vec<TimedSegment>>, Vec<String>, bool) {
    let mut full_texts: Vec<String> = Vec::new();
    let mut track_segments: Vec<Vec<TimedSegment>> = Vec::new();
    let mut any_error = false;

    for track in archived {
        let samples = match read_pcm_i16(&track.pcm) {
            Ok(raw) => raw,
            Err(e) => {
                // Its audio is already in the WAV; the History retry can re-transcribe.
                warn!(
                    "Session track {:?} PCM unreadable at transcription: {e}",
                    track.source
                );
                any_error = true;
                continue;
            }
        };

        // A mic track is a single known voice ("Me") — the dual-session mic AND a solo mic
        // recording alike — so it is labelled, never diarized: a 90-min solo dictation must not
        // pay sherpa's whole-track diarization cost for one known speaker (C2). Everything else
        // (system audio) is diarized. Diarize before transcription consumes `samples`.
        let label_as_me = matches!(track.source, Source::Mic);
        let turns = if label_as_me {
            Vec::new()
        } else {
            diarizer.diarize(&samples)
        };
        match tm.transcribe_with_segments(samples) {
            Ok((text, asr)) => {
                if !text.trim().is_empty() {
                    full_texts.push(text);
                }
                // M3: the lead silence is metadata, not data. Diarizer turns and ASR segments
                // both come back in the same track-local frame (both engines ran on the raw
                // samples), so one arithmetic shift of the aligned result lands them on the
                // shared timeline — identical timestamps to the old physically-prepended
                // zeros, without copying a multi-hour buffer to represent an offset.
                let mut segments = if label_as_me {
                    label_segments(&asr, MIC_LABEL)
                } else {
                    align(&asr, &turns)
                };
                shift_segments(&mut segments, lead_ms(track.lead_samples));
                track_segments.push(segments);
            }
            Err(e) => {
                warn!("Session track {:?} transcription failed: {e}", track.source);
                any_error = true;
            }
        }
    }
    (track_segments, full_texts, any_error)
}

/// Duration of a track's lead silence in ms — the shift that places its segments on the shared
/// session timeline. Equal by construction to the `lead_silence_ms` this was derived from
/// (`lead_samples = lead_silence_ms * WHISPER_SAMPLE_RATE / 1000`), so it exactly reproduces the
/// old physical pad's offset.
fn lead_ms(lead_samples: usize) -> i64 {
    use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
    lead_samples as i64 * 1000 / WHISPER_SAMPLE_RATE as i64
}

/// Move every segment forward by the track's lead (M3): the pure-arithmetic replacement for
/// prepending `lead_samples` zeros to the engine input. A zero lead (the session's first track)
/// is a no-op.
fn shift_segments(segments: &mut [TimedSegment], lead_ms: i64) {
    if lead_ms == 0 {
        return;
    }
    for s in segments {
        s.start_ms += lead_ms;
        s.end_ms += lead_ms;
    }
}

/// Build the flat transcript from the de-duped timeline when segments exist (so the bleed
/// copy is gone from the flat text too); fall back to the raw per-track texts only when the
/// ASR model returned no segment timings to de-dup on.
fn flat_transcript(merged: &[TimedSegment], full_texts: &[String]) -> String {
    if merged.is_empty() {
        full_texts.join("\n")
    } else {
        merged
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Dual-track fail-fast honesty (C1): a session where ANY track's transcription errored is
/// marked `Failed`, never `Done`. A meeting that silently lost half its speakers must not read
/// as a completed transcript — the earlier "partial is Done" policy hid exactly that. The
/// partial transcript AND its segments are still persisted (visible in History) and the audio
/// is safe in the mixed WAV, so the History retry can re-transcribe the whole recording; only
/// the status flips so the retry affordance stays visible. A clean run (no per-track error) is
/// `Done` — including a silence-only session that produced no text but errored on nothing.
fn resolve_status(any_error: bool) -> TranscriptionStatus {
    if any_error {
        TranscriptionStatus::Failed
    } else {
        TranscriptionStatus::Done
    }
}

/// Probe the track PCMs and stream-mix them into the playable WAV archive — chunk-by-chunk,
/// never the whole multi-hour session in RAM. Returns the archived tracks, or `None` when
/// every readable track was empty (silence — discard; those PCMs are removed here).
///
/// Alignment: a track that started `lead_silence_ms` after the session's first track is
/// shifted by that much on the shared timeline, both in the mix (here, by placing its samples
/// at the `lead_samples` offset) and in the segment timestamps (`transcribe_tracks` shifts the
/// aligned segments by the same lead — M3: arithmetic, no prepended zeros).
///
/// PCM lifecycle (the audio-safety invariant): on success the PCMs are KEPT — they are still
/// the per-track transcription source; the caller removes them only once the whole finalize
/// succeeded. On failure (unreadable PCM, disk-full WAV write) everything is kept for
/// `recover_interrupted`, whose "WAV already exists" rule prevents duplicate rows. No failure
/// path may delete the only copy of the user's audio.
fn archive_tracks(tracks: &[CapturedTrack], wav_path: &Path) -> Result<Option<Vec<ArchivedTrack>>> {
    use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;

    // Probe by metadata (no decode): a PCM with fewer than 2 bytes has zero samples. Per-track
    // failures degrade gracefully — skip the track (keeping its file on disk for recovery) so
    // one bad PCM can't hold the other track's audio hostage; fail only when nothing was
    // readable at all.
    let mut archived: Vec<ArchivedTrack> = Vec::new();
    let mut any_unreadable = false;
    for track in tracks {
        match fs::metadata(&track.pcm) {
            Ok(meta) if meta.len() >= 2 => archived.push(ArchivedTrack {
                pcm: track.pcm.clone(),
                source: track.source,
                lead_samples: (track.lead_silence_ms * WHISPER_SAMPLE_RATE as u64 / 1000) as usize,
            }),
            Ok(_) => {
                // Empty capture — nothing to lose.
                let _ = fs::remove_file(&track.pcm);
            }
            Err(e) => {
                any_unreadable = true;
                warn!(
                    "Session track {:?} PCM unreadable, kept for recovery: {e}",
                    track.source
                );
            }
        }
    }
    if archived.is_empty() {
        if any_unreadable {
            return Err(anyhow!("no session track could be read"));
        }
        warn!("Session has no audio in any track; discarding");
        return Ok(None);
    }

    stream_mix_to_wav(&archived, wav_path)?;
    Ok(Some(archived))
}

/// Sum the archived tracks' PCM files into the mono 16 kHz WAV chunk-by-chunk (clamped to
/// [-1, 1], shorter tracks padded with silence). Peak RAM is one chunk per track (~256 KiB),
/// not the whole session — the finalize counterpart of the capture path's streaming design.
///
/// ponytail: plain sum + clamp — this is the whole mixer (the old in-RAM `mix_tracks` was
/// replaced by this streaming version). Two people rarely talk over each other, so clipping is
/// rare; a soft limiter is the upgrade path if overlap distortion is ever heard.
fn stream_mix_to_wav(tracks: &[ArchivedTrack], wav_path: &Path) -> Result<()> {
    use std::io::{BufReader, Read};

    const CHUNK_SAMPLES: usize = 65_536;

    // Each track occupies [lead, lead + len) on the shared timeline. A torn trailing byte
    // (kill -9 mid-sample) is excluded by the len/2 floor, mirroring `read_pcm_i16`.
    struct TrackReader {
        reader: BufReader<File>,
        start: usize,
        end: usize,
    }
    let mut readers = Vec::with_capacity(tracks.len());
    for t in tracks {
        let len_samples = (fs::metadata(&t.pcm)?.len() / 2) as usize;
        readers.push(TrackReader {
            reader: BufReader::new(File::open(&t.pcm)?),
            start: t.lead_samples,
            end: t.lead_samples + len_samples,
        });
    }
    let total = readers.iter().map(|r| r.end).max().unwrap_or(0);

    let mut writer = crate::audio_toolkit::create_wav_writer(wav_path)?;
    let mut acc = vec![0.0f32; CHUNK_SAMPLES];
    let mut byte_buf = vec![0u8; CHUNK_SAMPLES * 2];
    let mut pos = 0usize;
    while pos < total {
        let fill = CHUNK_SAMPLES.min(total - pos);
        acc[..fill].fill(0.0);
        for tr in &mut readers {
            // This track's overlap with the chunk [pos, pos + fill). Chunks advance
            // sequentially, so each file is read strictly in order — no seeking.
            let from = tr.start.max(pos);
            let to = tr.end.min(pos + fill);
            if from >= to {
                continue;
            }
            let n = to - from;
            let off = from - pos;
            tr.reader.read_exact(&mut byte_buf[..n * 2])?;
            for (i, b) in byte_buf[..n * 2].chunks_exact(2).enumerate() {
                acc[off + i] += i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32;
            }
        }
        for &s in &acc[..fill] {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        pos += fill;
    }
    writer.finalize()?;
    Ok(())
}

/// Decode a raw little-endian i16 PCM file to normalised f32. A dangling odd byte
/// from a torn final write is dropped, so crash-truncated files decode cleanly instead
/// of panicking.
///
/// M3: read in fixed-size chunks straight into the one f32 buffer (`with_capacity` sized from
/// the file length) instead of `fs::read` + convert. The old shape held BOTH the raw byte Vec
/// and the f32 Vec alive at once — a 512 MB transient at 89 min just to obtain a 342 MB buffer.
/// Now only the destination buffer plus one small read window ever live; `out` is the legitimate
/// resident working set (ONE track at a time), the transient is gone.
fn read_pcm_i16(path: &Path) -> Result<Vec<f32>> {
    use std::io::{BufReader, Read};

    // 64k i16 samples per read — the transient read window is 128 KiB regardless of track length.
    const CHUNK_BYTES: usize = 64 * 1024 * 2;

    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let mut reader = BufReader::new(file);
    let mut out: Vec<f32> = Vec::with_capacity(len / 2);
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut carry: Option<u8> = None;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut bytes = &buf[..n];
        if let Some(lo) = carry.take() {
            // A sample split across two reads: pair last read's dangling low byte with this
            // read's first high byte, so chunking never drops a real sample at a read boundary.
            out.push(i16::from_le_bytes([lo, bytes[0]]) as f32 / i16::MAX as f32);
            bytes = &bytes[1..];
        }
        let mut pairs = bytes.chunks_exact(2);
        for b in pairs.by_ref() {
            out.push(i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32);
        }
        if let Some(&lo) = pairs.remainder().first() {
            carry = Some(lo);
        }
    }
    // A single byte left in `carry` is a torn final write (odd file length) — dropped, matching
    // the old `chunks_exact(2)` behaviour.
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("plaude-{name}"))
    }

    #[test]
    fn pcm_roundtrips_le_i16_to_f32() {
        let path = tmp("roundtrip.session.pcm");
        let mut f = File::create(&path).unwrap();
        for s in [0i16, 1000, -1000, i16::MAX] {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        f.flush().unwrap();

        let decoded = read_pcm_i16(&path).unwrap();
        assert_eq!(decoded.len(), 4);
        assert!(decoded[0].abs() < 1e-6);
        assert!((decoded[1] - 1000.0 / i16::MAX as f32).abs() < 1e-6);
        assert!((decoded[3] - 1.0).abs() < 1e-4);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn torn_trailing_byte_is_dropped_not_panicked() {
        // Simulates a kill -9 mid-sample: an odd number of bytes on disk.
        let path = tmp("torn.session.pcm");
        let mut f = File::create(&path).unwrap();
        f.write_all(&1234i16.to_le_bytes()).unwrap();
        f.write_all(&[0x7f]).unwrap(); // dangling half-sample
        f.flush().unwrap();

        let decoded = read_pcm_i16(&path).unwrap();
        assert_eq!(decoded.len(), 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn source_serializes_as_external_tag_strings() {
        // The hand-mirrored TS binding (`type Source = "Mic" | "SystemAudio"`) and the
        // `start_session(source)` command depend on this exact wire form. Pin it so a
        // future serde attribute can't silently desync the frontend contract.
        assert_eq!(serde_json::to_string(&Source::Mic).unwrap(), "\"Mic\"");
        assert_eq!(
            serde_json::to_string(&Source::SystemAudio).unwrap(),
            "\"SystemAudio\""
        );
    }

    #[test]
    fn session_ids_are_unique_within_process() {
        // Guards the same-millisecond path-collision fix: the monotonic counter
        // must make consecutive ids differ even when the timestamp is identical.
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("session-"));
    }

    /// Fresh scratch dir per test so archive tests can't collide.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plaude-archive-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pcm(path: &Path, samples: &[i16]) {
        let mut f = File::create(path).unwrap();
        for s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        f.flush().unwrap();
    }

    fn ct(pcm: PathBuf, source: Source) -> CapturedTrack {
        CapturedTrack {
            pcm,
            source,
            lead_silence_ms: 0,
        }
    }

    #[test]
    fn source_round_trips_through_pcm_file_name() {
        // suffix() writes the tag, from_pcm_name() parses it back — pin the pair so a rename
        // on one side can't silently make recovery misclassify every orphan.
        for source in [Source::Mic, Source::SystemAudio] {
            let name = format!("session-1-0.{}{PCM_SUFFIX}", source.suffix());
            assert_eq!(
                std::mem::discriminant(&Source::from_pcm_name(&name)),
                std::mem::discriminant(&source)
            );
        }
        // Legacy pre-dual files had no tag → Mic.
        assert!(matches!(
            Source::from_pcm_name("session-1-0.session.pcm"),
            Source::Mic
        ));
    }

    #[test]
    fn stream_mix_pads_lead_sums_and_clamps() {
        // The system track opens hundreds of ms after the mic; its samples must shift right
        // on the shared timeline. Overlap sums; loud overlap clamps to full scale.
        let dir = scratch("skew");
        let mic = dir.join("a.mic.session.pcm");
        let sys = dir.join("a.system.session.pcm");
        write_pcm(&mic, &[10000; 32]);
        write_pcm(&sys, &[30000; 16]);
        let wav = dir.join("a.wav");

        stream_mix_to_wav(
            &[
                ArchivedTrack {
                    pcm: mic,
                    source: Source::Mic,
                    lead_samples: 0,
                },
                ArchivedTrack {
                    pcm: sys,
                    source: Source::SystemAudio,
                    lead_samples: 16, // 1 ms @ 16 kHz
                },
            ],
            &wav,
        )
        .unwrap();

        let mixed = crate::audio_toolkit::read_wav_samples(&wav).unwrap();
        assert_eq!(mixed.len(), 32, "timeline = max(lead + len) across tracks");
        let mic_only = 10000.0 / i16::MAX as f32;
        assert!(
            (mixed[0] - mic_only).abs() < 1e-2,
            "before the lead: mic alone"
        );
        assert!(
            mixed[20] > mic_only + 0.5,
            "after the lead the tracks sum ({} vs {mic_only})",
            mixed[20]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_mix_clamps_loud_overlap() {
        let dir = scratch("clamp");
        let a = dir.join("a.mic.session.pcm");
        let b = dir.join("a.system.session.pcm");
        write_pcm(&a, &[30000; 8]);
        write_pcm(&b, &[30000; 8]);
        let wav = dir.join("a.wav");

        stream_mix_to_wav(
            &[
                ArchivedTrack {
                    pcm: a,
                    source: Source::Mic,
                    lead_samples: 0,
                },
                ArchivedTrack {
                    pcm: b,
                    source: Source::SystemAudio,
                    lead_samples: 0,
                },
            ],
            &wav,
        )
        .unwrap();

        let mixed = crate::audio_toolkit::read_wav_samples(&wav).unwrap();
        assert!(
            mixed.iter().all(|&s| s <= 1.0 + 1e-4),
            "summed overlap must clamp to full scale, not wrap"
        );
        assert!((mixed[0] - 1.0).abs() < 1e-2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_mix_ignores_torn_trailing_byte() {
        // kill -9 mid-sample leaves an odd byte; the mix must drop it like read_pcm_i16 does.
        let dir = scratch("torn-mix");
        let pcm = dir.join("a.mic.session.pcm");
        let mut f = File::create(&pcm).unwrap();
        f.write_all(&1000i16.to_le_bytes()).unwrap();
        f.write_all(&[0x7f]).unwrap();
        f.flush().unwrap();
        drop(f);
        let wav = dir.join("a.wav");

        stream_mix_to_wav(
            &[ArchivedTrack {
                pcm,
                source: Source::Mic,
                lead_samples: 0,
            }],
            &wav,
        )
        .unwrap();
        assert_eq!(
            crate::audio_toolkit::read_wav_samples(&wav).unwrap().len(),
            1
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_streams_wav_and_keeps_pcms_as_transcription_source() {
        let dir = scratch("happy");
        let pcm = dir.join("a.mic.session.pcm");
        write_pcm(&pcm, &[1000, -1000, 500]);
        let wav = dir.join("a.wav");

        let archived = archive_tracks(&[ct(pcm.clone(), Source::Mic)], &wav).unwrap();

        assert_eq!(archived.map(|a| a.len()), Some(1));
        assert!(wav.exists(), "mixed WAV must be persisted");
        assert!(
            pcm.exists(),
            "the PCM stays: it is the per-track transcription source until finalize succeeds"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_keeps_pcms_when_wav_write_fails() {
        // The disk-full / unwritable-archive scenario: the PCM is the ONLY copy of the
        // user's audio and must survive so `recover_interrupted` can retry at startup.
        let dir = scratch("wavfail");
        let pcm = dir.join("a.mic.session.pcm");
        write_pcm(&pcm, &[1000, -1000]);
        let wav = dir.join("no-such-subdir").join("a.wav"); // WavWriter::create fails

        let result = archive_tracks(&[ct(pcm.clone(), Source::Mic)], &wav);

        assert!(
            result.is_err(),
            "a failed WAV write must surface as an error"
        );
        assert!(
            pcm.exists(),
            "the PCM must NOT be deleted when the WAV was never written"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_discards_all_empty_tracks_and_cleans_up() {
        let dir = scratch("empty");
        let pcm = dir.join("a.mic.session.pcm");
        write_pcm(&pcm, &[]);
        let wav = dir.join("a.wav");

        let decoded = archive_tracks(&[ct(pcm.clone(), Source::Mic)], &wav).unwrap();

        assert!(decoded.is_none(), "an all-silence session is discarded");
        assert!(!wav.exists(), "no WAV for a discarded session");
        assert!(!pcm.exists(), "empty PCMs are cleaned up");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_skips_unreadable_track_but_saves_the_readable_one() {
        // One unreadable track must not hold the other track's audio hostage: the good track
        // is archived now, the missing one is left to recovery.
        let dir = scratch("skip");
        let good = dir.join("a.mic.session.pcm");
        write_pcm(&good, &[2000, -2000]);
        let bad = dir.join("gone.system.session.pcm"); // never created → metadata fails
        let wav = dir.join("a.wav");

        let archived = archive_tracks(
            &[
                ct(good.clone(), Source::Mic),
                ct(bad.clone(), Source::SystemAudio),
            ],
            &wav,
        )
        .unwrap();

        assert_eq!(
            archived.map(|a| a.len()),
            Some(1),
            "only the readable track is archived"
        );
        assert!(wav.exists());
        assert!(good.exists(), "archived PCM stays until finalize succeeds");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_fails_when_no_track_is_readable() {
        let dir = scratch("allbad");
        let missing = dir.join("gone.mic.session.pcm"); // never created
        let wav = dir.join("a.wav");

        let result = archive_tracks(&[ct(missing, Source::Mic)], &wav);

        assert!(result.is_err());
        assert!(!wav.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_failure_invokes_on_fail_once() {
        use std::sync::atomic::AtomicBool;
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let bad_path = std::env::temp_dir()
            .join("plaude-no-such-dir")
            .join("w.session.pcm"); // File::create fails
        let (tx, rx) = mpsc::channel::<Vec<f32>>();

        let handle = spawn_pcm_writer(bad_path, rx, None, move || {
            flag.store(true, Ordering::SeqCst);
        });
        drop(tx);

        assert!(
            handle.join().unwrap().is_err(),
            "writer must report the failure"
        );
        assert!(
            failed.load(Ordering::SeqCst),
            "on_fail must fire so the session can stop"
        );
    }

    #[test]
    fn writer_success_counts_samples_and_never_invokes_on_fail() {
        use std::sync::atomic::AtomicBool;
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let dir = scratch("writer-ok");
        let path = dir.join("w.session.pcm");
        let (tx, rx) = mpsc::channel::<Vec<f32>>();

        let handle = spawn_pcm_writer(path.clone(), rx, None, move || {
            flag.store(true, Ordering::SeqCst);
        });
        tx.send(vec![0.5, -0.5, 0.25]).unwrap();
        tx.send(vec![1.0]).unwrap();
        drop(tx); // sink closes → writer finishes

        assert_eq!(handle.join().unwrap().unwrap(), 4);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            8,
            "4 samples × 2 bytes LE i16"
        );
        assert!(!failed.load(Ordering::SeqCst));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_marks_activity_on_loud_frames_only() {
        let dir = scratch("writer-rms");

        // Silence: below the RMS floor → never marked as heard.
        let quiet = Arc::new(AudioActivity::new());
        quiet.begin();
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let h = spawn_pcm_writer(
            dir.join("q.session.pcm"),
            rx,
            Some(ActivityTap::loudness(quiet.clone())),
            || {},
        );
        tx.send(vec![0.001; 480]).unwrap();
        drop(tx);
        h.join().unwrap().unwrap();
        assert!(
            !quiet.heard_audio(),
            "noise-floor frames must not count as audio"
        );

        // A loud frame flips the probation gate.
        let loud = Arc::new(AudioActivity::new());
        loud.begin();
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let h = spawn_pcm_writer(
            dir.join("l.session.pcm"),
            rx,
            Some(ActivityTap::loudness(loud.clone())),
            || {},
        );
        tx.send(vec![0.5; 480]).unwrap();
        drop(tx);
        h.join().unwrap().unwrap();
        assert!(
            loud.heard_audio(),
            "a loud frame must mark system audio as heard"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The invariant that keeps probation honest with the mic in the loop: a mic frame marks
    /// the clock ONLY when the VAD says "speech". LOUD room noise (fan, keyboard — well above
    /// the RMS floor) with a silent VAD must never count as heard, or a false auto-start would
    /// be finalized as a junk row instead of discarded.
    #[test]
    fn mic_tap_marks_on_vad_speech_never_on_loud_noise() {
        struct FixedVad(bool);
        impl crate::audio_toolkit::vad::VoiceActivityDetector for FixedVad {
            fn push_frame<'a>(
                &'a mut self,
                frame: &'a [f32],
            ) -> anyhow::Result<crate::audio_toolkit::vad::VadFrame<'a>> {
                Ok(if self.0 {
                    crate::audio_toolkit::vad::VadFrame::Speech(frame)
                } else {
                    crate::audio_toolkit::vad::VadFrame::Noise
                })
            }
        }

        // Loud noise, VAD says "not speech" → not heard.
        let clock = Arc::new(AudioActivity::new());
        clock.begin();
        let mut tap = ActivityTap::speech(clock.clone(), Box::new(FixedVad(false)));
        tap.observe(&vec![0.5; 480]);
        assert!(
            !clock.heard_audio(),
            "loud non-speech on the mic must NOT mark the clock"
        );

        // Speech → heard.
        let mut tap = ActivityTap::speech(clock.clone(), Box::new(FixedVad(true)));
        tap.observe(&vec![0.05; 480]);
        assert!(clock.heard_audio(), "VAD speech must mark the clock");
    }

    #[test]
    fn audio_activity_gates_probation_and_measures_idle_from_session_start() {
        let act = AudioActivity::new();
        act.begin();
        assert!(!act.heard_audio(), "a fresh session has heard nothing");
        // Before the first loud frame, idle is measured from session start (so the
        // supervisor's silent-start failsafe has a clock even in total silence).
        std::thread::sleep(Duration::from_millis(15));
        assert!(act.idle() >= Duration::from_millis(10));

        act.mark();
        assert!(act.heard_audio(), "a loud frame flips the probation gate");
        assert!(
            act.idle() < Duration::from_millis(10),
            "idle restarts at the last loud frame"
        );

        // begin() must fully reset — stop/cancel call it so the NEXT idle period can never
        // read the previous session's `heard` as its own.
        act.begin();
        assert!(!act.heard_audio());
    }

    // --- persist_session behind its seams (the previously untestable finalize half) --------

    use std::collections::VecDeque;

    /// Stub for Queries: scripted ASR results, no model. Records the sample counts it was
    /// handed so lead-padding is observable.
    struct StubTranscriber {
        loaded: bool,
        script: Mutex<VecDeque<Result<(String, Vec<AsrSegment>)>>>,
        seen_sample_counts: Mutex<Vec<usize>>,
    }

    impl StubTranscriber {
        fn with_script(script: Vec<Result<(String, Vec<AsrSegment>)>>) -> Self {
            Self {
                loaded: true,
                script: Mutex::new(script.into()),
                seen_sample_counts: Mutex::new(Vec::new()),
            }
        }
        fn unloaded() -> Self {
            Self {
                loaded: false,
                script: Mutex::new(VecDeque::new()),
                seen_sample_counts: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transcriber for StubTranscriber {
        fn ensure_model_ready(&self) -> bool {
            self.loaded
        }
        fn transcribe_with_segments(&self, samples: Vec<f32>) -> Result<(String, Vec<AsrSegment>)> {
            self.seen_sample_counts.lock().unwrap().push(samples.len());
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("stub script exhausted")))
        }
    }

    /// Records every history call so tests can assert the row lifecycle.
    #[derive(Default)]
    struct RecordingSink {
        pending: Mutex<Vec<(String, EntrySource)>>,
        segments: Mutex<Vec<Vec<TimedSegment>>>,
        updates: Mutex<Vec<(String, TranscriptionStatus)>>,
        /// When true, `entry_exists` reports the row as gone — simulates the user deleting the
        /// recording mid-finalize.
        row_deleted: bool,
    }

    impl SessionSink for RecordingSink {
        fn save_pending_entry(
            &self,
            file_name: String,
            source: EntrySource,
        ) -> Result<HistoryEntry> {
            self.pending
                .lock()
                .unwrap()
                .push((file_name.clone(), source));
            Ok(HistoryEntry {
                id: 1,
                file_name,
                timestamp: 0,
                saved: false,
                title: String::new(),
                transcription_text: String::new(),
                post_processed_text: None,
                post_process_prompt: None,
                post_process_requested: false,
                status: TranscriptionStatus::Transcribing,
                source,
            })
        }
        fn save_segments(&self, _history_id: i64, segments: &[TimedSegment]) -> Result<()> {
            self.segments.lock().unwrap().push(segments.to_vec());
            Ok(())
        }
        fn update_transcription(
            &self,
            _id: i64,
            transcription_text: String,
            status: TranscriptionStatus,
        ) -> Result<()> {
            self.updates
                .lock()
                .unwrap()
                .push((transcription_text, status));
            Ok(())
        }
        fn entry_exists(&self, _id: i64) -> Result<bool> {
            Ok(!self.row_deleted)
        }
    }

    /// A diarizer with no models on disk: `diarize` returns no turns (safe-by-default).
    fn no_model_diarizer() -> DiarizationManager {
        DiarizationManager::new(&std::env::temp_dir().join("plaude-no-models-here"))
    }

    fn asr_seg(start_ms: i64, end_ms: i64, text: &str) -> AsrSegment {
        AsrSegment {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    fn archived(
        dir: &Path,
        name: &str,
        source: Source,
        samples: &[i16],
        lead: usize,
    ) -> ArchivedTrack {
        let pcm = dir.join(name);
        write_pcm(&pcm, samples);
        ArchivedTrack {
            pcm,
            source,
            lead_samples: lead,
        }
    }

    #[test]
    fn persist_without_model_marks_failed_for_retry() {
        // Recovery runs before any model loads: the audio is saved, the transcript is not —
        // the row must land on Failed (retry affordance), never Done ("recorded silence").
        let dir = scratch("no-model");
        let tracks = vec![archived(
            &dir,
            "a.mic.session.pcm",
            Source::Mic,
            &[1000; 8],
            0,
        )];
        let tm = StubTranscriber::unloaded();
        let sink = RecordingSink::default();

        persist_session(
            &tm,
            &sink,
            &no_model_diarizer(),
            &tracks,
            &dir.join("a.wav"),
        )
        .unwrap();

        assert_eq!(
            *sink.updates.lock().unwrap(),
            vec![(String::new(), TranscriptionStatus::Failed)]
        );
        assert!(
            tm.seen_sample_counts.lock().unwrap().is_empty(),
            "no transcription may be attempted without a model"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_deleted_mid_finalize_is_a_benign_discard_not_an_error() {
        // The user deletes the recording from History while its (minutes-long) transcription
        // runs. Finalize must treat the vanished row as a benign discard: return Ok (so the PCMs
        // get cleaned and recovery can't resurrect it) and never write to the gone row.
        let dir = scratch("row-deleted");
        let tracks = vec![archived(
            &dir,
            "a.mic.session.pcm",
            Source::Mic,
            &[1000; 8],
            0,
        )];
        let tm =
            StubTranscriber::with_script(vec![Ok(("hello".into(), vec![asr_seg(0, 500, "hello")]))]);
        let sink = RecordingSink {
            row_deleted: true,
            ..Default::default()
        };

        let r = persist_session(&tm, &sink, &no_model_diarizer(), &tracks, &dir.join("a.wav"));

        assert!(r.is_ok(), "a row deleted mid-finalize must not surface as a failure");
        assert!(
            sink.segments.lock().unwrap().is_empty(),
            "must not write segments to a vanished row (FK constraint would fail)"
        );
        assert!(
            sink.updates.lock().unwrap().is_empty(),
            "must not write status to a vanished row (not-found hard error)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_marks_failed_only_when_everything_failed() {
        let dir = scratch("all-fail");
        let tracks = vec![
            archived(&dir, "a.mic.session.pcm", Source::Mic, &[1000; 8], 0),
            archived(
                &dir,
                "a.system.session.pcm",
                Source::SystemAudio,
                &[1000; 8],
                0,
            ),
        ];
        let tm = StubTranscriber::with_script(vec![
            Err(anyhow!("asr crashed")),
            Err(anyhow!("asr crashed again")),
        ]);
        let sink = RecordingSink::default();

        persist_session(
            &tm,
            &sink,
            &no_model_diarizer(),
            &tracks,
            &dir.join("a.wav"),
        )
        .unwrap();

        let updates = sink.updates.lock().unwrap();
        assert_eq!(updates[0].1, TranscriptionStatus::Failed);
        assert!(updates[0].0.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_marks_failed_but_keeps_partial_transcript_when_a_track_fails() {
        // C1 dual-track honesty: track 1 transcribes, track 2 errors. The row must land on
        // Failed — a meeting that silently lost half its speakers must NOT read as Done — while
        // the successfully transcribed track's text AND segments are preserved (visible in
        // History) and the WAV holds both sides so the retry can redo the whole thing.
        let dir = scratch("partial");
        let tracks = vec![
            archived(&dir, "a.mic.session.pcm", Source::Mic, &[1000; 8], 0),
            archived(
                &dir,
                "a.system.session.pcm",
                Source::SystemAudio,
                &[1000; 8],
                0,
            ),
        ];
        let tm = StubTranscriber::with_script(vec![
            Ok((
                "hello from mic".into(),
                vec![asr_seg(0, 500, "hello from mic")],
            )),
            Err(anyhow!("asr crashed")),
        ]);
        let sink = RecordingSink::default();

        persist_session(
            &tm,
            &sink,
            &no_model_diarizer(),
            &tracks,
            &dir.join("a.wav"),
        )
        .unwrap();

        let updates = sink.updates.lock().unwrap();
        assert_eq!(
            updates[0].1,
            TranscriptionStatus::Failed,
            "a lost track must fail the row, not hide behind a partial Done"
        );
        assert!(
            updates[0].0.contains("hello from mic"),
            "the good track's text is still preserved"
        );
        assert!(
            !sink.segments.lock().unwrap().is_empty(),
            "the good track's segments are still persisted (visible in History)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_labels_mic_as_me_in_a_dual_session_and_records_meeting_source() {
        let dir = scratch("dual-label");
        let tracks = vec![
            archived(&dir, "a.mic.session.pcm", Source::Mic, &[1000; 8], 0),
            archived(
                &dir,
                "a.system.session.pcm",
                Source::SystemAudio,
                &[1000; 8],
                0,
            ),
        ];
        let tm = StubTranscriber::with_script(vec![
            Ok(("me talking".into(), vec![asr_seg(0, 500, "me talking")])),
            Ok((
                "them talking".into(),
                vec![asr_seg(600, 1000, "them talking")],
            )),
        ]);
        let sink = RecordingSink::default();

        persist_session(
            &tm,
            &sink,
            &no_model_diarizer(),
            &tracks,
            &dir.join("a.wav"),
        )
        .unwrap();

        assert_eq!(sink.pending.lock().unwrap()[0].1, EntrySource::Meeting);
        let segs = sink.segments.lock().unwrap();
        let labels: Vec<Option<String>> = segs[0].iter().map(|s| s.speaker_label.clone()).collect();
        assert!(
            labels.contains(&Some(MIC_LABEL.to_string())),
            "the mic track of a dual session is labelled Me"
        );
        assert!(
            labels.contains(&None),
            "the system track is diarized (no-model diarizer → unknown speaker)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcribe_tracks_shifts_segments_by_the_lead_not_the_samples() {
        // M3: the lead silence is metadata, not data. The engine sees the RAW samples (no
        // multi-hour buffer of prepended zeros), and the returned segments are shifted onto the
        // shared timeline by the lead's duration — the identical timestamps the old physical pad
        // produced. lead_samples 1600 @ 16 kHz = 100 ms.
        let dir = scratch("lead-shift");
        let tracks = vec![archived(
            &dir,
            "a.system.session.pcm",
            Source::SystemAudio,
            &[1000; 8],
            1600,
        )];
        let tm = StubTranscriber::with_script(vec![Ok((
            "them".into(),
            vec![asr_seg(200, 500, "them")],
        ))]);

        let (track_segments, _, _) = transcribe_tracks(&tm, &no_model_diarizer(), &tracks);

        assert_eq!(
            *tm.seen_sample_counts.lock().unwrap(),
            vec![8],
            "the engine sees only the raw samples — the lead is no longer materialised as zeros"
        );
        assert_eq!(
            (track_segments[0][0].start_ms, track_segments[0][0].end_ms),
            (300, 600),
            "the aligned segment is shifted forward by the 100 ms lead"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lead_ms_matches_the_source_lead_silence_ms() {
        use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
        // The shift must equal the lead_silence_ms the lead_samples was derived from, so the
        // arithmetic offset reproduces the old physical pad exactly.
        for ms in [0u64, 1, 100, 250, 3_600_000] {
            let samples = (ms * WHISPER_SAMPLE_RATE as u64 / 1000) as usize;
            assert_eq!(lead_ms(samples), ms as i64);
        }
    }

    #[test]
    fn shift_segments_moves_both_bounds_and_no_ops_at_zero() {
        let base = vec![
            TimedSegment {
                start_ms: 0,
                end_ms: 500,
                speaker_id: None,
                speaker_label: Some(MIC_LABEL.to_string()),
                text: "a".into(),
            },
            TimedSegment {
                start_ms: 600,
                end_ms: 1000,
                speaker_id: Some(1),
                speaker_label: None,
                text: "b".into(),
            },
        ];

        let mut shifted = base.clone();
        shift_segments(&mut shifted, 250);
        assert_eq!((shifted[0].start_ms, shifted[0].end_ms), (250, 750));
        assert_eq!((shifted[1].start_ms, shifted[1].end_ms), (850, 1250));

        // The session's first track (lead 0) is untouched.
        let mut zeroed = base.clone();
        shift_segments(&mut zeroed, 0);
        assert_eq!(zeroed, base);
    }

    #[test]
    fn read_pcm_i16_streams_multiple_read_chunks_identically() {
        // Golden test for the chunked decoder: a file larger than one read window (64k samples)
        // must decode byte-identically to a direct convert — the carry logic across read
        // boundaries preserves every sample. Odd length also exercises the torn-tail drop.
        let path = tmp("stream-multichunk.session.pcm");
        let count = 64 * 1024 + 37; // spans two read windows
        let mut f = File::create(&path).unwrap();
        for i in 0..count {
            let s = (i as i32 % 30000 - 15000) as i16;
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        f.write_all(&[0x7f]).unwrap(); // torn final half-sample
        f.flush().unwrap();

        let decoded = read_pcm_i16(&path).unwrap();
        assert_eq!(decoded.len(), count, "the dangling odd byte is dropped");
        for i in 0..count {
            let expected = (i as i32 % 30000 - 15000) as i16 as f32 / i16::MAX as f32;
            assert!((decoded[i] - expected).abs() < 1e-9, "sample {i} mismatch");
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn flat_transcript_prefers_the_deduped_timeline() {
        let texts = vec!["raw a".to_string(), "raw b".to_string()];
        assert_eq!(flat_transcript(&[], &texts), "raw a\nraw b");

        let merged = vec![
            TimedSegment {
                start_ms: 0,
                end_ms: 1,
                speaker_id: None,
                speaker_label: None,
                text: "one".into(),
            },
            TimedSegment {
                start_ms: 1,
                end_ms: 2,
                speaker_id: None,
                speaker_label: None,
                text: "two".into(),
            },
        ];
        assert_eq!(flat_transcript(&merged, &texts), "one two");
    }

    #[test]
    fn resolve_status_fails_on_any_track_error_else_done() {
        // C1: ANY per-track error fails the row (even with a partial transcript produced) —
        // a clean run, including silence that produced no text, is Done.
        assert_eq!(resolve_status(true), TranscriptionStatus::Failed);
        assert_eq!(resolve_status(false), TranscriptionStatus::Done);
    }

    // --- recovery planning ------------------------------------------------------------------

    #[test]
    fn recovery_groups_a_dual_crash_into_one_session_mic_first() {
        let plans = plan_recovery(
            vec![
                PathBuf::from("/rec/session-9-0.system.session.pcm"),
                PathBuf::from("/rec/session-9-0.mic.session.pcm"),
            ],
            |_| false,
            |_| false,
        );
        assert_eq!(plans.len(), 1, "one crash = one session, even dual-stream");
        match &plans[0] {
            RecoveryPlan::Refinalize { tracks, wav_path } => {
                assert_eq!(wav_path, &PathBuf::from("/rec/session-9-0.wav"));
                assert_eq!(tracks.len(), 2);
                assert!(
                    matches!(tracks[0].source, Source::Mic),
                    "mic first for Me labelling"
                );
                assert!(matches!(tracks[1].source, Source::SystemAudio));
            }
            other => panic!("expected Refinalize, got {other:?}"),
        }
    }

    #[test]
    fn recovery_only_cleans_up_pcms_whose_wav_already_exists() {
        // Crash after the WAV was written AND the row was persisted, but before the PCMs were
        // removed: re-finalizing would insert a duplicate history row — the audio is already
        // archived and visible.
        let plans = plan_recovery(
            vec![PathBuf::from("/rec/session-9-0.mic.session.pcm")],
            |_| true,
            |_| true,
        );
        assert!(matches!(&plans[0], RecoveryPlan::CleanupArchived(p)
            if p == &PathBuf::from("/rec/session-9-0.mic.session.pcm")));
    }

    #[test]
    fn recovery_adopts_an_archived_session_that_has_no_history_row() {
        // The narrow finalize window: the WAV was written but the crash beat `save_pending_entry`,
        // so the audio exists with no row. Cleaning up the PCM (the old behavior) would lose the
        // recording forever — instead adopt it as ONE retryable Meeting row (dual PCMs → Meeting).
        let plans = plan_recovery(
            vec![
                PathBuf::from("/rec/session-9-0.system.session.pcm"),
                PathBuf::from("/rec/session-9-0.mic.session.pcm"),
            ],
            |_| true,  // WAV archived
            |_| false, // but no row points at it
        );
        assert_eq!(plans.len(), 1, "one archive = one adopted session");
        match &plans[0] {
            RecoveryPlan::AdoptOrphanArchive {
                pcms,
                wav_path,
                source,
            } => {
                assert_eq!(wav_path, &PathBuf::from("/rec/session-9-0.wav"));
                assert_eq!(pcms.len(), 2, "both PCMs removed once adopted");
                assert!(matches!(source, EntrySource::Meeting));
            }
            other => panic!("expected AdoptOrphanArchive, got {other:?}"),
        }
    }

    #[test]
    fn recovery_adopts_a_solo_system_orphan_with_its_source() {
        let plans = plan_recovery(
            vec![PathBuf::from("/rec/session-3-0.system.session.pcm")],
            |_| true,
            |_| false,
        );
        match &plans[0] {
            RecoveryPlan::AdoptOrphanArchive { source, .. } => {
                assert!(matches!(source, EntrySource::System));
            }
            other => panic!("expected AdoptOrphanArchive, got {other:?}"),
        }
    }

    #[test]
    fn recovery_keeps_unrelated_sessions_separate_and_reads_legacy_names_as_mic() {
        let plans = plan_recovery(
            vec![
                PathBuf::from("/rec/session-1-0.session.pcm"), // legacy single-track name
                PathBuf::from("/rec/session-2-0.system.session.pcm"),
            ],
            |_| false,
            |_| false,
        );
        assert_eq!(plans.len(), 2, "different session ids never merge");
        match &plans[0] {
            RecoveryPlan::Refinalize { tracks, .. } => {
                assert!(matches!(tracks[0].source, Source::Mic));
            }
            other => panic!("expected Refinalize, got {other:?}"),
        }
    }
}
