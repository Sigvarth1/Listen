//! macOS: a Core Audio process tap (macOS 14.2+) on Listen's own mpv processes, read through a
//! private aggregate device. Only Listen's sound is captured (never the microphone or other apps),
//! post-mix, as one stream (deck 0, like the Windows loopback).
//!
//! The first tap asks for the "System Audio Recording" permission; without it the tap delivers
//! silence. The tap API is looked up at run time so Listen still starts on older macOS.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{Message, msg_send};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};
use tokio::sync::mpsc;

type OSStatus = i32;
type AudioObjectId = u32;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const PID_TO_PROCESS: u32 = fourcc(b"id2p");
const TAP_FORMAT: u32 = fourcc(b"tfmt");
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const DEVICE_UID: u32 = fourcc(b"uid ");
const FORMAT_FLAG_FLOAT: u32 = 1;
const FORMAT_FLAG_NON_INTERLEAVED: u32 = 1 << 5;

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct StreamFormat {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type IoProc = extern "C" fn(
    AudioObjectId,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
    fn AudioHardwareCreateAggregateDevice(description: *const c_void, device: *mut AudioObjectId)
    -> OSStatus;
    fn AudioHardwareDestroyAggregateDevice(device: AudioObjectId) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectId,
        proc_: IoProc,
        client: *mut c_void,
        id: *mut *mut c_void,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectId, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectId, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectId, id: *mut c_void) -> OSStatus;
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
/// `RTLD_DEFAULT` on macOS.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

type CreateTap = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectId) -> OSStatus;
type DestroyTap = unsafe extern "C" fn(AudioObjectId) -> OSStatus;

/// The tap functions (macOS 14.2+), `None` on older systems.
fn tap_api() -> Option<(CreateTap, DestroyTap)> {
    let get = |name: &CStr| {
        // SAFETY: looking up a symbol in the already loaded CoreAudio framework.
        let ptr = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
        (!ptr.is_null()).then_some(ptr)
    };
    let create = get(c"AudioHardwareCreateProcessTap")?;
    let destroy = get(c"AudioHardwareDestroyProcessTap")?;
    // SAFETY: the symbols have exactly these C signatures.
    Some(unsafe {
        (
            std::mem::transmute::<*mut c_void, CreateTap>(create),
            std::mem::transmute::<*mut c_void, DestroyTap>(destroy),
        )
    })
}

fn get_property<T: Copy>(
    object: AudioObjectId,
    selector: u32,
    qualifier: Option<&u32>,
    out: &mut T,
) -> OSStatus {
    let address = PropertyAddress { selector, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN };
    let mut size = std::mem::size_of::<T>() as u32;
    let (qsize, qdata) = match qualifier {
        Some(q) => (4, q as *const u32 as *const c_void),
        None => (0, std::ptr::null()),
    };
    // SAFETY: `out` has room for `size` bytes of a plain C value.
    unsafe {
        AudioObjectGetPropertyData(object, &address, qsize, qdata, &mut size, out as *mut T as *mut c_void)
    }
}

/// Core Audio's object for the process `pid` (0: it has no audio client yet).
fn process_object(pid: u32) -> AudioObjectId {
    let mut object: AudioObjectId = 0;
    if get_property(SYSTEM_OBJECT, PID_TO_PROCESS, Some(&pid), &mut object) != 0 {
        return 0;
    }
    object
}

/// The default output device's UID (the aggregate device's clock).
fn default_output_uid() -> Option<Retained<NSString>> {
    let mut device: AudioObjectId = 0;
    if get_property(SYSTEM_OBJECT, DEFAULT_OUTPUT, None, &mut device) != 0 || device == 0 {
        return None;
    }
    let mut uid: *mut NSString = std::ptr::null_mut();
    if get_property(device, DEVICE_UID, None, &mut uid) != 0 {
        return None;
    }
    // SAFETY: the property hands over a +1 CFString, toll-free bridged to NSString.
    unsafe { Retained::from_raw(uid) }
}

fn any<T: Message>(object: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an `AnyObject`.
    unsafe { Retained::cast_unchecked(object) }
}

