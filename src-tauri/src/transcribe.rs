//! Transcripción en tiempo real en la nube (OpenAI o Gemini) por WebSocket.
//!
//! El motor entrega PCM16 mono a la tasa que pide cada proveedor; este módulo
//! lo envía, interpreta los eventos de transcripción y los entrega como
//! `TranscriptEvent`. Si la conexión cae, reintenta cada 3 s mientras siga activo.
//!
//! - OpenAI: `wss://api.openai.com/v1/realtime?intent=transcription`, PCM 24 kHz,
//!   `session.update` (type=transcription) + `input_audio_buffer.append/commit`.
//!   El modelo de streaming no hace VAD en el servidor: confirmamos turnos con un
//!   VAD sencillo por energía.
//! - Gemini Live: `BidiGenerateContent`, PCM 16 kHz, `inputAudioTranscription`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use crate::settings::Provider;

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    /// Código ISO (es, en…); vacío = detección automática.
    pub language: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Texto nuevo que se agrega al segmento (no cambia lo anterior).
    Delta,
    /// Texto provisional que reemplaza al segmento completo.
    Interim,
    /// Texto definitivo del segmento.
    Final,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEvent {
    pub item_id: String,
    pub text: String,
    pub kind: Kind,
}

pub type EventFn = Arc<dyn Fn(TranscriptEvent) + Send + Sync>;
/// (estado, mensaje): connecting | active | reconnecting | error
pub type StatusFn = Arc<dyn Fn(&'static str, Option<String>) + Send + Sync>;

/// rustls necesita un proveedor criptográfico explícito (usamos ring).
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn sample_rate(p: Provider) -> i32 {
    match p {
        Provider::Openai => 24_000,
        Provider::Gemini => 16_000,
    }
}

pub fn default_model(p: Provider) -> &'static str {
    match p {
        Provider::Openai => "gpt-live-transcribe",
        Provider::Gemini => "gemini-3.5-transcribe-live",
    }
}

/// Mientras exista, la sesión sigue; al soltarlo se cierra la conexión.
pub struct Transcriber {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Transcriber {
    pub fn start(cfg: Config, on_event: EventFn, on_status: StatusFn) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
        tauri::async_runtime::spawn(run(cfg, rx, on_event, on_status));
        Self { tx }
    }

    /// PCM16 LE mono. Si la red va lenta se descarta audio en vez de bloquear el video.
    pub fn push_audio(&self, pcm: Vec<u8>) {
        let _ = self.tx.try_send(pcm);
    }
}

enum End {
    /// El motor soltó el Transcriber: terminar.
    Closed,
}

async fn run(cfg: Config, mut rx: mpsc::Receiver<Vec<u8>>, on_event: EventFn, on_status: StatusFn) {
    init_tls();
    let mut attempt = 0u32;
    loop {
        on_status(if attempt == 0 { "connecting" } else { "reconnecting" }, None);
        let res = match cfg.provider {
            Provider::Openai => openai_session(&cfg, &mut rx, &on_event, &on_status).await,
            Provider::Gemini => gemini_session(&cfg, &mut rx, &on_event, &on_status).await,
        };
        match res {
            Ok(End::Closed) => break,
            Err(e) => {
                let msg = format!("{e:#}");
                crate::log::write("error", &format!("transcription: {msg}"));
                on_status("error", Some(msg));
            }
        }
        attempt += 1;
        // Esperar antes de reintentar, vaciando el audio acumulado.
        let wait = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => break,
                msg = rx.recv() => if msg.is_none() { return },
            }
        }
    }
}

// ---------------------------------------------------------------- OpenAI

/// VAD por energía para decidir cuándo confirmar un turno.
struct Vad {
    rate: f64,
    speech_ms: f64,
    silence_ms: f64,
    pending: bool,
}

impl Vad {
    const THRESHOLD: f64 = 0.012; // RMS normalizado (~ -38 dBFS)
    const END_SILENCE_MS: f64 = 700.0;
    const MAX_TURN_MS: f64 = 12_000.0;

    /// Devuelve true cuando conviene hacer commit.
    fn feed(&mut self, pcm: &[u8]) -> bool {
        let n = pcm.len() / 2;
        if n == 0 {
            return false;
        }
        let sum: f64 = pcm
            .chunks_exact(2)
            .map(|b| {
                let v = i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0;
                v * v
            })
            .sum();
        let rms = (sum / n as f64).sqrt();
        let ms = n as f64 * 1000.0 / self.rate;
        if rms > Self::THRESHOLD {
            self.pending = true;
            self.speech_ms += ms;
            self.silence_ms = 0.0;
        } else if self.pending {
            self.silence_ms += ms;
        }
        if self.pending && (self.silence_ms >= Self::END_SILENCE_MS || self.speech_ms >= Self::MAX_TURN_MS) {
            self.pending = false;
            self.speech_ms = 0.0;
            self.silence_ms = 0.0;
            return true;
        }
        false
    }
}

