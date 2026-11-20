//! Vista previa nativa (macOS), como OBS: el video se dibuja con la GPU en una
//! NSView propia colocada DETRÁS del WKWebView, que es transparente en el hueco
//! del lienzo. Así no hay JPEG, base64 ni copias a la CPU, y los controles HTML
//! (selección, esquinas) siguen dibujándose encima del video.
//!
//! `glimagesink` recibe el puntero de esa NSView con `VideoOverlay`.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSColor, NSView, NSWindow, NSWindowOrderingMode};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use tauri::{AppHandle, Runtime, WebviewWindow};

    static VIDEO_VIEW: AtomicUsize = AtomicUsize::new(0);
    static WEBVIEW: AtomicUsize = AtomicUsize::new(0);

    /// Color de fondo de la app (--bg en la interfaz).
    const BG: (f64, f64, f64) = (0x0f as f64 / 255.0, 0x11 as f64 / 255.0, 0x15 as f64 / 255.0);

    pub fn install<R: Runtime>(window: &WebviewWindow<R>) {
        // La ventana es transparente (para ver el video detrás del webview):
        // pintamos su fondo con el color de la app.
        if let Ok(ptr) = window.ns_window() {
            let ns_window = unsafe { &*(ptr as *const NSWindow) };
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(BG.0, BG.1, BG.2, 1.0);
            ns_window.setBackgroundColor(Some(&color));
        }
        let _ = window.with_webview(|wv| {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let wk = wv.inner() as *const NSView;
            if wk.is_null() {
                return;
            }
            let wk = unsafe { &*wk };
            let Some(parent) = (unsafe { wk.superview() }) else { return };
            let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
            view.setHidden(true);
            // Todo con capas de Core Animation: si se mezclan vistas con y sin
            // capa, la capa OpenGL de GStreamer termina dibujándose ENCIMA del
            // webview y tapa los controles HTML. Con capas, manda el orden de
            // las subvistas (video detrás del webview).
            parent.setWantsLayer(true);
            view.setWantsLayer(true);
            parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(wk));
            WEBVIEW.store(wk as *const NSView as usize, Ordering::SeqCst);
            VIDEO_VIEW.store(Retained::into_raw(view) as usize, Ordering::SeqCst);
            crate::log::write("info", "native preview view installed");
        });
    }

    fn fit_subviews(view: &NSView) {
        let bounds = view.bounds();
        for sub in view.subviews().iter() {
            sub.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
            sub.setFrame(bounds);
        }
    }

    /// GStreamer agrega su vista OpenGL de forma asíncrona al arrancar el
    /// pipeline: volver a ajustarla un par de veces después de iniciar.
    pub fn refit_soon<R: Runtime>(app: &AppHandle<R>) {
        let Some(view) = handle() else { return };
        let app = app.clone();
        std::thread::spawn(move || {
            for ms in [150u64, 500, 1500] {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let _ = app.run_on_main_thread(move || fit_subviews(unsafe { &*(view as *const NSView) }));
            }
        });
    }

    /// Puntero de la NSView para `VideoOverlay::set_window_handle`.
    pub fn handle() -> Option<usize> {
        let v = VIDEO_VIEW.load(Ordering::SeqCst);
        (v != 0).then_some(v)
    }

    /// Ubica la vista en el rectángulo del lienzo (coordenadas CSS del webview).
    /// `page_h` = `window.innerHeight`: el contenido HTML empieza debajo de la
    /// barra de título, así que la posición vertical se mide desde abajo.
    pub fn set_rect<R: Runtime>(app: &AppHandle<R>, x: f64, y: f64, w: f64, h: f64, page_h: f64, visible: bool) {
        let (Some(view), wk) = (handle(), WEBVIEW.load(Ordering::SeqCst)) else { return };
        if wk == 0 {
            return;
        }
        let _ = app.run_on_main_thread(move || {
            let view = unsafe { &*(view as *const NSView) };
            let wk = unsafe { &*(wk as *const NSView) };
            let f = wk.frame();
            let flipped = unsafe { wk.superview() }.is_some_and(|p| p.isFlipped());
            let from_bottom = page_h - y - h;
            let oy = if flipped { f.origin.y + f.size.height - from_bottom - h } else { f.origin.y + from_bottom };
            view.setFrame(NSRect::new(NSPoint::new(f.origin.x + x, oy), NSSize::new(w.max(1.0), h.max(1.0))));
            view.setHidden(!visible);
            // La vista OpenGL que GStreamer inserta conserva el tamaño con que se
            // creó: forzarla a ocupar exactamente nuestra vista (si no, el video
            // queda desplazado y recortado respecto al lienzo HTML).
            fit_subviews(view);
        });
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use tauri::{AppHandle, Runtime, WebviewWindow};

    pub fn install<R: Runtime>(_window: &WebviewWindow<R>) {}
    pub fn handle() -> Option<usize> {
        None
    }
    pub fn set_rect<R: Runtime>(_app: &AppHandle<R>, _x: f64, _y: f64, _w: f64, _h: f64, _page_h: f64, _visible: bool) {}
    pub fn refit_soon<R: Runtime>(_app: &AppHandle<R>) {}
}

pub use imp::{handle, install, refit_soon, set_rect};

/// ¿Se usa la ruta GPU (composición GL + vista nativa)?
pub fn gpu_enabled() -> bool {
    use gstreamer as gst;
    handle().is_some()
        && ["glvideomixer", "glimagesink", "glcolorconvert", "gldownload"]
            .iter()
            .all(|f| gst::ElementFactory::find(f).is_some())
        && std::env::var_os("TRANSMITIA_CPU_PREVIEW").is_none()
}