fn dict(pairs: &[(&str, Retained<AnyObject>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<AnyObject>> = pairs.iter().map(|(_, v)| v.clone()).collect();
    NSDictionary::from_retained_objects(&key_refs, &values)
}

/// Where the IO callback sends its samples.
struct Sink {
    tx: mpsc::Sender<(usize, Vec<f32>)>,
    format: StreamFormat,
    resampler: Mutex<crate::convert::Resampler>,
    broken: AtomicBool,
}

/// Runs on Core Audio's real-time thread: interleaves, resamples to 48 kHz and hands the samples on.
extern "C" fn io_proc(
    _device: AudioObjectId,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    _output: *mut AudioBufferList,
    _output_time: *const c_void,
    client: *mut c_void,
) -> OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    // SAFETY: `client` is the `Sink` kept alive by `Tap` until the IO proc is destroyed.
    let sink = unsafe { &*(client as *const Sink) };
    // SAFETY: Core Audio passes a valid buffer list for the duration of the call.
    let list = unsafe { &*input };
    let buffers = unsafe { std::slice::from_raw_parts(list.buffers.as_ptr(), list.count as usize) };
    let samples = |b: &AudioBuffer| -> &[f32] {
        if b.data.is_null() {
            return &[];
        }
        // SAFETY: float32 samples, `bytes` long (the tap format is checked to be float).
        unsafe { std::slice::from_raw_parts(b.data as *const f32, b.bytes as usize / 4) }
    };
    let non_interleaved = sink.format.format_flags & FORMAT_FLAG_NON_INTERLEAVED != 0;
    let (data, channels): (std::borrow::Cow<'_, [f32]>, usize) = if non_interleaved && buffers.len() >= 2 {
        let (l, r) = (samples(&buffers[0]), samples(&buffers[1]));
        (l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect::<Vec<f32>>().into(), 2)
    } else if let Some(b) = buffers.first() {
        (samples(b).into(), b.channels.max(1) as usize)
    } else {
        return 0;
    };
    let Ok(mut resampler) = sink.resampler.try_lock() else { return 0 };
    let out = resampler.push(&data, channels);
    if !out.is_empty() && sink.tx.try_send((0, out)).is_err() && sink.tx.is_closed() {
        sink.broken.store(true, Ordering::Relaxed);
    }
    0
}

/// A running tap; dropping it stops and removes everything.
struct Tap {
    destroy_tap: DestroyTap,
    tap: AudioObjectId,
    device: AudioObjectId,
    proc_id: *mut c_void,
    sink: *mut Sink,
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: tearing down in reverse order what `Tap::start` created.
        unsafe {
            if !self.proc_id.is_null() {
                AudioDeviceStop(self.device, self.proc_id);
                AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            }
            if self.device != 0 {
                AudioHardwareDestroyAggregateDevice(self.device);
            }
            if self.tap != 0 {
                (self.destroy_tap)(self.tap);
            }
            if !self.sink.is_null() {
                drop(Box::from_raw(self.sink));
            }
        }
    }
}

