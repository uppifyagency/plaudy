//! macOS system-audio (loopback) capture producer.
//!
//! Captures all system output audio via the CoreAudio Process Tap API
//! (macOS 14.4+) and feeds it into the same `run_consumer` pipeline used by the
//! microphone `AudioRecorder`. The only difference from the mic path is the
//! sample source: a CoreAudio IOProc block driven by a private aggregate device
//! that wraps a global process tap, instead of a cpal input stream.
//!
//! All `unsafe` FFI is confined to this file behind the safe `SystemAudioRecorder`
//! API. Gated to Apple Silicon macOS where the objc2-core-audio Process Tap
//! bindings are available.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AllocAnyThread;

use objc2_core_audio::{
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
    AudioBufferList, AudioStreamBasicDescription,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSCopying, NSNumber, NSString, NSUUID};

use crate::audio_toolkit::vad::{self, VoiceActivityDetector};

use super::recorder::{run_consumer, AudioChunk, Cmd};

const NO_ERR: i32 = 0;

/// The IOProc block type the FFI expects (see AudioHardware.rs ~line 1233):
/// `*mut block2::DynBlock<dyn Fn(NonNull<AudioTimeStamp>, NonNull<AudioBufferList>,
/// NonNull<AudioTimeStamp>, NonNull<AudioBufferList>, NonNull<AudioTimeStamp>)>`.
/// We construct a matching closure with `RcBlock::new` and store it to keep it
/// alive for the IOProc's lifetime.
type IoProcBlock = RcBlock<
    dyn Fn(
        NonNull<objc2_core_audio_types::AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<objc2_core_audio_types::AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<objc2_core_audio_types::AudioTimeStamp>,
    ),
>;

pub struct SystemAudioRecorder {
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    vad: Option<Arc<Mutex<Box<dyn vad::VoiceActivityDetector>>>>,
    level_cb: Option<Arc<dyn Fn(Vec<f32>) + Send + Sync + 'static>>,
    chunk_sink: Option<mpsc::Sender<Vec<f32>>>,

    // CoreAudio objects, owned for teardown. 0 == not created.
    tap_id: AudioObjectID,
    aggregate_id: AudioObjectID,
    io_proc_id: AudioDeviceIOProcID,
    stop_flag: Arc<AtomicBool>,
}

