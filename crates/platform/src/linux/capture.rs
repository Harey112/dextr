use std::{num::NonZeroU32, os::unix::net::UnixStream};

use anyhow::Context as _;
use ashpd::desktop::{
    Session,
    input_capture::{
        Barrier, Capabilities, ConnectToEISOptions, CreateSession2Options, CreateSessionOptions,
        DisableOptions, EnableOptions, GetZonesOptions, InputCapture, ReleaseOptions,
        SetPointerBarriersOptions, StartOptions,
    },
};
use dextr_proto::{Rect, Side, keys};
use futures::StreamExt;
use reis::{
    ei,
    event::{DeviceCapability, EiEvent},
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{CaptureCommand, CaptureEvent, Captured};

pub(crate) fn spawn_capture(
    side: Side,
    commands: mpsc::UnboundedReceiver<CaptureCommand>,
    events: mpsc::UnboundedSender<CaptureEvent>,
    screens: watch::Sender<Vec<Rect>>,
    ready: oneshot::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    crate::spawn_backend_thread("dextr-capture", move || async move {
        let reason = match run(side, commands, &events, screens, ready).await {
            Ok(()) => "capture closed".to_string(),
            Err(e) => format!("{e:#}"),
        };
        let _ = events.send(CaptureEvent::Stopped { reason });
    })
}

struct Portal<'a> {
    proxy: &'a InputCapture,
    session: &'a Session<InputCapture>,
}

impl Portal<'_> {
    async fn screens(&self) -> anyhow::Result<(Vec<Rect>, u32)> {
        let zones = self
            .proxy
            .zones(self.session, GetZonesOptions::default())
            .await?
            .response()?;
        let rects = zones
            .regions()
            .iter()
            .map(|r| Rect::new(r.x_offset(), r.y_offset(), r.width(), r.height()))
            .collect();
        Ok((rects, zones.zone_set()))
    }

    /// Puts one barrier along the outer edge of every screen facing the peer.
    async fn set_barriers(&self, side: Side, screens: &[Rect], zone_set: u32) -> anyhow::Result<()> {
        let barriers: Vec<Barrier> = Rect::on_outer_edge(screens, side)
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (x, y, w, h) = (r.x, r.y, r.w as i32, r.h as i32);
                // Per the portal spec, right/bottom barriers sit one past the
                // last pixel; left/top ones on the first.
                let pos = match side {
                    Side::Left => (x, y, x, y + h - 1),
                    Side::Right => (x + w, y, x + w, y + h - 1),
                    Side::Top => (x, y, x + w - 1, y),
                    Side::Bottom => (x, y + h, x + w - 1, y + h),
                };
                Barrier::new(NonZeroU32::new(i as u32 + 1).unwrap(), pos)
            })
            .collect();
        let response = self
            .proxy
            .set_pointer_barriers(self.session, &barriers, zone_set, SetPointerBarriersOptions::default())
            .await?
            .response()?;
        if !response.failed_barriers().is_empty() {
            tracing::warn!("compositor rejected barriers {:?}", response.failed_barriers());
        }
        Ok(())
    }

    async fn refresh(&self, side: Side, screens_tx: &watch::Sender<Vec<Rect>>) -> anyhow::Result<()> {
        let (screens, zone_set) = self.screens().await?;
        tracing::info!(?screens, %side, "capture zones");
        self.set_barriers(side, &screens, zone_set).await?;
        screens_tx.send_replace(screens);
        Ok(())
    }
}

async fn open_session(proxy: &InputCapture) -> anyhow::Result<Session<InputCapture>> {
    let caps = Capabilities::KEYBOARD | Capabilities::POINTER;
    match proxy.create_session2(CreateSession2Options::default()).await {
        Ok(session) => {
            proxy
                .start(&session, None, StartOptions::default().set_capabilities(caps))
                .await?
                .response()
                .context("input capture was not allowed")?;
            Ok(session)
        }
        Err(ashpd::Error::RequiresVersion(_, _)) => {
            let options = CreateSessionOptions::default().set_capabilities(caps);
            Ok(proxy.create_session(None, options).await?.0)
        }
        Err(e) => Err(e.into()),
    }
}

