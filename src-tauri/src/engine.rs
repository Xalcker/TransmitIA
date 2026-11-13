//! Motor de video: construye y controla el pipeline de GStreamer.
//!
//! ```text
//! video ─► convert ─► [crop] ─► scale ─► rate ─► caps(lienzo) ─► tee ─┬─► vista previa (JPEG → UI)
//!                                                                    └─► encoder H.264 ─► tee ─┬─► FLV ─► RTMP
//! audio ─► convert ─► resample ─► caps(F32 48k) ─► tee ─┬─► (medidor)                          ├─► TS  ─► SRT
//!                                                       └─► AAC ─► tee ────────────────────────┴─► MKV ─► archivo
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime, Wry};

use crate::media::{self, make, set_prop, DeviceCache};
use crate::outputs::{OutputSpec, OutputStatus, Outputs, StatusFn, Target};
use crate::transcribe::{self, Kind, TranscriptEvent, Transcriber};
use crate::translate::{self, Translator};
use crate::settings::{DestinationKind, Fit, Layer, Rect, Settings, TextStyle};

const PREVIEW_MAX_SIDE: u32 = 640;
const PREVIEW_FPS: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Stopped,
    Preview,
    Live,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub mode: Mode,
    pub fps: f64,
    pub video_kbps: f64,
    pub uptime_sec: u64,
    pub encoder: Option<String>,
    pub streaming: bool,
    pub recording_path: Option<String>,
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    preview_frames: AtomicU64,
    video_bytes: AtomicU64,
    peak_l: AtomicU32,
    peak_r: AtomicU32,
    eos: AtomicBool,
}

struct Running {
    snapshots: Arc<Snapshots>,
    pipeline: gst::Pipeline,
    transcript: Option<Arc<Mutex<TranscriptSink>>>,
    layout: Arc<Mutex<Layout>>,
    outputs: Option<Arc<Outputs>>,
    stop_flag: Arc<AtomicBool>,
    bus_thread: Option<JoinHandle<()>>,
    mode: Mode,
}

pub struct Engine<R: Runtime = Wry> {
    app: AppHandle<R>,
    pub devices: DeviceCache,
    running: Option<Running>,
}

impl<R: Runtime> Engine<R> {
    pub fn new(app: AppHandle<R>) -> Result<Self> {
        gst::init().context("GStreamer init failed")?;
        let mut devices = DeviceCache::default();
        devices.refresh();
        Ok(Self { app, devices, running: None })
    }

    pub fn mode(&self) -> Mode {
        self.running.as_ref().map(|r| r.mode).unwrap_or(Mode::Stopped)
    }

    /// Arranca (o reinicia) en modo vista previa o en vivo.
    pub fn start(&mut self, settings: &Settings, mode: Mode, secrets: Secrets) -> Result<()> {
        self.stop();
        if mode == Mode::Stopped {
            return Ok(());
        }
        let counters = Arc::new(Counters::default());
        let built = self.build(settings, mode, &secrets, &counters)?;

        crate::log::write(
            "info",
            &format!(
                "start {mode:?} {:?} {}x{}@{} scene={} layers={}",
                settings.orientation,
                settings.canvas_size().0,
                settings.canvas_size().1,
                settings.fps(),
                settings.active_scene + 1,
                settings.scenes.iter().map(|s| s.layers.len()).sum::<usize>()
            ),
        );
        built.pipeline.set_state(gst::State::Playing).map_err(|_| {
            let _ = built.pipeline.set_state(gst::State::Null);
            anyhow!("could not start pipeline (device busy or permission denied?)")
        })?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let bus_thread = spawn_bus_thread(
            self.app.clone(),
            built.pipeline.clone(),
            stop_flag.clone(),
            counters.clone(),
            built.levels,
            StatsMeta {
                mode,
                encoder: built.encoder,
                streaming: built.streaming,
                recording_path: built.recording_path,
            },
        );

        self.running = Some(Running {
            snapshots: built.snapshots,
            pipeline: built.pipeline,
            transcript: built.transcript,
            layout: built.layout,
            outputs: built.outputs,
            stop_flag,
            bus_thread: Some(bus_thread),
            mode,
        });
        Ok(())
    }

    /// Cambios en vivo sin reiniciar: escena activa, posición, tamaño,
    /// visibilidad, ajuste y orden de las capas existentes.
    pub fn apply_layout(&self, settings: &Settings) {
        if let Some(run) = &self.running {
            run.layout.lock().apply(settings);
        }
    }

    /// Inyecta texto como si viniera del proveedor (pruebas y autoprueba).
    #[allow(dead_code)]
    pub fn inject_transcript(&self, ev: TranscriptEvent) {
        if let Some(t) = self.running.as_ref().and_then(|r| r.transcript.as_ref()) {
            t.lock().handle(&ev);
        }
    }

