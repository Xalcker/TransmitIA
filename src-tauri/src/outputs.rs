//! Salidas independientes (multidestino).
//!
//! El pipeline principal codifica una sola vez y entrega H.264/AAC por appsink.
//! Cada destino (RTMP, SRT o archivo) tiene su propio pipeline pequeño
//! `appsrc → mux → sink` con un hilo supervisor. Si un destino falla, solo ese
//! se reconecta (cada 3 s) y los demás siguen transmitiendo.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;
use parking_lot::Mutex;
use serde::Serialize;

use crate::media::{make, set_prop};

const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum Target {
    Rtmp { scheme: String, host: String, port: u16, application: String, stream: String },
    Srt { uri: String },
    File { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct OutputSpec {
    pub id: String,
    pub target: Target,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputStatus {
    pub id: String,
    /// connecting | live | reconnecting | error | stopped
    pub state: &'static str,
    pub message: Option<String>,
    pub kbps: f64,
}

pub type StatusFn = Arc<dyn Fn(OutputStatus) + Send + Sync>;

struct Feed {
    pipeline: gst::Pipeline,
    video: gst_app::AppSrc,
    audio: Option<gst_app::AppSrc>,
    /// Hasta el primer keyframe no se envía nada (inicio o reconexión).
    waiting_key: bool,
    offset: Option<gst::ClockTime>,
}

struct Output {
    spec: OutputSpec,
    has_audio: bool,
    feed: Mutex<Option<Feed>>,
    stopping: AtomicBool,
    bytes: Arc<AtomicU64>,
}

impl Output {
    fn build(&self) -> Result<Feed> {
        let pipeline = gst::Pipeline::with_name(&format!("out-{}", self.spec.id));
        let (mux, sink) = match &self.spec.target {
            Target::Rtmp { scheme, host, port, application, stream } => {
                let mux = make("flvmux")?;
                set_prop(&mux, "streamable", "true");
                let sink = make("rtmp2sink")?;
                set_prop(&sink, "scheme", scheme);
                sink.set_property("host", host);
                sink.set_property("port", *port as i32);
                sink.set_property("application", application);
                sink.set_property("stream", stream);
                (mux, sink)
            }
            Target::Srt { uri } => {
                let mux = make("mpegtsmux")?;
                set_prop(&mux, "alignment", "7");
                let sink = make("srtsink")?;
                sink.set_property("uri", uri);
                set_prop(&sink, "wait-for-connection", "false");
                (mux, sink)
            }
            Target::File { path } => {
                let mux = make("matroskamux")?;
                let sink = make("filesink")?;
                sink.set_property("location", path.to_string_lossy().to_string());
                (mux, sink)
            }
        };
        set_prop(&sink, "sync", "false");
        set_prop(&sink, "async", "false");

        let mk_src = || {
            gst_app::AppSrc::builder()
                .is_live(true)
                .format(gst::Format::Time)
                .stream_type(gst_app::AppStreamType::Stream)
                .max_bytes(32 * 1024 * 1024)
                .build()
        };
        let video = mk_src();
        let vq = make("queue")?;
        let vparse = make("h264parse")?;
        pipeline.add_many([video.upcast_ref(), &vq, &vparse, &mux, &sink])?;
        gst::Element::link_many([video.upcast_ref(), &vq, &vparse])?;
        vparse.link(&mux)?;
        mux.link(&sink)?;

        let audio = if self.has_audio {
            let a = mk_src();
            let aq = make("queue")?;
            let aparse = make("aacparse")?;
            pipeline.add_many([a.upcast_ref(), &aq, &aparse])?;
            gst::Element::link_many([a.upcast_ref(), &aq, &aparse])?;
            aparse.link(&mux)?;
            Some(a)
        } else {
            None
        };

        let counter = self.bytes.clone();
        sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                counter.fetch_add(b.size() as u64, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });

        Ok(Feed { pipeline, video, audio, waiting_key: true, offset: None })
    }

    fn push(&self, sample: &gst::Sample, is_video: bool) {
        let mut guard = self.feed.lock();
        let Some(feed) = guard.as_mut() else { return };
        let Some(buf) = sample.buffer() else { return };
        if is_video {
            if feed.waiting_key {
                if buf.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    return;
                }
                feed.waiting_key = false;
                crate::log::write("info", &format!("output {}: first keyframe", self.spec.id));
            }
        } else if feed.waiting_key || feed.audio.is_none() {
            return;
        }
        // Pasar a tiempo de ejecución (común a audio y video): los encoders
        // usan segmentos con desfase (x264/VideoToolbox empiezan en 1000 h).
        let Some(seg) = sample.segment().and_then(|sg| sg.downcast_ref::<gst::ClockTime>().cloned()) else {
            return;
        };
        let Some(rt) = buf.pts().and_then(|p| seg.to_running_time(p)) else { return };
        let offset = *feed.offset.get_or_insert(rt);
        if rt < offset {
            return;
        }
        let mut out = buf.copy();
        {
            let b = out.make_mut();
            let pts = rt - offset;
            b.set_pts(pts);
            let dts = buf
                .dts()
                .and_then(|d| seg.to_running_time(d))
                .and_then(|d| d.checked_sub(offset))
                .unwrap_or(pts);
            b.set_dts(dts.min(pts));
        }
        let caps = sample.caps().map(|c| c.to_owned());
        let mut sb = gst::Sample::builder().buffer(&out);
        if let Some(caps) = &caps {
            sb = sb.caps(caps);
        }
        let s = sb.build();
        let src = if is_video { &feed.video } else { feed.audio.as_ref().unwrap() };
        // Tras el EOS de cierre los push devuelven Eos: es esperado.
        let _ = src.push_sample(&s);
    }

    /// Hilo supervisor: arranca, vigila errores y reconecta.
    fn supervise(self: Arc<Self>, status: StatusFn) {
        let emit = |state: &'static str, message: Option<String>, kbps: f64| {
            status(OutputStatus { id: self.spec.id.clone(), state, message, kbps });
        };
        let is_file = matches!(self.spec.target, Target::File { .. });
        let mut first = true;
        while !self.stopping.load(Ordering::SeqCst) {
            if !first {
                emit("reconnecting", None, 0.0);
                let until = Instant::now() + RECONNECT_DELAY;
                while Instant::now() < until && !self.stopping.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
            }
            first = false;
            emit("connecting", None, 0.0);
            let feed = match self.build() {
                Ok(f) => f,
                Err(e) => {
                    emit("error", Some(format!("{e:#}")), 0.0);
                    crate::log::write("error", &format!("output {}: {e:#}", self.spec.id));
                    if is_file {
                        return;
                    }
                    continue;
                }
            };
            let pipeline = feed.pipeline.clone();
            let bus = pipeline.bus().unwrap();
            if pipeline.set_state(gst::State::Playing).is_err() {
                let _ = pipeline.set_state(gst::State::Null);
                emit("error", Some("could not start output".into()), 0.0);
                continue;
            }
            *self.feed.lock() = Some(feed);
            self.bytes.store(0, Ordering::Relaxed);

            let mut last = Instant::now();
            let mut eos_sent = false;
            let mut eos_deadline = None;
            let failed = loop {
                if self.stopping.load(Ordering::SeqCst) && !eos_sent {
                    // Cierre ordenado: EOS para finalizar archivos/conexiones.
                    if let Some(f) = self.feed.lock().as_ref() {
                        let _ = f.video.end_of_stream();
                        if let Some(a) = &f.audio {
                            let _ = a.end_of_stream();
                        }
                    }
                    eos_sent = true;
                    eos_deadline = Some(Instant::now() + Duration::from_secs(3));
                }
                if eos_deadline.is_some_and(|d| Instant::now() > d) {
                    break false;
                }
                if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            let text = e.error().to_string();
                            crate::log::write(
                                "error",
                                &format!("output {}: {text} | {}", self.spec.id, e.debug().unwrap_or_default()),
                            );
                            emit("error", Some(text), 0.0);
                            break true;
                        }
                        gst::MessageView::Eos(_) => break false,
                        _ => {}
                    }
                }
                let dt = last.elapsed();
                if dt >= Duration::from_secs(1) {
                    last = Instant::now();
                    let kbps = self.bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / dt.as_secs_f64();
                    emit(if kbps > 0.0 { "live" } else { "connecting" }, None, kbps);
                }
            };
            *self.feed.lock() = None;
            let _ = pipeline.set_state(gst::State::Null);
            if !failed || is_file {
                break;
            }
        }
        emit("stopped", None, 0.0);
    }
}

