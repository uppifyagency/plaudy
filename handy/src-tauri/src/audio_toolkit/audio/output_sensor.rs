//! Tap-free "is some OTHER app playing audio?" sensor for seamless auto-capture.
//!
//! Enumerates CoreAudio *process objects* (`kAudioHardwarePropertyProcessObjectList`, macOS 14.4+,
//! same floor as our process tap) and asks each one `kAudioProcessPropertyIsRunningOutput`,
//! excluding our own PID. Plain property reads: no tap, no permission, no macOS recording
//! indicator.
//!
//! History: v1 read the default output device's `kAudioDevicePropertyDeviceIsRunningSomewhere`,
//! which is device-level and cannot say *who* is playing — once our own tap had ever been opened,
//! the device read as perpetually "running" and auto-capture false-triggered in-app (validated
//! live: 17/17 idle starts were empty). Per-process attribution fixes that root cause: our PID is
//! filtered out, so our tap can never wake ourselves. (Approach spotted in Meetily's dormant
//! `system_detector.rs`, reimplemented here on raw `objc2_core_audio`.)
//!
//! All `unsafe` FFI is confined to this file; the trigger decision itself is pure and unit-tested.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress,
};

const NO_ERR: i32 = 0;
/// `kAudioObjectSystemObject` — the well-known root object id.
const SYSTEM_OBJECT: AudioObjectID = 1;

/// One CoreAudio process object's audio-output state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessAudio {
    pid: i32,
    running_output: bool,
}

/// The pure trigger decision: is any process other than us emitting audio?
fn any_external_output(processes: &[ProcessAudio], own_pid: i32) -> bool {
    processes
        .iter()
        .any(|p| p.running_output && p.pid != own_pid)
}

fn global_addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a single `u32` global/main property off a CoreAudio object. `None` on any OSStatus error.
unsafe fn get_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
    let mut address = global_addr(selector);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = AudioObjectGetPropertyData(
        object,
        NonNull::new(&mut address as *mut AudioObjectPropertyAddress)?,
        0,
        std::ptr::null(),
        NonNull::new(&mut size as *mut u32)?,
        NonNull::new(&mut value as *mut u32 as *mut c_void)?,
    );
    if status == NO_ERR {
        Some(value)
    } else {
        None
    }
}

/// Snapshot every CoreAudio process object's (pid, is-running-output). Empty on any error — the
/// safe default: auto-capture simply never triggers.
fn list_process_output() -> Vec<ProcessAudio> {
    unsafe {
        let mut address = global_addr(kAudioHardwarePropertyProcessObjectList);
        let mut size: u32 = 0;
        let Some(addr) = NonNull::new(&mut address as *mut AudioObjectPropertyAddress) else {
            return Vec::new();
        };
        let Some(size_ptr) = NonNull::new(&mut size as *mut u32) else {
            return Vec::new();
        };
        if AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, addr, 0, std::ptr::null(), size_ptr)
            != NO_ERR
        {
            return Vec::new();
        }
        let count = size as usize / std::mem::size_of::<AudioObjectID>();
        if count == 0 {
            return Vec::new();
        }
        let mut objects: Vec<AudioObjectID> = vec![0; count];
        let Some(data_ptr) = NonNull::new(objects.as_mut_ptr() as *mut c_void) else {
            return Vec::new();
        };
        if AudioObjectGetPropertyData(SYSTEM_OBJECT, addr, 0, std::ptr::null(), size_ptr, data_ptr)
            != NO_ERR
        {
            return Vec::new();
        }
        objects.truncate(size as usize / std::mem::size_of::<AudioObjectID>());

        objects
            .iter()
            .filter_map(|&obj| {
                let pid = get_u32(obj, kAudioProcessPropertyPID)? as i32;
                let running_output =
                    matches!(get_u32(obj, kAudioProcessPropertyIsRunningOutput), Some(v) if v != 0);
                Some(ProcessAudio {
                    pid,
                    running_output,
                })
            })
            .collect()
    }
}

/// True if any app *other than us* is currently emitting audio. Immune to our own tap by
/// construction (per-process attribution + own-PID filter). False on any error.
pub fn external_output_active() -> bool {
    any_external_output(&list_process_output(), std::process::id() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, running_output: bool) -> ProcessAudio {
        ProcessAudio {
            pid,
            running_output,
        }
    }

    const OWN: i32 = 42;

    #[test]
    fn no_processes_means_no_trigger() {
        assert!(!any_external_output(&[], OWN));
    }

    #[test]
    fn our_own_output_never_triggers() {
        // THE bug this sensor replaces: our tap keeping the device "running" woke ourselves up.
        assert!(!any_external_output(&[p(OWN, true)], OWN));
    }

    #[test]
    fn a_silent_bystander_does_not_trigger() {
        assert!(!any_external_output(&[p(OWN, true), p(7, false)], OWN));
    }

    #[test]
    fn another_app_emitting_audio_triggers() {
        assert!(any_external_output(&[p(OWN, true), p(7, true)], OWN));
    }

    // --- Live acceptance (manual: real CoreAudio + speakers) ----------------------------------
    // Run: cargo test --lib output_sensor -- --ignored --nocapture

    /// The regression that shelved auto-capture: with our own tap OPEN on a silent machine, the
    /// old device-level sensor read "running" forever. The per-process sensor must stay false.
    #[test]
    #[ignore = "manual: needs live CoreAudio; machine must be silent"]
    fn live_own_tap_open_does_not_trigger() {
        let mut recorder = crate::audio_toolkit::audio::SystemAudioRecorder::new()
            .expect("create system-audio recorder");
        recorder.open().expect("open process tap");
        recorder.start().expect("start tap");
        std::thread::sleep(std::time::Duration::from_millis(1500));

        let procs = list_process_output();
        eprintln!("process objects with tap open: {procs:?}");
        assert!(
            !external_output_active(),
            "own tap must not read as external audio"
        );

        let _ = recorder.stop();
        let _ = recorder.close();
    }

    /// Positive path: an external process (afplay) emitting audio must trigger the sensor.
    #[test]
    #[ignore = "manual: needs live CoreAudio + audio output device"]
    fn live_external_afplay_triggers() {
        let mut child = std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Submarine.aiff")
            .spawn()
            .expect("spawn afplay");
        std::thread::sleep(std::time::Duration::from_millis(600));

        let active = external_output_active();
        let _ = child.kill();
        let _ = child.wait();
        assert!(active, "afplay playing must read as external audio");
    }
}
