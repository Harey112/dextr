use std::{collections::HashSet, os::unix::net::UnixStream};

use anyhow::Context as _;
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
};
use dextr_proto::{Input, Rect, keys};
use futures::StreamExt;
use reis::{
    ei,
    event::{Connection, Device, DeviceCapability, EiEvent},
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{InjectCommand, RestoreToken};

pub(crate) fn spawn_injector(
    restore: Option<RestoreToken>,
    commands: mpsc::UnboundedReceiver<InjectCommand>,
    screens: watch::Sender<Vec<Rect>>,
    stopped: watch::Sender<Option<String>>,
    ready: oneshot::Sender<anyhow::Result<Option<RestoreToken>>>,
) -> anyhow::Result<()> {
    crate::spawn_backend_thread("dextr-inject", move || async move {
        let reason = match run(restore, commands, screens, ready).await {
            Ok(()) => "injector closed".to_string(),
            Err(e) => format!("{e:#}"),
        };
        stopped.send_replace(Some(reason));
    })
}

/// libei devices granted by the compositor, and what we're holding down on them.
struct Devices {
    connection: Connection,
    list: Vec<Device>,
    resumed: HashSet<u64>,
    sequence: u32,
    keys_down: HashSet<u32>,
    buttons_down: HashSet<u32>,
}

impl Devices {
    fn id(device: &Device) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        device.hash(&mut h);
        h.finish()
    }

    fn with(&self, cap: DeviceCapability) -> Option<&Device> {
        self.list
            .iter()
            .find(|d| d.has_capability(cap) && self.resumed.contains(&Self::id(d)))
    }

    fn screens(&self) -> Vec<Rect> {
        self.list
            .iter()
            .filter(|d| d.has_capability(DeviceCapability::PointerAbsolute))
            .flat_map(|d| d.regions())
            .map(|r| Rect::new(r.x as i32, r.y as i32, r.width, r.height))
            .collect()
    }

    fn frame(&self, device: &Device) {
        device.device().frame(self.connection.serial(), super::now_us());
    }

    fn move_to(&self, x: f32, y: f32) {
        if let Some(d) = self.with(DeviceCapability::PointerAbsolute)
            && let Some(p) = d.interface::<ei::PointerAbsolute>()
        {
            p.motion_absolute(x, y);
            self.frame(d);
        }
    }

    fn button(&mut self, code: u32, pressed: bool) {
        if let Some(d) = self.with(DeviceCapability::Button)
            && let Some(b) = d.interface::<ei::Button>()
        {
            let state = if pressed { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released };
            b.button(code, state);
            self.frame(d);
        }
        if pressed {
            self.buttons_down.insert(code);
        } else {
            self.buttons_down.remove(&code);
        }
    }

    fn key(&mut self, code: u32, pressed: bool) {
        if let Some(d) = self.with(DeviceCapability::Keyboard)
            && let Some(k) = d.interface::<ei::Keyboard>()
        {
            let state = if pressed { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released };
            k.key(code, state);
            self.frame(d);
        }
        if pressed {
            self.keys_down.insert(code);
        } else {
            self.keys_down.remove(&code);
        }
    }

    fn scroll(&self, f: impl FnOnce(&ei::Scroll)) {
        if let Some(d) = self.with(DeviceCapability::Scroll)
            && let Some(s) = d.interface::<ei::Scroll>()
        {
            f(&s);
            self.frame(d);
        }
    }

    fn inject(&mut self, input: Input) {
        match input {
            Input::Motion { x, y } => self.move_to(x, y),
            Input::Button { button, pressed, x, y } => {
                self.move_to(x, y);
                self.button(super::button_to_evdev(button), pressed);
            }
            Input::Scroll { dx, dy } => self.scroll(|s| s.scroll(dx, dy)),
            Input::ScrollDiscrete { dx, dy } => self.scroll(|s| s.scroll_discrete(dx, dy)),
            Input::ScrollStop => self.scroll(|s| s.scroll_stop(1, 1, 0)),
            Input::Key { key, pressed } => match keys::to_evdev(key) {
                Some(code) => self.key(code, pressed),
                None => tracing::debug!("no evdev mapping for {key:?}"),
            },
        }
    }

    fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys_down) {
            self.key(code, false);
        }
        for code in std::mem::take(&mut self.buttons_down) {
            self.button(code, false);
        }
    }
}

async fn run(
    restore: Option<RestoreToken>,
    mut commands: mpsc::UnboundedReceiver<InjectCommand>,
    screens_tx: watch::Sender<Vec<Rect>>,
    ready: oneshot::Sender<anyhow::Result<Option<RestoreToken>>>,
) -> anyhow::Result<()> {
    let setup = async {
        let proxy = RemoteDesktop::new()
            .await
            .context("the desktop has no RemoteDesktop portal")?;
        let session = proxy.create_session(CreateSessionOptions::default()).await?;
        let select = SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore.as_deref());
        proxy.select_devices(&session, select).await?.response()?;
        let started = proxy
            .start(&session, None, StartOptions::default())
            .await?
            .response()
            .context("remote input was not allowed")?;
        let token = started.restore_token().map(str::to_owned);
        let fd = proxy.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        let (connection, stream) = context
            .handshake_tokio("dextr", ei::handshake::ContextType::Sender)
            .await?;
        anyhow::Ok((proxy, session, context, connection, stream, token))
    };
    let (_proxy, session, _context, connection, mut ei_events, token) = match setup.await {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(token));

    let mut devices = Devices {
        connection: connection.clone(),
        list: Vec::new(),
        resumed: HashSet::new(),
        sequence: 0,
        keys_down: HashSet::new(),
        buttons_down: HashSet::new(),
    };

    let result = loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None => {
                        devices.release_all();
                        let _ = connection.flush();
                        break Ok(());
                    }
                    Some(InjectCommand::Input(input)) => devices.inject(input),
                    Some(InjectCommand::ReleaseAll) => devices.release_all(),
                }
                let _ = connection.flush();
            }
            event = ei_events.next() => match event {
                None => break Err(anyhow::anyhow!("libei connection closed")),
                Some(Err(e)) => break Err(e.into()),
                Some(Ok(event)) => {
                    match event {
                        EiEvent::SeatAdded(e) => e.seat.bind_capabilities(
                            DeviceCapability::Pointer
                                | DeviceCapability::PointerAbsolute
                                | DeviceCapability::Keyboard
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                        ),
                        EiEvent::DeviceAdded(e) => {
                            tracing::info!(name = ?e.device.name(), regions = ?e.device.regions(), "inject device added");
                            devices.list.push(e.device);
                            screens_tx.send_replace(devices.screens());
                        }
                        EiEvent::DeviceRemoved(e) => {
                            devices.resumed.remove(&Devices::id(&e.device));
                            devices.list.retain(|d| d != &e.device);
                            screens_tx.send_replace(devices.screens());
                        }
                        EiEvent::DeviceResumed(e) => {
                            e.device.device().start_emulating(e.serial, devices.sequence);
                            devices.sequence = devices.sequence.wrapping_add(1);
                            devices.resumed.insert(Devices::id(&e.device));
                        }
                        EiEvent::DevicePaused(e) => {
                            devices.resumed.remove(&Devices::id(&e.device));
                        }
                        EiEvent::Disconnected(e) => {
                            break Err(anyhow::anyhow!("compositor ended remote input: {:?}", e.reason));
                        }
                        _ => {}
                    }
                    let _ = connection.flush();
                }
            },
        }
    };
    let _ = session.close().await;
    result
}