    /// Agrega una capa nueva (imagen, color o texto) sin reiniciar: sirve en vivo,
    /// por ejemplo para poner un póster recién generado.
    pub fn add_layer_live(&self, settings: &Settings, layer_id: &str) -> Result<()> {
        let run = self.running.as_ref().ok_or_else(|| anyhow!("not running"))?;
        let layer = settings
            .scenes
            .iter()
            .flat_map(|sc| sc.layers.iter())
            .find(|l| l.id == layer_id)
            .ok_or_else(|| anyhow!("layer not found"))?;
        if run.layout.lock().handles.contains_key(layer_id) {
            run.layout.lock().apply(settings);
            return Ok(());
        }
        let ok = ["image:", "color:", "text:"].iter().any(|p| layer.source.starts_with(p));
        if !ok {
            return Err(anyhow!("only image, color or text layers can be added live"));
        }
        let pipeline = &run.pipeline;
        let mix = pipeline.by_name("mix").ok_or_else(|| anyhow!("compositor not found"))?;
        let canvas = run.layout.lock().canvas;
        let (mut chain, caps) = if layer.source.starts_with("text:") {
            let (els, handle) = make_text_source(layer, canvas)?;
            run.layout.lock().texts.insert(layer.id.clone(), handle);
            (els, None)
        } else {
            let (e, c) = self.devices.make_video_source(&layer.source)?;
            (vec![e], c)
        };
        if let Some(caps) = caps {
            let f = make("capsfilter")?;
            f.set_property("caps", caps);
            chain.push(f);
        }
        let q = make("queue")?;
        let conv = make("videoconvert")?;
        let vbox = make("videobox")?;
        chain.extend([q.clone(), conv, vbox.clone()]);
        pipeline.add_many(&chain)?;
        gst::Element::link_many(&chain)?;
        let lay = run.layout.clone();
        let source_id = layer.source.clone();
        q.static_pad("sink").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) {
                if let Some(st) = c.caps().structure(0) {
                    if let (Ok(sw), Ok(sh)) = (st.get::<i32>("width"), st.get::<i32>("height")) {
                        let mut l = lay.lock();
                        l.src_dims.insert(source_id.clone(), (sw, sh));
                        l.reapply();
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
        let pad = mix.request_pad_simple("sink_%u").ok_or_else(|| anyhow!("compositor pad"))?;
        vbox.static_pad("src").unwrap().link(&pad)?;
        {
            let mut l = run.layout.lock();
            l.handles.insert(layer.id.clone(), LayerHandle { pad, vbox, source: layer.source.clone() });
            l.apply(settings);
        }
        for el in &chain {
            el.sync_state_with_parent()?;
        }
        crate::log::write("info", &format!("layer {} added live ({})", layer.id, layer.source));
        Ok(())
    }

    /// Acceso a las capturas sin retener el candado del motor mientras se espera.
    pub fn snapshots(&self) -> Option<Arc<Snapshots>> {
        self.running.as_ref().map(|r| r.snapshots.clone())
    }

    /// Inyecta una traducción (pruebas).
    #[allow(dead_code)]
    pub fn inject_translation(&self, item_id: &str, text: &str) {
        if let Some(t) = self.running.as_ref().and_then(|r| r.transcript.as_ref()) {
            t.lock().handle_translation(item_id, text);
        }
    }

    /// Detiene el pipeline. En vivo envía EOS primero para cerrar bien la grabación.
    pub fn stop(&mut self) {
        let Some(mut run) = self.running.take() else { return };
        // Cada salida cierra en orden (EOS) mientras el codificador sigue vivo.
        if let Some(outputs) = run.outputs.take() {
            outputs.stop();
        }
        let t0 = Instant::now();
        let _ = run.pipeline.set_state(gst::State::Null);
        crate::log::write("info", &format!("stop {:?} took {} ms", run.mode, t0.elapsed().as_millis()));
        run.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = run.bus_thread.take() {
            let _ = t.join();
        }
    }

    fn build(
        &self,
        s: &Settings,
        mode: Mode,
        secrets: &Secrets,
        counters: &Arc<Counters>,
    ) -> Result<Built> {
        let pipeline = gst::Pipeline::with_name("transmitia");
        let (w, h) = s.canvas_size();
        let fps = s.fps() as i32;

        // ---------- Video: fuentes → capas → compositor → lienzo ----------
        let mix = gst::ElementFactory::make("compositor").name("mix").build()?;
        set_prop(&mix, "background", "black");
        // No esperar a fuentes que tardan en arrancar (p. ej. cámaras).
        set_prop(&mix, "ignore-inactive-pads", "true");
        pipeline.add(&mix)?;

        // Fondo negro en vivo: marca el ritmo del lienzo aunque no haya capas.
        let bg = make("videotestsrc")?;
        bg.set_property("is-live", true);
        set_prop(&bg, "pattern", "black");
        let bg_caps = make("capsfilter")?;
        bg_caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", w as i32)
                .field("height", h as i32)
                .field("framerate", gst::Fraction::new(fps, 1))
                .build(),
        );
        pipeline.add_many([&bg, &bg_caps])?;
        bg.link(&bg_caps)?;
        let bg_pad = mix.request_pad_simple("sink_%u").ok_or_else(|| anyhow!("compositor pad"))?;
        bg_caps.static_pad("src").unwrap().link(&bg_pad)?;
        bg_pad.set_property("zorder", 0u32);

        let layout = Arc::new(Mutex::new(Layout { canvas: (w, h), ..Default::default() }));
        let mut levels: Vec<(String, Arc<Peak>)> = Vec::new();
        let snapshots = Arc::new(Snapshots::default());

        // Cada fuente se abre una sola vez aunque aparezca en varias capas/escenas.
        let mut source_tees: HashMap<String, gst::Element> = HashMap::new();
        for layer in s.scenes.iter().flat_map(|sc| sc.layers.iter()) {
            if source_tees.contains_key(&layer.source) {
                continue;
            }
            let made = if layer.source.starts_with("text:") {
                make_text_source(layer, (w, h)).map(|(els, handle)| {
                    layout.lock().texts.insert(layer.id.clone(), handle);
                    (els, None)
                })
            } else {
                self.devices.make_video_source(&layer.source).map(|(e, c)| (vec![e], c))
            };
            let (src_els, src_caps) = match made {
                Ok(v) => v,
                Err(e) => {
                    // Una fuente rota no debe tumbar todo el lienzo.
                    let msg = format!("{}: {e:#}", layer.source);
                    crate::log::write("error", &msg);
                    let _ = self.app.emit("engine-error", msg);
                    continue;
                }
            };
            let mut chain = src_els;
            if let Some(caps) = src_caps {
                crate::log::write("info", &format!("video source {} caps {caps}", layer.source));
                let f = make("capsfilter")?;
                f.set_property("caps", caps);
                chain.push(f);
            }
            let q = make("queue")?;
            let tee = make("tee")?;
            set_prop(&tee, "allow-not-linked", "true");
            chain.extend([q, tee.clone()]);
            pipeline.add_many(&chain)?;
            gst::Element::link_many(&chain)?;

            // Al conocer el tamaño real de la fuente, recalcular recortes.
            let lay = layout.clone();
            let source_id = layer.source.clone();
            tee.static_pad("sink").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) {
                    if let Some(st) = c.caps().structure(0) {
                        if let (Ok(sw), Ok(sh)) = (st.get::<i32>("width"), st.get::<i32>("height")) {
                            let mut l = lay.lock();
                            l.src_dims.insert(source_id.clone(), (sw, sh));
                            l.reapply();
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
            snapshots.install(&tee.static_pad("sink").unwrap(), layer.source.clone());
            source_tees.insert(layer.source.clone(), tee);
        }

        // Una rama por capa (de todas las escenas); las inactivas quedan con alpha 0.
        for layer in s.scenes.iter().flat_map(|sc| sc.layers.iter()) {
            let Some(tee) = source_tees.get(&layer.source) else { continue };
            let q = leaky_queue()?;
            let conv = make("videoconvert")?;
            // videobox recorta copiando píxeles (videocrop falla con búferes de AVFoundation).
            let vbox = make("videobox")?;
            pipeline.add_many([&q, &conv, &vbox])?;
            gst::Element::link_many([tee, &q, &conv, &vbox])?;
            let pad = mix.request_pad_simple("sink_%u").ok_or_else(|| anyhow!("compositor pad"))?;
            vbox.static_pad("src").unwrap().link(&pad)?;
            layout.lock().handles.insert(
                layer.id.clone(),
                LayerHandle { pad, vbox, source: layer.source.clone() },
            );
        }
        let vcaps = make("capsfilter")?;
        vcaps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", w as i32)
                .field("height", h as i32)
                .field("framerate", gst::Fraction::new(fps, 1))
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        );
        let vtee = make("tee")?;
        set_prop(&vtee, "allow-not-linked", "true");
        pipeline.add_many([&vcaps, &vtee])?;
        gst::Element::link_many([&mix, &vcaps, &vtee])?;

        // "program" = el lienzo compuesto tal como sale al aire.
        snapshots.install(&vcaps.static_pad("src").unwrap(), PROGRAM.into());

        // Contador de cuadros a la salida del lienzo.
        {
            let c = counters.clone();
            vcaps.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                c.frames.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }

        // ---------- Vista previa ----------
        self.add_preview_branch(&pipeline, &vtee, w, h, counters)?;

        // ---------- Audio: entradas → mezclador ----------
        let has_audio = !s.audio_inputs.is_empty();
        let atee = if has_audio { Some(self.add_audio_mixer(&pipeline, s, counters, &layout, &mut levels)?) } else { None };
        layout.lock().apply(s);

        // ---------- Transcripción ----------
        let tsink = Arc::new(Mutex::new(TranscriptSink::default()));
        let transcribing = match (&atee, &secrets.api_key) {
            (Some(atee), Some(key)) if s.transcription.enabled => {
                self.add_transcription(&pipeline, atee, s, key, &tsink)?;
                true
            }
            (atee, key) if s.transcription.enabled => {
                let msg = if atee.is_none() { "no-audio" } else if key.is_none() { "no-api-key" } else { "" };
                let _ = self.app.emit("transcription-status", serde_json::json!({ "state": "error", "message": msg }));
                false
            }
            _ => false,
        };

        let translating = transcribing && s.translation.enabled && secrets.translate_key.is_some();
        if transcribing && s.translation.enabled && secrets.translate_key.is_none() {
            let _ = self.app.emit("translation-status", serde_json::json!({ "state": "error", "message": "no-api-key" }));
        }
        if translating {
            let key = secrets.translate_key.clone().unwrap_or_default();
            if key != OFFLINE_API_KEY {
                let tr = &s.translation;
                let model = if tr.model.trim().is_empty() { translate::default_model(tr.provider).to_string() } else { tr.model.trim().to_string() };
                let cfg = translate::Config {
                    provider: tr.provider,
                    model,
                    api_key: key,
                    target: tr.target.clone(),
                    source: s.transcription.language.clone(),
                };
                let app = self.app.clone();
                let sink = tsink.clone();
                let on_result: translate::ResultFn = Arc::new(move |id, text| {
                    sink.lock().handle_translation(&id, &text);
                    let _ = app.emit("translation", serde_json::json!({ "itemId": id, "text": text }));
                });
                let app = self.app.clone();
                let on_error: translate::ErrorFn = Arc::new(move |msg| {
                    let _ = app.emit("translation-status", serde_json::json!({ "state": "error", "message": msg }));
                });
                tsink.lock().translator = Some(Arc::new(Translator::start(cfg, on_result, on_error)));
            }
        }

        let mut built = Built {
            snapshots: snapshots.clone(),
            pipeline: pipeline.clone(),
            transcript: transcribing.then(|| tsink.clone()),
            layout: layout.clone(),
            levels,
            outputs: None,
            encoder: None,
            streaming: false,
            recording_path: None,
        };
        if mode != Mode::Live {
            if let Some(atee) = &atee {
                // Sin salida: el audio solo alimenta los medidores.
                let q = leaky_queue()?;
                let sink = make("fakesink")?;
                set_prop(&sink, "sync", "false");
                pipeline.add_many([&q, &sink])?;
                gst::Element::link_many([atee, &q, &sink])?;
            }
            return Ok(built);
        }

        // ---------- Salidas (se validan antes de codificar) ----------
        let mut specs = Vec::new();
        for d in s.destinations.iter().filter(|d| d.enabled && !d.url.trim().is_empty()) {
            let key = secrets.stream_keys.get(&d.id).map(String::as_str);
            let target = match d.kind {
                DestinationKind::Rtmp => {
                    let t = RtmpTarget::parse(&d.url, key)
                        .with_context(|| format!("{}", if d.name.is_empty() { &d.url } else { &d.name }))?;
                    Target::Rtmp {
                        scheme: t.scheme,
                        host: t.host,
                        port: t.port,
                        application: t.application,
                        stream: t.stream,
                    }
                }
                DestinationKind::Srt => Target::Srt { uri: srt_uri(&d.url, key) },
            };
            specs.push(OutputSpec { id: d.id.clone(), target });
        }
        built.streaming = !specs.is_empty();
        if s.recording.enabled {
            let path = recording_path(&s.recording.folder)?;
            built.recording_path = Some(path.to_string_lossy().to_string());
            if transcribing {
                let mut t = tsink.lock();
                t.srt = Some(SrtFile { path: path.with_extension("srt"), entries: Vec::new() });
                if translating {
                    let ext = format!("{}.srt", s.translation.target);
                    t.srt_translated = Some(SrtFile { path: path.with_extension(ext), entries: Vec::new() });
                }
            }
            specs.push(OutputSpec { id: "recording".into(), target: Target::File { path } });
        }
        if specs.is_empty() {
            return Err(anyhow!("no output: enable a destination or recording"));
        }

        // ---------- Codificación (una sola vez para todas las salidas) ----------
        let venc_q = make("queue")?;
        let venc_conv = make("videoconvert")?;
        let (venc, enc_name) = media::make_video_encoder(s.encoder.as_deref(), s.video_bitrate(), s.fps())?;
        let vparse = make("h264parse")?;
        set_prop(&vparse, "config-interval", "-1");
        let vcaps_out = make("capsfilter")?;
        vcaps_out.set_property(
            "caps",
            gst::Caps::builder("video/x-h264").field("stream-format", "avc").field("alignment", "au").build(),
        );
        let vsink = gst_app::AppSink::builder().sync(false).build();
        pipeline.add_many([&venc_q, &venc_conv, &venc, &vparse, &vcaps_out, vsink.upcast_ref()])?;
        let (orig_ch, tr_ch) = caption_routing(s, translating);
        // Los subtítulos necesitan plugins que no todas las distribuciones traen
        // (tttocea608 viene de gst-plugins-rs): sin ellos se transmite igual.
        let cc_available = ["cccombiner", "h264ccinserter", "cea608mux", "tttocea608", "ccconverter", "capssetter"]
            .iter()
            .all(|f| gst::ElementFactory::find(f).is_some());
        if transcribing && s.transcription.captions && !cc_available {
            crate::log::write("warn", "closed-caption plugins missing: captions disabled");
            let _ = self.app.emit("transcription-status", serde_json::json!({ "state": "error", "message": "captions-unavailable" }));
        }
        if transcribing && s.transcription.captions && cc_available && (orig_ch.is_some() || tr_ch.is_some()) {
            // Subtítulos CEA-608: texto → tttocea608 → cea608mux (CC1 en el campo 1,
            // CC3 en el campo 2) → cccombiner (meta en cada cuadro) → el encoder
            // conserva la meta → h264ccinserter la escribe como SEI.
            let comb = make("cccombiner")?;
            let inserter = make("h264ccinserter")?;
            let mux = make("cea608mux")?;
            let ccconv = make("ccconverter")?;
            let cccaps = make("capsfilter")?;
            cccaps.set_property(
                "caps",
                gst::Caps::builder("closedcaption/x-cea-708").field("format", "cc_data").build(),
            );
            // Las colas dan margen de latencia a los agregadores (sin ellas descartan subtítulos).
            let ccq = make("queue")?;
            pipeline.add_many([&comb, &inserter, &mux, &ccconv, &cccaps, &ccq])?;
            gst::Element::link_many([&vtee, &venc_q, &comb, &venc_conv, &venc, &vparse, &inserter, &vcaps_out, vsink.upcast_ref()])?;
            gst::Element::link_many([&mux, &ccconv, &cccaps, &ccq])?;
            ccq.static_pad("src").unwrap().link(&comb.request_pad_simple("caption").ok_or_else(|| anyhow!("cccombiner caption pad"))?)?;

            let channel = |ch: Ch| -> Result<CaptionFeed> {
                let src = gst_app::AppSrc::builder()
                    .is_live(true)
                    .format(gst::Format::Time)
                    .caps(&gst::Caps::builder("text/x-raw").field("format", "utf8").build())
                    .build();
                let to608 = make("tttocea608")?;
                set_prop(&to608, "mode", "roll-up2");
                set_prop(&to608, "roll-up-timeout", "6000000000");
                let mut chain: Vec<gst::Element> = vec![src.clone().upcast(), to608];
                if ch == Ch::Cc3 {
                    // tttocea608 produce campo 1; CC3 es el mismo canal de datos en el campo 2.
                    let setter = make("capssetter")?;
                    setter.set_property("caps", gst::Caps::builder("closedcaption/x-cea-608").field("field", 1i32).build());
                    chain.push(setter);
                }
                let q = make("queue")?;
                chain.push(q.clone());
                pipeline.add_many(&chain)?;
                gst::Element::link_many(&chain)?;
                let pad = mux
                    .request_pad_simple(if ch == Ch::Cc1 { "cc1" } else { "cc3" })
                    .ok_or_else(|| anyhow!("cea608mux pad"))?;
                q.static_pad("src").unwrap().link(&pad)?;
                Ok(CaptionFeed { src, pipeline: pipeline.downgrade(), last_end: None })
            };
            let used: Vec<Ch> = [orig_ch, tr_ch].into_iter().flatten().collect();
            let cc1 = if used.contains(&Ch::Cc1) { Some(channel(Ch::Cc1)?) } else { None };
            let cc3 = if used.contains(&Ch::Cc3) { Some(channel(Ch::Cc3)?) } else { None };
            let mut t = tsink.lock();
            t.cc1 = cc1;
            t.cc3 = cc3;
            t.original_to = orig_ch;
            t.translation_to = tr_ch;
        } else {
            gst::Element::link_many([&vtee, &venc_q, &venc_conv, &venc, &vparse, &vcaps_out, vsink.upcast_ref()])?;
        }
        built.encoder = Some(enc_name);
        {
            let c = counters.clone();
            vparse.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buf) = info.buffer() {
                    c.video_bytes.fetch_add(buf.size() as u64, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        }

        let app = self.app.clone();
        let status: StatusFn = Arc::new(move |st: OutputStatus| {
            let _ = app.emit("output-status", st);
        });
        let outputs = Arc::new(Outputs::start(specs, atee.is_some(), status));

        let o = outputs.clone();
        vsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    o.push_video(&sample);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        if let Some(atee) = &atee {
            let q = make("queue")?;
            let conv = make("audioconvert")?;
            let enc = media::make_audio_encoder(s.audio_bitrate_kbps)?;
            let parse = make("aacparse")?;
            let asink = gst_app::AppSink::builder().sync(false).build();
            pipeline.add_many([&q, &conv, &enc, &parse, asink.upcast_ref()])?;
            gst::Element::link_many([atee, &q, &conv, &enc, &parse, asink.upcast_ref()])?;
            let o = outputs.clone();
            asink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        o.push_audio(&sample);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        built.outputs = Some(outputs);
        Ok(built)
    }

    /// Audio de la mezcla → PCM16 mono a la tasa del proveedor → Transcriber.
    fn add_transcription(
        &self,
        pipeline: &gst::Pipeline,
        atee: &gst::Element,
        s: &Settings,
        api_key: &str,
        tsink: &Arc<Mutex<TranscriptSink>>,
    ) -> Result<()> {
        let t = &s.transcription;
        let model = if t.model.trim().is_empty() { transcribe::default_model(t.provider).to_string() } else { t.model.trim().to_string() };
        let cfg = transcribe::Config { provider: t.provider, model, language: t.language.clone(), api_key: api_key.to_string() };

        let app = self.app.clone();
        let sink = tsink.clone();
        let on_event: transcribe::EventFn = Arc::new(move |ev| {
            sink.lock().handle(&ev);
            let _ = app.emit("transcript", ev);
        });
        let app = self.app.clone();
        let on_status: transcribe::StatusFn = Arc::new(move |state, message| {
            let _ = app.emit("transcription-status", serde_json::json!({ "state": state, "message": message }));
        });
        // Clave especial para pruebas: sin conexión, el texto se inyecta con `inject_transcript`.
        let tr = (api_key != OFFLINE_API_KEY).then(|| Arc::new(Transcriber::start(cfg, on_event, on_status)));

        let q = leaky_queue()?;
        set_prop(&q, "max-size-buffers", "50");
        let conv = make("audioconvert")?;
        let res = make("audioresample")?;
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("layout", "interleaved")
                .field("rate", transcribe::sample_rate(t.provider))
                .field("channels", 1i32)
                .build(),
        );
        let asink = gst_app::AppSink::builder().sync(false).build();
        pipeline.add_many([&q, &conv, &res, &caps, asink.upcast_ref()])?;
        gst::Element::link_many([atee, &q, &conv, &res, &caps, asink.upcast_ref()])?;
        asink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let (Some(tr), Some(buf)) = (&tr, sample.buffer()) {
                        if let Ok(map) = buf.map_readable() {
                            tr.push_audio(map.as_slice().to_vec());
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        Ok(())
    }

    fn add_preview_branch(
        &self,
        pipeline: &gst::Pipeline,
        vtee: &gst::Element,
        w: u32,
        h: u32,
        counters: &Arc<Counters>,
    ) -> Result<()> {
        let (pw, ph) = if w >= h {
            (PREVIEW_MAX_SIDE, PREVIEW_MAX_SIDE * h / w)
        } else {
            (PREVIEW_MAX_SIDE * w / h, PREVIEW_MAX_SIDE)
        };
        let q = leaky_queue()?;
        let rate = make("videorate")?;
        set_prop(&rate, "drop-only", "true");
        let scale = make("videoscale")?;
        let conv = make("videoconvert")?;
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", (pw & !1) as i32)
                .field("height", (ph & !1) as i32)
                .field("framerate", gst::Fraction::new(PREVIEW_FPS as i32, 1))
                .build(),
        );
        let jpeg = make("jpegenc")?;
        set_prop(&jpeg, "quality", "70");
        let sink = gst_app::AppSink::builder().max_buffers(1).drop(true).sync(false).build();

        let app = self.app.clone();
        let pc = counters.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(buf) = sample.buffer() {
                        if let Ok(map) = buf.map_readable() {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(map.as_slice());
                            if let Ok(dir) = std::env::var("TRANSMITIA_DUMP_DIR") {
                                let _ = std::fs::write(std::path::Path::new(&dir).join("latest.jpg"), map.as_slice());
                            }
                            let _ = app.emit("preview-frame", b64);
                            pc.preview_frames.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let sink_el: gst::Element = sink.upcast();
        pipeline.add_many([&q, &rate, &scale, &conv, &caps, &jpeg, &sink_el])?;
        gst::Element::link_many([vtee, &q, &rate, &scale, &conv, &caps, &jpeg, &sink_el])?;
        Ok(())
    }

    /// Mezclador: cada entrada con su volumen/silencio y medidor; un silencio
    /// de fondo mantiene el ritmo aunque una entrada falle.
    fn add_audio_mixer(
        &self,
        pipeline: &gst::Pipeline,
        s: &Settings,
        counters: &Arc<Counters>,
        layout: &Arc<Mutex<Layout>>,
        levels: &mut Vec<(String, Arc<Peak>)>,
    ) -> Result<gst::Element> {
        let f32_caps = || {
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48000i32)
                .field("channels", 2i32)
                .build()
        };
        let mixer = make("audiomixer")?;
        set_prop(&mixer, "ignore-inactive-pads", "true");
        pipeline.add(&mixer)?;

        let silence = make("audiotestsrc")?;
        silence.set_property("is-live", true);
        set_prop(&silence, "wave", "silence");
        let scaps = make("capsfilter")?;
        scaps.set_property("caps", f32_caps());
        pipeline.add_many([&silence, &scaps])?;
        gst::Element::link_many([&silence, &scaps, &mixer])?;

        for input in &s.audio_inputs {
            let src = match self.devices.make_audio_source(&input.source) {
                Ok(Some(src)) => src,
                Ok(None) => continue,
                Err(e) => {
                    let msg = format!("{}: {e:#}", input.source);
                    crate::log::write("error", &msg);
                    let _ = self.app.emit("engine-error", msg);
                    continue;
                }
            };
            let q = make("queue")?;
            let conv = make("audioconvert")?;
            let res = make("audioresample")?;
            let caps = make("capsfilter")?;
            caps.set_property("caps", f32_caps());
            let vol = make("volume")?;
            pipeline.add_many([&src, &q, &conv, &res, &caps, &vol])?;
            gst::Element::link_many([&src, &q, &conv, &res, &caps, &vol, &mixer])?;

            let peak = Arc::new(Peak::default());
            let p = peak.clone();
            vol.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buf) = info.buffer() {
                    let (l, r) = buffer_peaks(buf);
                    p.bump(l, r);
                }
                gst::PadProbeReturn::Ok
            });
            levels.push((input.id.clone(), peak));
            layout.lock().volumes.insert(input.id.clone(), vol);
        }

        let mcaps = make("capsfilter")?;
        mcaps.set_property("caps", f32_caps());
        let tee = make("tee")?;
        set_prop(&tee, "allow-not-linked", "true");
        pipeline.add_many([&mcaps, &tee])?;
        gst::Element::link_many([&mixer, &mcaps, &tee])?;

        let c = counters.clone();
        mcaps.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                let (l, r) = buffer_peaks(buf);
                bump_peak(&c.peak_l, l);
                bump_peak(&c.peak_r, r);
            }
            gst::PadProbeReturn::Ok
        });
        Ok(tee)
    }
}

