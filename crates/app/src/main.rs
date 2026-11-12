//! dextr desktop app: status, displays, pairing, input and clipboard settings.
//!
//! The UI only edits settings and shows state. All input sharing happens in
//! `dextr-core` on its own thread, so the UI never sits in the input path.

mod arrangement;
mod autostart;
mod icon;
mod pages;

use std::collections::VecDeque;

use dextr_core::{
    Command, Config, Event, LocalAddress, Mode, Nearby, PairingState,
    settings::{Appearance, ModifierMap, ReturnShortcut},
};
use dextr_proto::{Os, Rect};
use iced::{
    Subscription, Task, Theme,
    futures::{SinkExt, Stream},
};
use tokio::sync::mpsc::UnboundedSender;
use dextr_core::edge::Link;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    icon::install_desktop_entry();

    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(980.0, 680.0),
            icon: icon::window_icon(),
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: icon::APP_ID.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Overview,
    Displays,
    Pairing,
    Mouse,
    Keyboard,
    Clipboard,
    Activity,
    Settings,
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::Overview,
        Page::Displays,
        Page::Pairing,
        Page::Mouse,
        Page::Keyboard,
        Page::Clipboard,
        Page::Activity,
        Page::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Page::Overview => "Overview",
            Page::Displays => "Displays",
            Page::Pairing => "Pairing",
            Page::Mouse => "Mouse",
            Page::Keyboard => "Keyboard",
            Page::Clipboard => "Clipboard",
            Page::Activity => "Activity",
            Page::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum Access {
    #[default]
    Pending,
    Granted,
    Denied(String),
}

pub struct Peer {
    pub name: String,
    pub os: Os,
    pub address: String,
    pub screens: Vec<Rect>,
}

/// Actions that need a second click to confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Danger {
    ForgetPeer,
    NewFingerprint,
    ResetSettings,
}

#[derive(Default)]
pub struct App {
    engine: Option<UnboundedSender<Command>>,
    pub page: Page,
    /// Settings as last saved.
    pub saved: Option<Config>,
    /// Settings being edited. Text fields apply on Save; switches, lists and
    /// sliders apply immediately.
    pub form: Form,
    pub fingerprint: String,
    pub listening: Option<u16>,
    pub capture: Access,
    pub inject: Access,
    pub peer: Option<Peer>,
    pub connecting: bool,
    pub reconnect_in: Option<u64>,
    pub mode: Option<Mode>,
    pub paused: bool,
    pub local_screens: Vec<Rect>,
    pub log: VecDeque<String>,
    pub notice: Option<String>,
    pub launch_at_login: bool,
    pub confirm: Option<Danger>,
    pub restart_needed: bool,
    pub pairing: Option<PairingState>,
    pub pairing_error: Option<String>,
    pub nearby: Vec<Nearby>,
    pub addresses: Vec<LocalAddress>,
    /// The "pair by address" field.
    pub pair_address: String,
}

/// The editable copy of the settings.
#[derive(Default)]
pub struct Form {
    pub name: String,
    pub port: String,
    pub address: String,
    pub fingerprint: String,
    /// Everything else, edited in place.
    pub config: Config,
}

impl Form {
    fn from_config(c: &Config) -> Self {
        Self {
            name: c.device_name.clone(),
            port: c.port.to_string(),
            address: c.peer_address.clone(),
            fingerprint: c.peer_fingerprint.clone(),
            config: c.clone(),
        }
    }

    fn to_config(&self, base: &Config) -> Result<Config, String> {
        let Some(port) = self.port.trim().parse::<u16>().ok().filter(|p| *p > 0) else {
            return Err("Port must be a number from 1 to 65535.".into());
        };
        if !self.fingerprint.trim().is_empty() && dextr_core::identity::parse(&self.fingerprint).is_none() {
            return Err("That fingerprint isn't valid. It should be 64 letters and digits (0–9, A–F).".into());
        }
        if self.name.trim().is_empty() {
            return Err("Give this device a name.".into());
        }
        Ok(Config {
            device_name: self.name.trim().to_owned(),
            port,
            peer_address: self.address.trim().to_owned(),
            peer_fingerprint: self.fingerprint.trim().to_owned(),
            // Owned by the engine.
            inject_restore_token: base.inject_restore_token.clone(),
            last_peer_name: base.last_peer_name.clone(),
            last_peer_screens: base.last_peer_screens.clone(),
            ..self.config.clone()
        })
    }
}