pub fn openai_session_update(model: &str, language: &str) -> Value {
    let mut transcription = json!({ "model": model });
    if !language.trim().is_empty() {
        transcription["languages"] = json!([language.trim()]);
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "transcription": transcription,
                    "turn_detection": null
                }
            }
        }
    })
}

/// Interpreta un evento del servidor de OpenAI.
pub fn parse_openai(v: &Value) -> Result<Option<TranscriptEvent>> {
    let ty = v["type"].as_str().unwrap_or_default();
    let item_id = v["item_id"].as_str().unwrap_or_default().to_string();
    match ty {
        "conversation.item.input_audio_transcription.delta" => Ok(Some(TranscriptEvent {
            item_id,
            text: v["delta"].as_str().unwrap_or_default().to_string(),
            kind: Kind::Delta,
        })),
        "conversation.item.input_audio_transcription.completed" => Ok(Some(TranscriptEvent {
            item_id,
            text: v["transcript"].as_str().unwrap_or_default().to_string(),
            kind: Kind::Final,
        })),
        "error" | "conversation.item.input_audio_transcription.failed" => {
            let msg = v["error"]["message"].as_str().unwrap_or("transcription error");
            Err(anyhow!("OpenAI: {msg}"))
        }
        _ => Ok(None),
    }
}

