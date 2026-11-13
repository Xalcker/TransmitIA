//! Descubrimiento de dispositivos, encoders y creación de fuentes por sistema operativo.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gst::prelude::*;
use serde::Serialize;

use crate::permissions::{self, Media};

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EncoderInfo {
    pub id: String,
    pub label: String,
    pub hardware: bool,
}

/// Dispositivos detectados; el índice en cada lista forma el id `dev:N`.
#[derive(Default)]
pub struct DeviceCache {
    pub video: Vec<gst::Device>,
    pub audio: Vec<gst::Device>,
}

impl DeviceCache {
    pub fn refresh(&mut self) {
        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(Some("Video/Source"), None);
        monitor.add_filter(Some("Audio/Source"), None);
        if monitor.start().is_err() {
            return;
        }
        let devices = monitor.devices();
        monitor.stop();
        self.video.clear();
        self.audio.clear();
        for d in devices {
            let class = d.device_class();
            if class.contains("Video/Source") {
                self.video.push(d);
            } else if class.contains("Audio/Source") {
                self.audio.push(d);
            }
        }
    }

    pub fn video_sources(&self) -> Vec<SourceInfo> {
        let mut out = vec![SourceInfo {
            id: "test".into(),
            label: "test-pattern".into(),
            kind: "test".into(),
        }];
        for (i, d) in self.video.iter().enumerate() {
            out.push(SourceInfo {
                id: format!("dev:{i}"),
                label: d.display_name().to_string(),
                kind: "device".into(),
            });
        }
        for i in 0..screen_count() {
            out.push(SourceInfo {
                id: format!("screen:{i}"),
                label: format!("{}", i + 1),
                kind: "screen".into(),
            });
        }
        out
    }

    pub fn audio_sources(&self) -> Vec<SourceInfo> {
        let mut out = vec![SourceInfo { id: "test".into(), label: "test-tone".into(), kind: "test".into() }];
        // Audio del sistema: Windows (WASAPI loopback). En macOS requiere
        // ScreenCaptureKit (pendiente); en Linux aparecen los "Monitor of …".
        if cfg!(target_os = "windows") && gst::ElementFactory::find("wasapi2src").is_some() {
            out.push(SourceInfo { id: "system".into(), label: "system-audio".into(), kind: "test".into() });
        }
        for (i, d) in self.audio.iter().enumerate() {
            out.push(SourceInfo {
                id: format!("dev:{i}"),
                label: d.display_name().to_string(),
                kind: "device".into(),
            });
        }
        out
    }

    /// Devuelve la fuente y, si aplica, los caps que deben ir justo después de ella.
    pub fn make_video_source(&self, id: &str) -> Result<(gst::Element, Option<gst::Caps>)> {
        if id == "test" {
            let el = make("videotestsrc")?;
            el.set_property("is-live", true);
            set_prop(&el, "pattern", "smpte");
            return Ok((el, None));
        }
        if let Some(n) = id.strip_prefix("dev:") {
            let i: usize = n.parse()?;
            let dev = self.video.get(i).ok_or_else(|| anyhow!("video device {i} not found"))?;
            permissions::ensure(Media::Camera)?;
            return Ok((dev.create_element(None)?, capture_caps(dev.caps())));
        }
        if let Some(path) = id.strip_prefix("image:") {
            return Ok((make_image_source(path)?, None));
        }
        if let Some(hex) = id.strip_prefix("color:") {
            let rgb = u32::from_str_radix(hex.trim_start_matches('#'), 16)
                .map_err(|_| anyhow!("invalid color {hex}"))?;
            let el = make("videotestsrc")?;
            el.set_property("is-live", true);
            set_prop(&el, "pattern", "solid-color");
            el.set_property("foreground-color", 0xFF00_0000u32 | (rgb & 0x00FF_FFFF));
            // Basta un cuadro pequeño: el compositor lo escala al rectángulo.
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", 16i32)
                .field("height", 16i32)
                .field("framerate", gst::Fraction::new(5, 1))
                .build();
            return Ok((el, Some(caps)));
        }
        if let Some(n) = id.strip_prefix("screen:") {
            let el = make_screen_source(n.parse()?)?;
            let caps = cfg!(target_os = "macos")
                .then(|| gst::Caps::builder("video/x-raw").field("format", "BGRA").build());
            return Ok((el, caps));
        }
        Err(anyhow!("unknown video source {id}"))
    }

    /// Devuelve None si no hay audio seleccionado.
    pub fn make_audio_source(&self, id: &str) -> Result<Option<gst::Element>> {
        match id {
            "none" | "" => Ok(None),
            "system" => {
                let el = make("wasapi2src")?;
                set_prop(&el, "loopback", "true");
                set_prop(&el, "low-latency", "true");
                Ok(Some(el))
            }
            "test" => {
                let el = make("audiotestsrc")?;
                el.set_property("is-live", true);
                set_prop(&el, "wave", "sine");
                set_prop(&el, "volume", "0.1");
                Ok(Some(el))
            }
            _ => {
                let i: usize = id
                    .strip_prefix("dev:")
                    .ok_or_else(|| anyhow!("unknown audio source {id}"))?
                    .parse()?;
                let dev = self.audio.get(i).ok_or_else(|| anyhow!("audio device {i} not found"))?;
                permissions::ensure(Media::Microphone)?;
                Ok(Some(dev.create_element(None)?))
            }
        }
    }
}

