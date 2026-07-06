//! Idle-time microphone voice sensor for seamless auto-capture.
//!
//! While auto-capture is enabled and NO session is recording, this holds a mic stream open and
//! runs Silero VAD on each 30 ms frame, remembering when speech was last heard. The supervisor
//! polls [`MicVoiceSensor::voice_recent`] as its "someone is talking" trigger — the counterpart
//! of the per-process system-audio sensor, so the trigger covers *any* audible source.
//!
//! The sensor must be **stopped whenever a session records** (the session's own mic track takes
//! over via the shared loudness clock) and while auto-capture is off — holding the mic also
//! shows macOS's mic-in-use indicator, which must never lie.

use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio_toolkit::audio::AudioRecorder;
use crate::audio_toolkit::vad::{SileroVad, VoiceActivityDetector};

/// Stricter than dictation's 0.3: a *trigger* prefers missing a mumble over starting a session
/// on a breath; the supervisor's debounce needs sustained speech anyway.
const TRIGGER_THRESHOLD: f32 = 0.5;

pub struct MicVoiceSensor {
    recorder: AudioRecorder,
    last_speech: Arc<Mutex<Option<Instant>>>,
    worker: Option<JoinHandle<()>>,
}

impl MicVoiceSensor {
    /// Open the default microphone and start VAD-marking speech. Fails cleanly (no partial
    /// stream) if the device or the VAD model is unavailable.
    pub fn start(vad_model: &Path) -> Result<Self, String> {
        let mut vad = SileroVad::new(vad_model, TRIGGER_THRESHOLD)
            .map_err(|e| format!("mic sensor VAD: {e}"))?;
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let mut recorder = AudioRecorder::new()
            .map_err(|e| format!("mic sensor recorder: {e}"))?
            .with_chunk_sink(tx);
        recorder
            .open(None)
            .map_err(|e| format!("mic sensor open: {e}"))?;
        recorder
            .start()
            .map_err(|e| format!("mic sensor start: {e}"))?;

        let last_speech = Arc::new(Mutex::new(None::<Instant>));
        let marker = last_speech.clone();
        // Ends when the recorder closes (sink sender dropped). Frames arrive as the recorder's
        // 30 ms 16 kHz chunks — exactly what Silero expects; anything else is skipped.
        let worker = std::thread::spawn(move || {
            while let Ok(frame) = rx.recv() {
                if vad.is_voice(&frame).unwrap_or(false) {
                    if let Ok(mut t) = marker.lock() {
                        *t = Some(Instant::now());
                    }
                }
            }
        });

        Ok(Self {
            recorder,
            last_speech,
            worker: Some(worker),
        })
    }

    /// Was speech heard within the last `within`? (Bridges VAD frame gaps into one presence
    /// signal, mirroring how captured-loudness idle is read.)
    pub fn voice_recent(&self, within: Duration) -> bool {
        self.last_speech
            .lock()
            .ok()
            .and_then(|t| *t)
            .is_some_and(|t| t.elapsed() < within)
    }

    /// Release the microphone (the mic-in-use indicator turns off) and join the VAD thread.
    pub fn stop(mut self) {
        let _ = self.recorder.stop();
        let _ = self.recorder.close();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
