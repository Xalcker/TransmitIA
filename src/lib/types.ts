export type Mode = "stopped" | "preview" | "live";
export type Fit = "contain" | "cover" | "stretch";

/** Rectángulo normalizado (0..1) relativo al lienzo. */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface TextStyle {
  content: string;
  font: string;
  bold: boolean;
  /** % de la altura de la capa */
  size: number;
  color: string;
  outline: boolean;
  background: string;
  backgroundOpacity: number;
  align: "left" | "center" | "right";
}

export interface Layer {
  id: string;
  /** test | dev:N | screen:N | image:/ruta | color:#RRGGBB | text:<id> */
  source: string;
  rect: Rect;
  fit: Fit;
  visible: boolean;
  text?: TextStyle;
}

export interface AudioInput {
  id: string;
  /** test | dev:N | system */
  source: string;
  /** 0..2 */
  volume: number;
  muted: boolean;
}

export interface Destination {
  id: string;
  name: string;
  enabled: boolean;
  kind: "rtmp" | "srt";
  url: string;
}

export interface OutputStatus {
  id: string;
  state: "connecting" | "live" | "reconnecting" | "error" | "stopped";
  message: string | null;
  kbps: number;
}

export type Provider = "openai" | "gemini";

export interface Transcription {
  enabled: boolean;
  provider: Provider;
  /** vacío = modelo por omisión */
  model: string;
  /** ISO (es, en…); vacío = automático */
  language: string;
  captions: boolean;
}

/** Qué idioma va en cada canal CEA-608. */
export type CaptionRouting = "original-cc1" | "translated-cc1" | "translated-only";

export interface Translation {
  enabled: boolean;
  provider: Provider;
  model: string;
  target: string;
  captions: CaptionRouting;
}

export interface TranscriptEvent {
  itemId: string;
  text: string;
  kind: "delta" | "interim" | "final";
}

export interface Scene {
  name: string;
  /** Orden de apilado: la primera queda al fondo. */
  layers: Layer[];
}

export interface Settings {
  resolution: "1080p" | "720p";
  fps: 30 | 60;
  orientation: "landscape" | "portrait";
  videoBitrateKbps: number | null;
  audioBitrateKbps: number;
  encoder: string | null;
  scenes: Scene[];
  activeScene: number;
  audioInputs: AudioInput[];
  destinations: Destination[];
  recording: { enabled: boolean; folder: string };
  transcription: Transcription;
  translation: Translation;
  posters: PosterSettings;
  language: "es" | "en";
}

export interface SourceInfo {
  id: string;
  label: string;
  kind: "test" | "device" | "screen" | "none";
}

export interface EncoderInfo {
  id: string;
  label: string;
  hardware: boolean;
}

export interface Stats {
  mode: Mode;
  fps: number;
  videoKbps: number;
  uptimeSec: number;
  encoder: string | null;
  streaming: boolean;
  recordingPath: string | null;
}

export interface PosterSettings {
  provider: Provider;
  model: string;
  /** id de fuente o "program" */
  source: string;
  styleId: string | null;
  template: string;
  title: string;
  aspect: "canvas" | "1:1";
  quality: "low" | "medium" | "high";
  autoMinutes: number;
}

export interface PosterInfo {
  path: string;
  created: number;
  prompt: string;
  styleId: string | null;
  provider: Provider;
  model: string;
  aspect: string;
}

export interface Style {
  id: string;
  name: string;
  category: string;
  description: string;
  prompt: string;
  enabledByDefault: boolean;
}