async fn run(
    mut side: Side,
    mut commands: mpsc::UnboundedReceiver<CaptureCommand>,
    events: &mpsc::UnboundedSender<CaptureEvent>,
    screens_tx: watch::Sender<Vec<Rect>>,
    ready: oneshot::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let setup = async {
        let proxy = InputCapture::new()
            .await
            .context("the desktop has no InputCapture portal (needs KDE Plasma 6.1+ or GNOME 45+)")?;
        let session = open_session(&proxy).await?;
        let fd = proxy.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        let (connection, stream) = context
            .handshake_tokio("dextr", ei::handshake::ContextType::Receiver)
            .await?;
        anyhow::Ok((proxy, session, context, connection, stream))
    };
    let (proxy, session, _context, connection, mut ei_events) = match setup.await {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let portal = Portal { proxy: &proxy, session: &session };
    if let Err(e) = portal.refresh(side, &screens_tx).await {
        let _ = ready.send(Err(e));
        return Ok(());
    }

    let mut activated = Box::pin(proxy.receive_activated().await?);
    let mut deactivated = Box::pin(proxy.receive_deactivated().await?);
    let mut disabled = Box::pin(proxy.receive_disabled().await?);
    let mut zones_changed = Box::pin(proxy.receive_zones_changed().await?);
    let _ = ready.send(Ok(()));

    let mut enabled = false;
    let mut activation_id = None;
    // Where capture last activated, while it is active (cursor hidden).
    let mut active: Option<(f32, f32)> = None;
    let emit = |e: Captured| {
        let _ = events.send(CaptureEvent::Input(e));
    };

    let result = loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break Ok(()),
                Some(CaptureCommand::Enable) if !enabled => {
                    proxy.enable(&session, EnableOptions::default()).await?;
                    enabled = true;
                }
                Some(CaptureCommand::Disable) if enabled => {
                    // Disabling alone can leave an active capture holding the
                    // pointer hidden: release it first.
                    if let Some((x, y)) = active.take() {
                        let options = ReleaseOptions::default()
                            .set_activation_id(activation_id)
                            .set_cursor_position(Some((x as f64, y as f64)));
                        proxy.release(&session, options).await?;
                    }
                    proxy.disable(&session, DisableOptions::default()).await?;
                    enabled = false;
                }
                Some(CaptureCommand::Enable | CaptureCommand::Disable) => {}
                Some(CaptureCommand::Release { x, y }) => {
                    active = None;
                    let options = ReleaseOptions::default()
                        .set_activation_id(activation_id)
                        .set_cursor_position(Some((x as f64, y as f64)));
                    proxy.release(&session, options).await?;
                }
                Some(CaptureCommand::SetSide(new_side)) => {
                    side = new_side;
                    portal.refresh(side, &screens_tx).await?;
                }
            },
            Some(signal) = activated.next() => {
                activation_id = signal.activation_id();
                let (x, y) = signal.cursor_position().unwrap_or_default();
                if !enabled {
                    // Raced with a Disable (for example the peer just took
                    // control and moved our pointer onto the edge): give the
                    // pointer straight back instead of hiding it.
                    let options = ReleaseOptions::default()
                        .set_activation_id(activation_id)
                        .set_cursor_position(Some((x as f64, y as f64)));
                    proxy.release(&session, options).await?;
                    continue;
                }
                active = Some((x, y));
                let _ = events.send(CaptureEvent::Activated { x, y });
            }
            Some(_) = deactivated.next() => {
                active = None;
                let _ = events.send(CaptureEvent::Deactivated);
            }
            Some(_) = disabled.next() => {
                // The compositor disables the session when screens change.
                // Re-arm it with fresh barriers if we were active.
                tracing::info!("capture disabled by compositor");
                if enabled {
                    portal.refresh(side, &screens_tx).await?;
                    proxy.enable(&session, EnableOptions::default()).await?;
                }
            }
            Some(_) = zones_changed.next() => {
                portal.refresh(side, &screens_tx).await?;
            }
            event = ei_events.next() => match event {
                None => break Err(anyhow::anyhow!("libei connection closed")),
                Some(Err(e)) => break Err(e.into()),
                Some(Ok(event)) => match event {
                    EiEvent::SeatAdded(e) => {
                        e.seat.bind_capabilities(
                            DeviceCapability::Pointer
                                | DeviceCapability::PointerAbsolute
                                | DeviceCapability::Keyboard
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                        );
                        let _ = connection.flush();
                    }
                    EiEvent::PointerMotion(e) => emit(Captured::Motion { dx: e.dx, dy: e.dy }),
                    EiEvent::Button(e) => emit(Captured::Button {
                        button: super::button_from_evdev(e.button),
                        pressed: e.state == ei::button::ButtonState::Press,
                    }),
                    EiEvent::ScrollDelta(e) => emit(Captured::Scroll { dx: e.dx, dy: e.dy }),
                    EiEvent::ScrollDiscrete(e) => emit(Captured::ScrollDiscrete {
                        dx: e.discrete_dx,
                        dy: e.discrete_dy,
                    }),
                    EiEvent::ScrollStop(_) | EiEvent::ScrollCancel(_) => emit(Captured::ScrollStop),
                    EiEvent::KeyboardKey(e) => match keys::from_evdev(e.key) {
                        Some(key) => emit(Captured::Key {
                            key,
                            pressed: e.state == ei::keyboard::KeyState::Press,
                        }),
                        None => tracing::debug!("no HID mapping for evdev key {}", e.key),
                    },
                    EiEvent::Disconnected(e) => {
                        break Err(anyhow::anyhow!("compositor ended the capture: {:?}", e.reason));
                    }
                    _ => {}
                },
            },
        }
    };
    let _ = session.close().await;
    result
}