impl SystemAudioRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(SystemAudioRecorder {
            cmd_tx: None,
            worker_handle: None,
            vad: None,
            level_cb: None,
            chunk_sink: None,
            tap_id: 0,
            aggregate_id: 0,
            io_proc_id: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
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

    pub fn with_chunk_sink(mut self, sink: mpsc::Sender<Vec<f32>>) -> Self {
        self.chunk_sink = Some(sink);
        self
    }

    pub fn open(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            return Ok(()); // already open
        }

        // 1. Build the global mono tap that captures all system audio.
        let desc = unsafe {
            CATapDescription::initMonoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &NSArray::new(),
            )
        };
        unsafe {
            desc.setName(&NSString::from_str("Plaude System Tap"));
            desc.setPrivate(true);
            // Default behavior is Unmuted: the tap observes audio while the user
            // keeps hearing it. Set it explicitly for clarity.
            desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
        }

        let tap_uid: Retained<NSString> = unsafe { desc.UUID().UUIDString() };

        let mut tap_id: AudioObjectID = 0;
        let status = unsafe {
            AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id as *mut AudioObjectID)
        };
        if status != NO_ERR || tap_id == 0 {
            return Err(os_err("AudioHardwareCreateProcessTap", status));
        }
        self.tap_id = tap_id;

        // 2. Read the tap's real stream format (do NOT hardcode 48000).
        let asbd = match unsafe { read_tap_format(tap_id) } {
            Ok(a) => a,
            Err(e) => {
                self.teardown_coreaudio();
                return Err(e);
            }
        };
        let in_sample_rate = asbd.mSampleRate as u32;
        // The per-callback AudioBuffer reports its own channel count, so the
        // IOProc downmixes from `mNumberChannels`; `asbd.mChannelsPerFrame`
        // (= asbd channel count) is only used here as a sanity floor.
        let _source_channels = asbd.mChannelsPerFrame.max(1);
        if in_sample_rate == 0 {
            self.teardown_coreaudio();
            return Err("Tap reported a sample rate of 0".into());
        }

        // 3. Build a private aggregate device that wraps the tap.
        let aggregate_uid = NSUUID::UUID().UUIDString();
        let dict = build_aggregate_description(&aggregate_uid, &tap_uid);
        // NSDictionary and CFDictionary are toll-free bridged.
        let cf_dict: &CFDictionary = unsafe { &*(Retained::as_ptr(&dict) as *const CFDictionary) };

        let mut aggregate_id: AudioObjectID = 0;
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                cf_dict,
                NonNull::new(&mut aggregate_id as *mut AudioObjectID).unwrap(),
            )
        };
        if status != NO_ERR || aggregate_id == 0 {
            self.teardown_coreaudio();
            return Err(os_err("AudioHardwareCreateAggregateDevice", status));
        }
        self.aggregate_id = aggregate_id;

        // 4. Wire the pipeline channels and spawn the consumer worker.
        let (sample_tx, sample_rx) = mpsc::channel::<AudioChunk>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = stop_flag.clone();

        let vad = self.vad.clone();
        let level_cb = self.level_cb.clone();
        let chunk_sink = self.chunk_sink.clone();

        let worker = std::thread::spawn(move || {
            run_consumer(
                in_sample_rate,
                super::recorder::FrameSink::from_parts(chunk_sink, vad),
                sample_rx,
                cmd_rx,
                level_cb,
                stop_flag,
            );
        });

        // 5. Build the realtime IOProc block. It downmixes into a freshly allocated Vec and
        //    does a non-blocking mpsc send — lock-free, but NOT allocation-free (the Vec and
        //    the channel node allocate on the realtime thread; conventional for this codebase's
        //    cpal path too, and a preallocated ring buffer is the named upgrade if audio ever
        //    glitches under memory pressure).
        let block_stop = self.stop_flag.clone();
        // Latch so EndOfStream is emitted exactly once per stop (the block is `Fn`,
        // so this needs interior mutability rather than a `mut bool`).
        let eos_sent = AtomicBool::new(false);
        let block: IoProcBlock = RcBlock::new(
            move |_in_now: NonNull<objc2_core_audio_types::AudioTimeStamp>,
                  in_input_data: NonNull<AudioBufferList>,
                  _in_input_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
                  _out_output: NonNull<AudioBufferList>,
                  _in_output_time: NonNull<objc2_core_audio_types::AudioTimeStamp>| {
                // Mirror the cpal callback (recorder.rs::build_stream): while
                // run_consumer drains on Cmd::Stop (shared stop_flag = true), send
                // EndOfStream ONCE so the drain breaks immediately, then stay quiet.
                // This is what makes every sample sent before the stop get drained;
                // silently returning here instead dropped the tail and forced the
                // full 2 s drain timeout.
                if block_stop.load(Ordering::Relaxed) {
                    if !eos_sent.swap(true, Ordering::Relaxed) {
                        let _ = sample_tx.send(AudioChunk::EndOfStream);
                    }
                    return;
                }
                eos_sent.store(false, Ordering::Relaxed);

                let list = unsafe { in_input_data.as_ref() };
                if list.mNumberBuffers == 0 {
                    return;
                }
                let buffer = &list.mBuffers[0];
                if buffer.mData.is_null() || buffer.mDataByteSize == 0 {
                    return;
                }

                let sample_count = (buffer.mDataByteSize / 4) as usize;
                let src = buffer.mData as *const f32;
                let in_channels = buffer.mNumberChannels.max(1) as usize;

                // Interleaved f32 validated once at startup by `read_tap_format`; reinterpret
                // the CoreAudio buffer as a slice and share the recorder's mixdown.
                let data = unsafe { std::slice::from_raw_parts(src, sample_count) };
                let mut out: Vec<f32> = Vec::new();
                super::recorder::downmix_interleaved(data, in_channels, &mut out);

                let _ = sample_tx.send(AudioChunk::Samples(out));
            },
        );

        // 6. Register the IOProc on the aggregate device (None queue == CoreAudio
        //    realtime thread).
        let mut io_proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcIDWithBlock(
                NonNull::new(&mut io_proc_id as *mut AudioDeviceIOProcID).unwrap(),
                aggregate_id,
                None,
                RcBlock::as_ptr(&block),
            )
        };
        if status != NO_ERR || io_proc_id.is_none() {
            // Tear down the worker channel + CoreAudio objects.
            self.stop_flag.store(true, Ordering::Relaxed);
            drop(cmd_tx);
            let _ = worker.join();
            self.teardown_coreaudio();
            return Err(os_err("AudioDeviceCreateIOProcIDWithBlock", status));
        }
        self.io_proc_id = io_proc_id;
        // CoreAudio copies (Block_copy) the IOProc block when registering it, so it
        // owns its own reference for the IOProc's lifetime. Drop our RcBlock now:
        // RcBlock is !Send, and keeping it as a field would make SystemAudioRecorder
        // !Send — which breaks storing it in the Send+Sync SessionManager state.
        drop(block);

        // 7. Start IO on the aggregate device.
        let status = unsafe { AudioDeviceStart(aggregate_id, io_proc_id) };
        if status != NO_ERR {
            self.stop_flag.store(true, Ordering::Relaxed);
            drop(cmd_tx);
            let _ = worker.join();
            self.teardown_coreaudio();
            return Err(os_err("AudioDeviceStart", status));
        }

        self.cmd_tx = Some(cmd_tx);
        self.worker_handle = Some(worker);
        Ok(())
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
        // Same handshake as the cpal path: Cmd::Stop sets the shared stop_flag, the IOProc
        // block sees it and injects EndOfStream once, and run_consumer's drain loop breaks on
        // the sentinel (falling back to its drain timeout only if the IOProc has already died).
        Ok(resp_rx.recv_timeout(super::recorder::STOP_REPLY_TIMEOUT)?)
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        self.stop_flag.store(true, Ordering::Relaxed);

        // Stop and tear down CoreAudio before joining so the IOProc stops sending.
        self.teardown_coreaudio();

        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    /// Destroy CoreAudio objects in reverse creation order. Safe to call on any
    /// exit path — each object is only destroyed if it was created (non-zero id).
    /// A leaked private aggregate device is the main hygiene risk, so this always
    /// runs on error paths too.
    fn teardown_coreaudio(&mut self) {
        if self.aggregate_id != 0 {
            if self.io_proc_id.is_some() {
                unsafe {
                    let _ = AudioDeviceStop(self.aggregate_id, self.io_proc_id);
                    let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, self.io_proc_id);
                }
            }
            unsafe {
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
            }
            self.aggregate_id = 0;
        }
        self.io_proc_id = None;

        if self.tap_id != 0 {
            unsafe {
                let _ = AudioHardwareDestroyProcessTap(self.tap_id);
            }
            self.tap_id = 0;
        }
    }
}

