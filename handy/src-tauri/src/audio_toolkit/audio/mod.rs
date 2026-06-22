// Re-export all audio components
mod device;
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