/// Imagen fija (PNG/JPEG) como fuente en vivo: filesrc → decodificador → imagefreeze.
fn make_image_source(path: &str) -> Result<gst::Element> {
    let lower = path.to_ascii_lowercase();
    let dec = if lower.ends_with(".png") {
        "pngdec"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "jpegdec"
    } else {
        return Err(anyhow!("unsupported image (use PNG or JPEG): {path}"));
    };
    if !std::path::Path::new(path).is_file() {
        return Err(anyhow!("image not found: {path}"));
    }
    let bin = gst::Bin::new();
    let src = make("filesrc")?;
    src.set_property("location", path);
    let dec = make(dec)?;
    let conv = make("videoconvert")?;
    let freeze = make("imagefreeze")?;
    set_prop(&freeze, "is-live", "true");
    bin.add_many([&src, &dec, &conv, &freeze])?;
    gst::Element::link_many([&src, &dec, &conv, &freeze])?;
    let ghost = gst::GhostPad::with_target(&freeze.static_pad("src").unwrap())?;
    bin.add_pad(&ghost)?;
    Ok(bin.upcast())
}

// ---------- Captura de pantalla por sistema operativo ----------

#[cfg(target_os = "macos")]
fn screen_count() -> usize {
    // avfvideosrc captura pantallas por índice; ofrecemos hasta 2 si el elemento existe.
    if gst::ElementFactory::find("avfvideosrc").is_some() { 2 } else { 0 }
}

#[cfg(target_os = "macos")]
fn make_screen_source(index: u32) -> Result<gst::Element> {
    let el = make("avfvideosrc")?;
    el.set_property("capture-screen", true);
    el.set_property("capture-screen-cursor", true);
    set_prop(&el, "device-index", &index.to_string());
    Ok(el)
}

#[cfg(target_os = "windows")]
fn screen_count() -> usize {
    if gst::ElementFactory::find("d3d11screencapturesrc").is_some() { 2 } else { 0 }
}