impl Drop for SystemAudioRecorder {
    fn drop(&mut self) {
        // Best-effort: never leak a private aggregate device or tap.
        let _ = self.close();
    }
}

/// Read the tap's `AudioStreamBasicDescription` via the global tap-format property.
unsafe fn read_tap_format(
    tap_id: AudioObjectID,
) -> Result<AudioStreamBasicDescription, Box<dyn std::error::Error>> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: kAudioTapPropertyFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut asbd = AudioStreamBasicDescription {
        mSampleRate: 0.0,
        mFormatID: 0,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: 0,
        mBytesPerFrame: 0,
        mChannelsPerFrame: 0,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    let mut data_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;

    let status = AudioObjectGetPropertyData(
        tap_id,
        NonNull::new(&mut address as *mut AudioObjectPropertyAddress).unwrap(),
        0,
        std::ptr::null(),
        NonNull::new(&mut data_size as *mut u32).unwrap(),
        NonNull::new(&mut asbd as *mut AudioStreamBasicDescription as *mut c_void).unwrap(),
    );
    if status != NO_ERR {
        return Err(os_err(
            "AudioObjectGetPropertyData(kAudioTapPropertyFormat)",
            status,
        ));
    }
    // The IOProc reinterprets each buffer as INTERLEAVED 32-bit float PCM (it reads
    // mBuffers[0] and downmixes by mNumberChannels). Validate the whole assumption once
    // here — including that the format is NOT non-interleaved (one buffer per channel,
    // which would silently record only channel 0) — so an unexpected format fails
    // cleanly at startup instead of type-punning bytes into garbage samples downstream.
    if asbd.mFormatID != kAudioFormatLinearPCM
        || (asbd.mFormatFlags & kAudioFormatFlagIsFloat) == 0
        || (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
        || asbd.mBitsPerChannel != 32
    {
        return Err(format!(
            "Unexpected tap format (id={:#x}, flags={:#x}, bits={}); expected interleaved 32-bit float PCM",
            asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel
        )
        .into());
    }
    Ok(asbd)
}

/// Build the aggregate-device description dictionary expected by
/// `AudioHardwareCreateAggregateDevice`:
/// `{ uid: <fresh uuid>, private: true, taps: [ { uid: <tap uid>, drift: true } ] }`.
///
/// Built as a heterogeneous `NSDictionary<NSString, AnyObject>` (toll-free
/// bridged to CFDictionary by the caller). Keys are the literal CoreAudio key
/// strings ("uid"/"private"/"taps"/"drift").
fn build_aggregate_description(
    aggregate_uid: &NSString,
    tap_uid: &NSString,
) -> Retained<objc2_foundation::NSDictionary<NSString, AnyObject>> {
    // Sub-tap entry: { uid: <tap uid>, drift: true }.
    let sub_tap_keys: [&NSString; 2] = [&NSString::from_str("uid"), &NSString::from_str("drift")];
    let sub_tap_uid_val: Retained<AnyObject> = unsafe { Retained::cast_unchecked(tap_uid.copy()) };
    let sub_tap_drift_val: Retained<AnyObject> =
        unsafe { Retained::cast_unchecked(NSNumber::numberWithBool(true)) };
    let sub_tap_values: [Retained<AnyObject>; 2] = [sub_tap_uid_val, sub_tap_drift_val];
    let sub_tap: Retained<objc2_foundation::NSDictionary<NSString, AnyObject>> =
        objc2_foundation::NSDictionary::from_retained_objects(&sub_tap_keys, &sub_tap_values);

    // Tap list: [ sub_tap ].
    let tap_list: Retained<NSArray<AnyObject>> = {
        let entry: Retained<AnyObject> = unsafe { Retained::cast_unchecked(sub_tap) };
        NSArray::from_retained_slice(&[entry])
    };

    // Outer description: { uid, private, taps }.
    let keys: [&NSString; 3] = [
        &NSString::from_str("uid"),
        &NSString::from_str("private"),
        &NSString::from_str("taps"),
    ];
    let uid_val: Retained<AnyObject> = unsafe { Retained::cast_unchecked(aggregate_uid.copy()) };
    let private_val: Retained<AnyObject> =
        unsafe { Retained::cast_unchecked(NSNumber::numberWithBool(true)) };
    let taps_val: Retained<AnyObject> = unsafe { Retained::cast_unchecked(tap_list) };
    let values: [Retained<AnyObject>; 3] = [uid_val, private_val, taps_val];

    objc2_foundation::NSDictionary::from_retained_objects(&keys, &values)
}

fn os_err(call: &str, status: i32) -> Box<dyn std::error::Error> {
    format!("{call} failed with OSStatus {status}").into()
}
