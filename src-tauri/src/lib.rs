mod engine;
mod gst_env;
mod log;
mod media;
mod outputs;
mod poster;
mod permissions;
mod settings;
mod transcribe;
mod translate;

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{Manager, State};

use engine::{Engine, Mode};
use media::{EncoderInfo, SourceInfo};
use settings::Settings;

const KEYCHAIN_SERVICE: &str = "edu.asu.transmitia";
const KEYCHAIN_STREAM_KEY: &str = "stream-key";

struct AppState {
    engine: Mutex<Engine>,
    settings_path: PathBuf,
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    let msg = format!("{e:#}");
    log::write("error", &msg);
    msg
}

#[derive(Serialize)]
struct Sources {
    video: Vec<SourceInfo>,
    audio: Vec<SourceInfo>,
}

#[tauri::command]
async fn list_sources(state: State<'_, AppState>, refresh: bool) -> CmdResult<Sources> {
    let mut eng = state.engine.lock();
    if refresh {
        eng.devices.refresh();
    }
    Ok(Sources { video: eng.devices.video_sources(), audio: eng.devices.audio_sources() })
}

#[tauri::command]
async fn list_encoders() -> CmdResult<Vec<EncoderInfo>> {
    Ok(media::available_encoders())
}

#[tauri::command]
async fn start_preview(state: State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    state.engine.lock().start(&settings, Mode::Preview, secrets_for(&settings, false)).map_err(err)
}

#[tauri::command]
async fn go_live(state: State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    let secrets = secrets_for(&settings, true);
    let mut eng = state.engine.lock();
    if let Err(e) = eng.start(&settings, Mode::Live, secrets) {
        // Si falla, volvemos a la vista previa para no dejar la pantalla negra.
        let _ = eng.start(&settings, Mode::Preview, secrets_for(&settings, false));
        return Err(err(e));
    }
    Ok(())
}

/// Termina la transmisión/grabación y regresa a la vista previa.
/// Cambios de composición en vivo (no reinicia la transmisión).
#[tauri::command]
async fn apply_layout(state: State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    state.engine.lock().apply_layout(&settings);
    Ok(())
}

#[tauri::command]
async fn end_live(state: State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    let mut eng = state.engine.lock();
    eng.stop();
    eng.start(&settings, Mode::Preview, secrets_for(&settings, false)).map_err(err)
}

#[tauri::command]
async fn stop_all(state: State<'_, AppState>) -> CmdResult<()> {
    state.engine.lock().stop();
    Ok(())
}

#[tauri::command]
async fn engine_mode(state: State<'_, AppState>) -> CmdResult<Mode> {
    Ok(state.engine.lock().mode())
}

