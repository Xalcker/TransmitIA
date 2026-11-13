# TransmitIA

App de transmisión minimalista (tipo OBS) con funciones de IA. Hecha con Tauri 2 (Rust) + GStreamer y una interfaz en SvelteKit.

## Estado: fase 2b (escenas, capas, multidestino y mezclador)

- 4 escenas; cada una con capas apiladas (cámara/capturadora, pantalla, imagen PNG/JPEG, color sólido, texto, patrón de prueba).
- Capas de texto: contenido, fuente, negrita, tamaño, color, contorno, alineación y fondo con transparencia; se editan en vivo.
- Cada capa se mueve y redimensiona sobre la vista previa (imán a bordes/centro, Shift mantiene proporción) y tiene ajuste: encajar, rellenar (recorte) o estirar.
- Cambiar de escena, mover, redimensionar, ocultar o reordenar capas se aplica en vivo sin cortar la transmisión. Agregar/quitar fuentes reinicia la vista previa (bloqueado mientras se transmite).
- Una fuente usada en varias capas/escenas se abre una sola vez.
- Mezclador de audio: varias entradas (micrófonos, tono de prueba; audio del sistema en Windows) con volumen, silencio y medidor por entrada, en vivo.
- Salida: 1080p/720p · 30/60 fps · 16:9 o 9:16.
- Encoder H.264 por hardware con detección automática: VideoToolbox (macOS), NVENC/QSV/AMF/Media Foundation (Windows), VA-API/NVENC (Linux), con x264 como respaldo.
- Multidestino: varios RTMP/RTMPS o SRT a la vez (URL + clave por destino, guardada en el llavero del sistema). Se codifica una sola vez; cada destino tiene su propia conexión con reconexión automática cada 3 s y estado/bitrate propio, así que si uno falla los demás siguen.
- Grabación local en MKV (por defecto en `~/Movies/TransmitIA`), independiente de los destinos.
- Interfaz bilingüe (ES/EN), medidor de audio, fps, bitrate y tiempo en vivo.

### Fase 3: transcripción y subtítulos

- Transcripción en tiempo real en la nube: OpenAI (`gpt-live-transcribe`, PCM 24 kHz) o Gemini Live (`gemini-3.5-transcribe-live`, PCM 16 kHz). Modelo e idioma configurables; la clave de API se guarda en el llavero.
- Panel con la transcripción en vivo (copiar/limpiar) y reconexión automática.
- Subtítulos CEA-608 (roll-up) incrustados como SEI en el H.264 que va a todos los destinos y a la grabación (`tttocea608 → cccombiner → encoder → h264ccinserter`).
- Al grabar se escribe también un `.srt` junto al `.mkv`.

### Fase 4: traducción y subtítulos bilingües

- Cada segmento transcrito se traduce en orden con OpenAI (`gpt-5-nano` por omisión) o Gemini (`gemini-flash-latest`), usando las últimas líneas como contexto. El modelo es configurable y el botón ↻ consulta al proveedor qué modelos ofrece la clave.
- Subtítulos CEA-608 en dos canales (`cea608mux`): CC1 y CC3, con el original y la traducción en el orden que elijas, o solo la traducción.
- Al grabar: `.srt` original y `.<idioma>.srt` traducido junto al `.mkv`.
- El panel de transcripción muestra cada línea con su traducción debajo.

### Fase 5: pósters con IA

