// Re-export all audio components
mod device;
mod mic_sensor;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod output_sensor;
mod recorder;
mod resampler;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod system_audio;
mod utils;
mod visualizer;

pub use device::{list_input_devices, list_output_devices, CpalDeviceInfo};
pub use mic_sensor::MicVoiceSensor;
pub use recorder::{is_microphone_access_denied, is_no_input_device_error, AudioRecorder};
pub use resampler::FrameResampler;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use system_audio::SystemAudioRecorder;
pub use utils::{create_wav_writer, read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;

use std::sync::atomic::{AtomicBool, Ordering};

/// True while the app is playing back one of its OWN recordings. The WebView plays replay audio
/// from a separate process, so the sensor's own-PID filter can't recognize it as ours — without
/// this flag, hitting play on a recording auto-triggers a capture of ourselves. The AudioPlayer
/// sets it via `set_playback_active` on play/pause/ended.
static PLAYBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

// ponytail: a single bool assumes one player at a time — correct for the whole app today (you
// play one recording, from one history card). If two AudioPlayers ever play at once, one ending
// would clear the flag while the other still sounds. Upgrade path when that becomes real: make
// this an AtomicI32 refcount (fetch_add(1)/fetch_sub(1), active = count > 0).
pub fn set_playback_active(active: bool) {
    PLAYBACK_ACTIVE.store(active, Ordering::Relaxed);
}

pub fn playback_active() -> bool {
    PLAYBACK_ACTIVE.load(Ordering::Relaxed)
}

/// The trigger decision after masking our own playback: a raw sensor read is only "external
/// audio" when we aren't the ones playing. Pure so the mask is unit-testable off-device.
fn gated_external(sensor: bool, playback: bool) -> bool {
    sensor && !playback
}

/// Tap-free "is some OTHER app playing audio?" sensor for seamless auto-capture. On macOS aarch64
/// it reads per-process CoreAudio output state (own PID excluded); elsewhere it never triggers.
/// Masked while WE are playing back a recording (see `PLAYBACK_ACTIVE`).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn external_output_active() -> bool {
    gated_external(output_sensor::external_output_active(), playback_active())
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn external_output_active() -> bool {
    false
}

#[cfg(test)]
mod playback_gate_tests {
    use super::*;

    #[test]
    fn our_own_playback_is_not_external_audio() {
        // The bug: replay audio (real sensor read = true) must be masked while we're playing.
        assert!(!gated_external(true, true));
    }

    #[test]
    fn another_app_still_triggers_when_we_are_not_playing() {
        assert!(gated_external(true, false));
    }

    #[test]
    fn playback_flag_round_trips() {
        set_playback_active(true);
        assert!(playback_active());
        set_playback_active(false);
        assert!(!playback_active());
    }
}