pub const OFFLINE_API_KEY: &str = "offline-test";

/// Clave de captura del lienzo compuesto.
pub const PROGRAM: &str = "program";

/// Capturas bajo demanda: una sonda en cada fuente entrega el próximo cuadro
/// solo cuando alguien lo pidió (sin costo el resto del tiempo).
#[derive(Default)]
pub struct Snapshots {
    pending: AtomicBool,
    waiting: Mutex<HashMap<String, Vec<std::sync::mpsc::Sender<gst::Sample>>>>,
}

impl Snapshots {
    fn install(self: &Arc<Self>, pad: &gst::Pad, key: String) {
        let me = self.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if !me.pending.load(Ordering::Relaxed) {
                return gst::PadProbeReturn::Ok;
            }
            let mut waiting = me.waiting.lock();
            if let Some(list) = waiting.get_mut(&key).filter(|l| !l.is_empty()) {
                if let (Some(buf), Some(caps)) = (info.buffer(), pad.current_caps()) {
                    let sample = gst::Sample::builder().buffer(buf).caps(&caps).build();
                    for tx in list.drain(..) {
                        let _ = tx.send(sample.clone());
                    }
                }
            }
            if waiting.values().all(|l| l.is_empty()) {
                me.pending.store(false, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// Captura el próximo cuadro de `key` (id de fuente o `program`) como JPEG,
    /// con el lado mayor limitado a `max_side`.
    pub fn capture_jpeg(&self, key: &str, max_side: u32) -> Result<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.waiting.lock().entry(key.to_string()).or_default().push(tx);
        self.pending.store(true, Ordering::Relaxed);
        let sample = rx
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("no frame from source {key} (is it in a scene and running?)"))?;
        let caps = sample.caps().ok_or_else(|| anyhow!("frame without caps"))?;
        let st = caps.structure(0).ok_or_else(|| anyhow!("frame without caps"))?;
        let (w, h) = (st.get::<i32>("width")? as u32, st.get::<i32>("height")? as u32);
        let scale = (max_side as f64 / w.max(h) as f64).min(1.0);
        let (ow, oh) = (((w as f64 * scale) as i32) & !1, ((h as f64 * scale) as i32) & !1);
        let jpeg_caps = gst::Caps::builder("image/jpeg").field("width", ow).field("height", oh).build();
        let out = gstreamer_video::convert_sample(&sample, &jpeg_caps, Some(gst::ClockTime::from_seconds(5)))?;
        let buf = out.buffer().ok_or_else(|| anyhow!("empty jpeg"))?;
        Ok(buf.map_readable()?.as_slice().to_vec())
    }
}

/// Canal CEA-608 del texto original y de la traducción.
fn caption_routing(s: &Settings, translating: bool) -> (Option<Ch>, Option<Ch>) {
    use crate::settings::CaptionRouting::*;
    if !translating {
        return (Some(Ch::Cc1), None);
    }
    match s.translation.captions {
        OriginalCc1 => (Some(Ch::Cc1), Some(Ch::Cc3)),
        TranslatedCc1 => (Some(Ch::Cc3), Some(Ch::Cc1)),
        TranslatedOnly => (None, Some(Ch::Cc1)),
    }
}

/// Claves leídas del llavero para este arranque.
#[derive(Default)]
pub struct Secrets {
    /// id de destino → clave de transmisión
    pub stream_keys: HashMap<String, String>,
    /// Clave del proveedor de transcripción elegido.
    pub api_key: Option<String>,
    /// Clave del proveedor de traducción elegido.
    pub translate_key: Option<String>,
}

struct CaptionFeed {
    src: gst_app::AppSrc,
    pipeline: gst::glib::WeakRef<gst::Pipeline>,
    /// Fin del último subtítulo: los siguientes no se enciman.
    last_end: Option<gst::ClockTime>,
}

impl CaptionFeed {
    fn push(&mut self, text: &str) {
        let Some(p) = self.pipeline.upgrade() else { return };
        let Some(now) = p.current_running_time() else { return };
        let pts = self.last_end.map_or(now, |e| e.max(now));
        // CEA-608 transmite ~2 caracteres por cuadro: dar tiempo suficiente.
        let ms = (text.chars().count() as u64 * 40 + 100).max(250);
        let dur = gst::ClockTime::from_mseconds(ms);
        self.last_end = Some(pts + dur);
        let mut buf = gst::Buffer::from_slice(text.as_bytes().to_vec());
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(pts);
            b.set_duration(dur);
        }
        let _ = self.src.push_buffer(buf);
    }
}

