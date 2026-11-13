//! Pósters con IA: un cuadro capturado + prompt → modelo de imagen.
//!
//! - Gemini (Nano Banana): `models/{model}:generateContent` con la imagen como
//!   `inlineData`, `responseModalities: ["IMAGE"]` e `imageConfig.aspectRatio`.
//! - OpenAI (GPT Image): `POST /v1/images/edits` multipart con `image[]`.
//!
//! Los resultados se guardan en ~/Pictures/TransmitIA con un .json de metadatos.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::settings::Provider;

const STYLES_URL: &str = "https://raw.githubusercontent.com/Xalcker/image-gen-styles/main/catalog/styles.json";
const CATEGORIES_URL: &str = "https://raw.githubusercontent.com/Xalcker/image-gen-styles/main/catalog/categories.json";

pub fn default_model(p: Provider) -> &'static str {
    match p {
        Provider::Openai => "gpt-image-2.6-flare",
        Provider::Gemini => "gemini-3.1-flash-image",
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub provider: Provider,
    pub model: String,
    /// Prompt final (estilo + plantilla ya con variables resueltas).
    pub prompt: String,
    /// "16:9", "9:16" o "1:1"
    pub aspect: String,
    /// low | medium | high (OpenAI); Gemini usa 1K/2K según esto.
    pub quality: String,
    /// Fuente capturada (para los metadatos).
    pub source: String,
    pub style_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PosterInfo {
    pub path: String,
    pub created: u64,
    pub prompt: String,
    pub style_id: Option<String>,
    pub provider: Provider,
    pub model: String,
    pub aspect: String,
}

pub fn posters_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow!("cannot find home folder"))?;
    let dir = PathBuf::from(home).join("Pictures").join("TransmitIA");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Genera un póster a partir del cuadro `frame_jpeg` y lo guarda.
pub async fn generate(req: &Request, api_key: &str, frame_jpeg: Vec<u8>) -> Result<PosterInfo> {
    crate::transcribe::init_tls();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(180)).build()?;
    let model = if req.model.trim().is_empty() { default_model(req.provider).to_string() } else { req.model.trim().to_string() };
    let (bytes, ext) = match req.provider {
        Provider::Gemini => gemini(&http, api_key, &model, req, &frame_jpeg).await?,
        Provider::Openai => openai(&http, api_key, &model, req, frame_jpeg).await?,
    };
    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let path = posters_dir()?.join(format!("poster-{created}.{ext}"));
    std::fs::write(&path, &bytes).context("saving poster")?;
    let info = PosterInfo {
        path: path.to_string_lossy().to_string(),
        created,
        prompt: req.prompt.clone(),
        style_id: req.style_id.clone(),
        provider: req.provider,
        model,
        aspect: req.aspect.clone(),
    };
    std::fs::write(path.with_extension("json"), serde_json::to_vec_pretty(&info)?)?;
    Ok(info)
}

async fn gemini(http: &reqwest::Client, key: &str, model: &str, req: &Request, frame: &[u8]) -> Result<(Vec<u8>, &'static str)> {
    let model = model.trim_start_matches("models/");
    let size = if req.quality == "high" { "2K" } else { "1K" };
    let body = |modalities: Value| {
        json!({
            "contents": [{
                "role": "user",
                "parts": [
                    { "text": req.prompt },
                    { "inlineData": { "mimeType": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(frame) } }
                ]
            }],
            "generationConfig": {
                "responseModalities": modalities,
                "imageConfig": { "aspectRatio": req.aspect, "imageSize": size }
            }
        })
    };
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
    let mut v = post_json(http.post(&url).header("x-goog-api-key", key).json(&body(json!(["IMAGE"])))).await;
    // Algunos modelos exigen TEXT+IMAGE.
    if matches!(&v, Err(e) if e.to_string().to_ascii_lowercase().contains("modalit")) {
        v = post_json(http.post(&url).header("x-goog-api-key", key).json(&body(json!(["TEXT", "IMAGE"])))).await;
    }
    let v = v?;
    let (mime, data) = find_inline_image(&v).ok_or_else(|| {
        let reason = v["candidates"][0]["finishReason"].as_str().unwrap_or("");
        let text = v["candidates"][0]["content"]["parts"][0]["text"].as_str().unwrap_or("");
        anyhow!("Gemini returned no image {reason} {text}")
    })?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    Ok((bytes, if mime.contains("png") { "png" } else { "jpg" }))
}

