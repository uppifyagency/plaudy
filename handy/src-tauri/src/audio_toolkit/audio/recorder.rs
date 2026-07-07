use std::{
    io::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Sample, SizedSample,
};

use crate::audio_toolkit::{
    audio::{AudioVisualiser, FrameResampler},
    constants,
    vad::{self, VadFrame},
    VoiceActivityDetector,
};

pub(crate) enum Cmd {
    Start,
    Stop(mpsc::Sender<Vec<f32>>),
    Shutdown,
}

pub(crate) enum AudioChunk {
    Samples(Vec<f32>),
    EndOfStream,
}

/// How long the consumer waits for audio before checking for commands anyway. A stalled
/// device (unplugged mic, dead cpal backend) delivers no samples — the consumer must still
/// see Stop/Shutdown within this bound instead of blocking forever (the old blocking `recv`
/// deadlocked `stop()`/`close()` in exactly that scenario).
const CMD_POLL: Duration = Duration::from_millis(100);

/// Upper bound `stop()` waits for the consumer's drained-samples reply before giving up.
/// Generous: a healthy consumer replies within DRAIN_TIMEOUT plus scheduling slack.
pub(crate) const STOP_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long Cmd::Stop's drain loop waits for the producer's EndOfStream sentinel before
/// giving up. Referenced by the system-audio IOProc's EOS handshake too — one constant,
/// not a magic 2 s echoed across files.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// What consumes each resampled 16 kHz frame while recording — one explicit mode instead of
/// steering behavior through which `Option` happens to be set.
pub(crate) enum FrameSink {
    /// Long-form session: every frame — faithful, un-VAD-gated — streams to the PCM writer.
    /// `alive` latches the first send failure so a dead writer logs once, not per frame.
    Streaming {
        sink: mpsc::Sender<Vec<f32>>,
        alive: bool,
    },
    /// Dictation: VAD-gate (when present) and accumulate for the one-shot reply on Stop.
    Dictation {
        vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    },
}

impl FrameSink {
    /// A chunk sink wins: "streaming mode" is precisely "a sink was attached".
    pub(crate) fn from_parts(
        chunk_sink: Option<mpsc::Sender<Vec<f32>>>,
        vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    ) -> Self {
        match chunk_sink {
            Some(sink) => FrameSink::Streaming { sink, alive: true },
            None => FrameSink::Dictation { vad },
        }
    }

    /// Consume one frame while recording. Streaming pushes to the writer; dictation
    /// VAD-gates into `out_buf` for the one-shot transcribe. Streaming must NOT also
    /// accumulate into `out_buf`: for a multi-hour meeting that buffer would grow to the
    /// full PCM in RAM, defeating the point of streaming to disk.
    fn consume(&mut self, samples: &[f32], out_buf: &mut Vec<f32>) {
        match self {
            FrameSink::Streaming { sink, alive } => {
                if sink.send(samples.to_vec()).is_err() && *alive {
                    *alive = false;
                    log::error!(
                        "Chunk sink closed mid-recording; subsequent frames are dropped (writer died?)"
                    );
                }
            }
            FrameSink::Dictation { vad } => {
                if let Some(vad_arc) = vad {
                    let mut det = vad_arc.lock().unwrap();
                    match det.push_frame(samples).unwrap_or(VadFrame::Speech(samples)) {
                        VadFrame::Speech(buf) => out_buf.extend_from_slice(buf),
                        VadFrame::Noise => {}
                    }
                } else {
                    out_buf.extend_from_slice(samples);
                }
            }
        }
    }

    /// Reset per-recording state on Start (the dictation VAD's rolling context).
    fn reset(&mut self) {
        if let FrameSink::Dictation { vad: Some(v) } = self {
            v.lock().unwrap().reset();
        }
    }
}

