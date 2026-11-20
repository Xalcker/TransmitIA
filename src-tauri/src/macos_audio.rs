//! Audio del sistema en macOS con Core Audio process taps (macOS 14.2+).
//!
//! 1. Un "tap" global estéreo que excluye a la propia app.
//! 2. Un dispositivo agregado privado que contiene ese tap.
//! 3. Un IOProc que recibe el audio (Float32) y lo empuja a un `appsrc`.
//!
//! El permiso que pide macOS es solo "grabar audio del sistema"
//! (NSAudioCaptureUsageDescription), no el de grabación de pantalla. Si se
//! niega, el tap entrega silencio.
//!
//! `AudioHardwareCreateProcessTap` se resuelve con `dlsym` para que la app siga
//! abriendo en macOS anteriores a 14.2 (ahí la fuente simplemente no aparece).

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::AnyThread;
use objc2_core_audio::*;
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

type OSStatus = i32;
type CreateTapFn = unsafe extern "C" fn(*const AnyObject, *mut AudioObjectID) -> OSStatus;
type DestroyTapFn = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

fn sym<T>(name: &CStr) -> Option<T> {
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy(&p) })
}

/// ¿Este macOS tiene process taps?
pub fn available() -> bool {
    AnyClass::get(c"CATapDescription").is_some() && sym::<CreateTapFn>(c"AudioHardwareCreateProcessTap").is_some()
}

struct Ctx {
    src: gst_app::AppSrc,
    non_interleaved: bool,
}

/// Mientras exista, se captura. Se guarda dentro del `appsrc` y se libera con él.
struct Capture {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    destroy_tap: DestroyTapFn,
}

unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.aggregate, self.proc_id);
            AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            (self.destroy_tap)(self.tap);
            drop(Box::from_raw(self.ctx));
        }
        crate::log::write("info", "system audio capture stopped");
    }
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!("{what} failed (OSStatus {status})"))
    }
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Lee una propiedad de tamaño fijo.
unsafe fn get<T: Copy>(obj: AudioObjectID, selector: AudioObjectPropertySelector, qualifier: Option<&[u8]>) -> Result<T> {
    let addr = address(selector);
    // SAFETY: T son tipos C planos (enteros, punteros, AudioStreamBasicDescription).
    let mut out: T = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<T>() as u32;
    let (qsize, qptr) = qualifier.map_or((0, std::ptr::null()), |q| (q.len() as u32, q.as_ptr() as *const c_void));
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            qsize,
            qptr,
            NonNull::from(&mut size),
            NonNull::new_unchecked(&mut out as *mut T as *mut c_void),
        )
    };
    check(st, "AudioObjectGetPropertyData")?;
    Ok(out)
}

/// UID (texto) de un dispositivo o tap.
unsafe fn get_uid(obj: AudioObjectID, selector: AudioObjectPropertySelector) -> Result<Retained<NSString>> {
    let ptr: usize = unsafe { get(obj, selector, None)? };
    let ptr = ptr as *mut NSString;
    unsafe { Retained::from_raw(ptr) }.ok_or_else(|| anyhow!("empty UID"))
}

fn key(k: &CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

fn dict(pairs: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| key(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let vals: Vec<&AnyObject> = pairs.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&key_refs, &vals)
}

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> OSStatus {
    let ctx = unsafe { &*(client as *const Ctx) };
    let abl = unsafe { input.as_ref() };
    let n = abl.mNumberBuffers as usize;
    if n == 0 {
        return 0;
    }
    let bufs: &[AudioBuffer] = unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), n) };
    let data: Vec<u8> = if !ctx.non_interleaved || n == 1 {
        let b = &bufs[0];
        if b.mData.is_null() {
            return 0;
        }
        unsafe { std::slice::from_raw_parts(b.mData as *const u8, b.mDataByteSize as usize) }.to_vec()
    } else {
        // Un búfer por canal → intercalar.
        let frames = bufs.iter().map(|b| b.mDataByteSize as usize / 4).min().unwrap_or(0);
        let chans: Vec<&[f32]> = bufs
            .iter()
            .map(|b| {
                if b.mData.is_null() {
                    &[][..]
                } else {
                    unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames) }
                }
            })
            .collect();
        if chans.iter().any(|c| c.len() < frames) {
            return 0;
        }
        let mut out = Vec::with_capacity(frames * n * 4);
        for i in 0..frames {
            for c in &chans {
                out.extend_from_slice(&c[i].to_le_bytes());
            }
        }
        out
    };
    let _ = ctx.src.push_buffer(gst::Buffer::from_mut_slice(data));
    0
}