struct SrtFile {
    path: PathBuf,
    entries: Vec<(Duration, Duration, String)>,
}

impl SrtFile {
    fn write(&self) {
        let fmt = |d: Duration| {
            let ms = d.as_millis();
            format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
        };
        let body: String = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, (a, b, t))| format!("{}\n{} --> {}\n{}\n\n", i + 1, fmt(*a), fmt(*b), t.trim()))
            .collect();
        if let Err(e) = std::fs::write(&self.path, body) {
            crate::log::write("error", &format!("srt write: {e}"));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ch {
    Cc1,
    Cc3,
}

/// Recibe la transcripción (y su traducción): la convierte en subtítulos
/// CEA-608 (CC1/CC3) y en archivos .srt.
struct TranscriptSink {
    started: Instant,
    items: HashMap<String, (Duration, bool)>,
    /// Tiempos de segmentos ya cerrados, para ubicar su traducción en el .srt.
    done: HashMap<String, (Duration, Duration)>,
    cc1: Option<CaptionFeed>,
    cc3: Option<CaptionFeed>,
    original_to: Option<Ch>,
    translation_to: Option<Ch>,
    srt: Option<SrtFile>,
    srt_translated: Option<SrtFile>,
    translator: Option<Arc<Translator>>,
}

impl Default for TranscriptSink {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            items: HashMap::new(),
            done: HashMap::new(),
            cc1: None,
            cc3: None,
            original_to: Some(Ch::Cc1),
            translation_to: None,
            srt: None,
            srt_translated: None,
            translator: None,
        }
    }
}

