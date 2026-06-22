#![allow(dead_code)]
// ponytail: this is the PURE domain core of Fase 2 diarization — assign each ASR transcript
// segment to a speaker by maximum temporal overlap with the diarizer's speaker turns
// ("who said what"). No I/O, no engine, no DB, so it is unit-tested in isolation. The
// sherpa-onnx engine, the SQLite `transcription_segments`/`speakers` schema, and the timeline
// UI all hang off this function and are wired in later phases — hence `dead_code` until then.
//
// Acceptance intent (outer loop, exercised once the engine + schema land):
//   Given a recording with two speakers
//   When the session is finalized
//   Then the timeline shows segments labelled with two distinct speakers.

/// A speaker turn from the diarizer: speaker `speaker` was talking during `[start_ms, end_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerTurn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: i64,
}

/// One timed ASR transcript segment, before speaker attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsrSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// An ASR segment after speaker attribution — the shape persisted in `transcription_segments`.
/// `speaker_id` is `None` when no diarizer turn overlaps the segment (graceful fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker_id: Option<i64>,
    pub text: String,
    // ponytail: the `confidence` schema column originates in the ASR pass, not here — threaded
    // in when `transcribe_with_segments` lands (Phase D). Alignment only decides the speaker.
}

/// Overlap of `[a0, a1)` and `[b0, b1)` in ms; `0` when they are disjoint.
fn overlap_ms(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// Assign each ASR segment the speaker whose turn overlaps it most.
///
/// Determinism: on equal overlap the lower `speaker` id wins. A segment that no turn overlaps
/// gets `speaker_id: None` (so an empty or sparse diarization degrades to "unknown speaker"
/// rather than dropping the transcript). Output preserves input order, timing and text.
pub fn align(asr: &[AsrSegment], turns: &[SpeakerTurn]) -> Vec<TimedSegment> {
    asr.iter()
        .map(|seg| {
            let mut best: Option<(i64, i64)> = None; // (overlap, speaker)
            for t in turns {
                let ov = overlap_ms(seg.start_ms, seg.end_ms, t.start_ms, t.end_ms);
                if ov == 0 {
                    continue;
                }
                let take = match best {
                    // keep current best if it has more overlap, or equal overlap with a
                    // lower-or-equal speaker id (deterministic tie-break)
                    Some((bov, bspk)) => ov > bov || (ov == bov && t.speaker < bspk),
                    None => true,
                };
                if take {
                    best = Some((ov, t.speaker));
                }
            }
            TimedSegment {
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
                speaker_id: best.map(|(_, spk)| spk),
                text: seg.text.clone(),
            }
        })
        .collect()
}

/// Local speaker diarizer (sherpa-onnx). Loads the pyannote segmentation + speaker-embedding
/// ONNX models from `<models_dir>/diarization/` and runs an offline pass over a recording's
/// 16 kHz mono samples, producing speaker turns to feed [`align`].
///
/// ponytail: safe-by-default. `diarize` no-ops (returns no turns) when the two model files are
/// absent, so the app behaves exactly as before until the user provides them — and sherpa's
/// onnxruntime is only ever initialized (coexisting with ort's) when diarization actually runs.
/// The sherpa engine is created per call and dropped immediately, keeping that window minimal.
/// Model auto-download via `ModelManager` is the documented upgrade path (see HANDOFF §6/Phase D).
pub struct DiarizationManager {
    seg_model: std::path::PathBuf,
    emb_model: std::path::PathBuf,
}

impl DiarizationManager {
    /// Subfolder of the app-data models dir holding the diarization models, and the two
    /// fixed filenames inside it. These are the single source of truth shared with
    /// `ModelManager::download_diarization_models` so the downloader and the engine can
    /// never disagree on where the files live.
    pub const SUBDIR: &'static str = "diarization";
    pub const SEG_FILE: &'static str = "segmentation.onnx";
    pub const EMB_FILE: &'static str = "embedding.onnx";

    /// `models_dir` is the app-data models directory; diarization models live in its
    /// `diarization/` subfolder as `segmentation.onnx` + `embedding.onnx`.
    pub fn new(models_dir: &std::path::Path) -> Self {
        let dir = models_dir.join(Self::SUBDIR);
        Self {
            seg_model: dir.join(Self::SEG_FILE),
            emb_model: dir.join(Self::EMB_FILE),
        }
    }

    /// True when both model files are present (diarization can run).
    pub fn is_available(&self) -> bool {
        self.seg_model.is_file() && self.emb_model.is_file()
    }

    /// Diarize 16 kHz mono `samples` into speaker turns (ms). Returns an empty Vec when the
    /// models are absent or the engine fails — [`align`] then degrades to "unknown speaker".
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn diarize(&self, samples: &[f32]) -> Vec<SpeakerTurn> {
        use sherpa_onnx::{OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig};

        if !self.is_available() {
            return Vec::new();
        }
        let mut config = OfflineSpeakerDiarizationConfig::default();
        config.segmentation.pyannote.model = Some(self.seg_model.to_string_lossy().into_owned());
        config.embedding.model = Some(self.emb_model.to_string_lossy().into_owned());

        let Some(diarizer) = OfflineSpeakerDiarization::create(&config) else {
            log::warn!("diarization: failed to load models, skipping");
            return Vec::new();
        };
        let Some(result) = diarizer.process(samples) else {
            return Vec::new();
        };
        result
            .sort_by_start_time()
            .into_iter()
            .map(|s| SpeakerTurn {
                start_ms: (s.start as f64 * 1000.0) as i64,
                end_ms: (s.end as f64 * 1000.0) as i64,
                speaker: s.speaker as i64,
            })
            .collect()
    }

    /// Non-macOS-aarch64 stub: diarization engine not built for this target (mirrors how the
    /// CoreAudio system-audio recorder is gated).
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    pub fn diarize(&self, _samples: &[f32]) -> Vec<SpeakerTurn> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn single_speaker_covering_the_segment_is_assigned() {
        let out = align(&[asr(0, 1000, "ciao")], &[turn(0, 2000, 0)]);
        assert_eq!(out[0].speaker_id, Some(0));
        assert_eq!(out[0].text, "ciao");
    }

    #[test]
    fn segment_goes_to_the_speaker_it_overlaps_most() {
        // seg [800,1400): overlaps turn0 [0,1000) by 200ms, turn1 [1000,2000) by 400ms -> spk 1
        let out = align(&[asr(800, 1400, "x")], &[turn(0, 1000, 0), turn(1000, 2000, 1)]);
        assert_eq!(out[0].speaker_id, Some(1));
    }

    #[test]
    fn segment_with_no_overlapping_turn_is_unknown() {
        let out = align(&[asr(5000, 6000, "x")], &[turn(0, 1000, 0)]);
        assert_eq!(out[0].speaker_id, None);
    }

    #[test]
    fn empty_diarization_degrades_to_unknown_but_keeps_the_text() {
        let out = align(&[asr(0, 1000, "x")], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker_id, None);
        assert_eq!(out[0].text, "x");
    }

    #[test]
    fn equal_overlap_breaks_the_tie_to_the_lower_speaker_id() {
        // seg [0,1000): overlaps turn(0..500, spk 2)=500 and turn(500..1000, spk 1)=500 -> spk 1
        let out = align(&[asr(0, 1000, "x")], &[turn(0, 500, 2), turn(500, 1000, 1)]);
        assert_eq!(out[0].speaker_id, Some(1));
    }

    #[test]
    fn each_segment_keeps_its_own_timing_text_and_speaker() {
        let out = align(
            &[asr(0, 1000, "a"), asr(1000, 2000, "b")],
            &[turn(0, 1000, 0), turn(1000, 2000, 1)],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].start_ms, out[0].end_ms, out[0].speaker_id, out[0].text.as_str()),
            (0, 1000, Some(0), "a")
        );
        assert_eq!(
            (out[1].start_ms, out[1].end_ms, out[1].speaker_id, out[1].text.as_str()),
            (1000, 2000, Some(1), "b")
        );
    }
}