#[cfg(target_os = "windows")]
fn make_screen_source(index: u32) -> Result<gst::Element> {
    let el = make("d3d11screencapturesrc")?;
    set_prop(&el, "monitor-index", &index.to_string());
    set_prop(&el, "show-cursor", "true");
    Ok(el)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn screen_count() -> usize {
    if gst::ElementFactory::find("ximagesrc").is_some() { 1 } else { 0 }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn make_screen_source(_index: u32) -> Result<gst::Element> {
    // TODO: pipewiresrc + xdg-desktop-portal para Wayland.
    let el = make("ximagesrc")?;
    set_prop(&el, "use-damage", "false");
    set_prop(&el, "show-pointer", "true");
    Ok(el)
}

/// Modo de captura que se le pide a una cámara/capturadora.
///
/// Si no fijamos el modo, GStreamer negocia el que más se parece al lienzo; en
/// 9:16 eso elige el modo "vertical" 1080×1920 que anuncia la cámara FaceTime,
/// pero la cámara sigue entregando cuadros horizontales y la imagen sale como
/// ruido y bloques verdes. Por eso abrimos siempre el mejor modo horizontal
/// (16:9 de preferencia, hasta 1920 de ancho, en memoria de sistema) y el lienzo hace el encuadre.
/// En macOS además pedimos BGRA para que AVFoundation convierta por hardware.
fn capture_caps(device_caps: Option<gst::Caps>) -> Option<gst::Caps> {
    let mut best: Option<(i32, i32)> = None;
    if let Some(caps) = device_caps {
        for (s, features) in caps.iter_with_features() {
            if s.name() != "video/x-raw" || features.contains("memory:GLMemory") {
                continue;
            }
            let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) else { continue };
            if w < h || w > 1920 {
                continue;
            }
            // Prioridad: modos 16:9 exactos; luego el de mayor área.
            let rank = |w: i32, h: i32| (w * 9 == h * 16, w * h);
            if best.is_none_or(|(bw, bh)| rank(w, h) > rank(bw, bh)) {
                best = Some((w, h));
            }
        }
    }
    let mut b = gst::Caps::builder("video/x-raw");
    if cfg!(target_os = "macos") {
        b = b.field("format", "BGRA");
    } else if best.is_none() {
        return None;
    }
    if let Some((w, h)) = best {
        b = b.field("width", w).field("height", h);
    }
    Some(b.build())
}

// ---------- Encoders ----------

/// (fábrica, etiqueta, ¿hardware?) en orden de preferencia.
const ENCODERS: &[(&str, &str, bool)] = &[
    ("vtenc_h264_hw", "Apple VideoToolbox (H.264)", true),
    ("nvh264enc", "NVIDIA NVENC (H.264)", true),
    ("qsvh264enc", "Intel Quick Sync (H.264)", true),
    ("amfh264enc", "AMD AMF (H.264)", true),
    ("vah264enc", "VA-API (H.264)", true),
    ("vah264lpenc", "VA-API low-power (H.264)", true),
    ("mfh264enc", "Media Foundation (H.264)", true),
    ("x264enc", "x264 (software)", false),
];

pub fn available_encoders() -> Vec<EncoderInfo> {
    ENCODERS
        .iter()
        .filter(|(f, _, _)| gst::ElementFactory::find(f).is_some())
        .map(|(f, l, hw)| EncoderInfo { id: f.to_string(), label: l.to_string(), hardware: *hw })
        .collect()
}

/// Crea y configura el encoder H.264 (CBR, keyframe cada 2 s, sin B-frames).
pub fn make_video_encoder(preferred: Option<&str>, kbps: u32, fps: u32) -> Result<(gst::Element, String)> {
    let available = available_encoders();
    let factory = preferred
        .filter(|p| available.iter().any(|e| e.id == *p))
        .map(str::to_string)
        .or_else(|| available.first().map(|e| e.id.clone()))
        .ok_or_else(|| anyhow!("no H.264 encoder available"))?;

    let enc = make(&factory)?;
    let gop = (fps * 2).to_string();
    let br = kbps.to_string();
    match factory.as_str() {
        "vtenc_h264_hw" => {
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "realtime", "true");
            set_prop(&enc, "allow-frame-reordering", "false");
            set_prop(&enc, "max-keyframe-interval", &gop);
            set_prop(&enc, "max-keyframe-interval-duration", "2000000000");
            set_prop(&enc, "rate-control", "cbr");
        }
        "nvh264enc" => {
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "gop-size", &gop);
            set_prop(&enc, "rc-mode", "cbr");
            set_prop(&enc, "zerolatency", "true");
            set_prop(&enc, "bframes", "0");
        }
        "qsvh264enc" | "amfh264enc" => {
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "gop-size", &gop);
            set_prop(&enc, "rate-control", "cbr");
            set_prop(&enc, "b-frames", "0");
            set_prop(&enc, "usage", "low-latency");
        }
        "vah264enc" | "vah264lpenc" => {
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "key-int-max", &gop);
            set_prop(&enc, "rate-control", "cbr");
            set_prop(&enc, "b-frames", "0");
        }
        "mfh264enc" => {
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "gop-size", &gop);
            set_prop(&enc, "rc-mode", "cbr");
            set_prop(&enc, "bframes", "0");
            set_prop(&enc, "low-latency", "true");
        }
        _ => {
            // x264enc
            set_prop(&enc, "bitrate", &br);
            set_prop(&enc, "key-int-max", &gop);
            set_prop(&enc, "tune", "zerolatency");
            set_prop(&enc, "speed-preset", "veryfast");
            set_prop(&enc, "vbv-buf-capacity", "1000");
        }
    }
    Ok((enc, factory))
}

/// Encoder AAC disponible en este sistema.
pub fn make_audio_encoder(kbps: u32) -> Result<gst::Element> {
    for f in ["fdkaacenc", "avenc_aac", "voaacenc", "faac"] {
        if gst::ElementFactory::find(f).is_some() {
            let enc = make(f)?;
            set_prop(&enc, "bitrate", &(kbps * 1000).to_string());
            return Ok(enc);
        }
    }
    Err(anyhow!("no AAC encoder available"))
}

// ---------- Utilidades ----------

pub fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| anyhow!("missing GStreamer element: {factory}"))
}

/// Asigna una propiedad a partir de texto solo si existe y el valor es válido
/// (los encoders varían entre versiones y plataformas).
pub fn set_prop<O: IsA<gst::glib::Object>>(el: &O, name: &str, value: &str) -> bool {
    let Some(pspec) = el.find_property(name) else { return false };
    if !pspec.flags().contains(gst::glib::ParamFlags::WRITABLE) {
        return false;
    }
    match gst::glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) => {
            el.set_property_from_value(name, &v);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La cámara FaceTime anuncia modos verticales y en GLMemory; debe elegirse 1920×1080 en memoria de sistema.
    #[test]
    fn capture_caps_picks_best_landscape_mode() {
        gst::init().unwrap();
        let dev: gst::Caps = "video/x-raw(memory:GLMemory), width=1920, height=1080, format=UYVY; \
            video/x-raw, width=1552, height=1552, format={ UYVY, NV12, BGRA }; \
            video/x-raw, width=1080, height=1920, format={ UYVY, NV12, BGRA }; \
            video/x-raw, width=1760, height=1328, format={ UYVY, NV12, BGRA }; \
            video/x-raw, width=1280, height=720, format={ UYVY, NV12, BGRA }; \
            video/x-raw, width=1920, height=1080, format={ UYVY, NV12, BGRA }"
            .parse()
            .unwrap();
        let caps = capture_caps(Some(dev)).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 1920);
        assert_eq!(s.get::<i32>("height").unwrap(), 1080);
        assert!(caps.features(0).map_or(true, |f| !f.contains("memory:GLMemory")));
    }
}