- Captura el cuadro actual de cualquier fuente usada en una escena (cámara, capturadora, pantalla, imagen) o del programa completo, sin costo mientras no se pide.
- Estilos del catálogo [image-gen-styles](https://github.com/Xalcker/image-gen-styles) (45 estilos, filtros por categoría y miniaturas), con copia local para usar sin red.
- Plantilla de prompt editable con variables `{titulo}`, `{transcripcion}` (últimas frases), `{fecha}`, `{hora}`; se muestra el prompt final.
- Gemini (Nano Banana, `generateContent` con `imageConfig.aspectRatio`) u OpenAI (GPT Image, `/v1/images/edits`). Aspecto como el lienzo (16:9 o 9:16) o 1:1; calidad baja/media/alta; generación automática cada N minutos.
- Galería en `~/Pictures/TransmitIA` (imagen + metadatos). "Agregar a la escena" pone el póster como capa; en vivo se inserta sin cortar la transmisión.
- La barra lateral se organiza en pestañas: Escena, Salida e IA.

## Requisitos

- Rust (rustup) y Node 20+.
- GStreamer 1.24+ con plugins base, good, bad y ugly, más libav.
  - macOS: `brew install gstreamer`
  - Windows: instaladores MSVC *runtime* y *development* de gstreamer.freedesktop.org
  - Linux (Debian/Ubuntu): `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-{good,bad,ugly} gstreamer1.0-libav gstreamer1.0-vaapi`

## Desarrollo

```bash
npm install
npm run tauri dev
```

Prueba de humo del motor (graba 3 s con fuentes de prueba):

```bash
cd src-tauri && cargo test -- --nocapture
```

## Estructura

- `src-tauri/src/engine.rs`: pipeline de GStreamer (compositor de capas, mezclador, vista previa, encoder, estadísticas).
- `src-tauri/src/outputs.rs`: salidas independientes por destino (appsrc → mux → sink) con reconexión.
- `src-tauri/src/media.rs`: dispositivos, fuentes por sistema operativo y selección/configuración de encoders.
- `src-tauri/src/settings.rs`: modelo de configuración compartido con la interfaz.
- `src/routes/+page.svelte`: interfaz; `src/lib/i18n.ts` tiene los textos ES/EN.

## Distribución

### macOS (`.app` + `.dmg` autocontenidos)

```bash
./scripts/bundle-macos.sh
```

- Compila en modo release y copia dentro de `TransmitIA.app` solo los 32 plugins de GStreamer que usa la app y sus librerías (`Contents/Frameworks`), además del escáner de plugins y el módulo TLS de GIO (RTMPS). Reescribe las rutas con `install_name_tool`, comprueba que nada apunte a Homebrew y genera `src-tauri/target/release/bundle/TransmitIA_<versión>_<arch>.dmg`.
- Al arrancar, `src-tauri/src/gst_env.rs` detecta el GStreamer empaquetado y lo usa en lugar del del sistema.
- **Compatibilidad:** el paquete hereda la versión mínima de macOS y la arquitectura de las librerías de Homebrew con que se construye (aquí: Apple Silicon, macOS actual). En CI se construye en `macos-14` para cubrir macOS 14+.
- **Firma:** sin configuración se firma ad hoc; en otra Mac, Gatekeeper pedirá abrirla con clic derecho → Abrir (o `xattr -dr com.apple.quarantine TransmitIA.app`). Para distribuir sin avisos: `APPLE_SIGNING_IDENTITY="Developer ID Application: …" ./scripts/bundle-macos.sh` y notarizar el `.dmg` con `xcrun notarytool` (requiere cuenta de Apple Developer).

### Windows y Linux (GitHub Actions)

`.github/workflows/build.yml` compila en cada plataforma (manual o con un tag `v*`):

- **Windows:** instala GStreamer MSVC oficial y genera un instalador NSIS con las DLL y plugins incluidos (`src-tauri/tauri.windows.conf.json`). Encoders por hardware: NVENC, Quick Sync, AMF o Media Foundation según la GPU; audio del sistema por WASAPI loopback.
- **Linux:** `.deb` (declara los paquetes de GStreamer como dependencias) y AppImage con GStreamer incluido (`bundleMediaFramework`). Los subtítulos CEA-608 requieren `gstreamer1.0-plugins-rs`; si falta, la app transmite sin ellos y lo avisa.

Windows y Linux aún no se han compilado: el primer run de CI puede requerir ajustes.

## Diagnóstico (macOS)

Solo `TransmitIA.app` tiene permiso de cámara. Para probar el motor con una cámara real:

```bash
open src-tauri/target/debug/bundle/macos/TransmitIA.app --env TRANSMITIA_SELFTEST=dev:0 --env TRANSMITIA_DUMP_DIR=/tmp/transmitia-dump
```

El log queda en `~/Library/Logs/edu.asu.transmitia/transmitia.log`.

## Próximas fases

Pendiente de fase 2: audio del sistema en macOS (ScreenCaptureKit) y captura de una ventana específica.

- Verificar y ajustar las compilaciones de Windows y Linux en CI.
- Firma con Developer ID y notarización en macOS; firma de código en Windows.
