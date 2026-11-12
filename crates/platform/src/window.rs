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
    pub(crate) input: InputHandle,
}

/// Prepares a shared window for input from the other device. A window
/// parked out of sight is brought back for a moment first, so injected
/// clicks land on it; it's parked again once input stops.
#[derive(Clone, Default)]
pub struct InputHandle(Option<Arc<dyn Fn() + Send + Sync>>);

impl InputHandle {
    #[allow(dead_code)] // used by the macOS backend
    pub(crate) fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Call before injecting input; updates the geometry right away.
    pub fn prepare(&self) {
        if let Some(f) = &self.0 {
            f();
        }
    }
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
        Self { title, app, frames, geometry, ended, stop: None, input: InputHandle::default() }
    }

    pub fn input_handle(&self) -> InputHandle {
        self.input.clone()
    }

    #[allow(dead_code)]
    pub(crate) fn with_stop(
        title: String,
        app: String,
        frames: watch::Receiver<Option<Arc<CapturedFrame>>>,
        geometry: watch::Receiver<Option<WindowGeometry>>,
        ended: oneshot::Receiver<String>,
        stop: oneshot::Sender<()>,
        input: InputHandle,
    ) -> Self {
        Self { title, app, frames, geometry, ended, stop: Some(stop), input }
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
/// pick one in the OS's dialog when `id` is `None`. With `park`, the real
/// window is moved out of sight while shared (it was dragged to the other
/// device) and put back when sharing stops.
pub async fn capture_window(id: Option<u64>, park: bool) -> anyhow::Result<WindowCapture> {
    crate::native::capture_window(id, park).await
}

/// Moves one of dextr's own windows (a window shared by the other device)
/// so its content's top-left is at (x, y), where the OS doesn't let apps place their
/// windows themselves (Wayland). Returns false if it can't.
pub async fn place_own_window(title: &str, x: f32, y: f32) -> bool {
    crate::native::place_own_window(title, x, y).await
}