/// Downmix an interleaved multi-channel frame to mono f32 by averaging channels
/// (`channels <= 1` is a plain convert-copy). Appends to `out` without clearing it.
/// The one mixdown both capture paths share — the cpal callback (any sample type) and the
/// system-audio IOProc (f32) — so they can't silently diverge.
pub(crate) fn downmix_interleaved<T>(data: &[T], channels: usize, out: &mut Vec<f32>)
where
    T: Sample,
    f32: cpal::FromSample<T>,
{
    if channels <= 1 {
        out.extend(data.iter().map(|&sample| sample.to_sample::<f32>()));
    } else {
        out.reserve(data.len() / channels);
        for frame in data.chunks_exact(channels) {
            let mono = frame
                .iter()
                .map(|&sample| sample.to_sample::<f32>())
                .sum::<f32>()
                / channels as f32;
            out.push(mono);
        }
    }
}

pub struct AudioRecorder {
    device: Option<Device>,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    chunk_sink: Option<mpsc::Sender<Vec<f32>>>,
}

impl AudioRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(AudioRecorder {
            device: None,
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
            chunk_sink: None,
        })
    }

    pub fn with_vad(mut self, vad: Box<dyn VoiceActivityDetector>) -> Self {
        self.vad = Some(Arc::new(Mutex::new(vad)));
        self
    }

    pub fn with_level_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(Vec<f32>) + Send + Sync + 'static,
    {
        self.level_cb = Some(Arc::new(cb));
        self
    }

    /// Stream every captured 16 kHz frame — un-VAD-gated — to `sink` while
    /// recording, so a long-form session can persist a faithful recording.
    pub fn with_chunk_sink(mut self, sink: mpsc::Sender<Vec<f32>>) -> Self {
        self.chunk_sink = Some(sink);
        self
    }

    pub fn open(&mut self, device: Option<Device>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            return Ok(()); // already open
        }

        // Unbounded on purpose: the cpal audio callback is realtime and must NEVER block or drop
        // a frame, so `send` can't fail-fast on a full buffer. Backpressure limitation: if the
        // consumer stalls, this queue grows silently (~64-192 KB/s). std::mpsc exposes no depth,
        // and a live samples-in/samples-out lag counter would cost more than it's worth here — the
        // consumer is a tight per-frame loop that has never been observed to fall behind. Left
        // unbounded (a bounded channel would change the live-validated capture semantics).
        let (sample_tx, sample_rx) = mpsc::channel::<AudioChunk>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let host = crate::audio_toolkit::get_cpal_host();
        let device = match device {
            Some(dev) => dev,
            None => host
                .default_input_device()
                .ok_or_else(|| Error::new(std::io::ErrorKind::NotFound, "No input device found"))?,
        };

        let thread_device = device.clone();
        let vad = self.vad.clone();
        // Move the optional level callback into the worker thread
        let level_cb = self.level_cb.clone();
        let chunk_sink = self.chunk_sink.clone();

        let worker = std::thread::spawn(move || {
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_flag_for_stream = stop_flag.clone();
            let init_result = (|| -> Result<(cpal::Stream, u32), String> {
                let config = AudioRecorder::get_preferred_config(&thread_device)
                    .map_err(|e| format!("Failed to fetch preferred config: {e}"))?;

                let sample_rate = config.sample_rate().0;
                let channels = config.channels() as usize;

                log::info!(
                    "Using device: {:?}\nSample rate: {}\nChannels: {}\nFormat: {:?}",
                    thread_device.name(),
                    sample_rate,
                    channels,
                    config.sample_format()
                );

                let stream = match config.sample_format() {
                    cpal::SampleFormat::U8 => AudioRecorder::build_stream::<u8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I8 => AudioRecorder::build_stream::<i8>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I16 => AudioRecorder::build_stream::<i16>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::I32 => AudioRecorder::build_stream::<i32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    cpal::SampleFormat::F32 => AudioRecorder::build_stream::<f32>(
                        &thread_device,
                        &config,
                        sample_tx,
                        channels,
                        stop_flag_for_stream,
                    )
                    .map_err(|e| format!("Failed to build input stream: {e}"))?,
                    sample_format => {
                        return Err(format!("Unsupported sample format: {sample_format:?}"));
                    }
                };

                stream
                    .play()
                    .map_err(|e| format!("Failed to start microphone stream: {e}"))?;

                Ok((stream, sample_rate))
            })();

            match init_result {
                Ok((stream, sample_rate)) => {
                    let _ = init_tx.send(Ok(()));
                    // Keep the stream alive while we process samples.
                    run_consumer(
                        sample_rate,
                        FrameSink::from_parts(chunk_sink, vad),
                        sample_rx,
                        cmd_rx,
                        level_cb,
                        stop_flag,
                    );
                    drop(stream);
                }
                Err(error_message) => {
                    log::error!("{error_message}");
                    let _ = init_tx.send(Err(error_message));
                }
            }
        });

        match init_rx.recv() {
            Ok(Ok(())) => {
                self.device = Some(device);
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                Ok(())
            }
            Ok(Err(error_message)) => {
                let _ = worker.join();
                let kind = if is_microphone_access_denied(&error_message) {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::Other
                };
                Err(Box::new(Error::new(kind, error_message)))
            }
            Err(recv_error) => {
                let _ = worker.join();
                Err(Box::new(Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to initialize microphone worker: {recv_error}"),
                )))
            }
        }
    }

    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = &self.cmd_tx {
            tx.send(Cmd::Start)?;
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let (resp_tx, resp_rx) = mpsc::channel();
        if let Some(tx) = &self.cmd_tx {
            tx.send(Cmd::Stop(resp_tx))?;
        }
        // Bounded wait: a consumer that never replies (worker died) must surface as an
        // error, not hang the caller's UI thread forever.
        Ok(resp_rx.recv_timeout(STOP_REPLY_TIMEOUT)?)
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
        // Release the retained chunk-sink sender too. The worker's clone drops when it exits
        // above, but this original outlives it — and a downstream consumer that ends on channel
        // close (MicVoiceSensor's VAD worker joins on it) would hang forever otherwise. "Close"
        // means release every resource, sink included.
        self.chunk_sink = None;
        self.device = None;
        Ok(())
    }

    fn build_stream<T>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        sample_tx: mpsc::Sender<AudioChunk>,
        channels: usize,
        stop_flag: Arc<AtomicBool>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: Sample + SizedSample + Send + 'static,
        f32: cpal::FromSample<T>,
    {
        let mut output_buffer = Vec::new();
        let mut eos_sent = false;

        let stream_cb = move |data: &[T], _: &cpal::InputCallbackInfo| {
            if stop_flag.load(Ordering::Relaxed) {
                if !eos_sent {
                    let _ = sample_tx.send(AudioChunk::EndOfStream);
                    eos_sent = true;
                }
                return;
            }
            eos_sent = false;

            output_buffer.clear();
            downmix_interleaved(data, channels, &mut output_buffer);

            if sample_tx
                .send(AudioChunk::Samples(output_buffer.clone()))
                .is_err()
            {
                log::error!("Failed to send samples");
            }
        };

        device.build_input_stream(
            &config.clone().into(),
            stream_cb,
            |err| log::error!("Stream error: {}", err),
            None,
        )
    }

    fn get_preferred_config(
        device: &cpal::Device,
    ) -> Result<cpal::SupportedStreamConfig, Box<dyn std::error::Error>> {
        // Use the device's native/default sample rate and let the FrameResampler
        // in run_consumer() downsample to 16kHz. This avoids forcing hardware into
        // a non-native rate which can cause issues on some devices (Bluetooth
        // codecs, certain ALSA drivers, etc.).
        let default_config = device.default_input_config()?;
        let target_rate = default_config.sample_rate();

        // Try to find the best sample format at the device's default rate
        let supported_configs = match device.supported_input_configs() {
            Ok(configs) => configs,
            Err(e) => {
                log::warn!("Could not enumerate input configs ({e}), using device default");
                return Ok(default_config);
            }
        };
        let mut best_config: Option<cpal::SupportedStreamConfigRange> = None;

        for config_range in supported_configs {
            if config_range.min_sample_rate() <= target_rate
                && config_range.max_sample_rate() >= target_rate
            {
                match best_config {
                    None => best_config = Some(config_range),
                    Some(ref current) => {
                        // Prioritize F32 > I16 > I32 > others
                        let score = |fmt: cpal::SampleFormat| match fmt {
                            cpal::SampleFormat::F32 => 4,
                            cpal::SampleFormat::I16 => 3,
                            cpal::SampleFormat::I32 => 2,
                            _ => 1,
                        };

                        if score(config_range.sample_format()) > score(current.sample_format()) {
                            best_config = Some(config_range);
                        }
                    }
                }
            }
        }

        if let Some(config) = best_config {
            return Ok(config.with_sample_rate(target_rate));
        }

        // Fall back to device default if no config matched (exotic/virtual devices)
        log::warn!(
            "No supported config matched device default rate {:?}, using default config",
            target_rate
        );
        Ok(default_config)
    }
}