/// Crea la fuente GStreamer con el audio del sistema (Float32 intercalado).
pub fn make_source() -> Result<gst::Element> {
    if !available() {
        return Err(anyhow!("system audio requires macOS 14.2 or later"));
    }
    let create_tap: CreateTapFn = sym(c"AudioHardwareCreateProcessTap").unwrap();
    let destroy_tap: DestroyTapFn =
        sym(c"AudioHardwareDestroyProcessTap").ok_or_else(|| anyhow!("AudioHardwareDestroyProcessTap missing"))?;

    unsafe {
        // Excluir a la propia app para no capturarnos a nosotros mismos.
        let pid = std::process::id() as i32;
        let me: AudioObjectID = get(
            kAudioObjectSystemObject as AudioObjectID,
            kAudioHardwarePropertyTranslatePIDToProcessObject,
            Some(&pid.to_ne_bytes()),
        )
        .unwrap_or(0);
        let exclude: Retained<NSArray<NSNumber>> = if me != 0 {
            NSArray::from_retained_slice(&[NSNumber::new_u32(me)])
        } else {
            NSArray::new()
        };
        let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &exclude);
        desc.setPrivate(true);
        desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
        desc.setName(&NSString::from_str("TransmitIA"));

        let mut tap: AudioObjectID = 0;
        check(create_tap(&*desc as *const CATapDescription as *const AnyObject, &mut tap), "create tap")?;
        let tap_uid = desc.UUID().UUIDString();

        let fmt: AudioStreamBasicDescription = match get(tap, kAudioTapPropertyFormat, None) {
            Ok(f) => f,
            Err(e) => {
                destroy_tap(tap);
                return Err(e);
            }
        };
        if fmt.mFormatFlags & kAudioFormatFlagIsFloat == 0 || fmt.mBitsPerChannel != 32 {
            destroy_tap(tap);
            return Err(anyhow!("unexpected tap format (flags {:#x}, {} bits)", fmt.mFormatFlags, fmt.mBitsPerChannel));
        }

        // Dispositivo agregado privado: salida actual como reloj + el tap.
        let out_dev: AudioObjectID =
            get(kAudioObjectSystemObject as AudioObjectID, kAudioHardwarePropertyDefaultOutputDevice, None)?;
        let out_uid = get_uid(out_dev, kAudioDevicePropertyDeviceUID)?;
        let agg_uid = NSString::from_str(&format!("edu.asu.transmitia.tap.{}", std::process::id()));
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);
        let sub = dict(&[(kAudioSubDeviceUIDKey, &out_uid)]);
        let tap_entry = dict(&[(kAudioSubTapUIDKey, &tap_uid), (kAudioSubTapDriftCompensationKey, &yes)]);
        let subs = NSArray::from_retained_slice(&[sub]);
        let taps = NSArray::from_retained_slice(&[tap_entry]);
        let name = NSString::from_str("TransmitIA System Audio");
        let desc_dict = dict(&[
            (kAudioAggregateDeviceNameKey, &name),
            (kAudioAggregateDeviceUIDKey, &agg_uid),
            (kAudioAggregateDeviceMainSubDeviceKey, &out_uid),
            (kAudioAggregateDeviceIsPrivateKey, &yes),
            (kAudioAggregateDeviceIsStackedKey, &no),
            (kAudioAggregateDeviceTapAutoStartKey, &yes),
            (kAudioAggregateDeviceSubDeviceListKey, &subs),
            (kAudioAggregateDeviceTapListKey, &taps),
        ]);
        let cf: &CFDictionary = &*(Retained::as_ptr(&desc_dict) as *const CFDictionary);
        let mut aggregate: AudioObjectID = 0;
        if let Err(e) = check(AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut aggregate)), "create aggregate") {
            destroy_tap(tap);
            return Err(e);
        }

        let channels = fmt.mChannelsPerFrame.max(1) as i32;
        let src = gst_app::AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Time)
            .do_timestamp(true)
            .caps(
                &gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", fmt.mSampleRate as i32)
                    .field("channels", channels)
                    .build(),
            )
            .build();
        let ctx = Box::into_raw(Box::new(Ctx {
            src: src.clone(),
            non_interleaved: fmt.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0,
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        let cleanup = |proc_id: AudioDeviceIOProcID| {
            if proc_id.is_some() {
                AudioDeviceDestroyIOProcID(aggregate, proc_id);
            }
            AudioHardwareDestroyAggregateDevice(aggregate);
            destroy_tap(tap);
            drop(Box::from_raw(ctx));
        };
        if let Err(e) = check(
            AudioDeviceCreateIOProcID(aggregate, Some(io_proc), ctx as *mut c_void, NonNull::from(&mut proc_id)),
            "create IOProc",
        ) {
            cleanup(proc_id);
            return Err(e);
        }
        if let Err(e) = check(AudioDeviceStart(aggregate, proc_id), "start device") {
            cleanup(proc_id);
            return Err(e);
        }
        crate::log::write(
            "info",
            &format!("system audio capture started: {} Hz, {} ch", fmt.mSampleRate, channels),
        );
        let capture = Capture { tap, aggregate, proc_id, ctx, destroy_tap };
        src.set_data("transmitia-capture", capture);
        Ok(src.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captura real: reproduce un sonido del sistema y mide el pico recibido.
    /// `cargo test -- --ignored system_audio_capture --nocapture`
    #[test]
    #[ignore]
    fn system_audio_capture() {
        gst::init().unwrap();
        assert!(available(), "process taps need macOS 14.2+");
        let src = make_source().unwrap();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let p = gst::Pipeline::new();
        p.add_many([&src, sink.upcast_ref()]).unwrap();
        src.link(&sink).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let player = std::process::Command::new("afplay").arg("/System/Library/Sounds/Glass.aiff").spawn().unwrap();
        let start = std::time::Instant::now();
        let (mut peak, mut buffers) = (0f32, 0);
        while start.elapsed() < std::time::Duration::from_secs(3) {
            if let Some(s) = sink.try_pull_sample(gst::ClockTime::from_mseconds(200)) {
                buffers += 1;
                let map = s.buffer().unwrap().map_readable().unwrap();
                for c in map.as_slice().chunks_exact(4) {
                    peak = peak.max(f32::from_le_bytes(c.try_into().unwrap()).abs());
                }
            }
        }
        drop(player);
        p.set_state(gst::State::Null).unwrap();
        println!("buffers={buffers} peak={peak}");
        assert!(buffers > 10, "no audio buffers");
    }
}
