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
///
/// `speaker_label`, when `Some`, is an authoritative speaker name that overrides the
/// diarizer-index → "Speaker N" generation at persist time. A dual-stream session uses it to
/// tag the mic track "Me", distinct from the diarized remote speakers on the system track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker_id: Option<i64>,
    pub speaker_label: Option<String>,
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
                speaker_label: None,
                text: seg.text.clone(),
            }
        })
        .collect()
}

/// Tag every ASR segment with a fixed speaker name (e.g. the mic track of a dual-stream
/// session is all "Me"). `speaker_id` is left `None`: the name is authoritative, not a
/// diarizer index, so it survives the merge and persist steps unchanged.
pub fn label_segments(asr: &[AsrSegment], label: &str) -> Vec<TimedSegment> {
    asr.iter()
        .map(|s| TimedSegment {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            speaker_id: None,
            speaker_label: Some(label.to_string()),
            text: s.text.clone(),
        })
        .collect()
}

/// Merge per-track attributed segments into one chronological "who said what" timeline.
///
/// Stable sort by start time: equal-timestamp segments keep track-then-input order, so the
/// caller's track ordering (mic before system) is a deterministic tie-break. This is how a
/// dual-stream session — "Me" from the microphone plus the diarized remote speakers from the
/// system-audio tap — becomes a single ordered transcript.
pub fn merge_segments(tracks: Vec<Vec<TimedSegment>>) -> Vec<TimedSegment> {
    let mut all: Vec<TimedSegment> = tracks.into_iter().flatten().collect();
    all.sort_by_key(|s| s.start_ms);
    all
}

/// Word tokens of `s`, lowercased, punctuation stripped — the comparison basis for echo detection.
fn word_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `a` and `b` are near-identical speech: ≥70% of the shorter segment's words appear
/// in the other. Robust to minor ASR/punctuation differences between the two captures of one sound.
fn is_echo_text(a: &str, b: &str) -> bool {
    let (wa, wb) = (word_tokens(a), word_tokens(b));
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    use std::collections::HashSet;
    let sb: HashSet<&String> = wb.iter().collect();
    let shared = wa.iter().filter(|w| sb.contains(w)).count();
    let smaller = wa.len().min(wb.len());
    shared * 10 >= smaller * 7
}

