//! Configuración de salida y destinos (lo que la interfaz envía al motor).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Resolution {
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// 16:9
    Landscape,
    /// 9:16
    Portrait,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Encajar completo dentro del rectángulo (sin deformar).
    Contain,
    /// Rellenar el rectángulo recortando los bordes.
    Cover,
    /// Estirar al rectángulo (puede deformar).
    Stretch,
}

/// Rectángulo normalizado (0..1) relativo al lienzo, así la composición se
/// conserva al cambiar resolución u orientación.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const FULL: Rect = Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

    /// A píxeles del lienzo (ancho/alto mínimos de 2).
    pub fn to_px(&self, cw: u32, ch: u32) -> (i32, i32, i32, i32) {
        let (cw, ch) = (cw as f64, ch as f64);
        (
            (self.x * cw).round() as i32,
            (self.y * ch).round() as i32,
            ((self.w * cw).round() as i32).max(2),
            ((self.h * ch).round() as i32).max(2),
        )
    }
}

/// Una capa: una fuente colocada en el lienzo. El orden en la lista es el
/// orden de apilado (la primera queda al fondo).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    pub id: String,
    /// `test`, `dev:N`, `screen:N`, `image:/ruta`, `color:#RRGGBB`, `text:<id de capa>`
    pub source: String,
    pub rect: Rect,
    pub fit: Fit,
    pub visible: bool,
    /// Solo para capas de texto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextStyle>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextStyle {
    pub content: String,
    pub font: String,
    pub bold: bool,
    /// Tamaño de letra en % de la altura de la capa.
    pub size: f64,
    /// #RRGGBB
    pub color: String,
    pub outline: bool,
    /// #RRGGBB
    pub background: String,
    /// 0..1
    pub background_opacity: f64,
    /// left | center | right
    pub align: String,
}

/// Una entrada del mezclador de audio.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInput {
    pub id: String,
    /// `test`, `dev:N`, `system`
    pub source: String,
    /// 0..2 (1 = sin cambio)
    pub volume: f64,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scene {
    pub name: String,
    pub layers: Vec<Layer>,
}

pub const SCENE_COUNT: usize = 4;