/// One setting changed in the UI.
#[derive(Debug, Clone)]
pub enum Setting {
    Arrange(Link),
    DeadCorners(u32),
    SwitchDelay(u32),
    PointerSpeed(f32),
    ScrollSpeed(f32),
    NaturalScroll(bool),
    SwapButtons(bool),
    Modifiers(ModifierMap),
    ReturnShortcut(ReturnShortcut),
    AutoConnect(bool),
    AutoReconnect(bool),
    AllowIncoming(bool),
    AllowControl(bool),
    Appearance(Appearance),
    ClipboardEnabled(bool),
    ClipboardImages(bool),
    ClipboardMaxMb(u32),
}

impl Setting {
    /// Sliders send many values while dragging; save once on release.
    fn saves_immediately(&self) -> bool {
        !matches!(
            self,
            Setting::DeadCorners(_)
                | Setting::SwitchDelay(_)
                | Setting::PointerSpeed(_)
                | Setting::ScrollSpeed(_)
                | Setting::ClipboardMaxMb(_)
        )
    }

    fn apply(self, c: &mut Config) {
        match self {
            Setting::Arrange(l) => {
                c.peer_side = l.side;
                c.peer_offset = l.offset;
            }
            Setting::DeadCorners(v) => c.switching.dead_corners = v,
            Setting::SwitchDelay(v) => c.switching.delay_ms = v,
            Setting::PointerSpeed(v) => c.mouse.speed = v,
            Setting::ScrollSpeed(v) => c.mouse.scroll_speed = v,
            Setting::NaturalScroll(v) => c.mouse.natural_scroll = v,
            Setting::SwapButtons(v) => c.mouse.swap_buttons = v,
            Setting::Modifiers(v) => c.keyboard.modifiers = v,
            Setting::ReturnShortcut(v) => c.keyboard.return_shortcut = v,
            Setting::AutoConnect(v) => c.connection.auto_connect = v,
            Setting::AutoReconnect(v) => c.connection.auto_reconnect = v,
            Setting::AllowIncoming(v) => c.connection.allow_incoming = v,
            Setting::AllowControl(v) => c.connection.allow_control = v,
            Setting::Appearance(v) => c.appearance = v,
            Setting::ClipboardEnabled(v) => c.clipboard.enabled = v,
            Setting::ClipboardImages(v) => c.clipboard.images = v,
            Setting::ClipboardMaxMb(v) => c.clipboard.max_mb = v,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    EngineStarted(UnboundedSender<Command>),
    Engine(Box<Event>),
    Navigate(Page),
    Name(String),
    Port(String),
    PairAddress(String),
    PairWith(String),
    PairDecision(bool),
    Copy(String),
    Set(Setting),
    Save,
    Connect,
    Disconnect,
    Paused(bool),
    RetryPermissions,
    CopyFingerprint,
    ClearLog,
    OpenConfigFolder,
    DismissNotice,
    LaunchAtLogin(bool),
    Arm(Danger),
    Confirm(Danger),
    Cancel,
}

/// Starts the engine and turns its events into UI messages.
fn engine_events() -> impl Stream<Item = Message> {
    iced::stream::channel(256, async |mut out| {
        let (commands, mut events) = dextr_core::spawn();
        let _ = out.send(Message::EngineStarted(commands)).await;
        while let Some(event) = events.recv().await {
            if out.send(Message::Engine(Box::new(event))).await.is_err() {
                break;
            }
        }
    })
}

impl App {
    fn new() -> Self {
        Self { launch_at_login: autostart::enabled(), ..Self::default() }
    }

    fn title(&self) -> String {
        match (&self.peer, self.mode) {
            (Some(p), Some(Mode::Controlling)) => format!("dextr — controlling {}", p.name),
            (Some(p), _) => format!("dextr — connected to {}", p.name),
            _ => "dextr".into(),
        }
    }

    fn theme(&self) -> Option<Theme> {
        match self.form.config.appearance {
            Appearance::System => None,
            Appearance::Light => Some(Theme::Light),
            Appearance::Dark => Some(Theme::Dark),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(engine_events)
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push_front(line.into());
        self.log.truncate(500);
    }

    fn notify(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.push_log(line.clone());
        self.notice = Some(line);
    }

    fn send(&self, command: Command) {
        if let Some(engine) = &self.engine {
            let _ = engine.send(command);
        }
    }

    pub fn unsaved(&self) -> bool {
        self.saved.as_ref().is_some_and(|base| self.form.to_config(base).ok().as_ref() != Some(base))
    }

    /// Saves the form if it changed. Returns false if it's invalid.
    fn save(&mut self) -> bool {
        let Some(base) = &self.saved else { return false };
        match self.form.to_config(base) {
            Ok(config) => {
                if &config != base {
                    self.send(Command::Apply(config.clone()));
                    self.saved = Some(config);
                }
                true
            }
            Err(e) => {
                self.notify(e);
                false
            }
        }
    }

    /// Saved settings, or defaults before the engine reports in.
    pub fn config(&self) -> &Config {
        &self.form.config
    }

    /// The connected peer's screens, if it has reported any.
    pub fn peer_screens(&self) -> Option<&[Rect]> {
        self.peer.as_ref().map(|p| p.screens.as_slice()).filter(|s| !s.is_empty())
    }

    pub fn peer_name(&self) -> String {
        if let Some(p) = &self.peer {
            return p.name.clone();
        }
        self.saved
            .as_ref()
            .and_then(|c| c.last_peer_name.clone())
            .unwrap_or_else(|| "Other device".into())
    }

    pub fn peer_os(&self) -> Option<Os> {
        self.peer.as_ref().map(|p| p.os)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if !matches!(message, Message::Arm(_) | Message::Confirm(_) | Message::Engine(_)) {
            self.confirm = None;
        }
        match message {
            Message::EngineStarted(tx) => {
                self.engine = Some(tx);
                // The app is running now (main thread): give it its Dock icon.
                #[cfg(target_os = "macos")]
                icon::set_dock_icon();
            }
            Message::Engine(event) => return self.on_event(*event),
            Message::Navigate(page) => self.page = page,
            Message::Name(v) => self.form.name = v,
            Message::Port(v) => self.form.port = v,
            Message::PairAddress(v) => self.pair_address = v,
            Message::PairWith(address) => {
                if !address.trim().is_empty() {
                    self.pairing_error = None;
                    self.send(Command::Pair(address));
                }
            }
            Message::PairDecision(ok) => self.send(Command::PairDecision(ok)),
            Message::Copy(text) => {
                self.push_log(format!("Copied {text}."));
                return iced::clipboard::write(text);
            }
            Message::Set(setting) => {
                let save_now = setting.saves_immediately();
                setting.apply(&mut self.form.config);
                if save_now {
                    self.save();
                }
            }
            Message::Save => {
                if self.save() {
                    self.notice = None;
                }
            }
            Message::Connect => {
                if self.save() {
                    self.connecting = true;
                    self.send(Command::Connect);
                }
            }
            Message::Disconnect => {
                self.reconnect_in = None;
                self.send(Command::Disconnect);
            }
            Message::Paused(paused) => {
                self.paused = paused;
                self.send(Command::SetPaused(paused));
            }
            Message::RetryPermissions => {
                if matches!(self.capture, Access::Denied(_)) {
                    self.capture = Access::Pending;
                }
                if matches!(self.inject, Access::Denied(_)) {
                    self.inject = Access::Pending;
                }
                self.send(Command::RetryPermissions);
            }
            Message::CopyFingerprint => {
                self.push_log("Fingerprint copied.");
                return iced::clipboard::write(self.fingerprint.clone());
            }
            Message::ClearLog => self.log.clear(),
            Message::OpenConfigFolder => {
                if let Ok(dir) = dextr_core::config::dir() {
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else if cfg!(windows) {
                        "explorer"
                    } else {
                        "xdg-open"
                    };
                    if let Err(e) = std::process::Command::new(opener).arg(&dir).spawn() {
                        self.notify(format!("Cannot open {}: {e}", dir.display()));
                    }
                }
            }
            Message::DismissNotice => self.notice = None,
            Message::LaunchAtLogin(on) => match autostart::set(on) {
                Ok(()) => {
                    self.launch_at_login = on;
                    self.push_log(if on { "dextr will start when you log in." } else { "dextr won't start at login." });
                }
                Err(e) => self.notify(format!("Cannot change launch at login: {e}")),
            },
            Message::Arm(danger) => self.confirm = Some(danger),
            Message::Cancel => self.confirm = None,
            Message::Confirm(danger) => {
                self.confirm = None;
                match danger {
                    Danger::ForgetPeer => self.send(Command::ForgetPeer),
                    Danger::ResetSettings => self.send(Command::ResetSettings),
                    Danger::NewFingerprint => self.new_fingerprint(),
                }
            }
        }
        Task::none()
    }

    fn new_fingerprint(&mut self) {
        let result = dextr_core::config::dir().and_then(|dir| {
            for f in ["cert.der", "key.der"] {
                match std::fs::remove_file(dir.join(f)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.restart_needed = true;
                self.notify("A new fingerprint will be created when you restart dextr. Then pair the devices again.");
            }
            Err(e) => self.notify(format!("Cannot reset the fingerprint: {e:#}")),
        }
    }

    fn on_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Ready { config, fingerprint, listening } => {
                self.form = Form::from_config(&config);
                self.saved = Some(config);
                self.fingerprint = fingerprint;
                self.listening = listening.map(|a| a.port());
                self.mode = Some(Mode::Local);
            }
            Event::Config(config) => {
                self.form = Form::from_config(&config);
                self.saved = Some(config);
            }
            Event::CaptureReady(r) => {
                self.capture = match r {
                    Ok(()) => Access::Granted,
                    Err(e) => {
                        Access::Denied(e)
                    }
                }
            }
            Event::InjectReady(r) => {
                self.inject = match r {
                    Ok(()) => Access::Granted,
                    Err(e) => {
                        Access::Denied(e)
                    }
                }
            }
            Event::Connected { name, os, address, screens } => {
                self.connecting = false;
                self.reconnect_in = None;
                self.notice = None;
                self.push_log(format!("Connected to {name}."));
                if let Some(saved) = &mut self.saved {
                    saved.last_peer_name = Some(name.clone());
                    saved.last_peer_screens = screens.clone();
                }
                self.peer = Some(Peer { name, os, address: address.ip().to_string(), screens });
            }
            Event::Disconnected { reason } => {
                self.connecting = false;
                self.push_log(format!("Disconnected: {reason}"));
                self.peer = None;
            }
            Event::Reconnecting(secs) => {
                self.connecting = false;
                self.reconnect_in = (secs > 0).then_some(secs);
            }
            Event::LocalScreens(screens) => self.local_screens = screens,
            Event::PeerScreens(screens) => {
                if let Some(saved) = &mut self.saved {
                    saved.last_peer_screens = screens.clone();
                }
                if let Some(p) = &mut self.peer {
                    p.screens = screens;
                }
            }
            Event::Mode(m) => self.mode = Some(m),
            Event::Pairing(state) => {
                // A request from the other device: bring the code into view.
                if let Some(PairingState::Confirm { incoming: true, .. }) = &state
                    && !matches!(self.pairing, Some(PairingState::Confirm { .. }))
                {
                    self.page = Page::Pairing;
                }
                if state.is_some() {
                    self.pairing_error = None;
                }
                self.pairing = state;
            }
            Event::PairingFailed(reason) => self.pairing_error = Some(reason),
            Event::Nearby(list) => self.nearby = list,
            Event::Addresses(list) => self.addresses = list,
            Event::Log(line) => {
                if line.starts_with("Connection failed") {
                    self.connecting = false;
                    if self.reconnect_in.is_none() {
                        self.notify(line);
                    } else {
                        self.push_log(line);
                    }
                } else {
                    self.push_log(line);
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> iced::Element<'_, Message> {
        pages::view(self)
    }
}

pub fn os_name(os: Os) -> &'static str {
    match os {
        Os::Linux => "Linux",
        Os::MacOs => "macOS",
        Os::Windows => "Windows",
        Os::Other => "Other",
    }
}