/// Conjunto de salidas activas mientras se está en vivo.
pub struct Outputs {
    list: Vec<Arc<Output>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Outputs {
    pub fn start(specs: Vec<OutputSpec>, has_audio: bool, status: StatusFn) -> Self {
        let list: Vec<Arc<Output>> = specs
            .into_iter()
            .map(|spec| {
                Arc::new(Output {
                    spec,
                    has_audio,
                    feed: Mutex::new(None),
                    stopping: AtomicBool::new(false),
                    bytes: Arc::new(AtomicU64::new(0)),
                })
            })
            .collect();
        let threads = list
            .iter()
            .map(|o| {
                let o = o.clone();
                let st = status.clone();
                std::thread::spawn(move || o.supervise(st))
            })
            .collect();
        Self { list, threads: Mutex::new(threads) }
    }

    pub fn push_video(&self, sample: &gst::Sample) {
        for o in &self.list {
            o.push(sample, true);
        }
    }

    pub fn push_audio(&self, sample: &gst::Sample) {
        for o in &self.list {
            o.push(sample, false);
        }
    }

    /// Cierre ordenado de todas las salidas (espera EOS hasta 3 s cada una).
    pub fn stop(&self) {
        for o in &self.list {
            o.stopping.store(true, Ordering::SeqCst);
        }
        let threads: Vec<_> = self.threads.lock().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

impl Drop for Outputs {
    fn drop(&mut self) {
        self.stop();
    }
}