/// Remove acoustic-bleed duplicates from a merged dual-stream transcript. When the Mac plays a
/// call through the SPEAKERS (no headphones), the microphone re-captures that sound, so the
/// system audio is duplicated into the mic ("Me") track — one person appears as two speakers.
/// Drop a `mic_label` segment when an overlapping segment from another speaker has near-identical
/// text (that is the echo); genuinely distinct mic speech (you actually talking) is kept.
///
/// ponytail: cheap, no-DSP mitigation. The real fix is acoustic echo cancellation on the mic
/// input (subtract the system reference signal) — the named upgrade path for clean speaker use.
pub fn drop_bleed(segments: Vec<TimedSegment>, mic_label: &str) -> Vec<TimedSegment> {
    /// A mic segment with fewer words than this is never dropped as echo: one-word utterances
    /// ("okay", "yeah", "sì") are how a listener actually backchannels during a call, and the
    /// overlapping remote speech very often contains that word somewhere — treating them as
    /// echo deleted the user's own genuine speech from the transcript.
    const MIN_ECHO_TOKENS: usize = 2;

    let is_mic = |s: &TimedSegment| s.speaker_label.as_deref() == Some(mic_label);
    let others: Vec<&TimedSegment> = segments.iter().filter(|s| !is_mic(s)).collect();
    segments
        .iter()
        .filter(|seg| {
            if !is_mic(seg) {
                return true;
            }
            if word_tokens(&seg.text).len() < MIN_ECHO_TOKENS {
                return true;
            }
            let echo = others.iter().any(|o| {
                // An echo is time-aligned with its source, so require the overlap to cover a
                // meaningful share (≥30%) of the mic segment — a 1 ms graze between adjacent
                // segments is coincidence, not acoustic bleed.
                let dur = (seg.end_ms - seg.start_ms).max(1);
                let ov = overlap_ms(seg.start_ms, seg.end_ms, o.start_ms, o.end_ms);
                ov * 10 >= dur * 3 && is_echo_text(&seg.text, &o.text)
            });
            !echo
        })
        .cloned()
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
        let out = align(
            &[asr(800, 1400, "x")],
            &[turn(0, 1000, 0), turn(1000, 2000, 1)],
        );
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
    fn label_segments_tags_every_segment_with_the_name_and_no_diarizer_id() {
        let out = label_segments(&[asr(0, 500, "ciao"), asr(500, 900, "tutto bene")], "Me");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.speaker_label.as_deref() == Some("Me")));
        assert!(out.iter().all(|s| s.speaker_id.is_none()));
        assert_eq!(out[1].text, "tutto bene");
    }

    #[test]
    fn merge_interleaves_tracks_chronologically() {
        // "Me" (mic) at [0,1000) and [2000,3000); a remote speaker at [1000,2000).
        let me = label_segments(&[asr(0, 1000, "hi"), asr(2000, 3000, "bye")], "Me");
        let them = align(&[asr(1000, 2000, "hello")], &[turn(1000, 2000, 0)]);
        let merged = merge_segments(vec![me, them]);
        let order: Vec<&str> = merged.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(order, vec!["hi", "hello", "bye"]);
        assert_eq!(merged[1].speaker_id, Some(0)); // remote keeps its diarized id
        assert_eq!(merged[0].speaker_label.as_deref(), Some("Me"));
    }

    #[test]
    fn merge_is_stable_so_mic_wins_ties_over_system() {
        // Both tracks have a segment starting at 0; mic is passed first → it sorts first.
        let me = label_segments(&[asr(0, 500, "me-first")], "Me");
        let them = align(&[asr(0, 500, "them")], &[turn(0, 500, 0)]);
        let merged = merge_segments(vec![me, them]);
        assert_eq!(merged[0].text, "me-first");
        assert_eq!(merged[1].text, "them");
    }

    #[test]
    fn merge_of_empty_tracks_is_empty() {
        assert!(merge_segments(vec![vec![], vec![]]).is_empty());
    }

    fn me_seg(start_ms: i64, end_ms: i64, text: &str) -> TimedSegment {
        TimedSegment {
            start_ms,
            end_ms,
            speaker_id: None,
            speaker_label: Some("Me".into()),
            text: text.into(),
        }
    }

    #[test]
    fn drop_bleed_removes_mic_echo_of_overlapping_system_text() {
        // Listening to one person on the speakers: system tap got it cleanly (Speaker 0), the mic
        // re-captured the same words (labelled "Me"). The "Me" echo must be dropped.
        let merged = merge_segments(vec![
            vec![me_seg(0, 1000, "great work")],
            align(&[asr(0, 1000, "Great work.")], &[turn(0, 1000, 0)]),
        ]);
        let out = drop_bleed(merged, "Me");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker_id, Some(0)); // the system copy survives
    }

    #[test]
    fn drop_bleed_keeps_genuinely_distinct_mic_speech() {
        // A real meeting: you say something different from the remote speaker at the same time.
        let merged = merge_segments(vec![
            vec![me_seg(0, 1000, "i totally agree with that plan")],
            align(
                &[asr(0, 1000, "let us review the budget")],
                &[turn(0, 1000, 0)],
            ),
        ]);
        assert_eq!(drop_bleed(merged, "Me").len(), 2);
    }

    #[test]
    fn drop_bleed_keeps_same_words_at_different_times() {
        // Coincidental identical short words but no time overlap → not an echo.
        let merged = merge_segments(vec![
            vec![me_seg(2000, 3000, "okay")],
            align(&[asr(0, 1000, "okay")], &[turn(0, 1000, 0)]),
        ]);
        assert_eq!(drop_bleed(merged, "Me").len(), 2);
    }

    #[test]
    fn drop_bleed_keeps_single_word_backchannel_during_remote_speech() {
        // You say "okay" WHILE the remote speaker talks and their sentence contains "okay":
        // that is you backchanneling, not acoustic bleed — your speech must survive.
        let merged = merge_segments(vec![
            vec![me_seg(500, 900, "okay")],
            align(
                &[asr(0, 3000, "okay so let us start with the budget")],
                &[turn(0, 3000, 0)],
            ),
        ]);
        let out = drop_bleed(merged, "Me");
        assert_eq!(out.len(), 2, "a one-word backchannel is never echo");
    }

    #[test]
    fn drop_bleed_requires_aligned_overlap_not_a_graze() {
        // Same words, but the segments merely graze by 10 ms out of a 1s mic segment: an
        // echo is time-aligned with its source, so this is coincidence and must be kept.
        let merged = merge_segments(vec![
            vec![me_seg(990, 2000, "great work everyone today")],
            align(
                &[asr(0, 1000, "great work everyone today")],
                &[turn(0, 1000, 0)],
            ),
        ]);
        assert_eq!(drop_bleed(merged, "Me").len(), 2);
    }

    #[test]
    fn echo_text_threshold_is_seventy_percent_of_the_shorter_side() {
        // Exactly 7 of the shorter side's 10 words shared → echo; 6 of 10 → not.
        let long = "one two three four five six seven eight nine ten";
        assert!(is_echo_text(
            long,
            "one two three four five six seven x y z"
        ));
        assert!(!is_echo_text(long, "one two three four five six a b c d"));
    }

    #[test]
    fn echo_text_ignores_case_and_punctuation_and_rejects_empty() {
        assert!(is_echo_text("Great, WORK!", "great work"));
        assert!(!is_echo_text("", "anything"));
        assert!(!is_echo_text("...", "anything")); // punctuation-only tokenizes to nothing
    }

    #[test]
    fn each_segment_keeps_its_own_timing_text_and_speaker() {
        let out = align(
            &[asr(0, 1000, "a"), asr(1000, 2000, "b")],
            &[turn(0, 1000, 0), turn(1000, 2000, 1)],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            (
                out[0].start_ms,
                out[0].end_ms,
                out[0].speaker_id,
                out[0].text.as_str()
            ),
            (0, 1000, Some(0), "a")
        );
        assert_eq!(
            (
                out[1].start_ms,
                out[1].end_ms,
                out[1].speaker_id,
                out[1].text.as_str()
            ),
            (1000, 2000, Some(1), "b")
        );
    }
}
