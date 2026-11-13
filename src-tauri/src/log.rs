//! Registro simple en archivo (y stderr) para diagnosticar errores.
//! macOS: ~/Library/Logs/edu.asu.transmitia/transmitia.log

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = LOG_PATH.set(dir.join("transmitia.log"));
}

pub fn path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

pub fn write(level: &str, msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let line = format!("{ts} [{level}] {msg}");
    eprintln!("{line}");
    if let Some(p) = LOG_PATH.get() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
}
