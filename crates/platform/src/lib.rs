//! OS backends.
//!
//! Every OS implements the same two jobs:
//!
//! * **Capture**: when the local pointer hits the edge facing the peer, take
//!   exclusive ownership of the local mouse and keyboard and report their
//!   events, until told to release.
//! * **Inject**: replay the peer's input on this machine.
//!
//! Each backend runs on its own thread and talks to the core only through the
//! handles below, so the core never sees an OS type and works the same in
//! every direction.

use dextr_proto::{Button, Input, Key, Rect, Side};
use tokio::sync::{mpsc, oneshot, watch};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as native;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as native;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported as native;

/// Input read from the local devices while captured. Motion is relative and
/// in local logical pixels; the core turns it into absolute peer positions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Captured {
    Motion { dx: f32, dy: f32 },
    Button { button: Button, pressed: bool },
    Scroll { dx: f32, dy: f32 },
    ScrollDiscrete { dx: i32, dy: i32 },
    ScrollStop,
    Key { key: Key, pressed: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// The pointer crossed the edge at (x, y), local coordinates. All local
    /// input now goes to us until [`CaptureHandle::release`].
    Activated { x: f32, y: f32 },
    Input(Captured),
    /// The compositor ended the capture on its own (for example, the user
    /// pressed its own escape shortcut).
    Deactivated,
    /// The backend stopped and won't send more events.
    Stopped { reason: String },
}

#[derive(Debug)]
pub(crate) enum CaptureCommand {
    Enable,
    Disable,
    Release { x: f32, y: f32 },
    SetSide(Side),
}

/// Controls the local input capture. Dropping it ends the capture session.
#[derive(Debug, Clone)]
pub struct CaptureHandle {
    commands: mpsc::UnboundedSender<CaptureCommand>,
    screens: watch::Receiver<Vec<Rect>>,
}

impl CaptureHandle {
    /// Start watching the edge. Capture stays off until this is called.
    pub fn enable(&self) {
        let _ = self.commands.send(CaptureCommand::Enable);
    }

    /// Stop watching the edge (for example while the peer controls us, so the
    /// injected pointer doesn't trip our own edge).
    pub fn disable(&self) {
        let _ = self.commands.send(CaptureCommand::Disable);
    }

    /// Give the pointer back to this machine, placing it at (x, y).
    pub fn release(&self, x: f32, y: f32) {
        let _ = self.commands.send(CaptureCommand::Release { x, y });
    }

    pub fn set_side(&self, side: Side) {
        let _ = self.commands.send(CaptureCommand::SetSide(side));
    }

    /// The local screens as the capture backend sees them.
    pub fn screens(&self) -> Vec<Rect> {
        self.screens.borrow().clone()
    }
}

/// Starts capture with the peer on `side`. Resolves once the OS has granted
/// access (this may show a permission dialog).
pub async fn start_capture(
    side: Side,
) -> anyhow::Result<(CaptureHandle, mpsc::UnboundedReceiver<CaptureEvent>)> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (screens_tx, screens_rx) = watch::channel(Vec::new());
    let (ready_tx, ready_rx) = oneshot::channel();
    native::spawn_capture(side, cmd_rx, event_tx, screens_tx, ready_tx)?;
    ready_rx.await??;
    Ok((
        CaptureHandle { commands: cmd_tx, screens: screens_rx },
        event_rx,
    ))
}

#[derive(Debug)]
pub(crate) enum InjectCommand {
    Input(Input),
    ReleaseAll,
}

/// Replays input on this machine. Dropping every clone ends the session.
#[derive(Debug, Clone)]
pub struct InjectorHandle {
    commands: mpsc::UnboundedSender<InjectCommand>,
    screens: watch::Receiver<Vec<Rect>>,
    stopped: watch::Receiver<Option<String>>,
}

impl InjectorHandle {
    pub fn inject(&self, input: Input) {
        let _ = self.commands.send(InjectCommand::Input(input));
    }

    /// Releases every key and button this injector is holding down.
    pub fn release_all(&self) {
        let _ = self.commands.send(InjectCommand::ReleaseAll);
    }

    /// The local screens in the coordinate space [`Input::Motion`] uses.
    pub fn screens(&self) -> Vec<Rect> {
        self.screens.borrow().clone()
    }

    /// Resolves when the screen layout changes.
    pub async fn screens_changed(&mut self) -> Vec<Rect> {
        if self.screens.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        self.screens.borrow_and_update().clone()
    }

    /// Resolves with a reason if the backend stops.
    pub async fn stopped(&mut self) -> String {
        loop {
            if let Some(reason) = self.stopped.borrow_and_update().clone() {
                return reason;
            }
            if self.stopped.changed().await.is_err() {
                return "injector thread exited".into();
            }
        }
    }
}

/// Permission the OS granted for injection, to reuse next time so the user
/// isn't asked again. Opaque; store it as-is.
pub type RestoreToken = String;

/// Starts input injection. Resolves once the OS has granted access (this may
/// show a permission dialog, skipped when `restore` is still valid).
pub async fn start_injector(
    restore: Option<RestoreToken>,
) -> anyhow::Result<(InjectorHandle, Option<RestoreToken>)> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (screens_tx, screens_rx) = watch::channel(Vec::new());
    let (stopped_tx, stopped_rx) = watch::channel(None);
    let (ready_tx, ready_rx) = oneshot::channel();
    native::spawn_injector(restore, cmd_rx, screens_tx, stopped_tx, ready_tx)?;
    let token = ready_rx.await??;
    Ok((
        InjectorHandle { commands: cmd_tx, screens: screens_rx, stopped: stopped_rx },
        token,
    ))
}

/// Runs `f` on a dedicated thread with its own single-threaded Tokio runtime.
/// OS sessions (D-Bus portals, libei sockets) live there, isolated from the
/// core's runtime and free of `Send` constraints.
#[allow(dead_code)]
pub(crate) fn spawn_backend_thread<F, Fut>(name: &str, f: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::Builder::new().name(name.into()).spawn(move || {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(f()),
            Err(e) => tracing::error!("cannot start backend runtime: {e}"),
        }
    })?;
    Ok(())
}
