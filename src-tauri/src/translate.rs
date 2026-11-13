//! Traducción en vivo de los segmentos transcritos (OpenAI o Gemini por HTTP).
//!
//! Los segmentos se traducen en orden, de uno en uno, con las últimas líneas
//! como contexto para mantener coherencia (nombres, género, tiempos verbales).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::settings::Provider;

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub api_key: String,
    /// Código ISO del idioma destino.
    pub target: String,
    /// Código ISO del idioma original; vacío = automático.
    pub source: String,
}

pub fn default_model(p: Provider) -> &'static str {
    match p {
        Provider::Openai => "gpt-5-nano",
        // Alias que Google mantiene apuntando al Flash estable más reciente.
        Provider::Gemini => "gemini-flash-latest",
    }
}

pub fn language_name(code: &str) -> &str {
    match code {
        "es" => "Spanish",
        "en" => "English",
        "pt" => "Portuguese",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese (Simplified)",
        other => other,
    }
}

/// (id del segmento, traducción)
pub type ResultFn = Arc<dyn Fn(String, String) + Send + Sync>;
pub type ErrorFn = Arc<dyn Fn(String) + Send + Sync>;

pub struct Translator {
    tx: mpsc::UnboundedSender<(String, String)>,
}

impl Translator {
    pub fn start(cfg: Config, on_result: ResultFn, on_error: ErrorFn) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tauri::async_runtime::spawn(worker(cfg, rx, on_result, on_error));
        Self { tx }
    }

    pub fn submit(&self, item_id: String, text: String) {
        let _ = self.tx.send((item_id, text));
    }
}

async fn worker(
    cfg: Config,
    mut rx: mpsc::UnboundedReceiver<(String, String)>,
    on_result: ResultFn,
    on_error: ErrorFn,
) {
    crate::transcribe::init_tls();
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
        Ok(c) => c,
        Err(e) => return on_error(format!("{e}")),
    };
    let mut ctx = Client { http: client, cfg, use_fast_hint: true, history: VecDeque::new() };
    while let Some((id, text)) = rx.recv().await {
        if text.trim().is_empty() {
            continue;
        }
        match ctx.translate(&text).await {
            Ok(t) => {
                ctx.remember(&text, &t);
                on_result(id, t);
            }
            Err(e) => {
                crate::log::write("error", &format!("translation: {e:#}"));
                on_error(format!("{e:#}"));
            }
        }
    }
}

struct Client {
    http: reqwest::Client,
    cfg: Config,
    /// Pedir latencia mínima (reasoning/thinking). Se desactiva si el modelo no lo admite.
    use_fast_hint: bool,
    history: VecDeque<(String, String)>,
}

impl Client {
    fn remember(&mut self, src: &str, dst: &str) {
        self.history.push_back((src.to_string(), dst.to_string()));
        while self.history.len() > 4 {
            self.history.pop_front();
        }
    }

    async fn translate(&mut self, text: &str) -> Result<String> {
        let (system, user) = build_prompt(&self.cfg, &self.history, text);
        for _ in 0..2 {
            let res = match self.cfg.provider {
                Provider::Openai => self.openai(&system, &user).await,
                Provider::Gemini => self.gemini(&system, &user).await,
            };
            match res {
                Err(e) if self.use_fast_hint && is_unsupported_param(&e.to_string()) => {
                    self.use_fast_hint = false;
                    continue;
                }
                other => return other.map(|t| clean(&t)),
            }
        }
        Err(anyhow!("translation failed"))
    }

    async fn openai(&self, system: &str, user: &str) -> Result<String> {
        let mut body = json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });
        if self.use_fast_hint {
            body["reasoning_effort"] = json!("minimal");
        }
        let v = send(
            self.http
                .post("https://api.openai.com/v1/chat/completions")
                .bearer_auth(&self.cfg.api_key)
                .json(&body),
        )
        .await?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("OpenAI: empty response"))
    }

    async fn gemini(&self, system: &str, user: &str) -> Result<String> {
        let model = self.cfg.model.trim_start_matches("models/");
        let mut body = json!({
            "systemInstruction": { "parts": [{ "text": system }] },
            "contents": [{ "role": "user", "parts": [{ "text": user }] }],
            "generationConfig": { "temperature": 0.2 }
        });
        if self.use_fast_hint {
            body["generationConfig"]["thinkingConfig"] = json!({ "thinkingBudget": 0 });
        }
        let v = send(
            self.http
                .post(format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"))
                .header("x-goog-api-key", &self.cfg.api_key)
                .json(&body),
        )
        .await?;
        let text: String = v["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        if text.is_empty() {
            return Err(anyhow!("Gemini: empty response"));
        }
        Ok(text)
    }
}

