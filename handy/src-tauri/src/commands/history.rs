use crate::actions::process_transcription_output;
use crate::managers::{
    diarization::{align, DiarizationManager, SpeakerTurn, TimedSegment},
    history::{EntrySource, HistoryEntry, HistoryManager, PaginatedHistory, PersistedSegment},
    session::Transcriber,
    transcription::TranscriptionManager,
};
use std::sync::Arc;
use tauri::{AppHandle, State};

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    cursor: Option<i64>,
    limit: Option<usize>,
) -> Result<PaginatedHistory, String> {
    history_manager
        .get_history_entries(cursor, limit)
        .map_err(|e| e.to_string())
}

/// Workstation search: literal substring match over transcript + title, newest first.
#[tauri::command]
#[specta::specta]
pub async fn search_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<crate::managers::history::HistoryEntry>, String> {
    history_manager
        .search_history_entries(&query, limit.map(|l| l as usize))
        .map_err(|e| e.to_string())
}

/// Batched list-view summaries (speakers + duration) for a page of entries — one IPC call per
/// History page instead of a full-segment fetch per row.
#[tauri::command]
#[specta::specta]
pub async fn get_session_overviews(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    ids: Vec<i64>,
) -> Result<Vec<crate::managers::history::SessionOverview>, String> {
    history_manager
        .get_session_overviews(&ids)
        .map_err(|e| e.to_string())
}

