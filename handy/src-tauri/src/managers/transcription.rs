use crate::audio_toolkit::{apply_custom_words, filter_transcription_output};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::diarization::AsrSegment;
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{
    get_settings, ModelUnloadTimeout, OrtAcceleratorSetting, WhisperAcceleratorSetting,
};
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use transcribe_rs::{
    onnx::{
        canary::CanaryModel,
        cohere::CohereModel,
        gigaam::GigaAMModel,
        moonshine::{MoonshineModel, MoonshineVariant, StreamingModel},
        parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity},
        sense_voice::{SenseVoiceModel, SenseVoiceParams},
        Quantization,
    },
    whisper_cpp::{WhisperEngine, WhisperInferenceParams},
    SpeechModel, TranscribeOptions,
};

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

enum LoadedEngine {
    Whisper(WhisperEngine),
    Parakeet(ParakeetModel),
    Moonshine(MoonshineModel),
    MoonshineStreaming(StreamingModel),
    SenseVoice(SenseVoiceModel),
    GigaAM(GigaAMModel),
    Canary(CanaryModel),
    Cohere(CohereModel),
}

/// RAII guard that clears the `is_loading` flag and notifies waiters on drop.
/// Ensures the loading flag is always reset, even on early returns or panics.
pub struct LoadingGuard {
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        *is_loading = false;
        self.loading_condvar.notify_all();
    }
}

/// RAII proof that the caller holds the `inference_gate` for an exclusive engine-slot mutation
/// (a model switch). Handed out by [`TranscriptionManager::try_reserve_for_switch`]; releasing
/// the gate is just dropping the held `MutexGuard`. Its existence is what stops a model switch
/// from loading a SECOND engine while a run owns the slot (the double-engine incident).
pub struct InferenceReservation<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

#[derive(Clone)]
pub struct TranscriptionManager {
    engine: Arc<Mutex<Option<LoadedEngine>>>,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    current_model_id: Arc<Mutex<Option<String>>>,
    last_activity: Arc<AtomicU64>,
    shutdown_signal: Arc<AtomicBool>,
    watcher_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
    /// Held for the whole engine run. Serializes inference AND lets `is_model_loaded` report
    /// true while the engine is temporarily checked out of its slot (the take-on-panic
    /// pattern) — without this, a second caller saw "no model", loaded a SECOND engine and
    /// ran a duplicate inference (observed live: 2 × 89-min Parakeet runs ≈ 37 GB RAM).
    inference_gate: Arc<Mutex<()>>,
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        let manager = Self {
            engine: Arc::new(Mutex::new(None)),
            model_manager,
            app_handle: app_handle.clone(),
            current_model_id: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(AtomicU64::new(Self::now_ms())),
            shutdown_signal: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            is_loading: Arc::new(Mutex::new(false)),
            loading_condvar: Arc::new(Condvar::new()),
            inference_gate: Arc::new(Mutex::new(())),
        };