impl TranscriptSink {
    fn feed(&mut self, ch: Option<Ch>) -> Option<&mut CaptionFeed> {
        match ch? {
            Ch::Cc1 => self.cc1.as_mut(),
            Ch::Cc3 => self.cc3.as_mut(),
        }
    }

    fn handle(&mut self, ev: &TranscriptEvent) {
        let now = self.started.elapsed();
        let (start, captioned) = *self.items.entry(ev.item_id.clone()).or_insert((now, false));
        let to = self.original_to;
        match ev.kind {
            // Los deltas solo agregan texto: se pueden subtitular de inmediato.
            Kind::Delta => {
                if let Some(c) = self.feed(to) {
                    c.push(&ev.text);
                }
                self.items.insert(ev.item_id.clone(), (start, true));
            }
            Kind::Interim => {}
            Kind::Final => {
                if !captioned {
                    let text = format!("{} ", ev.text.trim());
                    if let Some(c) = self.feed(to) {
                        c.push(&text);
                    }
                }
                self.items.remove(&ev.item_id);
                if ev.text.trim().is_empty() {
                    return;
                }
                let end = now.max(start + Duration::from_millis(800));
                self.done.insert(ev.item_id.clone(), (start, end));
                if self.done.len() > 500 {
                    self.done.clear();
                }
                if let Some(srt) = &mut self.srt {
                    srt.entries.push((start, end, ev.text.clone()));
                    srt.write();
                }
                if let Some(t) = &self.translator {
                    t.submit(ev.item_id.clone(), ev.text.clone());
                }
            }
        }
    }

    fn handle_translation(&mut self, item_id: &str, text: &str) {
        let to = self.translation_to;
        if let Some(c) = self.feed(to) {
            c.push(&format!("{} ", text.trim()));
        }
        let now = self.started.elapsed();
        let (start, end) = self.done.remove(item_id).unwrap_or((now, now + Duration::from_secs(2)));
        if let Some(srt) = &mut self.srt_translated {
            srt.entries.push((start, end, text.to_string()));
            srt.write();
        }
    }
}

struct Built {
    snapshots: Arc<Snapshots>,
    pipeline: gst::Pipeline,
    transcript: Option<Arc<Mutex<TranscriptSink>>>,
    layout: Arc<Mutex<Layout>>,
    levels: Vec<(String, Arc<Peak>)>,
    outputs: Option<Arc<Outputs>>,
    encoder: Option<String>,
    streaming: bool,
    recording_path: Option<String>,
}

struct StatsMeta {
    mode: Mode,
    encoder: Option<String>,
    streaming: bool,
    recording_path: Option<String>,
}

struct LayerHandle {
    pad: gst::Pad,
    vbox: gst::Element,
    source: String,
}

#[derive(Clone)]
struct LayerSpec {
    rect: Rect,
    fit: Fit,
    alpha: f64,
    zorder: u32,
}

