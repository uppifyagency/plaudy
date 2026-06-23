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

use crate::audio_toolkit::{save_wav_file, AudioRecorder};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::audio_toolkit::audio::SystemAudioRecorder;
use crate::managers::diarization::{
    align, drop_bleed, label_segments, merge_segments, DiarizationManager, TimedSegment,
};
use crate::managers::history::{HistoryManager, TranscriptionStatus};
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

/// Which audio source a session captures.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, specta::Type)]
pub enum Source {
    /// The microphone (Fase 0).
    Mic,
    /// macOS system / loopback audio — the other side of a call/meeting (Fase 1).
    SystemAudio,
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
/// capture*, so it can't tell when the call goes quiet. Instead the system track's PCM writer marks
/// the wall-clock of the last loud frame here; the supervisor finalizes once it has been idle long
/// enough. Cheap (one RMS per 30 ms frame) and immune to our own tap.
struct AudioActivity {
    /// `None` until the first loud system-audio frame of the current session, then the wall-clock
    /// of the most recent loud frame. Lets auto-capture tell a real session (heard audio) from a
    /// false start (the OS "device running" sensor lies once our own tap is open).
    last_loud: Mutex<Option<Instant>>,
    started: Mutex<Instant>,
}

impl AudioActivity {
    fn new() -> Self {
        Self {
            last_loud: Mutex::new(None),
            started: Mutex::new(Instant::now()),
        }
    }
    /// Reset at session start: no loud frame heard yet, stamp the start time.
    fn begin(&self) {
        *self.last_loud.lock().unwrap() = None;
        *self.started.lock().unwrap() = Instant::now();
    }
    /// Mark "system audio heard now" (called by the system track's writer on a loud frame).
    fn mark(&self) {
        *self.last_loud.lock().unwrap() = Some(Instant::now());
    }
    /// True once any loud system-audio frame has been captured this session (probation gate).
    fn heard_audio(&self) -> bool {
        self.last_loud.lock().unwrap().is_some()
    }
    /// How long since the last loud frame; if none yet, since the session started.
    fn idle(&self) -> Duration {
        match *self.last_loud.lock().unwrap() {
            Some(t) => t.elapsed(),
            None => self.started.lock().unwrap().elapsed(),
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
}

/// An active session is one or more capture tracks that finalize into a single history entry:
/// their audio is mixed into one playable WAV and their transcripts merged into one
/// speaker-attributed timeline. A solo track behaves exactly as before; two tracks (mic +
/// system audio) are the dual-stream "meeting" capture.
struct ActiveSession {
    tracks: Vec<Track>,
    wav_path: PathBuf,
}

pub struct SessionManager {
    app: AppHandle,
    recordings_dir: PathBuf,
    active: Mutex<Option<ActiveSession>>,
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
            activity: Arc::new(AudioActivity::new()),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// How long the captured system audio has been silent — the auto-capture supervisor's
    /// tap-immune STOP signal (the OS device sensor is useless once our own tap is open).
    pub fn system_audio_idle(&self) -> Duration {
        self.activity.idle()
    }

    /// Whether any real (loud) system audio has been captured in the current session — the
    /// auto-capture supervisor's probation gate to discard false starts.
    pub fn system_audio_heard(&self) -> bool {
        self.activity.heard_audio()
    }

    /// Start if idle, stop if active. Returns whether a session is active afterwards.
    pub fn toggle(&self, source: Source) -> Result<bool> {
        self.toggle_sources(&[source])
    }

    /// Toggle a multi-source session — the menu-bar "graffetta" uses `[Mic, SystemAudio]` so one
    /// click captures both sides of a call. Returns whether a session is active afterwards.
    pub fn toggle_sources(&self, sources: &[Source]) -> Result<bool> {
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
        let mut guard = self.active.lock().unwrap();
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
            let activity = matches!(source, Source::SystemAudio).then(|| self.activity.clone());
            match build_track(source, &id, &self.recordings_dir, activity) {
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
        *guard = Some(ActiveSession { tracks, wav_path });
        // Release the lock BEFORE emitting. The SessionStateChanged listener (lib.rs) runs
        // INLINE on this thread and calls `is_active()`, which re-locks `active`. A
        // std::sync::Mutex is non-reentrant, so emitting while still holding `guard` deadlocks
        // the start path. (stop() is already safe: it `.take()`s the guard as a temporary, so
        // it is dropped before stop's own emit.)
        drop(guard);
        let _ = SessionStateChanged {
            active: true,
            source: primary,
        }
        .emit(&self.app);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let session = self
            .active
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("No active session"))?;

        let ActiveSession { tracks, wav_path } = session;

        // Flip the live indicator to idle as soon as the user stops — before the (possibly slow)
        // frame drain — so the UI feels responsive. Finalization and the row that follows happen
        // off-thread.
        let _ = SessionStateChanged {
            active: false,
            source: None,
        }
        .emit(&self.app);

        // Tear down every track and collect its (pcm_path, source) for finalize. `stop()` drains
        // each track's buffered frames; dropping the recorder closes its writer channel.
        let mut captured: Vec<(PathBuf, Source)> = Vec::with_capacity(tracks.len());
        for Track {
            mut recorder,
            writer,
            pcm_path,
            source,
        } in tracks
        {
            let _ = recorder.stop();
            let _ = recorder.close();
            drop(recorder);
            match writer.join() {
                Ok(Ok(samples)) => info!("Session track {source:?} captured {samples} samples"),
                Ok(Err(e)) => error!("Session track {source:?} writer error: {e}"),
                Err(_) => error!("Session track {source:?} writer thread panicked"),
            }
            captured.push((pcm_path, source));
        }

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
        let session = self
            .active
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("No active session"))?;
        let ActiveSession { tracks, wav_path: _ } = session;

        let _ = SessionStateChanged {
            active: false,
            source: None,
        }
        .emit(&self.app);

        for Track {
            mut recorder,
            writer,
            pcm_path,
            source,
        } in tracks
        {
            let _ = recorder.stop();
            let _ = recorder.close();
            drop(recorder);
            let _ = writer.join();
            let _ = fs::remove_file(&pcm_path);
            info!("Auto-capture: discarded false-start {source:?} track ({})", pcm_path.display());
        }
        Ok(())
    }

    /// Finalize any session whose process died mid-recording. Safe to call once at startup,
    /// after the history and transcription managers are in managed state. Each orphan PCM is
    /// recovered as its own single-track session (no cross-track merge across a crash).
    pub fn recover_interrupted(&self) {
        let Ok(entries) = fs::read_dir(&self.recordings_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let pcm_path = entry.path();
            let name = pcm_path.to_string_lossy();
            if !name.ends_with(PCM_SUFFIX) {
                continue;
            }
            let source = if name.contains(".system.") {
                Source::SystemAudio
            } else {
                Source::Mic
            };
            let wav_path = wav_path_for(&pcm_path);
            warn!("Recovering interrupted session → {}", pcm_path.display());
            let app = self.app.clone();
            std::thread::spawn(move || {
                if let Err(e) = finalize_session(&app, &[(pcm_path.clone(), source)], &wav_path) {
                    error!("Recovery failed for {}: {e}", pcm_path.display());
                }
            });
        }
    }
}

/// `session-<id>.session.pcm` → `session-<id>.wav`.
fn wav_path_for(pcm_path: &Path) -> PathBuf {
    pcm_path.with_extension("").with_extension("wav")
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

/// Short on-disk tag for a track's source, e.g. `session-<id>.mic.session.pcm`. Lets
/// `recover_interrupted` tell which source an orphan PCM came from.
fn source_suffix(source: Source) -> &'static str {
    match source {
        Source::Mic => "mic",
        Source::SystemAudio => "system",
    }
}

/// Build + start one capture track: open the recorder for `source` and spawn its PCM writer.
/// The recorder is started FIRST so a failure (e.g. denied permission) leaves no orphan file.
fn build_track(
    source: Source,
    id: &str,
    dir: &Path,
    activity: Option<Arc<AudioActivity>>,
) -> Result<Track> {
    let pcm_path = dir.join(format!("{id}.{}{PCM_SUFFIX}", source_suffix(source)));
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let recorder = build_recorder(source, tx)?;
    let writer = spawn_pcm_writer(pcm_path.clone(), rx, activity);
    Ok(Track {
        recorder,
        writer,
        pcm_path,
        source,
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
            r.open()
                .map_err(|e| anyhow!("open system audio (grant Audio Recording permission?): {e}"))?;
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
fn spawn_pcm_writer(
    pcm_path: PathBuf,
    rx: mpsc::Receiver<Vec<f32>>,
    activity: Option<Arc<AudioActivity>>,
) -> JoinHandle<Result<u64>> {
    /// RMS above this (on [-1,1] samples) counts as "audio playing" vs silence / noise floor.
    const LOUD_RMS: f32 = 0.005;
    std::thread::spawn(move || -> Result<u64> {
        let mut out = BufWriter::new(File::create(&pcm_path)?);
        let mut written: u64 = 0;
        while let Ok(frame) = rx.recv() {
            if let Some(act) = &activity {
                if !frame.is_empty() {
                    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
                    if (sum_sq / frame.len() as f32).sqrt() > LOUD_RMS {
                        act.mark();
                    }
                }
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
    })
}

/// Sum equal-rate mono tracks into one buffer (pad shorter tracks with silence, clamp to
/// [-1, 1]). Folds a dual-stream session's mic + system audio into a single playable 16 kHz
/// WAV while the transcript keeps the speakers separate.
///
/// ponytail: plain sum + clamp — loud and simple. Two people rarely talk over each other, so
/// clipping is rare; a soft limiter is the upgrade path if overlap distortion is ever heard.
fn mix_tracks(tracks: &[&[f32]]) -> Vec<f32> {
    let len = tracks.iter().map(|t| t.len()).max().unwrap_or(0);
    let mut out = vec![0.0f32; len];
    for track in tracks {
        for (o, &s) in out.iter_mut().zip(track.iter()) {
            *o += s;
        }
    }
    for s in out.iter_mut() {
        *s = s.clamp(-1.0, 1.0);
    }
    out
}

/// Finalize a session from its captured tracks: mix the audio into one playable WAV, then
/// (best-effort, only if a model is resident) transcribe each track, attribute speakers, and
/// merge into one chronological timeline persisted as a single history entry.
///
/// One track → the existing behavior: diarize + align the single stream. Multiple tracks →
/// the mic track is labelled "Me" and the system track is diarized, then the two are merged —
/// the dual-stream "who said what across both sides of the call" timeline. Either way a row is
/// created immediately in `Transcribing` state so the session shows in History the moment the
/// user stops, then flips to `Done`/`Failed`.
fn finalize_session(app: &AppHandle, tracks: &[(PathBuf, Source)], wav_path: &Path) -> Result<()> {
    // Always remove the source PCMs afterward — on success, discard, OR error — so a
    // deterministic failure can never make `recover_interrupted` re-finalize the same files on
    // every startup (which would also re-insert a `transcribing` row each time). Once
    // `save_wav_file` succeeds the audio lives in the mixed WAV, so the only case this discards a
    // PCM "early" is a filesystem error where the PCM is already unusable.
    let outcome = finalize_session_inner(app, tracks, wav_path);
    for (pcm, _) in tracks {
        let _ = fs::remove_file(pcm);
    }
    outcome
}

fn finalize_session_inner(
    app: &AppHandle,
    tracks: &[(PathBuf, Source)],
    wav_path: &Path,
) -> Result<()> {
    let mut decoded: Vec<(Vec<f32>, Source)> = Vec::with_capacity(tracks.len());
    for (pcm, source) in tracks {
        decoded.push((read_pcm_i16(pcm)?, *source));
    }
    if decoded.iter().all(|(s, _)| s.is_empty()) {
        warn!("Session has no audio in any track; discarding");
        return Ok(());
    }

    // Mix into the single playable archive (mono 16 kHz) by reference — never clone the
    // (possibly multi-hour) track buffers, which transcription still consumes below.
    let buffers: Vec<&[f32]> = decoded.iter().map(|(s, _)| s.as_slice()).collect();
    save_wav_file(wav_path, &mix_tracks(&buffers))?;
    drop(buffers);

    let tm = app.state::<Arc<TranscriptionManager>>();
    let hm = app.state::<Arc<HistoryManager>>();
    let file_name = wav_path
        .file_name()
        .ok_or_else(|| anyhow!("session WAV path has no file name"))?
        .to_string_lossy()
        .to_string();

    let entry = hm.save_pending_entry(file_name)?;

    // Transcribe only when a model is resident: recovery runs at startup before any model
    // loads, and `transcribe` would otherwise block on the load condvar. A missing transcript
    // never costs the recording — the audio is already saved.
    if !tm.is_model_loaded() {
        warn!("No transcription model loaded; saved session audio without a transcript");
        hm.update_transcription(entry.id, String::new(), None, None, TranscriptionStatus::Done)?;
        return Ok(());
    }

    let dual = decoded.len() > 1;
    let diarizer = DiarizationManager::new(&crate::portable::app_data_dir(app)?.join("models"));
    let mut full_texts: Vec<String> = Vec::new();
    let mut track_segments: Vec<Vec<TimedSegment>> = Vec::new();
    let mut any_error = false;

    for (samples, source) in decoded {
        // The mic track of a dual session is a single known voice ("Me"); everything else is
        // diarized. Diarize before transcription consumes `samples`.
        let label_as_me = dual && matches!(source, Source::Mic);
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
                track_segments.push(if label_as_me {
                    label_segments(&asr, "Me")
                } else {
                    align(&asr, &turns)
                });
            }
            Err(e) => {
                warn!("Session track {source:?} transcription failed: {e}");
                any_error = true;
            }
        }
    }

    // Merge the tracks chronologically, then drop microphone "Me" segments that are just the
    // system audio echoing back through the speakers (acoustic bleed when not on headphones) —
    // one person must never appear as two speakers. Persist before flipping status so the UI
    // finds the segments when it re-fetches on completion.
    let merged = drop_bleed(merge_segments(track_segments), "Me");
    if !merged.is_empty() {
        if let Err(e) = hm.save_segments(entry.id, &merged) {
            warn!("Failed to persist merged segments: {e}");
        }
    }
    // Build the flat transcript from the de-duped timeline when segments exist (so the bleed copy
    // is gone from the flat text too); fall back to the raw per-track texts only when the ASR
    // model returned no segment timings to de-dup on.
    let transcript = if merged.is_empty() {
        full_texts.join("\n")
    } else {
        merged
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    // ponytail (accepted degradation): in a multi-track meeting, a single track erroring (rare —
    // same model + machine for both) yields a partial transcript still marked `Done` rather than
    // `Failed`. We keep the partial content visible instead of hiding it; the mixed WAV holds
    // both sides, so the History retry re-transcribes the whole recording if the user wants it.
    // Only a session where *everything* failed and nothing was produced becomes `Failed`.
    let status = if transcript.trim().is_empty() && merged.is_empty() && any_error {
        TranscriptionStatus::Failed
    } else {
        TranscriptionStatus::Done
    };
    hm.update_transcription(entry.id, transcript, None, None, status)?;

    info!("Session finalized → {}", wav_path.display());
    Ok(())
}

/// Decode a raw little-endian i16 PCM file to normalised f32. A dangling odd byte
/// from a torn final write is dropped (`chunks_exact`), so crash-truncated files
/// decode cleanly instead of panicking.
fn read_pcm_i16(path: &Path) -> Result<Vec<f32>> {
    let bytes = fs::read(path)?;
    Ok(bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32)
        .collect())
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
    fn mix_sums_tracks_pads_shorter_and_clamps() {
        let a = vec![0.5, 0.8, 0.5];
        let b = vec![0.5, 0.8]; // shorter → padded with silence for the tail
        let mixed = mix_tracks(&[a.as_slice(), b.as_slice()]);
        assert_eq!(mixed.len(), 3);
        assert!((mixed[0] - 1.0).abs() < 1e-6); // 0.5 + 0.5
        assert!((mixed[1] - 1.0).abs() < 1e-6); // 0.8 + 0.8 = 1.6 → clamped to 1.0
        assert!((mixed[2] - 0.5).abs() < 1e-6); // 0.5 + silence
    }

    #[test]
    fn mix_of_no_tracks_is_empty() {
        let empty: &[&[f32]] = &[];
        assert!(mix_tracks(empty).is_empty());
    }

    #[test]
    fn recovery_derives_wav_path_from_pcm() {
        let pcm = Path::new("/data/recordings/session-1750000000123-7.session.pcm");
        assert_eq!(
            wav_path_for(pcm),
            Path::new("/data/recordings/session-1750000000123-7.wav")
        );
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
}