async fn send(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or("request failed");
        return Err(anyhow!("{status}: {msg}"));
    }
    Ok(v)
}

fn is_unsupported_param(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("400") && (m.contains("reasoning") || m.contains("thinking") || m.contains("unsupported"))
}

pub fn build_prompt(cfg: &Config, history: &VecDeque<(String, String)>, text: &str) -> (String, String) {
    let target = language_name(&cfg.target);
    let source = if cfg.source.trim().is_empty() { "the source language".to_string() } else { language_name(&cfg.source).to_string() };
    let system = format!(
        "You translate live speech captions from {source} into {target}. \
         Reply with the {target} translation only: no quotes, notes, or explanations. \
         Keep it natural and concise for subtitles, keep names and numbers, and do not answer questions in the text."
    );
    let mut user = String::new();
    if !history.is_empty() {
        user.push_str("Previous lines (context only, do not translate again):\n");
        for (s, t) in history {
            user.push_str(&format!("- {s} => {t}\n"));
        }
        user.push('\n');
    }
    user.push_str("Translate:\n");
    user.push_str(text.trim());
    (system, user)
}

/// Quita comillas o prefijos que a veces agregan los modelos.
fn clean(t: &str) -> String {
    let t = t.trim();
    let t = t.strip_prefix("Translation:").unwrap_or(t).trim();
    t.trim_matches(|c| c == '"' || c == '“' || c == '”').trim().to_string()
}

/// Modelos disponibles para la clave (para sugerirlos en la interfaz).
/// `purpose`: "text" (traducción/imágenes de texto) o "transcribe".
pub async fn list_models(provider: Provider, api_key: &str, purpose: &str) -> Result<Vec<String>> {
    crate::transcribe::init_tls();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let mut out: Vec<String> = match provider {
        Provider::Openai => {
            let v = send(http.get("https://api.openai.com/v1/models").bearer_auth(api_key)).await?;
            v["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str())
                .filter(|id| {
                    if purpose == "transcribe" {
                        id.contains("transcribe")
                    } else {
                        (id.starts_with("gpt-") || id.starts_with('o'))
                            && !["audio", "realtime", "transcribe", "tts", "image", "search", "embedding", "moderation"]
                                .iter()
                                .any(|x| id.contains(x))
                    }
                })
                .map(str::to_string)
                .collect()
        }
        Provider::Gemini => {
            let v = send(
                http.get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000")
                    .header("x-goog-api-key", api_key),
            )
            .await?;
            let method = if purpose == "transcribe" { "bidiGenerateContent" } else { "generateContent" };
            v["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| {
                    m["supportedGenerationMethods"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(method)))
                })
                .filter_map(|m| m["name"].as_str())
                .map(|n| n.trim_start_matches("models/").to_string())
                .collect()
        }
    };
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_context_and_target() {
        let cfg = Config { provider: Provider::Openai, model: "m".into(), api_key: "k".into(), target: "en".into(), source: "es".into() };
        let mut h = VecDeque::new();
        h.push_back(("Hola".to_string(), "Hello".to_string()));
        let (sys, user) = build_prompt(&cfg, &h, " ¿Cómo estás? ");
        assert!(sys.contains("from Spanish into English"));
        assert!(user.contains("- Hola => Hello"));
        assert!(user.ends_with("Translate:\n¿Cómo estás?"));
    }

    #[test]
    fn cleans_model_output() {
        assert_eq!(clean("  \"Hello world\" "), "Hello world");
        assert_eq!(clean("Translation: Hi"), "Hi");
    }

    /// Llamadas reales con clave inválida: deben devolver un error claro.
    /// `cargo test -- --ignored translate_bad_key`
    #[test]
    #[ignore]
    fn translate_bad_key() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            crate::transcribe::init_tls();
            for provider in [Provider::Openai, Provider::Gemini] {
                let cfg = Config { provider, model: default_model(provider).into(), api_key: "invalid".into(), target: "en".into(), source: "es".into() };
                let http = reqwest::Client::new();
                let mut c = Client { http, cfg, use_fast_hint: true, history: VecDeque::new() };
                let e = c.translate("Hola").await.unwrap_err().to_string();
                println!("{provider:?} translate: {e}");
                let m = list_models(provider, "invalid", "text").await.unwrap_err().to_string();
                println!("{provider:?} models: {m}");
                assert!(e.contains("40"), "{e}");
            }
        });
    }

    #[test]
    fn detects_unsupported_param_errors() {
        assert!(is_unsupported_param("400 Bad Request: Unsupported value: 'reasoning_effort'"));
        assert!(!is_unsupported_param("401 Unauthorized: bad key"));
    }
}