/// Estado de composición compartido entre comandos y los hilos de GStreamer.
#[derive(Default)]
struct Layout {
    canvas: (u32, u32),
    handles: HashMap<String, LayerHandle>,
    specs: HashMap<String, LayerSpec>,
    src_dims: HashMap<String, (i32, i32)>,
    /// Volumen por entrada de audio.
    volumes: HashMap<String, gst::Element>,
    /// Elementos de las capas de texto (por id de capa).
    texts: HashMap<String, TextHandle>,
}

struct TextHandle {
    bg: gst::Element,
    overlay: gst::Element,
    height: i32,
}

impl TextHandle {
    fn apply(&self, t: &TextStyle) {
        let px = ((t.size / 100.0) * self.height as f64).round().max(6.0) as i32;
        let font = if t.font.trim().is_empty() { "Sans" } else { t.font.trim() };
        let weight = if t.bold { " Bold" } else { "" };
        self.overlay.set_property("text", &t.content);
        self.overlay.set_property("font-desc", format!("{font}{weight} {px}px"));
        self.overlay.set_property("color", 0xFF00_0000u32 | parse_rgb(&t.color, 0xFFFFFF));
        set_prop(&self.overlay, "draw-outline", if t.outline { "true" } else { "false" });
        set_prop(&self.overlay, "draw-shadow", "false");
        let align = match t.align.as_str() {
            "left" => "left",
            "right" => "right",
            _ => "center",
        };
        set_prop(&self.overlay, "halignment", align);
        let alpha = ((t.background_opacity.clamp(0.0, 1.0)) * 255.0).round() as u32;
        self.bg.set_property("foreground-color", (alpha << 24) | parse_rgb(&t.background, 0));
    }
}

fn parse_rgb(hex: &str, default: u32) -> u32 {
    u32::from_str_radix(hex.trim().trim_start_matches('#'), 16).map(|v| v & 0xFF_FFFF).unwrap_or(default)
}

/// Capa de texto: fondo (con transparencia) del tamaño de la capa + textoverlay.
fn make_text_source(layer: &Layer, canvas: (u32, u32)) -> Result<(Vec<gst::Element>, TextHandle)> {
    let (_, _, lw, lh) = layer.rect.to_px(canvas.0, canvas.1);
    let (lw, lh) = ((lw.max(16) + 1) & !1, (lh.max(16) + 1) & !1);
    let bg = make("videotestsrc")?;
    bg.set_property("is-live", true);
    set_prop(&bg, "pattern", "solid-color");
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", lw)
            .field("height", lh)
            .field("framerate", gst::Fraction::new(10, 1))
            .build(),
    );
    let overlay = make("textoverlay")?;
    set_prop(&overlay, "valignment", "center");
    set_prop(&overlay, "wrap-mode", "word");
    set_prop(&overlay, "auto-resize", "false");
    let handle = TextHandle { bg: bg.clone(), overlay: overlay.clone(), height: lh };
    if let Some(t) = &layer.text {
        handle.apply(t);
    }
    Ok((vec![bg, caps, overlay], handle))
}

impl Layout {
    fn apply(&mut self, s: &Settings) {
        for input in &s.audio_inputs {
            if let Some(v) = self.volumes.get(&input.id) {
                v.set_property("volume", input.volume.clamp(0.0, 4.0));
                v.set_property("mute", input.muted);
            }
        }
        for layer in s.scenes.iter().flat_map(|sc| sc.layers.iter()) {
            if let (Some(h), Some(t)) = (self.texts.get(&layer.id), &layer.text) {
                h.apply(t);
            }
        }
        self.specs.clear();
        for (si, scene) in s.scenes.iter().enumerate() {
            for (li, layer) in scene.layers.iter().enumerate() {
                let active = si == s.active_scene && layer.visible;
                self.specs.insert(
                    layer.id.clone(),
                    LayerSpec {
                        rect: layer.rect,
                        fit: layer.fit,
                        alpha: if active { 1.0 } else { 0.0 },
                        zorder: li as u32 + 1,
                    },
                );
            }
        }
        self.reapply();
    }

    fn reapply(&self) {
        for (id, h) in &self.handles {
            // Capa borrada desde la interfaz pero el pipeline aún no se reconstruyó.
            let spec = self.specs.get(id).cloned().unwrap_or(LayerSpec {
                rect: Rect::FULL,
                fit: Fit::Contain,
                alpha: 0.0,
                zorder: 1,
            });
            let (x, y, lw, lh) = spec.rect.to_px(self.canvas.0, self.canvas.1);
            h.pad.set_property("xpos", x);
            h.pad.set_property("ypos", y);
            h.pad.set_property("width", lw);
            h.pad.set_property("height", lh);
            h.pad.set_property("alpha", spec.alpha);
            h.pad.set_property("zorder", spec.zorder);
            let policy = if spec.fit == Fit::Contain { "keep-aspect-ratio" } else { "none" };
            set_prop(&h.pad, "sizing-policy", policy);

            let crop = match (spec.fit, self.src_dims.get(&h.source)) {
                (Fit::Cover, Some(&(sw, sh))) => cover_crop(sw, sh, (lw as u32, lh as u32)),
                _ => [0; 4],
            };
            for (prop, v) in ["left", "right", "top", "bottom"].iter().zip(crop) {
                if h.vbox.property::<i32>(prop) != v {
                    h.vbox.set_property(prop, v);
                }
            }
        }
    }
}


/// Recorte [izq, der, arriba, abajo] para llenar `target` sin deformar (valores pares).
fn cover_crop(sw: i32, sh: i32, target: (u32, u32)) -> [i32; 4] {
    let (tw, th) = (target.0 as i64, target.1 as i64);
    let (sw64, sh64) = (sw as i64, sh as i64);
    if sw64 * th > sh64 * tw {
        // Fuente más ancha: recortar a los lados.
        let keep = ((sh64 * tw / th) as i32) & !1;
        let cut = sw - keep;
        let l = (cut / 2) & !1;
        [l, cut - l, 0, 0]
    } else {
        let keep = ((sw64 * th / tw) as i32) & !1;
        let cut = sh - keep;
        let t = (cut / 2) & !1;
        [0, 0, t, cut - t]
    }
}

fn leaky_queue() -> Result<gst::Element> {
    let q = make("queue")?;
    set_prop(&q, "leaky", "downstream");
    set_prop(&q, "max-size-buffers", "2");
    set_prop(&q, "max-size-time", "0");
    set_prop(&q, "max-size-bytes", "0");
    Ok(q)
}

#[derive(Default)]
struct Peak {
    l: AtomicU32,
    r: AtomicU32,
}

impl Peak {
    fn bump(&self, l: f32, r: f32) {
        bump_peak(&self.l, l);
        bump_peak(&self.r, r);
    }
    fn take(&self) -> [f32; 2] {
        [f32::from_bits(self.l.swap(0, Ordering::Relaxed)), f32::from_bits(self.r.swap(0, Ordering::Relaxed))]
    }
}

/// Picos (izq, der) de un búfer F32 estéreo intercalado.
fn buffer_peaks(buf: &gst::BufferRef) -> (f32, f32) {
    let Ok(map) = buf.map_readable() else { return (0.0, 0.0) };
    let (mut l, mut r) = (0f32, 0f32);
    for frame in map.as_slice().chunks_exact(8) {
        l = l.max(f32::from_le_bytes(frame[0..4].try_into().unwrap()).abs());
        r = r.max(f32::from_le_bytes(frame[4..8].try_into().unwrap()).abs());
    }
    (l, r)
}

fn bump_peak(slot: &AtomicU32, v: f32) {
    let _ = slot.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        (v > f32::from_bits(old)).then_some(v.to_bits())
    });
}

/// Destino RTMP separado en partes. La clave se pasa tal cual como `stream`
/// (las de Facebook traen `?s_bl=1&...`, que no debe interpretarse como URL).
#[derive(Debug, PartialEq)]
struct RtmpTarget {
    scheme: String,
    host: String,
    port: u16,
    application: String,
    stream: String,
}