/// Busca la primera imagen en línea en cualquier parte de la respuesta
/// (inlineData / inline_data con mimeType image/*).
pub fn find_inline_image(v: &Value) -> Option<(String, String)> {
    match v {
        Value::Object(map) => {
            for key in ["inlineData", "inline_data"] {
                if let Some(d) = map.get(key) {
                    let mime = d.get("mimeType").or_else(|| d.get("mime_type")).and_then(Value::as_str).unwrap_or("");
                    if let (true, Some(data)) = (mime.starts_with("image/"), d.get("data").and_then(Value::as_str)) {
                        return Some((mime.to_string(), data.to_string()));
                    }
                }
            }
            map.values().find_map(find_inline_image)
        }
        Value::Array(a) => a.iter().find_map(find_inline_image),
        _ => None,
    }
}

async fn openai(http: &reqwest::Client, key: &str, model: &str, req: &Request, frame: Vec<u8>) -> Result<(Vec<u8>, &'static str)> {
    let size = match req.aspect.as_str() {
        "9:16" => "1024x1536",
        "16:9" => "1536x1024",
        _ => "1024x1024",
    };
    let part = reqwest::multipart::Part::bytes(frame).file_name("frame.jpg").mime_str("image/jpeg")?;
    let form = reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .text("prompt", req.prompt.clone())
        .text("size", size)
        .text("quality", if req.quality.is_empty() { "medium".to_string() } else { req.quality.clone() })
        .text("output_format", "jpeg")
        .part("image[]", part);
    let v = post_json(http.post("https://api.openai.com/v1/images/edits").bearer_auth(key).multipart(form)).await?;
    let b64 = v["data"][0]["b64_json"].as_str().ok_or_else(|| anyhow!("OpenAI returned no image"))?;
    Ok((base64::engine::general_purpose::STANDARD.decode(b64)?, "jpg"))
}

async fn post_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or("request failed");
        return Err(anyhow!("{status}: {msg}"));
    }
    Ok(v)
}

/// Pósters guardados, más recientes primero.
pub fn list() -> Result<Vec<PosterInfo>> {
    let mut out: Vec<PosterInfo> = std::fs::read_dir(posters_dir()?)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<PosterInfo>(&b).ok()))
        .filter(|i| Path::new(&i.path).is_file())
        .collect();
    out.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(out)
}

/// Borra un póster (imagen + metadatos); solo dentro de la carpeta de pósters.
pub fn delete(path: &str) -> Result<()> {
    let dir = posters_dir()?.canonicalize()?;
    let p = Path::new(path).canonicalize()?;
    if !p.starts_with(&dir) {
        return Err(anyhow!("not a poster"));
    }
    let _ = std::fs::remove_file(p.with_extension("json"));
    std::fs::remove_file(&p)?;
    Ok(())
}

/// Catálogo de estilos (Xalcker/image-gen-styles) con copia local para usar sin red.
pub async fn styles(cache_dir: &Path) -> Result<Value> {
    crate::transcribe::init_tls();
    let cache = cache_dir.join("styles-cache.json");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let fetched: Result<Value> = async {
        let styles: Value = http.get(STYLES_URL).send().await?.error_for_status()?.json().await?;
        let categories: Value = http.get(CATEGORIES_URL).send().await?.error_for_status()?.json().await?;
        Ok(json!({ "styles": styles, "categories": categories }))
    }
    .await;
    match fetched {
        Ok(v) => {
            let _ = std::fs::create_dir_all(cache_dir);
            let _ = std::fs::write(&cache, v.to_string());
            Ok(v)
        }
        Err(e) => {
            let b = std::fs::read(&cache).map_err(|_| e)?;
            Ok(serde_json::from_slice(&b)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_image_in_gemini_response() {
        let v = json!({"candidates":[{"content":{"parts":[{"text":"aquí está"},{"inlineData":{"mimeType":"image/png","data":"QUJD"}}]}}]});
        assert_eq!(find_inline_image(&v), Some(("image/png".into(), "QUJD".into())));
        assert_eq!(find_inline_image(&json!({"candidates":[{"content":{"parts":[{"text":"no"}]}}]})), None);
    }

    /// Llamadas reales con clave inválida. `cargo test -- --ignored poster_bad_key`
    #[test]
    #[ignore]
    fn poster_bad_key() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            for provider in [Provider::Gemini, Provider::Openai] {
                let req = Request {
                    provider,
                    model: String::new(),
                    prompt: "poster".into(),
                    aspect: "9:16".into(),
                    quality: "low".into(),
                    source: "test".into(),
                    style_id: None,
                };
                let e = generate(&req, "invalid", vec![0xff, 0xd8, 0xff, 0xd9]).await.unwrap_err().to_string();
                println!("{provider:?}: {e}");
                assert!(e.contains("40"), "{e}");
            }
            let dir = std::env::temp_dir().join("transmitia-styles");
            let v = styles(&dir).await.unwrap();
            println!("styles: {}", v["styles"].as_array().map_or(0, |a| a.len()));
            assert!(v["styles"].as_array().is_some_and(|a| !a.is_empty()));
        });
    }
}
