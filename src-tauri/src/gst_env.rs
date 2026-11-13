//! Configura GStreamer cuando viaja dentro del paquete de la app.
//!
//! Debe llamarse antes de `gst::init()`. Si la app corre en desarrollo (sin
//! GStreamer empaquetado) no cambia nada y se usa el GStreamer del sistema.
//!
//! - macOS: plugins en `TransmitIA.app/Contents/Resources/gstreamer-1.0` (lo arma
//!   `scripts/bundle-macos.sh`), escáner en `Contents/MacOS/gst-plugin-scanner`
//!   y el módulo TLS de GIO (necesario para RTMPS) en `Contents/Resources/gio/modules`.
//! - Windows: DLL junto al ejecutable y plugins en `gstreamer-1.0/` (instalador).
//! - Linux AppImage: el propio AppImage exporta las variables (bundleMediaFramework).

use std::path::{Path, PathBuf};

pub fn configure() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(exe_dir) = exe.parent() else { return };
    if let Some(root) = bundled_root(exe_dir) {
        apply(&root, exe_dir);
    }
}

/// Carpeta que contiene `gstreamer-1.0/` empaquetado, si existe.
fn bundled_root(exe_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        exe_dir.join("../Resources"), // macOS .app
        exe_dir.to_path_buf(),        // Windows (junto al .exe)
        exe_dir.join("gstreamer"),    // subcarpeta de recursos
    ];
    candidates.into_iter().find(|c| c.join("gstreamer-1.0").is_dir())
}

fn apply(root: &Path, exe_dir: &Path) {
    let plugins = root.join("gstreamer-1.0");
    set("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins);
    set("GST_PLUGIN_SYSTEM_PATH", &plugins);
    // Ignorar plugins de otras instalaciones de GStreamer en la máquina.
    unsafe {
        std::env::remove_var("GST_PLUGIN_PATH_1_0");
        std::env::remove_var("GST_PLUGIN_PATH");
    }

    let scanner_name = if cfg!(windows) { "gst-plugin-scanner.exe" } else { "gst-plugin-scanner" };
    for scanner in [exe_dir.join(scanner_name), root.join("libexec").join(scanner_name)] {
        if scanner.is_file() {
            set("GST_PLUGIN_SCANNER_1_0", &scanner);
            set("GST_PLUGIN_SCANNER", &scanner);
            break;
        }
    }

    // Registro propio: no mezclar con el caché de otra instalación.
    if let Some(cache) = cache_dir() {
        let _ = std::fs::create_dir_all(&cache);
        set("GST_REGISTRY_1_0", &cache.join("gstreamer-registry.bin"));
    }

    let gio = root.join("gio").join("modules");
    if gio.is_dir() {
        set("GIO_MODULE_DIR", &gio);
    }
    for fonts in [root.join("etc/fonts/fonts.conf"), exe_dir.join("../Resources/etc/fonts/fonts.conf")] {
        if fonts.is_file() {
            set("FONTCONFIG_FILE", &fonts);
            break;
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("LOCALAPPDATA"))?;
    let base = PathBuf::from(home);
    Some(if cfg!(target_os = "macos") {
        base.join("Library/Caches/edu.asu.transmitia")
    } else if cfg!(windows) {
        base.join("TransmitIA").join("cache")
    } else {
        base.join(".cache/transmitia")
    })
}

fn set(key: &str, value: &Path) {
    // SAFETY: se llama al inicio de main, antes de crear hilos.
    unsafe { std::env::set_var(key, value) };
}