pub fn is_microphone_access_denied(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("access is denied")
        || normalized.contains("permission denied")
        || normalized.contains("0x80070005")
}

pub fn is_no_input_device_error(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("no input device found")
        || (normalized.contains("failed to fetch preferred config")
            && normalized.contains("coreaudio"))
}

#[cfg(test)]
mod tests {
    use super::{is_microphone_access_denied, is_no_input_device_error};

    #[test]
    fn detects_access_is_denied() {
        assert!(is_microphone_access_denied("Access is denied"));
    }

    #[test]
    fn detects_permission_denied() {
        assert!(is_microphone_access_denied("permission denied"));
    }

    #[test]
    fn detects_windows_error_code() {
        assert!(is_microphone_access_denied("WASAPI error: 0x80070005"));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        assert!(!is_microphone_access_denied("device not found"));
    }

    #[test]
    fn detects_no_input_device() {
        assert!(is_no_input_device_error("No input device found"));
    }

    #[test]
    fn detects_coreaudio_config_error() {
        assert!(is_no_input_device_error(
            "Failed to fetch preferred config: A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred"
        ));
    }

    #[test]
    fn does_not_match_other_errors_for_no_device() {
        assert!(!is_no_input_device_error("permission denied"));
        assert!(!is_no_input_device_error("device not found"));
    }

    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn downmix_averages_interleaved_channels_and_passes_mono_through() {
        let mut out = Vec::new();
        downmix_interleaved(&[0.2f32, 0.4, -1.0, 1.0], 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6, "stereo frame averages");
        assert!(out[1].abs() < 1e-6, "opposite channels cancel");

        out.clear();
        downmix_interleaved(&[0.5f32, -0.5], 1, &mut out);
        assert_eq!(out, vec![0.5, -0.5], "mono is a plain copy");
    }