#[tauri::command]
async fn load_settings(state: State<'_, AppState>) -> CmdResult<Settings> {
    Ok(std::fs::read_to_string(&state.settings_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
        .normalized())
}

#[tauri::command]
async fn save_settings(state: State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    if let Some(dir) = state.settings_path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let json = serde_json::to_string_pretty(&settings).map_err(err)?;
    std::fs::write(&state.settings_path, json).map_err(err)
}

/// Claves del llavero necesarias para arrancar.
fn secrets_for(settings: &Settings, live: bool) -> engine::Secrets {
    let stream_keys = if live {
        settings
            .destinations
            .iter()
            .filter(|d| d.enabled)
            .filter_map(|d| read_stream_key(&d.id).map(|k| (d.id.clone(), k)))
            .collect()
    } else {
        HashMap::new()
    };
    let api_key = settings.transcription.enabled.then(|| read_api_key(settings.transcription.provider)).flatten();
    let translate_key = settings.translation.enabled.then(|| read_api_key(settings.translation.provider)).flatten();
    engine::Secrets { stream_keys, api_key, translate_key }
}

fn api_key_entry(p: settings::Provider) -> keyring::Result<keyring::Entry> {
    let name = match p {
        settings::Provider::Openai => "api-key:openai",
        settings::Provider::Gemini => "api-key:gemini",
    };
    keyring::Entry::new(KEYCHAIN_SERVICE, name)
}

fn read_api_key(p: settings::Provider) -> Option<String> {
    api_key_entry(p).ok()?.get_password().ok().filter(|k| !k.is_empty())
}

/// Claves de API de IA: solo en el llavero del sistema.
#[tauri::command]
async fn set_api_key(provider: settings::Provider, key: String) -> CmdResult<()> {
    let entry = api_key_entry(provider).map_err(err)?;
    if key.trim().is_empty() {
        let _ = entry.delete_credential();
        Ok(())
    } else {
        entry.set_password(key.trim()).map_err(err)
    }
}

/// Modelos que ofrece el proveedor con la clave guardada (`purpose`: text | transcribe).
#[tauri::command]
async fn list_models(provider: settings::Provider, purpose: String) -> CmdResult<Vec<String>> {
    let key = read_api_key(provider).ok_or_else(|| "no-api-key".to_string())?;
    translate::list_models(provider, &key, &purpose).await.map_err(err)
}

/// Cuadro actual de una fuente (o `program`) en JPEG base64, para la vista previa del póster.
#[tauri::command]
async fn capture_frame(state: State<'_, AppState>, source: String) -> CmdResult<String> {
    use base64::Engine as _;
    let snaps = state.engine.lock().snapshots().ok_or_else(|| "not-running".to_string())?;
    let jpeg = tauri::async_runtime::spawn_blocking(move || snaps.capture_jpeg(&source, 640))
        .await
        .map_err(err)?
        .map_err(err)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(jpeg))
}

#[tauri::command]
async fn generate_poster(state: State<'_, AppState>, request: poster::Request) -> CmdResult<poster::PosterInfo> {
    let key = read_api_key(request.provider).ok_or_else(|| "no-api-key".to_string())?;
    let snaps = state.engine.lock().snapshots().ok_or_else(|| "not-running".to_string())?;
    let source = request.source.clone();
    let frame = tauri::async_runtime::spawn_blocking(move || snaps.capture_jpeg(&source, 1536))
        .await
        .map_err(err)?
        .map_err(err)?;
    let info = poster::generate(&request, &key, frame).await.map_err(err)?;
    log::write("info", &format!("poster saved {}", info.path));
    Ok(info)
}

#[tauri::command]
async fn list_posters() -> CmdResult<Vec<poster::PosterInfo>> {
    poster::list().map_err(err)
}

#[tauri::command]
async fn delete_poster(path: String) -> CmdResult<()> {
    poster::delete(&path).map_err(err)
}

#[tauri::command]
async fn poster_styles(app: tauri::AppHandle) -> CmdResult<serde_json::Value> {
    let dir = app.path().app_cache_dir().map_err(err)?;
    poster::styles(&dir).await.map_err(err)
}

/// Agrega en caliente una capa de imagen/color/texto (permitido en vivo).
#[tauri::command]
async fn add_layer_live(state: State<'_, AppState>, settings: Settings, layer_id: String) -> CmdResult<()> {
    state.engine.lock().add_layer_live(&settings, &layer_id).map_err(err)
}

#[tauri::command]
async fn api_key_status() -> CmdResult<HashMap<String, bool>> {
    Ok(HashMap::from([
        ("openai".to_string(), read_api_key(settings::Provider::Openai).is_some()),
        ("gemini".to_string(), read_api_key(settings::Provider::Gemini).is_some()),
    ]))
}

fn key_entry(dest_id: &str) -> keyring::Result<keyring::Entry> {
    // El destino migrado de la fase 1 conserva su entrada original.
    let user = if dest_id == settings::LEGACY_DESTINATION_ID {
        KEYCHAIN_STREAM_KEY.to_string()
    } else {
        format!("{KEYCHAIN_STREAM_KEY}:{dest_id}")
    };
    keyring::Entry::new(KEYCHAIN_SERVICE, &user)
}

/// La clave de cada destino vive en el llavero del sistema, nunca en el JSON.
#[tauri::command]
async fn set_stream_key(dest_id: String, key: String) -> CmdResult<()> {
    let entry = key_entry(&dest_id).map_err(err)?;
    if key.trim().is_empty() {
        let _ = entry.delete_credential();
        Ok(())
    } else {
        entry.set_password(key.trim()).map_err(err)
    }
}

