//! Tap-free "is audio coming out of the speakers?" sensor for seamless auto-capture.
//!
//! Reads the default output device's `kAudioDevicePropertyDeviceIsRunningSomewhere` — a plain
//! CoreAudio property read. Unlike a process tap it costs nothing, needs no permission, and does
//! NOT light the macOS recording indicator. The auto-capture supervisor polls this to decide when
//! something worth recording is playing; only *then* does it open the (visible, honest) tap.
//!
//! Validated from a STANDALONE process: the property reads cleanly and flips TRUE only while audio
//! plays. ⚠️ CAVEAT (why auto-capture's system-audio trigger is shelved): from INSIDE this app,
//! once a CoreAudio process tap has ever been opened, the default output device reads as
//! perpetually "running" — so this sensor false-triggers in-app. Reliable only externally; do not
//! use it as an in-app gate without a confirmation step. All `unsafe` FFI is confined to this file.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, AudioObjectGetPropertyData,
    AudioObjectID, AudioObjectPropertyAddress,
};

const NO_ERR: i32 = 0;
/// `kAudioObjectSystemObject` — the well-known root object id.
const SYSTEM_OBJECT: AudioObjectID = 1;
/// `kAudioHardwarePropertyDefaultOutputDevice` = 'dOut'.
const DEFAULT_OUTPUT_DEVICE: u32 = 0x644F_7574;
/// `kAudioDevicePropertyDeviceIsRunningSomewhere` = 'livn'.
const IS_RUNNING_SOMEWHERE: u32 = 0x6C69_766E;

/// Read a single `u32` global/main property off a CoreAudio object. `None` on any OSStatus error.
unsafe fn get_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
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

/// True if the system's default output device is actively running (audio is playing through the
/// speakers/headphones). Returns false on any error or when there is no default output device — in
/// which case auto-capture simply never triggers, which is the safe default.
pub fn output_audio_active() -> bool {
    unsafe {
        let Some(device) = get_u32(SYSTEM_OBJECT, DEFAULT_OUTPUT_DEVICE) else {
            return false;
        };
        if device == 0 {
            return false;
        }
        matches!(get_u32(device as AudioObjectID, IS_RUNNING_SOMEWHERE), Some(v) if v != 0)
    }
}
