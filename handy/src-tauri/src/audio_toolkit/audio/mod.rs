// Re-export all audio components
mod device;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod output_sensor;
mod recorder;
mod resampler;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod system_audio;
mod utils;
mod visualizer;

pub use device::{list_input_devices, list_output_devices, CpalDeviceInfo};
pub use recorder::{is_microphone_access_denied, is_no_input_device_error, AudioRecorder};
pub use resampler::FrameResampler;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use system_audio::SystemAudioRecorder;
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;

/// Tap-free "is audio playing on the speakers?" sensor for seamless auto-capture. On macOS aarch64
/// it reads the default output device's run state; elsewhere it is a stub that never triggers.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use output_sensor::output_audio_active;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn output_audio_active() -> bool {
    false
}