async fn openai_session(
    cfg: &Config,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    on_event: &EventFn,
    on_status: &StatusFn,
) -> Result<End> {
    let mut req = "wss://api.openai.com/v1/realtime?intent=transcription".into_client_request()?;
    req.headers_mut().insert("Authorization", format!("Bearer {}", cfg.api_key).parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req).await.context("OpenAI connection")?;
    let (mut write, mut read) = ws.split();
    write
        .send(Message::text(openai_session_update(&cfg.model, &cfg.language).to_string()))
        .await?;
    let mut active = false;

    let mut vad = Vad { rate: 24_000.0, speech_ms: 0.0, silence_ms: 0.0, pending: false };
    let mut buf: Vec<u8> = Vec::with_capacity(9600);
    loop {
        tokio::select! {
            audio = rx.recv() => {
                let Some(pcm) = audio else {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(End::Closed);
                };
                let commit = vad.feed(&pcm);
                buf.extend_from_slice(&pcm);
                // ~100 ms por mensaje.
                if buf.len() >= 4800 || commit {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
                    buf.clear();
                    write.send(Message::text(json!({ "type": "input_audio_buffer.append", "audio": b64 }).to_string())).await?;
                }
                if commit {
                    write.send(Message::text(json!({ "type": "input_audio_buffer.commit" }).to_string())).await?;
                }
            }
            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow!("OpenAI closed the connection"))??;
                match msg {
                    Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t)?;
                        let parsed = parse_openai(&v)?;
                        if !active {
                            active = true;
                            on_status("active", None);
                        }
                        if let Some(ev) = parsed {
                            on_event(ev);
                        }
                    }
                    Message::Close(f) => {
                        return Err(anyhow!("OpenAI closed: {}", f.map(|f| f.reason.to_string()).unwrap_or_default()));
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------- Gemini

pub fn gemini_setup(model: &str, language: &str) -> Value {
    let model = if model.starts_with("models/") { model.to_string() } else { format!("models/{model}") };
    let langs: Vec<&str> = if language.trim().is_empty() { vec![] } else { vec![language.trim()] };
    json!({
        "setup": {
            "model": model,
            "generationConfig": { "responseModalities": ["TEXT"] },
            "inputAudioTranscription": { "languageCodes": langs }
        }
    })
}

/// Interpreta un mensaje de Gemini. `seq` numera los segmentos (Gemini no da ids).
pub fn parse_gemini(v: &Value, seq: &mut u64) -> Result<Option<TranscriptEvent>> {
    if let Some(err) = v.get("error") {
        return Err(anyhow!("Gemini: {}", err["message"].as_str().unwrap_or("error")));
    }
    let sc = &v["serverContent"];
    if let Some(t) = sc["inputTranscription"]["text"].as_str() {
        let ev = TranscriptEvent { item_id: format!("g{seq}"), text: t.to_string(), kind: Kind::Final };
        *seq += 1;
        return Ok(Some(ev));
    }
    if let Some(t) = sc["interimInputTranscription"]["text"].as_str() {
        return Ok(Some(TranscriptEvent { item_id: format!("g{seq}"), text: t.to_string(), kind: Kind::Interim }));
    }
    Ok(None)
}

async fn gemini_session(
    cfg: &Config,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    on_event: &EventFn,
    on_status: &StatusFn,
) -> Result<End> {
    let url = format!(
        "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={}",
        cfg.api_key
    );
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("Gemini connection")?;
    let (mut write, mut read) = ws.split();
    write.send(Message::text(gemini_setup(&cfg.model, &cfg.language).to_string())).await?;
    let mut active = false;

    let mut seq = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(3200);
    loop {
        tokio::select! {
            audio = rx.recv() => {
                let Some(pcm) = audio else {
                    let _ = write.send(Message::text(json!({ "realtimeInput": { "audioStreamEnd": true } }).to_string())).await;
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(End::Closed);
                };
                buf.extend_from_slice(&pcm);
                if buf.len() >= 3200 {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
                    buf.clear();
                    let m = json!({ "realtimeInput": { "audio": { "data": b64, "mimeType": "audio/pcm;rate=16000" } } });
                    write.send(Message::text(m.to_string())).await?;
                }
            }
            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow!("Gemini closed the connection"))??;
                // Gemini envía JSON tanto en marcos de texto como binarios.
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(f) => {
                        return Err(anyhow!("Gemini closed: {}", f.map(|f| f.reason.to_string()).unwrap_or_default()));
                    }
                    _ => continue,
                };
                let v: Value = serde_json::from_str(&text)?;
                let parsed = parse_gemini(&v, &mut seq)?;
                if !active {
                    active = true;
                    on_status("active", None);
                }
                if let Some(ev) = parsed {
                    on_event(ev);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_messages() {
        let u = openai_session_update("gpt-live-transcribe", "es");
        assert_eq!(u["session"]["type"], "transcription");
        assert_eq!(u["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(u["session"]["audio"]["input"]["transcription"]["languages"][0], "es");
        assert!(u["session"]["audio"]["input"]["turn_detection"].is_null());

        let d = json!({"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","content_index":0,"delta":"Hola"});
        let ev = parse_openai(&d).unwrap().unwrap();
        assert_eq!((ev.item_id.as_str(), ev.text.as_str(), ev.kind), ("i1", "Hola", Kind::Delta));
        let c = json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","transcript":"Hola mundo"});
        assert_eq!(parse_openai(&c).unwrap().unwrap().kind, Kind::Final);
        assert!(parse_openai(&json!({"type":"error","error":{"message":"bad key"}})).is_err());
        assert!(parse_openai(&json!({"type":"session.updated"})).unwrap().is_none());
    }

    #[test]
    fn gemini_messages() {
        let s = gemini_setup("gemini-3.5-transcribe-live", "");
        assert_eq!(s["setup"]["model"], "models/gemini-3.5-transcribe-live");
        assert_eq!(s["setup"]["inputAudioTranscription"]["languageCodes"], json!([]));
        let mut seq = 0;
        let i = parse_gemini(&json!({"serverContent":{"interimInputTranscription":{"text":"hol"}}}), &mut seq).unwrap().unwrap();
        assert_eq!((i.kind, i.item_id.as_str()), (Kind::Interim, "g0"));
        let f = parse_gemini(&json!({"serverContent":{"inputTranscription":{"text":"hola"}}}), &mut seq).unwrap().unwrap();
        assert_eq!((f.kind, f.item_id.as_str()), (Kind::Final, "g0"));
        assert_eq!(seq, 1);
    }

    /// Conexión real con clave inválida: debe reportar error sin colgarse.
    /// `cargo test -- --ignored transcription_bad_key`
    #[test]
    #[ignore]
    fn transcription_bad_key() {
        for provider in [Provider::Openai, Provider::Gemini] {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let cfg = Config { provider, model: default_model(provider).into(), language: "es".into(), api_key: "invalid".into() };
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let (atx, arx) = mpsc::channel::<Vec<u8>>(8);
                let on_event: EventFn = Arc::new(|_| {});
                let tx2 = tx.clone();
                let on_status: StatusFn = Arc::new(move |st, msg| {
                    let _ = tx2.send(format!("{st}: {}", msg.unwrap_or_default()));
                });
                let task = tokio::spawn(run(cfg, arx, on_event, on_status));
                for _ in 0..20 {
                    let _ = atx.send(vec![0u8; 3200]).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                drop(atx);
                let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
            });
            let all: Vec<String> = rx.try_iter().collect();
            println!("{provider:?}: {all:?}");
            assert!(all.iter().any(|m| m.starts_with("error")), "{provider:?} should report an error: {all:?}");
        }
    }

    #[test]
    fn vad_commits_after_silence() {
        let mut vad = Vad { rate: 24_000.0, speech_ms: 0.0, silence_ms: 0.0, pending: false };
        let loud: Vec<u8> = (0..2400).flat_map(|i| (((i % 50) as i16 - 25) * 400).to_le_bytes()).collect();
        let quiet = vec![0u8; 4800];
        assert!(!vad.feed(&loud));
        let mut committed = false;
        for _ in 0..8 {
            committed |= vad.feed(&quiet);
        }
        assert!(committed);
    }
}
