//! Permisos de cámara y micrófono.
//!
//! En macOS, `avfvideosrc` se queda colgado o falla sin mensaje claro si la app no
//! tiene permiso. Por eso pedimos el permiso antes de armar el pipeline y, si fue
//! negado, devolvemos un error que explica dónde activarlo.

use anyhow::Result;

#[derive(Clone, Copy, Debug)]
pub enum Media {
    Camera,
    Microphone,
}

#[cfg(target_os = "macos")]
pub fn ensure(media: Media) -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::anyhow;
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio, AVMediaTypeVideo};

    let media_type = unsafe {
        match media {
            Media::Camera => AVMediaTypeVideo,
            Media::Microphone => AVMediaTypeAudio,
        }
    }
    .ok_or_else(|| anyhow!("AVFoundation media type unavailable"))?;

    let denied = || match media {
        Media::Camera => anyhow!(
            "permission:camera — macOS denied camera access. Enable it in System Settings › Privacy & Security › Camera"
        ),
        Media::Microphone => anyhow!(
            "permission:microphone — macOS denied microphone access. Enable it in System Settings › Privacy & Security › Microphone"
        ),
    };

    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(());
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return Err(denied());
    }

    // Primera vez: macOS muestra el diálogo; esperamos la respuesta del usuario.
    let (tx, rx) = mpsc::channel::<bool>();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block) };
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(true) => Ok(()),
        _ => Err(denied()),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure(_media: Media) -> Result<()> {
    Ok(())
}