    #[test]
    fn downmix_converts_integer_samples_to_normalised_f32() {
        let mut out = Vec::new();
        downmix_interleaved(&[i16::MAX, i16::MAX], 2, &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 1.0).abs() < 1e-3, "full-scale i16 → ~1.0");
    }

    /// Drive run_consumer with in-memory channels — no device needed. 16 kHz in = 16 kHz out,
    /// so the resampler passes 30 ms (480-sample) frames through unchanged.
    fn spawn_consumer(
        sink: FrameSink,
    ) -> (
        mpsc::Sender<AudioChunk>,
        mpsc::Sender<Cmd>,
        std::thread::JoinHandle<()>,
    ) {
        let (sample_tx, sample_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || {
            run_consumer(16_000, sink, sample_rx, cmd_rx, None, stop_flag)
        });
        (sample_tx, cmd_tx, handle)
    }

    #[test]
    fn dictation_mode_accumulates_while_recording_and_replies_on_stop() {
        let (sample_tx, cmd_tx, handle) = spawn_consumer(FrameSink::Dictation { vad: None });
        cmd_tx.send(Cmd::Start).unwrap();
        sample_tx.send(AudioChunk::Samples(vec![0.1; 480])).unwrap();
        let (resp_tx, resp_rx) = mpsc::channel();
        cmd_tx.send(Cmd::Stop(resp_tx)).unwrap();
        sample_tx.send(AudioChunk::EndOfStream).unwrap(); // what the callback would inject

        let samples = resp_rx.recv_timeout(STOP_REPLY_TIMEOUT).unwrap();
        assert_eq!(samples.len(), 480);

        drop(sample_tx); // channel closes → consumer exits
        handle.join().unwrap();
    }

    #[test]
    fn streaming_mode_feeds_the_sink_and_never_accumulates() {
        let (chunk_tx, chunk_rx) = mpsc::channel();
        let (sample_tx, cmd_tx, handle) = spawn_consumer(FrameSink::Streaming {
            sink: chunk_tx,
            alive: true,
        });
        cmd_tx.send(Cmd::Start).unwrap();
        sample_tx.send(AudioChunk::Samples(vec![0.2; 480])).unwrap();
        let (resp_tx, resp_rx) = mpsc::channel();
        cmd_tx.send(Cmd::Stop(resp_tx)).unwrap();
        sample_tx.send(AudioChunk::EndOfStream).unwrap();

        let reply = resp_rx.recv_timeout(STOP_REPLY_TIMEOUT).unwrap();
        assert!(
            reply.is_empty(),
            "streaming sessions must not grow the in-RAM stop buffer"
        );
        let streamed: usize = chunk_rx.try_iter().map(|f| f.len()).sum();
        assert_eq!(streamed, 480, "every frame reaches the chunk sink");

        drop(sample_tx);
        handle.join().unwrap();
    }

    #[test]
    fn stop_replies_even_when_the_device_is_stalled() {
        // THE deadlock regression: no samples ever arrive (unplugged mic / dead backend).
        // Stop must still be seen (CMD_POLL) and answered (drain timeout), not hang forever.
        let (sample_tx, cmd_tx, handle) = spawn_consumer(FrameSink::Dictation { vad: None });
        let (resp_tx, resp_rx) = mpsc::channel();
        cmd_tx.send(Cmd::Stop(resp_tx)).unwrap();
        // No EndOfStream on purpose — the producer is dead.
        let samples = resp_rx
            .recv_timeout(STOP_REPLY_TIMEOUT)
            .expect("stop must not deadlock on a silent device");
        assert!(samples.is_empty());

        cmd_tx.send(Cmd::Shutdown).unwrap();
        drop(sample_tx);
        handle.join().unwrap();
    }

    #[test]
    fn start_clears_any_leftover_buffer() {
        let (sample_tx, cmd_tx, handle) = spawn_consumer(FrameSink::Dictation { vad: None });
        cmd_tx.send(Cmd::Start).unwrap();
        sample_tx.send(AudioChunk::Samples(vec![0.3; 480])).unwrap();
        // Let the consumer actually record the frame before re-Starting (commands are
        // served ahead of queued samples, so without this pause the sample would only be
        // consumed later, inside Stop's drain).
        std::thread::sleep(Duration::from_millis(300));
        // Re-Start without stopping: the previously recorded audio must be discarded.
        cmd_tx.send(Cmd::Start).unwrap();
        let (resp_tx, resp_rx) = mpsc::channel();
        cmd_tx.send(Cmd::Stop(resp_tx)).unwrap();
        sample_tx.send(AudioChunk::EndOfStream).unwrap();

        let samples = resp_rx.recv_timeout(STOP_REPLY_TIMEOUT).unwrap();
        assert!(samples.is_empty(), "Start must reset the recording buffer");

        drop(sample_tx);
        handle.join().unwrap();
    }

    #[test]
    fn close_releases_the_chunk_sink_so_a_consumer_can_terminate() {
        // THE mic-voice-trigger deadlock: MicVoiceSensor::stop() closes the recorder, then joins
        // a VAD worker that ends only when the chunk-sink channel closes. If close() keeps the
        // retained sink sender alive, that worker never terminates and the whole auto-capture
        // loop wedges for the process lifetime after the first session. close() must drop it.
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let mut rec = AudioRecorder::new().unwrap().with_chunk_sink(tx);
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            while rx.recv().is_ok() {}
            let _ = done_tx.send(());
        });
        rec.close().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "close() must drop the chunk_sink so the consumer channel closes"
        );
    }
}

