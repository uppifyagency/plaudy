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
use crate::managers::diarization::{align, DiarizationManager};
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use anyhow::{anyhow, Result};
use log::{error, info, warn};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tauri::{AppHandle, Manager};

/// Marks an in-progress raw capture. A file with this suffix that outlives its
/// process is an interrupted session to be recovered.
const PCM_SUFFIX: &str = ".session.pcm";

/// Which audio source a session captures.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// The microphone (Fase 0).
    Mic,
    /// macOS system / loopback audio — the other side of a call/meeting (Fase 1).
    SystemAudio,
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

struct ActiveSession {
    recorder: ActiveRecorder,
    writer: JoinHandle<Result<u64>>,
    pcm_path: PathBuf,
    wav_path: PathBuf,
}

pub struct SessionManager {
    app: AppHandle,
    recordings_dir: PathBuf,
    active: Mutex<Option<ActiveSession>>,
}

impl SessionManager {
    pub fn new(app: &AppHandle) -> Result<Self> {
        let recordings_dir = crate::portable::app_data_dir(app)?.join("recordings");
        fs::create_dir_all(&recordings_dir)?;
        Ok(Self {
            app: app.clone(),
            recordings_dir,
            active: Mutex::new(None),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// Start if idle, stop if active. Returns whether a session is active afterwards.
    pub fn toggle(&self, source: Source) -> Result<bool> {
        if self.is_active() {
            self.stop()?;
            Ok(false)
        } else {
            self.start(source)?;
            Ok(true)
        }
    }

    pub fn start(&self, source: Source) -> Result<()> {
        let mut guard = self.active.lock().unwrap();
        if guard.is_some() {
            return Err(anyhow!("A session is already active"));
        }

        let id = new_session_id();
        let pcm_path = self.recordings_dir.join(format!("{id}{PCM_SUFFIX}"));
        let wav_path = self.recordings_dir.join(format!("{id}.wav"));

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        // Build + start the capture backend FIRST: if it fails (e.g. the system-audio
        // permission is denied) we return before creating any on-disk artifacts, so
        // no empty `.session.pcm` is orphaned. Frames emitted between start and the
        // writer spawning buffer harmlessly in the unbounded channel.
        let recorder = build_recorder(source, tx)?;
        let writer = spawn_pcm_writer(pcm_path.clone(), rx);

        info!(
            "Long-form session started ({source:?}) → {}",
            pcm_path.display()
        );
        *guard = Some(ActiveSession {
            recorder,
            writer,
            pcm_path,
            wav_path,
        });
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let session = self
            .active
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("No active session"))?;

        let ActiveSession {
            mut recorder,
            writer,
            pcm_path,
            wav_path,
        } = session;

        // Tear down capture. `stop()` drains every buffered frame into the sink
        // before returning; dropping the recorder drops the last sink sender,
        // which closes the writer's channel so it can finalize the file.
        let _ = recorder.stop();
        let _ = recorder.close();
        drop(recorder);
        let samples = writer
            .join()
            .map_err(|_| anyhow!("PCM writer thread panicked"))??;
        info!("Session captured {samples} samples; finalizing");

        // Finalize off-thread: transcribing a long file is slow and must not block
        // the caller. The new row reaches the UI via HistoryUpdatePayload::Added.
        let app = self.app.clone();
        std::thread::spawn(move || {
            if let Err(e) = finalize(&app, &pcm_path, &wav_path) {
                error!("Failed to finalize session {}: {e}", wav_path.display());
            }
        });
        Ok(())
    }

    /// Finalize any session whose process died mid-recording. Safe to call once at
    /// startup, after the history and transcription managers are in managed state.
    pub fn recover_interrupted(&self) {
        let Ok(entries) = fs::read_dir(&self.recordings_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let pcm_path = entry.path();
            if !pcm_path.to_string_lossy().ends_with(PCM_SUFFIX) {
                continue;
            }
            let wav_path = wav_path_for(&pcm_path);
            warn!("Recovering interrupted session → {}", pcm_path.display());
            let app = self.app.clone();
            std::thread::spawn(move || {
                if let Err(e) = finalize(&app, &pcm_path, &wav_path) {
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
fn spawn_pcm_writer(pcm_path: PathBuf, rx: mpsc::Receiver<Vec<f32>>) -> JoinHandle<Result<u64>> {
    std::thread::spawn(move || -> Result<u64> {
        let mut out = BufWriter::new(File::create(&pcm_path)?);
        let mut written: u64 = 0;
        while let Ok(frame) = rx.recv() {
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

/// PCM → faithful WAV → (best-effort) transcript → history row, then delete the PCM.
fn finalize(app: &AppHandle, pcm_path: &Path, wav_path: &Path) -> Result<()> {
    let samples = read_pcm_i16(pcm_path)?;
    if samples.is_empty() {
        warn!("Session PCM {} is empty; discarding", pcm_path.display());
        let _ = fs::remove_file(pcm_path);
        return Ok(());
    }

    // Faithful archive: mono 16 kHz 16-bit WAV, identical spec to dictation clips.
    save_wav_file(wav_path, &samples)?;

    // Transcribe only when a model is resident: recovery runs at startup before any
    // model loads, and `transcribe` would otherwise block on the load condvar. A
    // missing transcript never costs the recording — the audio is already saved.
    let tm = app.state::<Arc<TranscriptionManager>>();
    let hm = app.state::<Arc<HistoryManager>>();
    let file_name = wav_path
        .file_name()
        .ok_or_else(|| anyhow!("session WAV path has no file name"))?
        .to_string_lossy()
        .to_string();

    if tm.is_model_loaded() {
        // Fase 2: diarize first (borrows `samples`), then transcribe (consumes it) — avoids
        // cloning a multi-hour buffer. Diarization no-ops instantly when its models are absent,
        // so this is free in the default install; `align` then labels every segment "unknown".
        let diarizer = DiarizationManager::new(&crate::portable::app_data_dir(app)?.join("models"));
        let turns = diarizer.diarize(&samples);

        let (transcript, asr_segments) =
            tm.transcribe_with_segments(samples).unwrap_or_else(|e| {
                warn!("Session transcription failed: {e}");
                (String::new(), Vec::new())
            });

        let entry = hm.save_entry(file_name, transcript, false, None, None)?;
        if !asr_segments.is_empty() {
            let segments = align(&asr_segments, &turns);
            if let Err(e) = hm.save_segments(entry.id, &segments) {
                warn!("Failed to persist diarized segments: {e}");
            }
        }
    } else {
        warn!("No transcription model loaded; saved session audio without a transcript");
        hm.save_entry(file_name, String::new(), false, None, None)?;
    }

    let _ = fs::remove_file(pcm_path);
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
    fn recovery_derives_wav_path_from_pcm() {
        let pcm = Path::new("/data/recordings/session-1750000000123-7.session.pcm");
        assert_eq!(
            wav_path_for(pcm),
            Path::new("/data/recordings/session-1750000000123-7.wav")
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
