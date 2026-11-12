//! Placeholder for macOS and Windows until their backends land.
//!
//! macOS: capture with a CGEventTap, inject with CGEventPost.
//! Windows: capture with Raw Input + a low-level hook, inject with SendInput.

use crate::{CaptureCommand, CaptureEvent, InjectCommand, RestoreToken};
use dextr_proto::{Rect, Side};
use tokio::sync::{mpsc, oneshot, watch};

fn unsupported() -> anyhow::Error {
    anyhow::anyhow!("input sharing is not implemented on this OS yet")
}

pub(crate) fn spawn_capture(
    _side: Side,
    _commands: mpsc::UnboundedReceiver<CaptureCommand>,
    _events: mpsc::UnboundedSender<CaptureEvent>,
    _screens: watch::Sender<Vec<Rect>>,
    ready: oneshot::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let _ = ready.send(Err(unsupported()));
    Ok(())
}

pub(crate) fn spawn_injector(
    _restore: Option<RestoreToken>,
    _commands: mpsc::UnboundedReceiver<InjectCommand>,
    _screens: watch::Sender<Vec<Rect>>,
    _stopped: watch::Sender<Option<String>>,
    ready: oneshot::Sender<anyhow::Result<Option<RestoreToken>>>,
) -> anyhow::Result<()> {
    let _ = ready.send(Err(unsupported()));
    Ok(())
}