impl RtmpTarget {
    fn parse(url: &str, key: Option<&str>) -> Result<Self> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| anyhow!("invalid RTMP URL (expected rtmp:// or rtmps://): {url}"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "rtmp" && scheme != "rtmps" {
            return Err(anyhow!("invalid RTMP scheme '{scheme}' (use rtmp:// or rtmps://)"));
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| anyhow!("invalid port in RTMP URL: {p}"))?),
            None => (authority.to_string(), if scheme == "rtmps" { 443 } else { 1935 }),
        };
        if host.is_empty() {
            return Err(anyhow!("missing host in RTMP URL"));
        }
        let path = path.trim_matches('/');
        let key = key.map(str::trim).filter(|k| !k.is_empty());
        let (application, stream) = match key {
            Some(k) => (path.to_string(), k.to_string()),
            // Sin clave guardada: la URL completa debe incluirla como último segmento.
            None => match path.rsplit_once('/') {
                Some((app, s)) => (app.to_string(), s.to_string()),
                None => return Err(anyhow!("missing stream key")),
            },
        };
        if application.is_empty() {
            return Err(anyhow!("missing application path in RTMP URL (e.g. /live2 or /rtmp)"));
        }
        Ok(Self { scheme, host, port, application, stream })
    }
}


/// SRT: la clave se usa como `streamid` si la URL no trae uno.
fn srt_uri(url: &str, key: Option<&str>) -> String {
    let base = url.trim();
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) if !base.contains("streamid=") => {
            let sep = if base.contains('?') { '&' } else { '?' };
            format!("{base}{sep}streamid={k}")
        }
        _ => base.to_string(),
    }
}

fn recording_path(folder: &str) -> Result<PathBuf> {
    let dir = if folder.trim().is_empty() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| anyhow!("cannot find home folder"))?;
        let movies = if cfg!(target_os = "macos") { "Movies" } else { "Videos" };
        PathBuf::from(home).join(movies).join("TransmitIA")
    } else {
        PathBuf::from(folder)
    };
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    Ok(dir.join(format!("transmitia-{stamp}.mkv")))
}