/// Qué destinos tienen clave guardada.
#[tauri::command]
async fn stream_key_status(ids: Vec<String>) -> CmdResult<HashMap<String, bool>> {
    Ok(ids.into_iter().map(|id| { let has = read_stream_key(&id).is_some(); (id, has) }).collect())
}

fn read_stream_key(dest_id: &str) -> Option<String> {
    key_entry(dest_id).ok()?.get_password().ok().filter(|k| !k.is_empty())
}

#[tauri::command]
async fn log_file_path() -> CmdResult<Option<String>> {
    Ok(log::path().map(|p| p.to_string_lossy().to_string()))
}


/// Autoprueba de diagnóstico: `open TransmitIA.app --env TRANSMITIA_SELFTEST=dev:0`
/// (opcional `--env TRANSMITIA_DUMP_DIR=/carpeta` para guardar un cuadro por paso).
/// Recorre ajustes y composiciones en vivo con la fuente indicada y deja fps en el log.
fn spawn_selftest(app: tauri::AppHandle, source: String) {
    use settings::{Fit, Layer, Orientation, Rect};
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let state = app.state::<AppState>();
        let mut s = Settings::default();
        s.audio_inputs.clear();
        s.scenes[0].layers = vec![Layer {
            id: "cam".into(),
            source: source.clone(),
            rect: Rect::FULL,
            fit: Fit::Contain,
            visible: true,
            text: None,
        }];
        s.scenes[1].layers = vec![
            Layer { id: "bg".into(), source: "color:#203040".into(), rect: Rect::FULL, fit: Fit::Stretch, visible: true, text: None },
            Layer {
                id: "pip".into(),
                source,
                rect: Rect { x: 0.55, y: 0.55, w: 0.4, h: 0.4 },
                fit: Fit::Cover,
                visible: true,
                text: None,
            },
        ];
        let dump = |name: &str| {
            if let Ok(dir) = std::env::var("TRANSMITIA_DUMP_DIR") {
                let d = std::path::Path::new(&dir);
                let _ = std::fs::copy(d.join("latest.jpg"), d.join(format!("{name}.jpg")));
            }
        };
        let wait = || std::thread::sleep(std::time::Duration::from_secs(4));
        for o in [Orientation::Landscape, Orientation::Portrait] {
            s.orientation = o;
            s.active_scene = 0;
            if let Err(e) = state.engine.lock().start(&s, Mode::Preview, engine::Secrets::default()) {
                log::write("error", &format!("selftest start failed: {e:#}"));
                continue;
            }
            for f in [Fit::Contain, Fit::Cover, Fit::Stretch] {
                s.scenes[0].layers[0].fit = f;
                state.engine.lock().apply_layout(&s);
                wait();
                dump(&format!("{o:?}-{f:?}"));
            }
            s.active_scene = 1;
            state.engine.lock().apply_layout(&s);
            wait();
            dump(&format!("{o:?}-scene2"));
        }
        state.engine.lock().stop();
        log::write("info", "selftest done");
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Antes de cualquier hilo y de gst::init(): GStreamer empaquetado (si lo hay).
    gst_env::configure();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            log::init(app.path().app_log_dir()?);
            log::write("info", &format!("TransmitIA {} starting", env!("CARGO_PKG_VERSION")));
            let engine = Engine::new(app.handle().clone())?;
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            app.manage(AppState { engine: Mutex::new(engine), settings_path });
            if let Ok(src) = std::env::var("TRANSMITIA_SELFTEST") {
                spawn_selftest(app.handle().clone(), src);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_sources,
            list_encoders,
            start_preview,
            go_live,
            apply_layout,
            end_live,
            stop_all,
            engine_mode,
            load_settings,
            save_settings,
            set_stream_key,
            stream_key_status,
            set_api_key,
            api_key_status,
            list_models,
            capture_frame,
            generate_poster,
            list_posters,
            delete_poster,
            poster_styles,
            add_layer_live,
            log_file_path,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.engine.lock().stop();
                }
            }
        });
}
