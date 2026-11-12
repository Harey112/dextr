//! Capturing one window to share it with the other device.
//!
//! Each OS delivers the window's own pixels (not a crop of the screen) at
//! its real resolution, plus where the window is on screen so input from the
//! other device can be placed on it.

use std::sync::Arc;

use tokio::sync::{oneshot, watch};

/// One frame of a window, 4 bytes per pixel in BGRA (or BGRx) order.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub data: Vec<u8>,
}

/// Where a window is, in the injector's screen coordinates (logical pixels
/// on Linux, points on macOS).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A window dextr can share without asking the OS for a picker.
#[derive(Clone, Debug, PartialEq)]
pub struct ShareableWindow {
    pub id: u64,
    pub title: String,
    pub app: String,
}

/// A window being captured. Dropping it stops the capture.
pub struct WindowCapture {
    pub title: String,
    pub app: String,
    /// The latest frame. Closes when the capture ends.
    pub frames: watch::Receiver<Option<Arc<CapturedFrame>>>,
    /// Where the window is, if the OS lets us know.
    pub geometry: watch::Receiver<Option<WindowGeometry>>,
    /// Why the capture ended (the window closed, the user stopped it…).
    pub ended: oneshot::Receiver<String>,
    pub(crate) stop: Option<oneshot::Sender<()>>,
}

impl WindowCapture {
    /// A capture fed by the caller (tests, synthetic sources).
    pub fn from_parts(
        title: String,
        app: String,
        frames: watch::Receiver<Option<Arc<CapturedFrame>>>,
        geometry: watch::Receiver<Option<WindowGeometry>>,
        ended: oneshot::Receiver<String>,
    ) -> Self {
        Self { title, app, frames, geometry, ended, stop: None }
    }

    #[allow(dead_code)]
    pub(crate) fn with_stop(
        title: String,
        app: String,
        frames: watch::Receiver<Option<Arc<CapturedFrame>>>,
        geometry: watch::Receiver<Option<WindowGeometry>>,
        ended: oneshot::Receiver<String>,
        stop: oneshot::Sender<()>,
    ) -> Self {
        Self { title, app, frames, geometry, ended, stop: Some(stop) }
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// The windows that can be shared directly, or `None` when the OS shows its
/// own picker instead (Linux).
pub async fn shareable_windows() -> anyhow::Result<Option<Vec<ShareableWindow>>> {
    crate::native::shareable_windows().await
}

/// Starts capturing window `id` from [`shareable_windows`], or lets the user
/// pick one in the OS's dialog when `id` is `None`.
pub async fn capture_window(id: Option<u64>) -> anyhow::Result<WindowCapture> {
    crate::native::capture_window(id).await
}