/// El destino migrado de la fase 1 conserva la clave guardada entonces.
pub const LEGACY_DESTINATION_ID: &str = "default";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DestinationKind {
    Rtmp,
    Srt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    /// Identifica la clave en el llavero del sistema.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub enabled: bool,
    pub kind: DestinationKind,
    /// Ej. rtmp://a.rtmp.youtube.com/live2  o  srt://host:port
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Openai,
    Gemini,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Transcription {
    pub enabled: bool,
    pub provider: Provider,
    /// Vacío = modelo por omisión del proveedor.
    pub model: String,
    /// Código ISO (es, en…); vacío = automático.
    pub language: String,
    /// Incrustar subtítulos CEA-608 en el video transmitido/grabado.
    pub captions: bool,
}

impl Default for Transcription {
    fn default() -> Self {
        Self { enabled: false, provider: Provider::Openai, model: String::new(), language: "es".into(), captions: true }
    }
}

/// Qué idioma va en cada canal de subtítulos CEA-608.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptionRouting {
    /// CC1 = original, CC3 = traducción
    OriginalCc1,
    /// CC1 = traducción, CC3 = original
    TranslatedCc1,
    /// Solo la traducción, en CC1
    TranslatedOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Translation {
    pub enabled: bool,
    pub provider: Provider,
    /// Vacío = modelo por omisión del proveedor.
    pub model: String,
    /// Idioma destino (ISO).
    pub target: String,
    pub captions: CaptionRouting,
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: Provider::Openai,
            model: String::new(),
            target: "en".into(),
            captions: CaptionRouting::OriginalCc1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Posters {
    pub provider: Provider,
    pub model: String,
    /// Fuente a capturar: id de fuente o `program`.
    pub source: String,
    pub style_id: Option<String>,
    pub template: String,
    pub title: String,
    /// `canvas` (según orientación) o `1:1`
    pub aspect: String,
    /// low | medium | high
    pub quality: String,
    /// Generación automática cada N minutos (0 = apagado).
    pub auto_minutes: u32,
}

impl Default for Posters {
    fn default() -> Self {
        Self {
            provider: Provider::Gemini,
            model: String::new(),
            source: "program".into(),
            style_id: None,
            template: String::new(),
            title: String::new(),
            aspect: "canvas".into(),
            quality: "medium".into(),
            auto_minutes: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub enabled: bool,
    /// Carpeta de destino; vacío = carpeta de Videos del usuario.
    pub folder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub resolution: Resolution,
    pub fps: u32,
    pub orientation: Orientation,
    /// None = automático según resolución/fps.
    pub video_bitrate_kbps: Option<u32>,
    pub audio_bitrate_kbps: u32,
    /// Nombre de la fábrica GStreamer del encoder; None = automático.
    pub encoder: Option<String>,
    pub scenes: Vec<Scene>,
    pub active_scene: usize,
    pub audio_inputs: Vec<AudioInput>,
    pub destinations: Vec<Destination>,
    pub recording: Recording,
    pub transcription: Transcription,
    pub translation: Translation,
    pub posters: Posters,
    pub language: String,
    /// Formato anterior (fase 1/2): se migra en `normalized()`.
    #[serde(skip_serializing, rename = "audioSource")]
    pub legacy_audio_source: Option<String>,
    #[serde(skip_serializing, rename = "destination")]
    pub legacy_destination: Option<Destination>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution: Resolution::P1080,
            fps: 30,
            orientation: Orientation::Landscape,
            video_bitrate_kbps: None,
            audio_bitrate_kbps: 160,
            encoder: None,
            scenes: (0..SCENE_COUNT)
                .map(|i| Scene {
                    name: String::new(),
                    layers: if i == 0 {
                        vec![Layer {
                            id: "layer-test".into(),
                            source: "test".into(),
                            rect: Rect::FULL,
                            fit: Fit::Contain,
                            visible: true,
                            text: None,
                        }]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            active_scene: 0,
            audio_inputs: vec![AudioInput {
                id: "audio-test".into(),
                source: "test".into(),
                volume: 0.3,
                muted: false,
            }],
            destinations: Vec::new(),
            recording: Recording {
                enabled: false,
                folder: String::new(),
            },
            transcription: Transcription::default(),
            translation: Translation::default(),
            posters: Posters::default(),
            language: "es".into(),
            legacy_audio_source: None,
            legacy_destination: None,
        }
    }
}

impl Settings {
    /// Solo se permiten 30 o 60 fps.
    pub fn fps(&self) -> u32 {
        if self.fps >= 60 { 60 } else { 30 }
    }

    /// Ancho y alto finales del lienzo.
    pub fn canvas_size(&self) -> (u32, u32) {
        let (w, h) = match self.resolution {
            Resolution::P1080 => (1920, 1080),
            Resolution::P720 => (1280, 720),
        };
        match self.orientation {
            Orientation::Landscape => (w, h),
            Orientation::Portrait => (h, w),
        }
    }

    /// Escenas normalizadas: siempre SCENE_COUNT, escena activa válida.
    pub fn normalized(mut self) -> Self {
        let defaults = Settings::default().scenes;
        while self.scenes.len() < SCENE_COUNT {
            self.scenes.push(defaults[self.scenes.len()].clone());
        }
        self.scenes.truncate(SCENE_COUNT);
        if self.active_scene >= SCENE_COUNT {
            self.active_scene = 0;
        }
        if let Some(src) = self.legacy_audio_source.take() {
            self.audio_inputs = match src.as_str() {
                "none" | "" => Vec::new(),
                _ => vec![AudioInput { id: "audio-1".into(), source: src, volume: 1.0, muted: false }],
            };
        }
        if let Some(mut d) = self.legacy_destination.take() {
            if self.destinations.is_empty() && !d.url.trim().is_empty() {
                d.id = LEGACY_DESTINATION_ID.into();
                self.destinations.push(d);
            }
        }
        for (i, d) in self.destinations.iter_mut().enumerate() {
            if d.id.is_empty() {
                d.id = format!("dest-{}", i + 1);
            }
            if d.name.trim().is_empty() {
                d.name = format!("{} {}", if self.language == "en" { "Destination" } else { "Destino" }, i + 1);
            }
        }
        self
    }

    pub fn video_bitrate(&self) -> u32 {
        self.video_bitrate_kbps.unwrap_or(match (self.resolution, self.fps()) {
            (Resolution::P1080, 60) => 6000,
            (Resolution::P1080, _) => 4500,
            (Resolution::P720, 60) => 4500,
            (Resolution::P720, _) => 3000,
        })
    }
}