/// Fase 2: the speaker-attributed segments for a history entry (empty when the entry was not
/// diarized). Drives the timeline view; the flat `transcription_text` remains the canonical text.
#[tauri::command]
#[specta::specta]
pub async fn get_session_segments(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    history_id: i64,
) -> Result<Vec<PersistedSegment>, String> {
    history_manager
        .get_segments(history_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .toggle_saved_status(id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager.delete_entry(id).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn retry_history_entry_transcription(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    id: i64,
) -> Result<(), String> {
    retry_entry_transcription(&app, &history_manager, &transcription_manager, id)
        .await
        .inspect_err(|e| log::warn!("Retry transcription of entry {id} failed: {e}"))
}

/// The slice of the diarizer the retry core needs — a seam (mirroring session.rs's
/// `Transcriber`) so the source-aware retry is unit-testable without ONNX models.
pub(crate) trait SpeakerDiarizer {
    fn is_available(&self) -> bool;
    fn diarize(&self, samples: &[f32]) -> Vec<SpeakerTurn>;
}

impl SpeakerDiarizer for DiarizationManager {
    fn is_available(&self) -> bool {
        DiarizationManager::is_available(self)
    }
    fn diarize(&self, samples: &[f32]) -> Vec<SpeakerTurn> {
        DiarizationManager::diarize(self, samples)
    }
}

/// The source-aware retry core (A3), handle-free and stub-testable. Meeting/System rows are
/// re-diarized from the mixed WAV so a healed row keeps a speaker timeline; Mic/Dictation rows
/// are one known voice and stay flat (the C2 policy: solo mic never pays diarization); a
/// pre-migration `Unknown` row degrades to flat text rather than guessing. C2's windowed
/// diarization inside `DiarizationManager::diarize` makes long retries (the 89-min class) safe
/// automatically.
///
/// CONSCIOUS COMPROMISE — "Me" is unrecoverable here. Finalize attributes the mic track by
/// construction (per-track PCMs), but those PCMs are deleted once finalize succeeds, so a retry
/// only ever sees the MIXED WAV where the tracks are already summed. Diarizing that mix yields
/// honest "Speaker 1/2/…" labels — the user's own voice becomes one of them, never "Me" again.
/// Faking a "Me" from the mix would be a guess; we don't.
fn retranscribe_for_retry(
    tm: &dyn Transcriber,
    diarizer: &dyn SpeakerDiarizer,
    source: EntrySource,
    samples: Vec<f32>,
) -> anyhow::Result<(String, Vec<TimedSegment>)> {
    let wants_diarization = matches!(source, EntrySource::Meeting | EntrySource::System);
    // Graceful degradation: diarization models absent → flat transcription. The caller purges
    // any stale segments either way, so the row stays honest — text, never a contradictory
    // leftover timeline.
    if !(wants_diarization && diarizer.is_available()) {
        let (text, _) = tm.transcribe_with_segments(samples)?;
        return Ok((text, Vec::new()));
    }
    // Diarize before transcription consumes `samples` (same order as finalize's
    // `transcribe_tracks`), then attribute each ASR segment by maximum turn overlap.
    let turns = diarizer.diarize(&samples);
    let (text, asr) = tm.transcribe_with_segments(samples)?;
    Ok((text, align(&asr, &turns)))
}

/// Re-transcribe one history row from its WAV and flip it to `Done`. Shared by the retry
/// command above and the startup pass that heals rows left `failed` by an earlier bug or
/// crash. Errors are strings because they surface as UI toasts — the callers log them so a
/// failure also leaves a trace in the log file.
///
/// M4 single-flight: the status column is the mutex. This atomically CLAIMS the row (flips it
/// to `transcribing`) BEFORE loading any audio. A double-clicked retry, or a manual retry
/// racing the startup heal on the same row, loses the CAS and returns immediately — no second
/// full inference run, no second full-audio Vec in RAM. Both callers (the command and the heal
/// in `lib.rs`) go through this one function, so the manual/heal mutual exclusion holds by
/// construction. The claim also flips the row to `transcribing` for EVERY view (via the
/// emitted `Updated` event), so other panes no longer show a stale `failed` while inference
/// runs. On any post-claim failure the row is returned to `failed` (retryable again); the
/// success outcomes inside `run_retry` flip it to `Done` themselves; a crash mid-run leaves it
/// `transcribing` for the startup `fail_stale_transcribing` pass to heal.
pub async fn retry_entry_transcription(
    app: &AppHandle,
    history_manager: &Arc<HistoryManager>,
    transcription_manager: &Arc<TranscriptionManager>,
    id: i64,
) -> Result<(), String> {
    let entry = match history_manager
        .try_begin_retry(id)
        .map_err(|e| e.to_string())?
    {
        Some(entry) => entry,
        None => {
            // CAS lost: disambiguate for an honest message. Neither branch loads audio.
            return Err(
                match history_manager
                    .get_entry_by_id(id)
                    .map_err(|e| e.to_string())?
                {
                    Some(_) => format!("Retry of entry {id} is already in progress"),
                    None => format!("History entry {id} not found"),
                },
            );
        }
    };

    match run_retry(app, history_manager, transcription_manager, entry).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Return the claimed row to `failed` so it stays retryable — never wedged on
            // `transcribing` by a transient failure.
            if let Err(revert) = history_manager.mark_retry_failed(id) {
                log::warn!("Failed to reset entry {id} to failed after a failed retry: {revert}");
            }
            Err(e)
        }
    }
}

/// The retry work itself, run once the row is claimed (M4). Loads the WAV, re-transcribes
/// (source-aware, A3), replaces the persisted timeline (A3), and flips the row to `Done` (an
/// empty transcript → terminal `Done`, A1). Returning `Err` leaves the caller to revert the
/// row to `failed`.
async fn run_retry(
    app: &AppHandle,
    history_manager: &Arc<HistoryManager>,
    transcription_manager: &Arc<TranscriptionManager>,
    entry: HistoryEntry,
) -> Result<(), String> {
    let id = entry.id;
    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    let samples = crate::audio_toolkit::read_wav_samples(&audio_path)
        .map_err(|e| format!("Failed to load audio: {}", e))?;

    if samples.is_empty() {
        return Err("Recording has no audio samples".to_string());
    }

    transcription_manager.initiate_model_load();

    // Same model location as finalize (`finalize_session`): app-data /models.
    let diarizer = DiarizationManager::new(
        &crate::portable::app_data_dir(app)
            .map_err(|e| e.to_string())?
            .join("models"),
    );
    let source = entry.source;
    let tm = Arc::clone(transcription_manager);
    let (transcription, segments) = tauri::async_runtime::spawn_blocking(move || {
        retranscribe_for_retry(&*tm, &diarizer, source, samples)
    })
    .await
    .map_err(|e| format!("Transcription task panicked: {}", e))?
    .map_err(|e| e.to_string())?;

    if transcription.is_empty() {
        // No speech is a COMPLETED outcome, not a failure. Persist it as a terminal `Done` row
        // with an empty transcript (the UI renders "no speech detected"). Returning Err here was
        // bug A1: the row stayed `failed`+empty, so `retryable_entry_ids` re-selected it and the
        // startup heal re-ran full inference on it at EVERY launch (a silent 89-min row ≈ 7.5 min
        // of wasted decode per boot). Marking it Done means it is never auto-retried again.
        // A3: purge any stale timeline too — segments persisted by an earlier partially-failed
        // finalize must not survive next to an empty transcript.
        history_manager
            .replace_segments(id, &[])
            .map_err(|e| e.to_string())?;
        history_manager
            .update_transcription(
                id,
                String::new(),
                None,
                None,
                crate::managers::history::TranscriptionStatus::Done,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // A3 consistency invariant: the retry REPLACES the persisted speaker timeline — new
    // segments for a diarized retry, a purge for a flat one — atomically and BEFORE the text
    // flips, so old segments can never sit next to (and contradict) the new transcript. If the
    // replace fails we abort with the row still self-consistent (old text + old segments).
    history_manager
        .replace_segments(id, &segments)
        .map_err(|e| e.to_string())?;

    let processed =
        process_transcription_output(app, &transcription, entry.post_process_requested).await;
    history_manager
        .update_transcription(
            id,
            transcription,
            processed.post_processed_text,
            processed.post_process_prompt,
            crate::managers::history::TranscriptionStatus::Done,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::managers::diarization::AsrSegment;
    use anyhow::anyhow;
    use std::sync::Mutex;

    /// Scripted ASR, no model (Stubs for Queries — same style as session.rs's StubTranscriber).
    struct StubTranscriber(Mutex<Option<anyhow::Result<(String, Vec<AsrSegment>)>>>);

    impl StubTranscriber {
        fn returning(text: &str, asr: Vec<AsrSegment>) -> Self {
            Self(Mutex::new(Some(Ok((text.to_string(), asr)))))
        }
        fn failing(msg: &str) -> Self {
            Self(Mutex::new(Some(Err(anyhow!("{msg}")))))
        }
    }

    impl Transcriber for StubTranscriber {
        fn ensure_model_ready(&self) -> bool {
            true
        }
        fn transcribe_with_segments(
            &self,
            _samples: Vec<f32>,
        ) -> anyhow::Result<(String, Vec<AsrSegment>)> {
            self.0
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(anyhow!("stub script exhausted")))
        }
    }

    /// Scripted diarizer; records whether it was consulted so the C2 "solo mic never pays
    /// diarization" policy is observable.
    struct StubDiarizer {
        available: bool,
        turns: Vec<SpeakerTurn>,
        called: Mutex<bool>,
    }

    impl StubDiarizer {
        fn with_turns(turns: Vec<SpeakerTurn>) -> Self {
            Self {
                available: true,
                turns,
                called: Mutex::new(false),
            }
        }
        fn unavailable() -> Self {
            Self {
                available: false,
                turns: Vec::new(),
                called: Mutex::new(false),
            }
        }
        fn was_called(&self) -> bool {
            *self.called.lock().unwrap()
        }
    }

    impl SpeakerDiarizer for StubDiarizer {
        fn is_available(&self) -> bool {
            self.available
        }
        fn diarize(&self, _samples: &[f32]) -> Vec<SpeakerTurn> {
            *self.called.lock().unwrap() = true;
            self.turns.clone()
        }
    }

    fn asr(start_ms: i64, end_ms: i64, text: &str) -> AsrSegment {
        AsrSegment {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }
    fn turn(start_ms: i64, end_ms: i64, speaker: i64) -> SpeakerTurn {
        SpeakerTurn {
            start_ms,
            end_ms,
            speaker,
        }
    }

    #[test]
    fn meeting_retry_rediarizes_and_returns_attributed_segments() {
        let tm = StubTranscriber::returning(
            "hi there",
            vec![asr(0, 900, "hi"), asr(1100, 1900, "there")],
        );
        let diarizer = StubDiarizer::with_turns(vec![turn(0, 1000, 0), turn(1000, 2000, 1)]);

        let (text, segments) =
            retranscribe_for_retry(&tm, &diarizer, EntrySource::Meeting, vec![0.0; 16]).unwrap();

        assert_eq!(text, "hi there");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].speaker_id, Some(0));
        assert_eq!(segments[1].speaker_id, Some(1));
        // Text and timeline come from the SAME ASR pass → they can never contradict.
        assert_eq!(segments[0].text, "hi");
        assert_eq!(segments[1].text, "there");
    }

    #[test]
    fn system_retry_also_diarizes() {
        let tm = StubTranscriber::returning("solo talk", vec![asr(0, 500, "solo talk")]);
        let diarizer = StubDiarizer::with_turns(vec![turn(0, 500, 0)]);

        let (_, segments) =
            retranscribe_for_retry(&tm, &diarizer, EntrySource::System, vec![0.0; 16]).unwrap();

        assert!(diarizer.was_called());
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].speaker_id, Some(0));
    }

    #[test]
    fn mic_dictation_and_unknown_retries_stay_flat_and_never_consult_the_diarizer() {
        for source in [
            EntrySource::Mic,
            EntrySource::Dictation,
            EntrySource::Unknown,
        ] {
            let tm = StubTranscriber::returning("just me", vec![asr(0, 500, "just me")]);
            let diarizer = StubDiarizer::with_turns(vec![turn(0, 500, 0)]);

            let (text, segments) =
                retranscribe_for_retry(&tm, &diarizer, source, vec![0.0; 16]).unwrap();

            assert_eq!(text, "just me");
            assert!(segments.is_empty(), "{source:?} must be flat");
            assert!(
                !diarizer.was_called(),
                "{source:?} must not pay diarization"
            );
        }
    }

    #[test]
    fn meeting_retry_degrades_to_flat_when_diarization_is_unavailable() {
        let tm = StubTranscriber::returning("plain text", vec![asr(0, 500, "plain text")]);
        let diarizer = StubDiarizer::unavailable();

        let (text, segments) =
            retranscribe_for_retry(&tm, &diarizer, EntrySource::Meeting, vec![0.0; 16]).unwrap();

        assert_eq!(text, "plain text");
        assert!(
            segments.is_empty(),
            "no models → honest flat retry, no fake timeline"
        );
        assert!(!diarizer.was_called());
    }

    #[test]
    fn transcription_failure_propagates_without_touching_segments() {
        let tm = StubTranscriber::failing("engine died");
        let diarizer = StubDiarizer::with_turns(vec![turn(0, 500, 0)]);

        let err = retranscribe_for_retry(&tm, &diarizer, EntrySource::Meeting, vec![0.0; 16])
            .unwrap_err();
        assert!(err.to_string().contains("engine died"));
    }
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}