fn spawn_bus_thread<R: Runtime>(
    app: AppHandle<R>,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    c: Arc<Counters>,
    levels: Vec<(String, Arc<Peak>)>,
    meta: StatsMeta,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let bus = pipeline.bus().expect("pipeline bus");
        let started = Instant::now();
        let mut last = Instant::now();
        let mut last_level = Instant::now();
        let mut stalled = 0u32;
        while !stop.load(Ordering::SeqCst) {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        let src = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
                        crate::log::write(
                            "error",
                            &format!("{src}: {} | {}", e.error(), e.debug().unwrap_or_default()),
                        );
                        let _ = app.emit("engine-error", format!("{src}: {}", e.error()));
                    }
                    gst::MessageView::Warning(w) => {
                        let src = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
                        crate::log::write(
                            "warn",
                            &format!("{src}: {} | {}", w.error(), w.debug().unwrap_or_default()),
                        );
                    }
                    gst::MessageView::Eos(_) => c.eos.store(true, Ordering::SeqCst),
                    _ => {}
                }
            }
            if last_level.elapsed() >= Duration::from_millis(100) {
                last_level = Instant::now();
                let master = [
                    f32::from_bits(c.peak_l.swap(0, Ordering::Relaxed)),
                    f32::from_bits(c.peak_r.swap(0, Ordering::Relaxed)),
                ];
                let inputs: HashMap<&str, [f32; 2]> = levels.iter().map(|(id, p)| (id.as_str(), p.take())).collect();
                let _ = app.emit("audio-levels", serde_json::json!({ "master": master, "inputs": inputs }));
            }
            let dt = last.elapsed();
            if dt >= Duration::from_secs(1) {
                last = Instant::now();
                let secs = dt.as_secs_f64();
                let stats = Stats {
                    mode: meta.mode,
                    fps: c.frames.swap(0, Ordering::Relaxed) as f64 / secs,
                    video_kbps: c.video_bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / secs,
                    uptime_sec: started.elapsed().as_secs(),
                    encoder: meta.encoder.clone(),
                    streaming: meta.streaming,
                    recording_path: meta.recording_path.clone(),
                };
                let up = started.elapsed().as_secs();
                let pfps = c.preview_frames.swap(0, Ordering::Relaxed) as f64 / secs;
                if up <= 5 || stats.fps < 1.0 || pfps < 1.0 || up % 30 == 0 {
                    crate::log::write(
                        "info",
                        &format!("t={up}s fps={:.1} preview={pfps:.1} kbps={:.0}", stats.fps, stats.video_kbps),
                    );
                }
                // La fuente dejó de enviar cuadros: avisar en la interfaz.
                if up >= 4 && stats.fps < 1.0 {
                    stalled += 1;
                    if stalled == 3 {
                        let _ = app.emit("engine-error", "source-stalled".to_string());
                    }
                } else {
                    stalled = 0;
                }
                let _ = app.emit("stats", stats);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Orientation, Resolution};

    /// Subtítulos CEA-608 incrustados + archivo .srt, con texto inyectado.
    #[test]
    fn captions_are_embedded_and_srt_written() {
        let app = tauri::test::mock_app();
        let mut eng = Engine::new(app.handle().clone()).unwrap();
        let dir = std::env::temp_dir().join("transmitia-cc-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Settings::default();
        s.resolution = Resolution::P720;
        s.recording.enabled = true;
        s.recording.folder = dir.to_string_lossy().to_string();
        s.transcription.enabled = true;
        s.transcription.captions = true;
        let secrets = Secrets { stream_keys: HashMap::new(), api_key: Some(OFFLINE_API_KEY.into()), translate_key: None };
        eng.start(&s, Mode::Live, secrets).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let ev = |id: &str, text: &str, kind| TranscriptEvent { item_id: id.into(), text: text.into(), kind };
        eng.inject_transcript(ev("a", "Hola mundo", Kind::Final));
        std::thread::sleep(Duration::from_millis(1200));
        eng.inject_transcript(ev("b", " desde", Kind::Delta));
        eng.inject_transcript(ev("b", " TransmitIA", Kind::Delta));
        eng.inject_transcript(ev("b", "desde TransmitIA", Kind::Final));
        std::thread::sleep(Duration::from_millis(1500));
        eng.stop();

        let mkv = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "mkv"))
            .unwrap();
        let srt = std::fs::read_to_string(mkv.with_extension("srt")).unwrap();
        assert!(srt.contains("Hola mundo") && srt.contains("desde TransmitIA"), "srt: {srt}");

        let text = decode_608(&mkv, 0);
        println!("captions: {text:?}");
        assert!(text.contains("Hola mundo"), "captions: {text:?}");
        assert!(text.contains("TransmitIA"), "captions: {text:?}");
    }

    /// Lee los subtítulos CEA-608 de un campo (0 = CC1, 1 = CC3) de una grabación.
    fn decode_608(mkv: &std::path::Path, field: i32) -> String {
        let p = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! matroskademux ! h264parse ! h264ccextractor ! ccconverter ! \
             closedcaption/x-cea-608,format=raw,field={field} ! appsink name=out sync=false",
            mkv.display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let out = p.by_name("out").unwrap().downcast::<gst_app::AppSink>().unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut text = String::new();
        while let Ok(sample) = out.pull_sample() {
            if let Some(b) = sample.buffer() {
                // Pares CEA-608: quitar paridad; 0x10–0x1f inicia un código de control.
                for pair in b.map_readable().unwrap().chunks_exact(2) {
                    let (a, c) = (pair[0] & 0x7f, pair[1] & 0x7f);
                    if (0x10..0x20).contains(&a) {
                        continue;
                    }
                    for ch in [a, c] {
                        if (0x20..0x7f).contains(&ch) {
                            text.push(ch as char);
                        }
                    }
                }
            }
        }
        p.set_state(gst::State::Null).unwrap();
        text
    }

    /// Subtítulos bilingües: original en CC1 y traducción en CC3, más .en.srt.
    #[test]
    fn bilingual_captions_cc1_cc3() {
        let app = tauri::test::mock_app();
        let mut eng = Engine::new(app.handle().clone()).unwrap();
        let dir = std::env::temp_dir().join("transmitia-cc2-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Settings::default();
        s.resolution = Resolution::P720;
        s.recording.enabled = true;
        s.recording.folder = dir.to_string_lossy().to_string();
        s.transcription.enabled = true;
        s.translation.enabled = true;
        s.translation.target = "en".into();
        let secrets = Secrets {
            stream_keys: HashMap::new(),
            api_key: Some(OFFLINE_API_KEY.into()),
            translate_key: Some(OFFLINE_API_KEY.into()),
        };
        eng.start(&s, Mode::Live, secrets).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        eng.inject_transcript(TranscriptEvent { item_id: "a".into(), text: "Hola amigos".into(), kind: Kind::Final });
        std::thread::sleep(Duration::from_millis(400));
        eng.inject_translation("a", "Hello friends");
        std::thread::sleep(Duration::from_millis(1800));
        eng.stop();

        let mkv = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "mkv"))
            .unwrap();
        let cc1 = decode_608(&mkv, 0);
        let cc3 = decode_608(&mkv, 1);
        println!("CC1: {cc1:?}  CC3: {cc3:?}");
        assert!(cc1.contains("Hola amigos"), "CC1: {cc1:?}");
        assert!(cc3.contains("Hello friends"), "CC3: {cc3:?}");
        let en = std::fs::read_to_string(mkv.with_extension("en.srt")).unwrap();
        assert!(en.contains("Hello friends"), "en.srt: {en}");
    }

    /// Captura de cuadros (fuente y programa) y capa de imagen agregada en vivo.
    #[test]
    fn snapshot_and_live_image_layer() {
        use crate::settings::Layer;
        let app = tauri::test::mock_app();
        let mut eng = Engine::new(app.handle().clone()).unwrap();
        let dir = std::env::temp_dir().join("transmitia-snap-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Settings::default();
        s.resolution = Resolution::P720;
        s.recording.enabled = true;
        s.recording.folder = dir.to_string_lossy().to_string();
        eng.start(&s, Mode::Live, Secrets::default()).unwrap();
        std::thread::sleep(Duration::from_millis(600));

        let snaps = eng.snapshots().unwrap();
        let src = snaps.capture_jpeg("test", 640).unwrap();
        let prog = snaps.capture_jpeg(PROGRAM, 640).unwrap();
        assert_eq!(&src[..2], &[0xff, 0xd8], "JPEG magic");
        assert_eq!(&prog[..2], &[0xff, 0xd8], "JPEG magic");
        assert!(snaps.capture_jpeg("dev:99", 640).is_err());

        // Usar la captura como "póster" y ponerla en vivo en un recuadro.
        let img = dir.join("poster.jpg");
        std::fs::write(&img, &src).unwrap();
        s.scenes[0].layers.push(Layer {
            id: "poster".into(),
            source: format!("image:{}", img.display()),
            rect: Rect { x: 0.6, y: 0.05, w: 0.35, h: 0.35 },
            fit: Fit::Contain,
            visible: true,
            text: None,
        });
        eng.add_layer_live(&s, "poster").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let after = snaps.capture_jpeg(PROGRAM, 640).unwrap();
        assert_eq!(&after[..2], &[0xff, 0xd8]);
        std::fs::write(dir.join("program-after.jpg"), &after).unwrap();
        eng.stop();
    }

    #[test]
    fn cover_crop_math() {
        assert_eq!(cover_crop(1920, 1080, (1080, 1920)), [656, 658, 0, 0]);
        assert_eq!(cover_crop(1920, 1080, (1920, 1080)), [0, 0, 0, 0]);
        assert_eq!(cover_crop(1080, 1920, (1920, 1080)), [0, 0, 656, 658]);
    }

    #[test]
    fn rtmp_target_facebook_key_with_query() {
        let key = "FB-123456789-0-AbCd?s_bl=1&s_psm=1&s_sw=0&s_vt=api-s&a=XyZ";
        let t = RtmpTarget::parse("rtmps://live-api-s.facebook.com:443/rtmp/", Some(key)).unwrap();
        assert_eq!(t.scheme, "rtmps");
        assert_eq!(t.host, "live-api-s.facebook.com");
        assert_eq!(t.port, 443);
        assert_eq!(t.application, "rtmp");
        assert_eq!(t.stream, key);
    }

    #[test]
    fn rtmp_target_defaults_and_errors() {
        let t = RtmpTarget::parse("rtmp://a.rtmp.youtube.com/live2", Some(" abcd-1234 ")).unwrap();
        assert_eq!((t.port, t.application.as_str(), t.stream.as_str()), (1935, "live2", "abcd-1234"));
        let t = RtmpTarget::parse("rtmp://host/app/inline-key", None).unwrap();
        assert_eq!((t.application.as_str(), t.stream.as_str()), ("app", "inline-key"));
        assert!(RtmpTarget::parse("rtmp://host/app", None).is_err());
        assert!(RtmpTarget::parse("https://host/app", Some("k")).is_err());
    }

    /// Prueba de humo: escenas/capas (incluye texto), mezclador con dos
    /// entradas y dos salidas a la vez (grabación + un RTMP que falla y debe
    /// reintentar sin afectar la grabación).
    #[test]
    fn preview_then_record_test_sources() {
        use crate::settings::{AudioInput, Destination, Layer, TextStyle};
        let app = tauri::test::mock_app();
        let mut eng = Engine::new(app.handle().clone()).unwrap();
        let dir = std::env::temp_dir().join("transmitia-test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut s = Settings::default();
        s.resolution = Resolution::P720;
        s.orientation = Orientation::Portrait;
        s.fps = 60;
        s.recording.enabled = true;
        s.recording.folder = dir.to_string_lossy().to_string();
        s.scenes[0].layers[0].fit = Fit::Cover;
        s.scenes[0].layers.push(Layer {
            id: "pip".into(),
            source: "color:#3366ff".into(),
            rect: Rect { x: 0.6, y: 0.6, w: 0.35, h: 0.35 },
            fit: Fit::Stretch,
            visible: true,
            text: None,
        });
        s.scenes[0].layers.push(Layer {
            id: "title".into(),
            source: "text:title".into(),
            rect: Rect { x: 0.05, y: 0.8, w: 0.9, h: 0.12 },
            fit: Fit::Contain,
            visible: true,
            text: Some(TextStyle {
                content: "Hola TransmitIA".into(),
                font: "Sans".into(),
                bold: true,
                size: 60.0,
                color: "#ffffff".into(),
                outline: true,
                background: "#000000".into(),
                background_opacity: 0.5,
                align: "center".into(),
            }),
        });
        let copy = s.scenes[0].layers[0].clone();
        s.scenes[1].layers.push(copy);
        s.scenes[1].layers[0].id = "scene2-test".into();
        s.audio_inputs.push(AudioInput { id: "tone2".into(), source: "test".into(), volume: 0.5, muted: false });
        // Destino inalcanzable: debe fallar y reintentar sin tumbar la grabación.
        s.destinations.push(Destination {
            id: "broken".into(),
            name: "broken".into(),
            enabled: true,
            kind: DestinationKind::Rtmp,
            url: "rtmp://127.0.0.1:1/live".into(),
        });
        let mut keys = HashMap::new();
        keys.insert("broken".to_string(), "key".to_string());

        eng.start(&s, Mode::Preview, Secrets::default()).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        eng.start(&s, Mode::Live, Secrets { stream_keys: keys, api_key: None, translate_key: None }).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        // Cambios en vivo: escena, geometría, texto y volumen.
        s.scenes[0].layers[1].rect.x = 0.1;
        s.scenes[0].layers[2].text.as_mut().unwrap().content = "Cambiado en vivo".into();
        s.audio_inputs[1].muted = true;
        eng.apply_layout(&s);
        std::thread::sleep(Duration::from_secs(1));
        s.active_scene = 1;
        eng.apply_layout(&s);
        std::thread::sleep(Duration::from_secs(2));
        eng.stop();

        let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let size = std::fs::metadata(&file).unwrap().len();
        println!("recorded {} ({size} bytes)", file.display());
        assert!(size > 100_000, "recording too small: {size}");
    }
}