pub(crate) fn run_consumer(
    in_sample_rate: u32,
    mut sink: FrameSink,
    sample_rx: mpsc::Receiver<AudioChunk>,
    cmd_rx: mpsc::Receiver<Cmd>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut frame_resampler = FrameResampler::new(
        in_sample_rate as usize,
        constants::WHISPER_SAMPLE_RATE as usize,
        Duration::from_millis(30),
    );

    let mut processed_samples = Vec::<f32>::new();
    let mut recording = false;

    // ---------- spectrum visualisation setup ---------------------------- //
    const BUCKETS: usize = 16;
    // Scale the FFT window to the device sample rate so the analysis window
    // (~33 ms) and frequency resolution (~30 Hz/bin) stay roughly constant
    // across devices. A fixed 512-sample window collapses the low vocal
    // buckets onto a single bin at 48 kHz (e.g. built-in laptop mics), and
    // would stutter at ~4-8 updates/sec on an 8-16 kHz Bluetooth headset.
    // Targets: 48 kHz -> 2048, 16 kHz -> 512, 8 kHz -> 256.
    let target_window = (f64::from(in_sample_rate) / 30.0).round() as usize;
    let window_size = [256usize, 512, 1024, 2048]
        .into_iter()
        .min_by_key(|w| w.abs_diff(target_window))
        .unwrap();
    let mut visualizer = AudioVisualiser::new(
        in_sample_rate,
        window_size,
        BUCKETS,
        400.0,  // vocal_min_hz
        4000.0, // vocal_max_hz
    );

    loop {
        // Serve pending commands FIRST, then wait (briefly) for audio: a command must never
        // queue behind a sample, and a stalled device (unplugged mic, dead backend) must
        // never starve Stop/Shutdown — see CMD_POLL. The old sample-first blocking recv
        // deadlocked stop()/close() exactly when the device went silent.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Start => {
                    stop_flag.store(false, Ordering::Relaxed);
                    processed_samples.clear();
                    recording = true;
                    visualizer.reset();
                    sink.reset();
                }
                Cmd::Stop(reply_tx) => {
                    recording = false;
                    stop_flag.store(true, Ordering::Relaxed);

                    // Drain all remaining audio until the producer confirms end-of-stream.
                    // The cpal callback sees the stop flag, sends EndOfStream, and goes
                    // silent — guaranteeing every captured sample is in the channel
                    // ahead of the sentinel.
                    loop {
                        match sample_rx.recv_timeout(DRAIN_TIMEOUT) {
                            Ok(AudioChunk::Samples(remaining)) => {
                                frame_resampler.push(&remaining, &mut |frame: &[f32]| {
                                    sink.consume(frame, &mut processed_samples)
                                });
                            }
                            Ok(AudioChunk::EndOfStream) => break,
                            Err(_) => {
                                log::warn!("Timed out waiting for EndOfStream from audio callback");
                                break;
                            }
                        }
                    }

                    frame_resampler
                        .finish(&mut |frame: &[f32]| sink.consume(frame, &mut processed_samples));

                    let _ = reply_tx.send(std::mem::take(&mut processed_samples));

                    // Resume the audio callback so the consumer loop can continue
                    // receiving chunks (important for always-on microphone mode).
                    stop_flag.store(false, Ordering::Relaxed);
                }
                Cmd::Shutdown => {
                    stop_flag.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }

        match sample_rx.recv_timeout(CMD_POLL) {
            Ok(AudioChunk::Samples(raw)) => {
                // ---------- spectrum processing ------------------------------ //
                if let Some(buckets) = visualizer.feed(&raw) {
                    if let Some(cb) = &level_cb {
                        cb(buckets);
                    }
                }

                // ---------- existing pipeline -------------------------------- //
                // The resampler always runs (its rolling state must stay continuous for
                // always-on mic mode); frames only reach the sink while recording.
                let sink = &mut sink;
                let out = &mut processed_samples;
                frame_resampler.push(&raw, &mut |frame: &[f32]| {
                    if recording {
                        sink.consume(frame, out);
                    }
                });
            }
            Ok(AudioChunk::EndOfStream) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {} // no audio — still serve commands
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // stream closed
        }
    }
}