        // Start the idle watcher
        {
            let app_handle_cloned = app_handle.clone();
            let manager_cloned = manager.clone();
            let shutdown_signal = manager.shutdown_signal.clone();
            let handle = thread::spawn(move || {
                debug!("Idle watcher thread started");
                while !shutdown_signal.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(10)); // Check every 10 seconds

                    // Check shutdown signal again after sleep
                    if shutdown_signal.load(Ordering::Relaxed) {
                        break;
                    }

                    let settings = get_settings(&app_handle_cloned);
                    let timeout = settings.model_unload_timeout;

                    // Skip Immediately — that variant is handled by
                    // maybe_unload_immediately() after each transcription.
                    // Treating it as 0s here would unload the model mid-recording.
                    if timeout == ModelUnloadTimeout::Immediately {
                        continue;
                    }

                    // While recording, keep the idle timer fresh so the
                    // model is never unloaded mid-session.
                    let is_recording = app_handle_cloned
                        .try_state::<Arc<AudioRecordingManager>>()
                        .map_or(false, |a| a.is_recording());
                    if is_recording {
                        manager_cloned.touch_activity();
                        continue;
                    }

                    if let Some(limit_seconds) = timeout.to_seconds() {
                        let last = manager_cloned.last_activity.load(Ordering::Relaxed);
                        let now_ms = TranscriptionManager::now_ms();
                        let idle_ms = now_ms.saturating_sub(last);
                        let limit_ms = limit_seconds * 1000;

                        // Probe for an in-flight inference WITHOUT blocking: a held gate means a
                        // transcription run owns the engine right now — or a session finalize is
                        // between ensure_model_ready() and its (possibly minutes-later)
                        // transcribe call, with the model legitimately resident the whole time.
                        // Evicting in that window is exactly the C1 race that failed rows in the
                        // field (finalize reads ~170 MB PCM + diarizes without touching the idle
                        // timer), so we must not unload while a run holds the gate. When we DO
                        // hold it (Ok), we keep it for the whole unload so a transcribe can't
                        // take the engine mid-unload.
                        // Recover a poisoned gate here too (same policy as `is_model_loaded` and
                        // `try_reserve_for_switch`): a poisoned gate means a past panicked run,
                        // NOT a live one — treating it as in-progress would defer eviction forever
                        // and, paired with `is_model_loaded`, wedge the "loaded" state permanently.
                        // Poison → we hold it (Some) and may proceed to evict; only a truly-held
                        // gate (WouldBlock → None) defers.
                        let gate = match manager_cloned.inference_gate.try_lock() {
                            Ok(g) => Some(g),
                            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
                            Err(std::sync::TryLockError::WouldBlock) => None,
                        };
                        let inference_in_progress = gate.is_none();
                        // With the gate held (Ok) no run has the engine checked out, so the slot
                        // reflects true residency; bound + dropped before unload so the engine
                        // lock isn't held across it.
                        let resident = { manager_cloned.lock_engine().is_some() };
                        if should_evict(idle_ms, limit_ms, resident, inference_in_progress) {
                            let unload_start = std::time::Instant::now();
                            info!(
                                "Model idle for {}s (limit: {}s), unloading",
                                idle_ms / 1000,
                                limit_seconds
                            );
                            match manager_cloned.unload_model() {
                                Ok(()) => info!(
                                    "Model unloaded due to inactivity (took {}ms)",
                                    unload_start.elapsed().as_millis()
                                ),
                                Err(e) => error!("Failed to unload idle model: {}", e),
                            }
                        } else if inference_in_progress && idle_ms > limit_ms {
                            debug!(
                                "Model idle but an inference run holds the gate; deferring unload"
                            );
                        }
                        drop(gate);
                    }
                }
                debug!("Idle watcher thread shutting down gracefully");
            });
            *manager.watcher_handle.lock().unwrap() = Some(handle);
        }

        Ok(manager)
    }

    /// Lock the engine mutex, recovering from poison if a previous transcription panicked.
    fn lock_engine(&self) -> MutexGuard<'_, Option<LoadedEngine>> {
        self.engine.lock().unwrap_or_else(|poisoned| {
            warn!("Engine mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    /// Lock the current-model-id mutex, recovering from poison. Same policy as `lock_engine`:
    /// a past panic while briefly holding this tiny lock must never permanently wedge the model
    /// state — every acquisition of `current_model_id` goes through here so poison is handled
    /// uniformly (some paths already did this inline; this is the single door).
    fn lock_current_model(&self) -> MutexGuard<'_, Option<String>> {
        self.current_model_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn is_model_loaded(&self) -> bool {
        // A busy inference gate means an engine exists but is checked out of the slot for a
        // running transcription — that still counts as "loaded" (see `inference_gate`).
        // Only a GENUINELY-held gate (WouldBlock) counts; a POISONED gate means the previous
        // holder panicked and no run owns the engine now — treating that as "held" would pin
        // `is_model_loaded` true forever after a single panic (an unreachable-today mine, but a
        // mine). Poison → not held; residency is then decided by the slot alone.
        let engine = self.lock_engine();
        engine.is_some()
            || matches!(
                self.inference_gate.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            )
    }

    /// Make sure a model is resident (or its load already underway), waiting for any load in
    /// flight. Returns whether transcription can proceed. Used by session finalize and the
    /// startup retry pass — call sites where "no model resident right now" must mean "load it",
    /// not "fail the row".
    pub fn ensure_model_ready(&self) -> bool {
        self.initiate_model_load();
        let mut is_loading = self.is_loading.lock().unwrap();
        while *is_loading {
            is_loading = self.loading_condvar.wait(is_loading).unwrap();
        }
        drop(is_loading);
        self.is_model_loaded()
    }

    /// Atomically check whether a model load is in progress and, if not, mark
    /// one as starting. Returns a [`LoadingGuard`] whose [`Drop`] impl will
    /// clear the flag and wake waiters. Returns `None` if a load is already in
    /// progress.
    pub fn try_start_loading(&self) -> Option<LoadingGuard> {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return None;
        }
        *is_loading = true;
        Some(LoadingGuard {
            is_loading: self.is_loading.clone(),
            loading_condvar: self.loading_condvar.clone(),
        })
    }

    /// Try to reserve the engine for an exclusive slot mutation (a model switch), WITHOUT
    /// blocking. `Some` means the caller now owns the `inference_gate` and may safely load a
    /// new engine into the slot; `None` means a transcription run currently owns it — a chunked
    /// long-file run can hold it for 10-20 minutes, so the caller should surface a "transcription
    /// in progress" error rather than block the UI or (worse) load a second engine whose
    /// put-back would clobber the switch. This is the model-switch side of "one resource, one
    /// protocol": every engine-slot mutation goes through the same gate that serializes inference.
    /// Poison (a prior panicked run) is recovered — consistent with the inference path — so a
    /// past crash can't wedge the switch permanently; only a genuinely-held gate returns `None`.
    pub fn try_reserve_for_switch(&self) -> Option<InferenceReservation<'_>> {
        match self.inference_gate.try_lock() {
            Ok(guard) => Some(InferenceReservation(guard)),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(InferenceReservation(p.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    pub fn unload_model(&self) -> Result<()> {
        let unload_start = std::time::Instant::now();
        debug!("Starting to unload model");

        {
            let mut engine = self.lock_engine();
            // Dropping the engine frees all resources
            *engine = None;
        }
        {
            let mut current_model = self.lock_current_model();
            *current_model = None;
        }

        // Emit unloaded event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );

        let unload_duration = unload_start.elapsed();
        debug!(
            "Model unloaded manually (took {}ms)",
            unload_duration.as_millis()
        );
        Ok(())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Reset the idle timer to now.
    fn touch_activity(&self) {
        self.last_activity.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Unloads the model immediately if the setting is enabled and the model is loaded
    pub fn maybe_unload_immediately(&self, context: &str) {
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            if let Err(e) = self.unload_model() {
                warn!("Failed to immediately unload model: {}", e);
            }
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        let load_start = std::time::Instant::now();
        debug!("Starting to load model: {}", model_id);

        // Emit loading started event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_started".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: None,
                error: None,
            },
        );

        let model_info = self
            .model_manager
            .get_model_info(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        if !model_info.is_downloaded {
            let error_msg = "Model not downloaded";
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
            return Err(anyhow::anyhow!(error_msg));
        }

        let model_path = self.model_manager.get_model_path(model_id)?;

        // Create appropriate engine based on model type
        let emit_loading_failed = |error_msg: &str| {
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
        };

        let loaded_engine = match model_info.engine_type {
            EngineType::Whisper => {
                let engine = WhisperEngine::load(&model_path).map_err(|e| {
                    let error_msg = format!("Failed to load whisper model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Whisper(engine)
            }
            EngineType::Parakeet => {
                let engine =
                    ParakeetModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load parakeet model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::Parakeet(engine)
            }
            EngineType::Moonshine => {
                let engine = MoonshineModel::load(
                    &model_path,
                    MoonshineVariant::Base,
                    &Quantization::default(),
                )
                .map_err(|e| {
                    let error_msg = format!("Failed to load moonshine model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Moonshine(engine)
            }
            EngineType::MoonshineStreaming => {
                let engine = StreamingModel::load(&model_path, 0, &Quantization::default())
                    .map_err(|e| {
                        let error_msg = format!(
                            "Failed to load moonshine streaming model {}: {}",
                            model_id, e
                        );
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::MoonshineStreaming(engine)
            }
            EngineType::SenseVoice => {
                let engine =
                    SenseVoiceModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load SenseVoice model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::SenseVoice(engine)
            }
            EngineType::GigaAM => {
                let engine = GigaAMModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load gigaam model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::GigaAM(engine)
            }
            EngineType::Canary => {
                let engine = CanaryModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load canary model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Canary(engine)
            }
            EngineType::Cohere => {
                let engine = CohereModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load cohere model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Cohere(engine)
            }
        };

        // Update the current engine and model ID
        {
            let mut engine = self.lock_engine();
            *engine = Some(loaded_engine);
        }
        {
            let mut current_model = self.lock_current_model();
            *current_model = Some(model_id.to_string());
        }

        // Reset idle timer so the watcher doesn't immediately unload a just-loaded model
        self.touch_activity();

        // Emit loading completed event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_completed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
            },
        );

        let load_duration = load_start.elapsed();
        debug!(
            "Successfully loaded transcription model: {} (took {}ms)",
            model_id,
            load_duration.as_millis()
        );
        Ok(())
    }

    /// Kicks off the model loading in a background thread if it's not already loaded
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading || self.is_model_loaded() {
            return;
        }

        *is_loading = true;
        let self_clone = self.clone();
        thread::spawn(move || {
            let settings = get_settings(&self_clone.app_handle);
            if let Err(e) = self_clone.load_model(&settings.selected_model) {
                error!("Failed to load model: {}", e);
            }
            let mut is_loading = self_clone.is_loading.lock().unwrap();
            *is_loading = false;
            self_clone.loading_condvar.notify_all();
        });
    }

    pub fn get_current_model(&self) -> Option<String> {
        let current_model = self.lock_current_model();
        current_model.clone()
    }

    // ponytail: single core for both the dictation hot path (`transcribe`) and the diarization
    // path (`transcribe_with_segments`) — one engine run, no duplication. Returns the cleaned
    // flat text plus the engine's per-segment timings (empty when the engine yields none).
    fn transcribe_inner(&self, audio: Vec<f32>) -> Result<(String, Vec<AsrSegment>)> {
        #[cfg(debug_assertions)]
        if std::env::var("HANDY_FORCE_TRANSCRIPTION_FAILURE").is_ok() {
            return Err(anyhow::anyhow!(
                "Simulated transcription failure (HANDY_FORCE_TRANSCRIPTION_FAILURE)"
            ));
        }

        // Update last activity timestamp
        self.touch_activity();

        let st = std::time::Instant::now();

        debug!("Audio vector length: {}", audio.len());

        if audio.is_empty() {
            debug!("Empty audio vector");
            self.maybe_unload_immediately("empty audio");
            return Ok((String::new(), Vec::new()));
        }

        // Wait for any in-flight background load to finish so we don't kick off a duplicate
        // one. Whether a model actually ends up resident is (re)checked under the inference
        // gate below, at the point of use — we deliberately do NOT bail here just because the
        // idle unloader evicted in the window between a caller's ensure_model_ready() and now
        // (the C1 race: finalize spends minutes reading PCM + diarizing in that gap, never
        // touching the idle timer). The gate section self-heals by loading on demand.
        {
            let mut is_loading = self.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.loading_condvar.wait(is_loading).unwrap();
            }
        }

        // Get current settings for configuration
        let settings = get_settings(&self.app_handle);

        // Validate selected language against the model's supported languages.
        // If the language isn't supported, fall back to "auto" to prevent errors.
        let validated_language = if settings.selected_language == "auto" {
            "auto".to_string()
        } else {
            let is_supported = self
                .model_manager
                .get_model_info(&settings.selected_model)
                .map(|info| {
                    info.supported_languages.is_empty()
                        || info
                            .supported_languages
                            .contains(&settings.selected_language)
                })
                .unwrap_or(true);

            if is_supported {
                settings.selected_language.clone()
            } else {
                warn!(
                    "Language '{}' not supported by current model, falling back to auto-detect",
                    settings.selected_language
                );
                "auto".to_string()
            }
        };

        // Long audio is transcribed in ~5 min windows, cut at the quietest point near each
        // boundary. The encoders (Parakeet's conformer above all) get each window as one
        // sequence — self-attention on a 90-min recording allocates tens of GB and fails;
        // chunking bounds it to a few hundred MB. `chunk_spans` is pure arithmetic, so we
        // compute it before touching the inference gate.
        let spans = chunk_spans(
            &audio,
            crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE as usize,
        );
        if spans.len() > 1 {
            info!(
                "Long audio: transcribing in {} chunks of ≈{} s",
                spans.len(),
                CHUNK_TARGET_SECS
            );
        }

        // A2: acquire/release the inference gate PER CHUNK, not once for the whole run. A startup
        // heal or manual retry of a long row runs 7-20 min; holding the gate that entire time made
        // a 5 s dictation started meanwhile queue until the very end — its paste arrived minutes
        // late. Now each chunk is one gated critical section (`transcribe_chunk_gated`): between
        // chunks the engine is back in its slot and the gate is free, so a waiting dictation slips
        // in, and the next chunk re-checks-out the engine under its own gate hold (C1 self-heal
        // preserved per chunk). The engine stays resident across chunks in the common case, so no
        // per-chunk reload.
        let mut merged_text = String::new();
        let mut merged_segments: Vec<transcribe_rs::TranscriptionSegment> = Vec::new();
        let mut any_segments = false;
        // The model id fixed by the run's FIRST chunk. If a switch lands between chunks, self-heal
        // reloads a DIFFERENT engine and `transcribe_chunk_gated` aborts rather than splice.
        let mut pinned_model_id: Option<String> = None;
        for (chunk_no, (start, end)) in spans.iter().copied().enumerate() {
            let chunk = &audio[start..end];
            let offset_s =
                start as f32 / crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE as f32;
            let (chunk_result, engine_model_id) = self.transcribe_chunk_gated(
                chunk,
                &validated_language,
                &settings,
                pinned_model_id.as_deref(),
            )?;
            if pinned_model_id.is_none() {
                pinned_model_id = engine_model_id;
            }
            if spans.len() > 1 {
                debug!("Chunk {}/{} transcribed", chunk_no + 1, spans.len());
            }
            let chunk_text = chunk_result.text.trim();
            if !chunk_text.is_empty() {
                if !merged_text.is_empty() {
                    merged_text.push(' ');
                }
                merged_text.push_str(chunk_text);
            }
            if let Some(mut segs) = chunk_result.segments {
                any_segments = true;
                for s in &mut segs {
                    s.start += offset_s;
                    s.end += offset_s;
                }
                merged_segments.extend(segs);
            }
            // `inference_gate` is a std::sync::Mutex — NOT fair: a thread that releases and
            // immediately re-locks can starve waiters. Yield between chunks so a dictation blocked
            // on the gate is actually scheduled to take it instead of us grabbing it right back.
            if spans.len() > 1 && chunk_no + 1 < spans.len() {
                std::thread::yield_now();
            }
        }
        let result = transcribe_rs::TranscriptionResult {
            text: merged_text,
            segments: any_segments.then_some(merged_segments),
        };

        // Fase 2: capture the engine's per-segment timings (seconds) before `result.text` is
        // consumed below; convert to ms `AsrSegment`s for diarization.
        let asr_segments = to_asr_segments(&result.segments);

        // Apply word correction if custom words are configured.
        // Skip for Whisper models since custom words are already passed as initial_prompt.
        let is_whisper = self
            .model_manager
            .get_model_info(&settings.selected_model)
            .map(|info| matches!(info.engine_type, EngineType::Whisper))
            .unwrap_or(false);

        let corrected_result = if !settings.custom_words.is_empty() && !is_whisper {
            apply_custom_words(
                &result.text,
                &settings.custom_words,
                settings.word_correction_threshold,
            )
        } else {
            result.text
        };

        // Filter out filler words and hallucinations
        let filtered_result = filter_transcription_output(
            &corrected_result,
            &settings.app_language,
            &settings.custom_filler_words,
        );

        let et = std::time::Instant::now();
        let translation_note = if settings.translate_to_english {
            " (translated)"
        } else {
            ""
        };
        info!(
            "Transcription completed in {}ms{}",
            (et - st).as_millis(),
            translation_note
        );

        let final_result = filtered_result;

        if final_result.is_empty() {
            info!("Transcription result is empty");
        } else {
            // Length only — never the transcript body: it can be hundreds of KB and is the
            // user's private speech; the log is not the place for it.
            info!(
                "Transcription result: {} chars",
                final_result.chars().count()
            );
        }

        self.maybe_unload_immediately("transcription");

        Ok((final_result, asr_segments))
    }

    /// Transcribe ONE chunk under a single hold of the inference gate, releasing it on return so a
    /// waiting dictation can get in between chunks (A2). Self-heals the model under the gate (C1)
    /// and applies the C3 compare-and-swap put-back — exactly what a whole-run hold used to do,
    /// just scoped to one chunk. Returns the raw chunk result plus the model id the engine
    /// represented, which the caller uses to PIN the run: `pinned_model_id` is that id from the
    /// run's first chunk, and if a model switch lands between chunks the engine reloaded here won't
    /// match it — we abort with an honest error rather than splice two models into one transcript.
    fn transcribe_chunk_gated(
        &self,
        chunk: &[f32],
        validated_language: &str,
        settings: &crate::settings::AppSettings,
        pinned_model_id: Option<&str>,
    ) -> Result<(transcribe_rs::TranscriptionResult, Option<String>)> {
        // One inference at a time. Also keeps `is_model_loaded()` honest while the engine is
        // checked out of its slot below — see `inference_gate`.
        let _inference = self
            .inference_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Keep the idle timer fresh under each chunk's gate hold so the watcher can't evict the
        // engine in the brief gate-free window between chunks (C1 point 2): chunks are ~300 s, so a
        // shorter unload timeout would otherwise let an eviction-then-reload slip in between them —
        // correct but wasteful.
        self.touch_activity();

        // Self-heal at the point of use, under the gate (C1): if the model was evicted since the
        // previous chunk (or a caller's ensure_model_ready() and here), reload it NOW. Holding the
        // gate makes this atomic with the inference — the idle watcher skips eviction while the
        // gate is held (see `should_evict`). Bind the emptiness check in its own scope so the
        // engine lock is released before load_model reacquires it.
        let needs_load = { self.lock_engine().is_none() };
        if needs_load {
            let model_id = get_settings(&self.app_handle).selected_model;
            self.load_model(&model_id).map_err(|e| {
                anyhow::anyhow!("Model is not loaded for transcription (reload failed): {e}")
            })?;
        }

        let mut engine_guard = self.lock_engine();
        // Take the engine out so we own it during transcription. If it panics, we don't put it
        // back (effectively unloading it) instead of poisoning the mutex.
        let mut engine = match engine_guard.take() {
            Some(e) => e,
            None => {
                return Err(anyhow::anyhow!(
                    "Model failed to load after auto-load attempt. Please check your model settings."
                ));
            }
        };
        // Which model does the engine we now hold represent? Captured under the engine lock (same
        // engine→current_model order as load/unload) for both the put-back CAS and the run pin.
        let engine_model_id = self.get_current_model();
        drop(engine_guard);

        // Abort — not splice — if a model switch landed between chunks: the engine we just took is
        // a DIFFERENT model than the run started with. Return it to the slot cleanly (it matches
        // the current selection, so the put-back policy keeps it) and fail the run; a retry then
        // picks up uniformly with the new model.
        if !run_model_consistent(pinned_model_id, engine_model_id.as_deref()) {
            let selected = get_settings(&self.app_handle).selected_model;
            let mut engine_guard = self.lock_engine();
            if putback_policy(engine_model_id.as_deref(), &selected) == PutBack::Keep {
                *engine_guard = Some(engine);
            }
            drop(engine_guard);
            return Err(anyhow::anyhow!(
                "Selected model changed mid-transcription (run pinned {:?}, now {:?}); aborting to avoid a spliced transcript",
                pinned_model_id,
                engine_model_id
            ));
        }

        // catch_unwind so an engine panic doesn't poison the mutex and hang the app.
        let transcribe_result = catch_unwind(AssertUnwindSafe(|| {
            run_engine_on_chunk(&mut engine, chunk, validated_language, settings)
        }));

        match transcribe_result {
            Ok(inner_result) => {
                // Compare-and-swap put-back (C3): only return the engine to the slot if the
                // selected model is still the one it represents; if the user switched or deleted it
                // mid-chunk, dropping it (it reloads on demand) avoids transcribing future audio
                // with the WRONG model and clobbering a freshly-loaded engine. Log the error here
                // too: callers surface it as a UI toast, which leaves no trace in the log.
                let selected = get_settings(&self.app_handle).selected_model;
                let mut engine_guard = self.lock_engine();
                match putback_policy(engine_model_id.as_deref(), &selected) {
                    PutBack::Keep => {
                        *engine_guard = Some(engine);
                    }
                    PutBack::Drop => {
                        // Never overwrite the slot: leave it as-is (normally empty, since we took
                        // the engine out) and let the stale engine free here.
                        let slot_empty = engine_guard.is_none();
                        drop(engine_guard);
                        drop(engine);
                        if slot_empty {
                            {
                                let mut current = self.lock_current_model();
                                // Only clear if it still names the stale engine — never stomp a
                                // model id a concurrent load may have just written.
                                if current.as_deref() == engine_model_id.as_deref() {
                                    *current = None;
                                }
                            }
                            let _ = self.app_handle.emit(
                                "model-state-changed",
                                ModelStateEvent {
                                    event_type: "unloaded".to_string(),
                                    model_id: None,
                                    model_name: None,
                                    error: None,
                                },
                            );
                        }
                        info!(
                            "Dropped stale engine ({:?}) after selected model changed to '{}' mid-run",
                            engine_model_id, selected
                        );
                    }
                }
                let result = inner_result.inspect_err(|e| warn!("Transcription failed: {e}"))?;
                Ok((result, engine_model_id))
            }
            Err(panic_payload) => {
                // Engine panicked — do NOT put it back (unknown state); dropping it unloads it.
                let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                error!(
                    "Transcription engine panicked: {}. Model has been unloaded.",
                    panic_msg
                );

                // Clear the model ID so it will be reloaded on next attempt
                {
                    let mut current_model = self.lock_current_model();
                    *current_model = None;
                }

                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "unloaded".to_string(),
                        model_id: None,
                        model_name: None,
                        error: Some(format!("Engine panicked: {}", panic_msg)),
                    },
                );

                Err(anyhow::anyhow!(
                    "Transcription engine panicked: {}. The model has been unloaded and will reload on next attempt.",
                    panic_msg
                ))
            }
        }
    }

    /// Transcribe and return only the cleaned, post-processed flat text (dictation hot path).
    pub fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        Ok(self.transcribe_inner(audio)?.0)
    }

    /// Transcribe and ALSO return per-segment timings for diarization. The flat text is the
    /// same post-processed transcript as `transcribe`; `segments` carry raw ASR text + ms
    /// timing (empty when the engine yields none — caller falls back to one whole-file segment).
    pub fn transcribe_with_segments(&self, audio: Vec<f32>) -> Result<(String, Vec<AsrSegment>)> {
        self.transcribe_inner(audio)
    }
}

/// Run the loaded engine on a single chunk. Free of locks and app state — just the per-engine
/// dispatch — so the gated chunk loop (`transcribe_chunk_gated`) stays readable and the
/// catch_unwind around the engine call is tight. `validated_language` is the already-validated
/// selection; `settings` supplies translate/custom-word options.
fn run_engine_on_chunk(
    engine: &mut LoadedEngine,
    chunk: &[f32],
    validated_language: &str,
    settings: &crate::settings::AppSettings,
) -> Result<transcribe_rs::TranscriptionResult> {
    match engine {
        LoadedEngine::Whisper(whisper_engine) => {
            let whisper_language = if validated_language == "auto" {
                None
            } else {
                let normalized =
                    if validated_language == "zh-Hans" || validated_language == "zh-Hant" {
                        "zh".to_string()
                    } else {
                        validated_language.to_string()
                    };
                Some(normalized)
            };

            let params = WhisperInferenceParams {
                language: whisper_language,
                translate: settings.translate_to_english,
                initial_prompt: if settings.custom_words.is_empty() {
                    None
                } else {
                    Some(settings.custom_words.join(", "))
                },
                ..Default::default()
            };

            whisper_engine
                .transcribe_with(chunk, &params)
                .map_err(|e| anyhow::anyhow!("Whisper transcription failed: {}", e))
        }
        LoadedEngine::Parakeet(parakeet_engine) => {
            let params = ParakeetParams {
                timestamp_granularity: Some(TimestampGranularity::Segment),
                ..Default::default()
            };
            parakeet_engine
                .transcribe_with(chunk, &params)
                .map_err(|e| anyhow::anyhow!("Parakeet transcription failed: {}", e))
        }
        LoadedEngine::Moonshine(moonshine_engine) => moonshine_engine
            .transcribe(chunk, &TranscribeOptions::default())
            .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
        LoadedEngine::MoonshineStreaming(streaming_engine) => streaming_engine
            .transcribe(chunk, &TranscribeOptions::default())
            .map_err(|e| anyhow::anyhow!("Moonshine streaming transcription failed: {}", e)),
        LoadedEngine::SenseVoice(sense_voice_engine) => {
            let language = match validated_language {
                "zh" | "zh-Hans" | "zh-Hant" => Some("zh".to_string()),
                "en" => Some("en".to_string()),
                "ja" => Some("ja".to_string()),
                "ko" => Some("ko".to_string()),
                "yue" => Some("yue".to_string()),
                _ => None,
            };
            let params = SenseVoiceParams {
                language,
                use_itn: Some(true),
            };
            sense_voice_engine
                .transcribe_with(chunk, &params)
                .map_err(|e| anyhow::anyhow!("SenseVoice transcription failed: {}", e))
        }
        LoadedEngine::GigaAM(gigaam_engine) => gigaam_engine
            .transcribe(chunk, &TranscribeOptions::default())
            .map_err(|e| anyhow::anyhow!("GigaAM transcription failed: {}", e)),
        LoadedEngine::Canary(canary_engine) => {
            let lang = if validated_language == "auto" {
                None
            } else {
                Some(validated_language.to_string())
            };
            let options = TranscribeOptions {
                language: lang,
                translate: settings.translate_to_english,
                ..Default::default()
            };
            canary_engine
                .transcribe(chunk, &options)
                .map_err(|e| anyhow::anyhow!("Canary transcription failed: {}", e))
        }
        LoadedEngine::Cohere(cohere_engine) => {
            let lang = if validated_language == "auto" {
                None
            } else if validated_language == "zh-Hans" || validated_language == "zh-Hant" {
                Some("zh".to_string())
            } else {
                Some(validated_language.to_string())
            };
            let options = TranscribeOptions {
                language: lang,
                ..Default::default()
            };
            cohere_engine
                .transcribe(chunk, &options)
                .map_err(|e| anyhow::anyhow!("Cohere transcription failed: {}", e))
        }
    }
}

/// Whether the current chunk's engine may proceed within a pinned run (A2), pure so it is
/// unit-testable without an engine. A long run pins the model id established by its FIRST chunk;
/// if a model switch lands between chunks the engine reloaded by self-heal represents a DIFFERENT
/// model, and continuing would splice two models into one transcript. `pinned == None` means
/// "first chunk" — it always proceeds and establishes the pin. `None` current under a real pin
/// (engine of unknown provenance) can't be proven to match, so it aborts — never splice on a
/// guess.
fn run_model_consistent(pinned: Option<&str>, current: Option<&str>) -> bool {
    match pinned {
        None => true,
        Some(p) => current == Some(p),
    }
}

/// The idle watcher's eviction policy, pure so it is unit-testable without threads or timing.
/// Eviction requires ALL of: a model resident, NO inference run in progress, and the idle
/// limit exceeded. The `inference_in_progress` guard is the C1 fix — a session finalize keeps
/// the model legitimately resident across minutes of PCM read + diarization between
/// `ensure_model_ready()` and its transcribe call, and evicting in that window failed rows in
/// the field. The `resident` guard avoids emitting a spurious "unloaded" event every tick once
/// idle past the limit with nothing loaded.
fn should_evict(idle_ms: u64, limit_ms: u64, resident: bool, inference_in_progress: bool) -> bool {
    resident && !inference_in_progress && idle_ms > limit_ms
}

/// What to do with an engine checked out for a run when the run finishes.
#[derive(Debug, PartialEq, Eq)]
enum PutBack {
    /// The engine still matches the selected model — return it to the slot.
    Keep,
    /// The user switched or deleted the selected model mid-run — discard the stale engine
    /// instead of putting it back; it reloads on demand at the next call.
    Drop,
}

/// The put-back policy (C3), pure so it is unit-testable without an engine or an AppHandle.
/// A run checks the engine out of the slot for the duration of transcription; when it returns,
/// the selected model may have changed under it (a Settings switch that beat the inference gate,
/// or a delete that cleared the selection). Putting a stale engine back would silently transcribe
/// with the WRONG model and could clobber a freshly-loaded one — so keep ONLY on an exact,
/// non-empty match; otherwise drop. `None` (engine of unknown provenance) also drops: reloading
/// on demand is safe, resurrecting a mystery engine is not.
fn putback_policy(engine_model_id: Option<&str>, selected_model_id: &str) -> PutBack {
    match engine_model_id {
        Some(id) if !selected_model_id.is_empty() && id == selected_model_id => PutBack::Keep,
        _ => PutBack::Drop,
    }
}

/// Long audio is transcribed in windows of about this many seconds. Sized so a conformer
/// encoder's self-attention stays a few hundred MB — one 90-min sequence allocates tens of GB.
const CHUNK_TARGET_SECS: usize = 300;
/// How far around each target boundary to search for the quietest cut point.
const CHUNK_SLACK_SECS: usize = 10;
/// A trailing remainder shorter than this is folded into the previous window instead of
/// becoming a tiny final chunk.
const CHUNK_MIN_TAIL_SECS: usize = 30;

/// Split audio into ~[`CHUNK_TARGET_SECS`] windows for transcription, cutting each boundary at
/// the quietest 100 ms hop within ±[`CHUNK_SLACK_SECS`] so words are (almost) never split.
/// Returns one span when the audio already fits. Spans are contiguous and cover the input.
///
/// ponytail: known ceiling — the chunks abut with NO overlap/dedup. On audio with no quiet
/// point near a boundary (uninterrupted speech for the full ±slack window) the cut can land
/// mid-word, dropping or splitting one word at the seam. With 300 s windows and the quiet-point
/// search this is theoretical, not observed, so we do NOT pay for overlap. Upgrade path if it
/// ever bites: transcribe a small overlap on each side and dedup the repeated words at the seam.
fn chunk_spans(audio: &[f32], sample_rate: usize) -> Vec<(usize, usize)> {
    chunk_spans_with(
        audio,
        sample_rate,
        CHUNK_TARGET_SECS,
        CHUNK_SLACK_SECS,
        CHUNK_MIN_TAIL_SECS,
    )
}

/// Generalized [`chunk_spans`]: split into ~`target_secs` windows, each boundary cut at the
/// quietest hop within ±`slack_secs`, folding a trailing remainder shorter than `min_tail_secs`
/// into the last window. Shared with diarization windowing (`managers::diarization`), which
/// reuses this quiet-cut logic with larger windows. Spans are contiguous and cover the input.
pub(crate) fn chunk_spans_with(
    audio: &[f32],
    sample_rate: usize,
    target_secs: usize,
    slack_secs: usize,
    min_tail_secs: usize,
) -> Vec<(usize, usize)> {
    let target = target_secs * sample_rate;
    let slack = slack_secs * sample_rate;
    let min_tail = min_tail_secs * sample_rate;
    let hop = (sample_rate / 10).max(1); // 100 ms energy windows
    let mut spans = Vec::new();
    let mut start = 0usize;
    while audio.len() - start > target + min_tail {
        let ideal = start + target;
        let lo = ideal - slack.min(target / 2);
        let hi = (ideal + slack).min(audio.len() - min_tail);
        let mut cut = ideal;
        let mut best = f32::INFINITY;
        let mut pos = lo;
        while pos + hop <= hi {
            let energy: f32 = audio[pos..pos + hop].iter().map(|s| s * s).sum();
            if energy < best {
                best = energy;
                cut = pos + hop / 2;
            }
            pos += hop;
        }
        spans.push((start, cut));
        start = cut;
    }
    spans.push((start, audio.len()));
    spans
}

/// Convert transcribe-rs segments (seconds) to diarization `AsrSegment`s (milliseconds).
/// `None`/empty → empty Vec, so the caller can fall back to a single whole-file segment.
fn to_asr_segments(segments: &Option<Vec<transcribe_rs::TranscriptionSegment>>) -> Vec<AsrSegment> {
    segments
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| AsrSegment {
            start_ms: (s.start as f64 * 1000.0) as i64,
            end_ms: (s.end as f64 * 1000.0) as i64,
            text: s.text.clone(),
        })
        .collect()
}

/// Apply the user's accelerator preferences to the transcribe-rs global atomics.
/// Called on startup and whenever the user changes the setting.
pub fn apply_accelerator_settings(app: &tauri::AppHandle) {
    use transcribe_rs::accel;

    let settings = get_settings(app);

    let whisper_pref = match settings.whisper_accelerator {
        WhisperAcceleratorSetting::Auto => accel::WhisperAccelerator::Auto,
        WhisperAcceleratorSetting::Cpu => accel::WhisperAccelerator::CpuOnly,
        WhisperAcceleratorSetting::Gpu => accel::WhisperAccelerator::Gpu,
    };
    accel::set_whisper_accelerator(whisper_pref);
    accel::set_whisper_gpu_device(settings.whisper_gpu_device);
    info!(
        "Whisper accelerator set to: {}, gpu_device: {}",
        whisper_pref,
        if settings.whisper_gpu_device == accel::GPU_DEVICE_AUTO {
            "auto".to_string()
        } else {
            settings.whisper_gpu_device.to_string()
        }
    );

    let ort_pref = match settings.ort_accelerator {
        OrtAcceleratorSetting::Auto => accel::OrtAccelerator::Auto,
        OrtAcceleratorSetting::Cpu => accel::OrtAccelerator::CpuOnly,
        OrtAcceleratorSetting::Cuda => accel::OrtAccelerator::Cuda,
        OrtAcceleratorSetting::DirectMl => accel::OrtAccelerator::DirectMl,
        OrtAcceleratorSetting::Rocm => accel::OrtAccelerator::Rocm,
    };
    accel::set_ort_accelerator(ort_pref);
    info!("ORT accelerator set to: {}", ort_pref);
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct GpuDeviceOption {
    pub id: i32,
    pub name: String,
    pub total_vram_mb: usize,
}

static GPU_DEVICES: OnceLock<Vec<GpuDeviceOption>> = OnceLock::new();

fn cached_gpu_devices() -> &'static [GpuDeviceOption] {
    use transcribe_rs::whisper_cpp::gpu::list_gpu_devices;

    GPU_DEVICES.get_or_init(|| {
        // ggml's Vulkan backend uses FMA3 instructions internally.
        // On older CPUs without FMA3 (e.g. Sandy Bridge Xeons) this causes
        // a SIGILL crash that cannot be caught. Skip enumeration entirely
        // on those CPUs — GPU-accelerated whisper won't work there anyway.
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("fma") {
            warn!("CPU lacks FMA3 support — skipping GPU device enumeration");
            return Vec::new();
        }

        list_gpu_devices()
            .into_iter()
            .map(|d| GpuDeviceOption {
                id: d.id,
                name: d.name,
                total_vram_mb: d.total_vram / (1024 * 1024),
            })
            .collect()
    })
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct AvailableAccelerators {
    pub whisper: Vec<String>,
    pub ort: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceOption>,
}

/// Return which accelerators are compiled into this build.
pub fn get_available_accelerators() -> AvailableAccelerators {
    use transcribe_rs::accel::OrtAccelerator;

    let ort_options: Vec<String> = OrtAccelerator::available()
        .into_iter()
        .map(|a| a.to_string())
        .collect();

    let whisper_options = vec!["auto".to_string(), "cpu".to_string(), "gpu".to_string()];

    AvailableAccelerators {
        whisper: whisper_options,
        ort: ort_options,
        gpu_devices: cached_gpu_devices().to_vec(),
    }
}

impl Drop for TranscriptionManager {
    fn drop(&mut self) {
        // Skip shutdown unless this is the very last clone. TranscriptionManager
        // is cloned by initiate_model_load() and the watcher thread — those
        // clones dropping must not kill the watcher. The watcher thread holds
        // its own clone, so engine's strong_count is always >= 2 while the
        // watcher is alive. When it reaches 1, only this instance remains
        // and we can safely shut down.
        if Arc::strong_count(&self.engine) > 1 {
            return;
        }

        // Signal the watcher thread to shutdown
        self.shutdown_signal.store(true, Ordering::Relaxed);

        // Wait for the thread to finish gracefully
        if let Some(handle) = self.watcher_handle.lock().unwrap().take() {
            if let Err(e) = handle.join() {
                warn!("Failed to join idle watcher thread: {:?}", e);
            } else {
                debug!("Idle watcher thread joined successfully");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{chunk_spans, putback_policy, should_evict, PutBack};

    // The C3 compare-and-swap put-back policy, as a pure state machine.
    #[test]
    fn putback_keeps_only_when_engine_matches_the_current_selection() {
        // Selection unchanged during the run → return the engine to the slot.
        assert_eq!(
            putback_policy(Some("parakeet-v3"), "parakeet-v3"),
            PutBack::Keep
        );
        // User switched models mid-run → dropping the stale engine avoids transcribing future
        // audio with the WRONG model (and clobbering the newly-loaded one).
        assert_eq!(
            putback_policy(Some("parakeet-v3"), "whisper-small"),
            PutBack::Drop
        );
        // Selected model deleted/deselected mid-run (empty selection) → drop, never resurrect.
        assert_eq!(putback_policy(Some("parakeet-v3"), ""), PutBack::Drop);
        // Engine of unknown provenance (no captured id) → drop and reload on demand.
        assert_eq!(putback_policy(None, "parakeet-v3"), PutBack::Drop);
        // Degenerate both-empty → still drop (nothing meaningful to keep).
        assert_eq!(putback_policy(None, ""), PutBack::Drop);
    }

    // The A2 per-chunk run-pinning decision, as a pure state machine. The gate is now released
    // between chunks, so a model switch can land mid-run; this is what turns that into an honest
    // abort instead of a spliced two-model transcript.
    #[test]
    fn run_pin_proceeds_on_first_chunk_and_on_match_but_aborts_on_mid_run_switch() {
        use super::run_model_consistent;
        // First chunk (nothing pinned yet) always proceeds and establishes the pin.
        assert!(run_model_consistent(None, Some("parakeet-v3")));
        // Degenerate first chunk with no identifiable engine model — still proceeds (can't pin).
        assert!(run_model_consistent(None, None));
        // Same model across chunks → proceed (the common long-run case, engine kept in the slot).
        assert!(run_model_consistent(
            Some("parakeet-v3"),
            Some("parakeet-v3")
        ));
        // A switch landed between chunks: the reloaded engine is a different model → abort.
        assert!(!run_model_consistent(
            Some("parakeet-v3"),
            Some("whisper-small")
        ));
        // Engine of unknown provenance mid-run can't be proven to match the pin → abort.
        assert!(!run_model_consistent(Some("parakeet-v3"), None));
    }

    // The idle-unloader eviction policy — the C1 point-2 guard, as a pure state machine.
    #[test]
    fn idle_unloader_defers_eviction_while_an_inference_run_holds_the_gate() {
        // Resident model, idle well past the limit, but a run is in progress: MUST NOT evict.
        // This is the C1 race — a session finalize keeps the model resident across minutes of
        // PCM read + diarization between ensure_model_ready() and the actual transcribe call.
        assert!(!should_evict(10_000, 5_000, true, true));
        // Same idle state, no run in progress → evict as before.
        assert!(should_evict(10_000, 5_000, true, false));
        // Nothing resident → never (avoids a spurious "unloaded" event on every tick).
        assert!(!should_evict(10_000, 5_000, false, false));
        // Within the idle limit → keep the model.
        assert!(!should_evict(1_000, 5_000, true, false));
        // At exactly the limit is still within (strict >), so keep.
        assert!(!should_evict(5_000, 5_000, true, false));
    }

    // Tiny sample rate keeps the test vectors small; chunk_spans only does arithmetic on it.
    const SR: usize = 100;
    const TARGET: usize = super::CHUNK_TARGET_SECS * SR; // 30_000 samples

    fn assert_contiguous_cover(spans: &[(usize, usize)], len: usize) {
        assert_eq!(spans.first().unwrap().0, 0);
        assert_eq!(spans.last().unwrap().1, len);
        for w in spans.windows(2) {
            assert_eq!(w[0].1, w[1].0, "spans must be contiguous");
        }
    }

    #[test]
    fn short_audio_is_a_single_span() {
        let audio = vec![0.5f32; TARGET]; // exactly one window's worth
        assert_eq!(chunk_spans(&audio, SR), vec![(0, audio.len())]);
    }

    #[test]
    fn long_audio_splits_into_roughly_target_sized_contiguous_spans() {
        let audio = vec![0.5f32; TARGET * 3 + TARGET / 2];
        let spans = chunk_spans(&audio, SR);
        assert!(spans.len() >= 3, "3.5 windows of audio must split");
        assert_contiguous_cover(&spans, audio.len());
        let slack = super::CHUNK_SLACK_SECS * SR;
        for &(s, e) in &spans[..spans.len() - 1] {
            assert!(e - s <= TARGET + slack, "no chunk may exceed target+slack");
        }
    }

    #[test]
    fn boundary_lands_in_a_quiet_gap_not_mid_speech() {
        // Loud everywhere except a silent pocket 4 s before the 300 s target boundary.
        let mut audio = vec![0.5f32; TARGET * 2];
        let quiet_at = TARGET - 4 * SR;
        for s in &mut audio[quiet_at..quiet_at + SR] {
            *s = 0.0;
        }
        let spans = chunk_spans(&audio, SR);
        let cut = spans[0].1;
        assert!(
            (quiet_at..quiet_at + SR).contains(&cut),
            "cut {cut} must fall inside the quiet pocket {quiet_at}..{}",
            quiet_at + SR
        );
        assert_contiguous_cover(&spans, audio.len());
    }

    #[test]
    fn a_short_tail_folds_into_the_last_chunk() {
        // One window plus 10 s of remainder (< CHUNK_MIN_TAIL_SECS) → still a single span.
        let audio = vec![0.5f32; TARGET + 10 * SR];
        assert_eq!(chunk_spans(&audio, SR), vec![(0, audio.len())]);
    }
}