impl Tap {
    /// Taps the processes `objects` (Core Audio process objects) and starts reading.
    fn start(objects: &[AudioObjectId], tx: &mpsc::Sender<(usize, Vec<f32>)>) -> Result<Tap, String> {
        let (create_tap, destroy_tap) = tap_api().ok_or("needs macOS 14.2 or newer")?;
        let class = AnyClass::get(c"CATapDescription").ok_or("needs macOS 14.2 or newer")?;
        let ids: Vec<Retained<NSNumber>> = objects.iter().map(|o| NSNumber::new_u32(*o)).collect();
        let ids = NSArray::from_retained_slice(&ids);
        // SAFETY: documented CATapDescription initializer and properties.
        let description: Retained<AnyObject> = unsafe {
            let allocated = msg_send![class, alloc];
            let description: Option<Retained<AnyObject>> =
                msg_send![allocated, initStereoMixdownOfProcesses: &*ids];
            let description = description.ok_or("could not describe the tap")?;
            // Private: only Listen sees the tap. The setter's name differs between SDK versions.
            let responds =
                |sel: objc2::runtime::Sel| -> bool { msg_send![&*description, respondsToSelector: sel] };
            if responds(objc2::sel!(setPrivate:)) {
                let _: () = msg_send![&*description, setPrivate: Bool::YES];
            } else if responds(objc2::sel!(setPrivateTap:)) {
                let _: () = msg_send![&*description, setPrivateTap: Bool::YES];
            }
            if responds(objc2::sel!(setName:)) {
                let _: () = msg_send![&*description, setName: &*NSString::from_str("Listen visualizer")];
            }
            description
        };
        // SAFETY: `UUID` is a property of CATapDescription.
        let uuid: Retained<NSUUID> = unsafe { msg_send![&*description, UUID] };

        let mut tap =
            Tap { destroy_tap, tap: 0, device: 0, proc_id: std::ptr::null_mut(), sink: std::ptr::null_mut() };
        // SAFETY: a valid description; the out pointer is ours.
        let status = unsafe { create_tap(Retained::as_ptr(&description) as *mut AnyObject, &mut tap.tap) };
        if status != 0 || tap.tap == 0 {
            return Err(format!("could not create the audio tap ({status})"));
        }
        let mut format = StreamFormat::default();
        if get_property(tap.tap, TAP_FORMAT, None, &mut format) != 0 {
            return Err("could not read the tap's format".into());
        }
        if format.format_flags & FORMAT_FLAG_FLOAT == 0 || format.bits_per_channel != 32 {
            return Err(format!("unsupported tap format {format:?}"));
        }

        let tap_entry = dict(&[("uid", any(uuid.UUIDString())), ("drift", any(NSNumber::new_bool(true)))]);
        let mut pairs: Vec<(&str, Retained<AnyObject>)> = vec![
            ("name", any(NSString::from_str("Listen visualizer"))),
            ("uid", any(NSString::from_str(&format!("io.github.leks2205.Listen.tap.{}", uuid.UUIDString())))),
            ("private", any(NSNumber::new_bool(true))),
            ("stacked", any(NSNumber::new_bool(false))),
            ("tapautostart", any(NSNumber::new_bool(true))),
            ("taps", any(NSArray::from_retained_slice(&[tap_entry]))),
        ];
        if let Some(output) = default_output_uid() {
            let sub = dict(&[("uid", any(output.clone()))]);
            pairs.push(("master", any(output)));
            pairs.push(("subdevices", any(NSArray::from_retained_slice(&[sub]))));
        }
        let description = dict(&pairs);
        // SAFETY: an NSDictionary is toll-free bridged to the CFDictionary the call expects.
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                Retained::as_ptr(&description) as *const c_void,
                &mut tap.device,
            )
        };
        if status != 0 || tap.device == 0 {
            return Err(format!("could not create the capture device ({status})"));
        }

        let sink = Box::new(Sink {
            tx: tx.clone(),
            format,
            resampler: Mutex::new(crate::convert::Resampler::new(format.sample_rate.round() as u32)),
            broken: AtomicBool::new(false),
        });
        tap.sink = Box::into_raw(sink);
        // SAFETY: `sink` outlives the IO proc (both are released in `drop`, the proc first).
        unsafe {
            let status =
                AudioDeviceCreateIOProcID(tap.device, io_proc, tap.sink as *mut c_void, &mut tap.proc_id);
            if status != 0 || tap.proc_id.is_null() {
                tap.proc_id = std::ptr::null_mut();
                return Err(format!("could not read the capture device ({status})"));
            }
            let status = AudioDeviceStart(tap.device, tap.proc_id);
            if status != 0 {
                return Err(format!("could not start the capture ({status})"));
            }
        }
        Ok(tap)
    }

    fn broken(&self) -> bool {
        // SAFETY: `sink` is valid while the tap lives.
        unsafe { (*self.sink).broken.load(Ordering::Relaxed) }
    }
}

/// The capture thread: (re)builds the tap whenever the set of mpv processes with audio changes.
fn run(
    tx: mpsc::Sender<(usize, Vec<f32>)>,
    stop: std::sync::Arc<AtomicBool>,
    first: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    if tap_api().is_none() || AnyClass::get(c"CATapDescription").is_none() {
        let _ = first.send(Err("the visualizer needs macOS 14.2 or newer".into()));
        return;
    }
    let mut first = Some(first);
    let mut current: Option<(Vec<AudioObjectId>, Tap)> = None;
    while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
        let mut objects: Vec<AudioObjectId> = listen_core::platform::tracked_children()
            .into_iter()
            .map(process_object)
            .filter(|o| *o != 0)
            .collect();
        objects.sort_unstable();
        let same = current.as_ref().is_some_and(|(o, tap)| *o == objects && !tap.broken());
        if !same {
            current = None; // the old tap goes first
            if !objects.is_empty() {
                match Tap::start(&objects, &tx) {
                    Ok(tap) => {
                        tracing::info!("visualizer: tapping {} audio process(es)", objects.len());
                        current = Some((objects, tap));
                    }
                    Err(e) => {
                        tracing::info!("visualizer: no process tap: {e}");
                        if let Some(f) = first.take() {
                            let _ = f.send(Err(e));
                            return;
                        }
                    }
                }
            }
        }
        // Nothing to tap yet (mpv opens its audio output with the first song): capture is on anyway.
        if let Some(f) = first.take() {
            let _ = f.send(Ok(()));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub async fn capture(tx: mpsc::Sender<(usize, Vec<f32>)>) -> Result<(), String> {
    struct Stop(std::sync::Arc<AtomicBool>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _guard = Stop(stop.clone());
    let (first, first_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("process-tap".into())
        .spawn(move || run(tx, stop, first))
        .map_err(|e| e.to_string())?;
    // Resolves once with the first attempt's outcome; then runs until dropped.
    first_rx.await.map_err(|_| "audio capture stopped".to_string())??;
    std::future::pending::<()>().await;
    Ok(())
}
