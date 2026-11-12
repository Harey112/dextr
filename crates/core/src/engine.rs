use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context as _, bail};
use dextr_platform::{CaptureEvent, CaptureHandle, Captured, InjectorHandle};
use dextr_proto::{Button, DEFAULT_PORT, Hello, Input, Key, MAX_FRAME, Message, Os, PROTOCOL_VERSION, Rect, Side};
use tokio::{sync::mpsc, time::Instant};

use crate::{
    config::{self, Config},
    edge,
    identity::{self, Identity},
    tls::{self, Trusted},
};

/// Requests from the UI.
#[derive(Debug, Clone)]
pub enum Command {
    /// Save settings and apply what can be applied live.
    Apply(Config),
    /// Connect to the configured peer.
    Connect,
    Disconnect,
    /// Ask the OS again for any permission that was refused.
    RetryPermissions,
    /// Pause or resume sending this mouse and keyboard to the peer.
    SetPaused(bool),
    /// Disconnect and remove the paired device.
    ForgetPeer,
    /// Restore every setting to its default (keeps this device's identity).
    ResetSettings,
}

/// Who owns the pointer right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pointer is on this device, used locally.
    Local,
    /// Our mouse and keyboard drive the peer.
    Controlling,
    /// The peer's mouse and keyboard drive this device.
    Controlled,
}

/// Notifications for the UI.
#[derive(Debug, Clone)]
pub enum Event {
    Ready { config: Config, fingerprint: String, listening: Option<SocketAddr> },
    /// Local input capture (sending to the peer) is available, or why not.
    CaptureReady(Result<(), String>),
    /// Local input injection (being controlled) is available, or why not.
    InjectReady(Result<(), String>),
    Connected { name: String, os: Os, address: SocketAddr, screens: Vec<Rect> },
    Disconnected { reason: String },
    /// This device's screens, in logical pixels.
    LocalScreens(Vec<Rect>),
    /// The connected peer's screens changed.
    PeerScreens(Vec<Rect>),
    Mode(Mode),
    /// The saved settings changed in the engine (forget, reset).
    Config(Config),
    /// Waiting to reconnect, in seconds.
    Reconnecting(u64),
    Log(String),
}

/// Starts the engine on its own thread and runtime.
pub fn spawn() -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("dextr-core".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("dextr-net")
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => rt.block_on(run(cmd_rx, event_tx)),
                Err(e) => {
                    let _ = event_tx.send(Event::Log(format!("cannot start engine: {e}")));
                }
            }
        })
        .expect("spawning the engine thread");
    (cmd_tx, event_rx)
}

const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

enum Internal {
    InjectorStarted(anyhow::Result<(InjectorHandle, Option<String>)>),
    CaptureStarted(anyhow::Result<(CaptureHandle, mpsc::UnboundedReceiver<CaptureEvent>)>),
    Established(anyhow::Result<Established>),
    Inbound { session: u64, message: Message },
    SessionClosed { session: u64, reason: String },
}

struct Established {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    peer: Hello,
}

struct Session {
    id: u64,
    conn: quinn::Connection,
    out: mpsc::UnboundedSender<Message>,
    peer: Hello,
}

struct Engine {
    config: Config,
    identity: Arc<Identity>,
    trusted: Trusted,
    endpoint: Option<quinn::Endpoint>,
    events: mpsc::UnboundedSender<Event>,
    internal: mpsc::UnboundedSender<Internal>,
    capture: Option<CaptureHandle>,
    injector: Option<InjectorHandle>,
    session: Option<Session>,
    next_session: u64,
    mode: Mode,
    /// Virtual pointer on the peer, in its coordinates, while controlling.
    pointer: (f32, f32),
    ctrl_down: bool,
    alt_down: bool,
    paused: bool,
    starting_capture: bool,
    starting_injector: bool,
    /// The user disconnected on purpose: don't reconnect on our own.
    user_disconnected: bool,
    reconnect_at: Option<Instant>,
    reconnect_delay: Duration,
    connecting: bool,
    /// Pointer is pushing against the edge, waiting for the switch delay.
    pending_switch: Option<PendingSwitch>,
}

struct PendingSwitch {
    deadline: Instant,
    /// Where the pointer hit our edge.
    at: (f32, f32),
    /// Where it will enter the peer.
    entry: (f32, f32),
    /// Movement back away from the edge since then.
    retreat: f32,
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn run(mut commands: mpsc::UnboundedReceiver<Command>, events: mpsc::UnboundedSender<Event>) {
    let log = |msg: String| {
        tracing::info!("{msg}");
        let _ = events.send(Event::Log(msg));
    };

    let config = Config::load().unwrap_or_else(|e| {
        log(format!("Using default settings: {e:#}"));
        Config::default()
    });
    let identity = match config::dir().and_then(|d| Identity::load_or_create(&d)) {
        Ok(id) => Arc::new(id),
        Err(e) => {
            log(format!("Cannot load this device's identity: {e:#}"));
            return;
        }
    };
    let trusted: Trusted = Arc::new(RwLock::new(identity::parse(&config.peer_fingerprint)));
    let endpoint = match bind(&identity, trusted.clone(), config.port) {
        Ok(ep) => Some(ep),
        Err(e) => {
            log(format!("Cannot listen on UDP port {}: {e:#}", config.port));
            None
        }
    };
    let _ = events.send(Event::Ready {
        config: config.clone(),
        fingerprint: identity::format(&identity.fingerprint),
        listening: endpoint.as_ref().and_then(|e| e.local_addr().ok()),
    });

    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();

    let mut engine = Engine {
        config,
        identity,
        trusted,
        endpoint: endpoint.clone(),
        events: events.clone(),
        internal: internal_tx,
        capture: None,
        injector: None,
        session: None,
        next_session: 0,
        mode: Mode::Local,
        pointer: (0.0, 0.0),
        ctrl_down: false,
        alt_down: false,
        paused: false,
        starting_capture: false,
        starting_injector: false,
        user_disconnected: false,
        reconnect_at: None,
        reconnect_delay: RECONNECT_MIN,
        connecting: false,
        pending_switch: None,
    };
    engine.start_backends();
    if engine.config.connection.auto_connect && engine.peer_configured() {
        engine.connect();
    }
    let mut capture_rx: Option<mpsc::UnboundedReceiver<CaptureEvent>> = None;
    // A second handle used only to await injector notifications, so those
    // futures don't borrow `engine`.
    let mut injector_watch: Option<InjectorHandle> = None;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(c) => engine.command(c),
                None => break,
            },
            Some(msg) = internal_rx.recv() => match msg {
                Internal::InjectorStarted(Ok((handle, token))) => {
                    engine.starting_injector = false;
                    injector_watch = Some(handle.clone());
                    engine.injector = Some(handle);
                    let _ = events.send(Event::InjectReady(Ok(())));
                    engine.report_screens();
                    if token.is_some() && token != engine.config.inject_restore_token {
                        engine.config.inject_restore_token = token;
                        if let Err(e) = engine.config.save() {
                            engine.log(format!("Cannot save settings: {e:#}"));
                        }
                    }
                }
                Internal::InjectorStarted(Err(e)) => {
                    engine.starting_injector = false;
                    let _ = events.send(Event::InjectReady(Err(format!("{e:#}"))));
                }
                Internal::CaptureStarted(Ok((handle, rx))) => {
                    engine.starting_capture = false;
                    engine.capture = Some(handle);
                    engine.sync_capture();
                    capture_rx = Some(rx);
                    let _ = events.send(Event::CaptureReady(Ok(())));
                    engine.report_screens();
                }
                Internal::CaptureStarted(Err(e)) => {
                    engine.starting_capture = false;
                    let _ = events.send(Event::CaptureReady(Err(format!("{e:#}"))));
                }
                other => engine.internal(other),
            },
            event = recv_opt(&mut capture_rx) => match event {
                Some(e) => engine.captured(e),
                None => {
                    capture_rx = None;
                    engine.capture = None;
                }
            },
            () = sleep_opt(engine.reconnect_at) => {
                engine.reconnect_at = None;
                if engine.session.is_none() && !engine.user_disconnected {
                    engine.connect();
                }
            }
            () = sleep_opt(engine.pending_switch.as_ref().map(|p| p.deadline)) => engine.commit_switch(),
            incoming = accept_opt(&endpoint) => {
                if let Some(incoming) = incoming {
                    if !engine.config.connection.allow_incoming {
                        engine.log(format!("Refused a connection from {} (incoming connections are off).", incoming.remote_address().ip()));
                        incoming.refuse();
                        continue;
                    }
                    let (tx, hello) = (engine.internal.clone(), engine.hello());
                    tokio::spawn(async move {
                        let _ = tx.send(Internal::Established(accept(incoming, hello).await));
                    });
                }
            }
            note = injector_note(&mut injector_watch) => match note {
                InjectorNote::Screens(screens) => {
                    engine.send(Message::Displays(screens));
                    engine.report_screens();
                }
                InjectorNote::Stopped(reason) => {
                    injector_watch = None;
                    engine.injector = None;
                    let _ = events.send(Event::InjectReady(Err(reason)));
                }
            },
        }
    }
    engine.end_session("app closed".into());
}

async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn accept_opt(endpoint: &Option<quinn::Endpoint>) -> Option<quinn::Incoming> {
    match endpoint {
        Some(ep) => ep.accept().await,
        None => std::future::pending().await,
    }
}

enum InjectorNote {
    Screens(Vec<Rect>),
    Stopped(String),
}

async fn injector_note(injector: &mut Option<InjectorHandle>) -> InjectorNote {
    let Some(i) = injector else { return std::future::pending().await };
    // Separate clones so each watch keeps its own "seen" state.
    let mut stop_watch = i.clone();
    tokio::select! {
        screens = i.screens_changed() => InjectorNote::Screens(screens),
        reason = stop_watch.stopped() => InjectorNote::Stopped(reason),
    }
}

fn bind(id: &Identity, trusted: Trusted, port: u16) -> anyhow::Result<quinn::Endpoint> {
    let server = tls::server_config(id, trusted)?;
    // Dual-stack first, then IPv4 only.
    quinn::Endpoint::server(server.clone(), SocketAddr::from(([0u16; 8], port)))
        .or_else(|_| quinn::Endpoint::server(server, SocketAddr::from(([0, 0, 0, 0], port))))
        .context("binding the UDP socket")
}

async fn accept(incoming: quinn::Incoming, hello: Hello) -> anyhow::Result<Established> {
    let conn = incoming.await.context("incoming connection failed (is the peer paired?)")?;
    let (send, recv) = conn.accept_bi().await?;
    exchange_hello(conn, send, recv, hello).await
}

async fn connect(
    endpoint: quinn::Endpoint,
    config: quinn::ClientConfig,
    address: String,
    hello: Hello,
) -> anyhow::Result<Established> {
    let address = if address.parse::<SocketAddr>().is_ok() || address.rsplit_once(':').is_some_and(|(h, _)| !h.contains(':')) {
        address
    } else {
        format!("{address}:{DEFAULT_PORT}")
    };
    let target = tokio::net::lookup_host(&address)
        .await
        .with_context(|| format!("cannot resolve {address}"))?
        .next()
        .with_context(|| format!("no address for {address}"))?;
    let conn = tokio::time::timeout(Duration::from_secs(8), endpoint.connect_with(config, target, "dextr")?)
        .await
        .context("timed out (is dextr running there, and is UDP allowed through its firewall?)")?
        .context("connection failed (check both fingerprints)")?;
    let (send, recv) = conn.open_bi().await?;
    exchange_hello(conn, send, recv, hello).await
}

async fn exchange_hello(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    hello: Hello,
) -> anyhow::Result<Established> {
    write_message(&mut send, &Message::Hello(hello)).await?;
    let peer = match tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv)).await?? {
        Message::Hello(h) => h,
        other => bail!("expected Hello, got {other:?}"),
    };
    if peer.protocol != PROTOCOL_VERSION {
        bail!("peer speaks protocol {} but this build speaks {PROTOCOL_VERSION}; update both", peer.protocol);
    }
    Ok(Established { conn, send, recv, peer })
}

async fn write_message(send: &mut quinn::SendStream, msg: &Message) -> anyhow::Result<()> {
    send.write_all(&msg.to_frame()?).await?;
    Ok(())
}

async fn read_message(recv: &mut quinn::RecvStream) -> anyhow::Result<Message> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("peer sent an oversized frame ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Message::from_bytes(&buf)?)
}

impl Engine {
    fn log(&self, msg: String) {
        tracing::info!("{msg}");
        let _ = self.events.send(Event::Log(msg));
    }

    /// Starts whichever OS backends aren't running, one after the other so
    /// permission dialogs don't stack up.
    fn start_backends(&mut self) {
        let need_injector = self.injector.is_none() && !self.starting_injector;
        let need_capture = self.capture.is_none() && !self.starting_capture;
        if !need_injector && !need_capture {
            return;
        }
        self.starting_injector |= need_injector;
        self.starting_capture |= need_capture;
        let tx = self.internal.clone();
        let (restore, side) = (self.config.inject_restore_token.clone(), self.config.peer_side);
        tokio::spawn(async move {
            if need_injector {
                let _ = tx.send(Internal::InjectorStarted(dextr_platform::start_injector(restore).await));
            }
            if need_capture {
                let _ = tx.send(Internal::CaptureStarted(dextr_platform::start_capture(side).await));
            }
        });
    }

    /// Edge watching is on only while connected, not paused, and not being
    /// controlled by the peer.
    fn sync_capture(&self) {
        if let Some(c) = &self.capture {
            if self.session.is_some() && !self.paused && self.mode != Mode::Controlled {
                c.enable();
            } else {
                c.disable();
            }
        }
    }

    fn report_screens(&self) {
        let _ = self.events.send(Event::LocalScreens(self.hello().displays));
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            let _ = self.events.send(Event::Mode(mode));
        }
    }

    /// Our screens in the coordinate space the peer should use for Motion.
    fn hello(&self) -> Hello {
        let from_injector = self.injector.as_ref().map(|i| i.screens()).unwrap_or_default();
        let displays = if from_injector.is_empty() {
            self.capture.as_ref().map(|c| c.screens()).unwrap_or_default()
        } else {
            from_injector
        };
        Hello { protocol: PROTOCOL_VERSION, name: self.config.device_name.clone(), os: Os::current(), displays }
    }

    fn our_screens(&self) -> Vec<Rect> {
        self.capture.as_ref().map(|c| c.screens()).unwrap_or_default()
    }

    fn peer_screens(&self) -> &[Rect] {
        self.session.as_ref().map(|s| s.peer.displays.as_slice()).unwrap_or(&[])
    }

    fn send(&self, msg: Message) {
        if let Some(s) = &self.session {
            let _ = s.out.send(msg);
        }
    }

    /// Unreliable and unordered: only for absolute pointer motion.
    fn send_motion(&self, x: f32, y: f32) {
        if let Some(s) = &self.session
            && let Ok(bytes) = Message::Input(Input::Motion { x, y }).to_bytes()
            && s.conn.send_datagram(bytes.into()).is_err()
        {
            // Datagrams unsupported or too large: fall back to the stream.
            let _ = s.out.send(Message::Input(Input::Motion { x, y }));
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Apply(new) => {
                if new.peer_side != self.config.peer_side
                    && let Some(c) = &self.capture
                {
                    c.set_side(new.peer_side);
                }
                *self.trusted.write().unwrap() = identity::parse(&new.peer_fingerprint);
                if new.port != self.config.port {
                    self.log("The new port takes effect after restarting dextr.".into());
                }
                let old = std::mem::take(&mut self.config);
                self.config = Config {
                    inject_restore_token: old.inject_restore_token,
                    last_peer_name: old.last_peer_name,
                    last_peer_screens: old.last_peer_screens,
                    ..new
                };
                match self.config.save() {
                    Ok(()) => self.log("Settings saved.".into()),
                    Err(e) => self.log(format!("Cannot save settings: {e:#}")),
                }
            }
            Command::Connect => {
                self.user_disconnected = false;
                self.reconnect_delay = RECONNECT_MIN;
                self.reconnect_at = None;
                self.connect();
            }
            Command::Disconnect => {
                self.user_disconnected = true;
                self.reconnect_at = None;
                let _ = self.events.send(Event::Reconnecting(0));
                self.end_session("disconnected".into());
            }
            Command::ForgetPeer => {
                self.user_disconnected = true;
                self.reconnect_at = None;
                self.end_session("device forgotten".into());
                *self.trusted.write().unwrap() = None;
                self.config.peer_address.clear();
                self.config.peer_fingerprint.clear();
                self.config.last_peer_name = None;
                self.config.last_peer_screens.clear();
                self.save_and_publish("Paired device forgotten.");
            }
            Command::ResetSettings => {
                let side_changed = self.config.peer_side != Config::default().peer_side;
                self.config = Config {
                    inject_restore_token: self.config.inject_restore_token.take(),
                    peer_address: std::mem::take(&mut self.config.peer_address),
                    peer_fingerprint: std::mem::take(&mut self.config.peer_fingerprint),
                    last_peer_name: self.config.last_peer_name.take(),
                    last_peer_screens: std::mem::take(&mut self.config.last_peer_screens),
                    port: self.config.port,
                    device_name: std::mem::take(&mut self.config.device_name),
                    ..Config::default()
                };
                if side_changed && let Some(c) = &self.capture {
                    c.set_side(self.config.peer_side);
                }
                self.paused = false;
                self.sync_capture();
                self.save_and_publish("Settings reset to defaults. Pairing, name and port were kept.");
            }
            Command::RetryPermissions => self.start_backends(),
            Command::SetPaused(paused) => {
                self.paused = paused;
                if paused && self.mode == Mode::Controlling {
                    let (x, y) = self.pointer;
                    self.leave_to_local(x, y);
                }
                self.sync_capture();
                self.log(if paused { "Sharing paused." } else { "Sharing resumed." }.into());
            }
        }
    }

    fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Established(Ok(est)) => self.start_session(est),
            Internal::Established(Err(e)) => {
                let quiet = self.connecting && self.reconnect_delay > RECONNECT_MIN;
                let was_dialing = std::mem::take(&mut self.connecting);
                if !quiet {
                    self.log(format!("Connection failed: {e:#}"));
                }
                if was_dialing {
                    self.schedule_reconnect();
                }
            }
            Internal::Inbound { session, message } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.inbound(message);
                }
            }
            Internal::SessionClosed { session, reason } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.end_session(reason);
                }
            }
            Internal::InjectorStarted(_) | Internal::CaptureStarted(_) => unreachable!("handled in run()"),
        }
    }

    fn peer_configured(&self) -> bool {
        identity::parse(&self.config.peer_fingerprint).is_some() && !self.config.peer_address.trim().is_empty()
    }

    fn save_and_publish(&mut self, message: &str) {
        match self.config.save() {
            Ok(()) => self.log(message.into()),
            Err(e) => self.log(format!("Cannot save settings: {e:#}")),
        }
        let _ = self.events.send(Event::Config(self.config.clone()));
    }

    fn connect(&mut self) {
        if self.session.is_some() || self.connecting {
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            self.log("Networking is unavailable (see the error above).".into());
            return;
        };
        let Some(fp) = identity::parse(&self.config.peer_fingerprint) else {
            self.log("Enter the other device's fingerprint first (Pairing page).".into());
            return;
        };
        if self.config.peer_address.trim().is_empty() {
            self.log("Enter the other device's address first (Pairing page).".into());
            return;
        }
        let client = match tls::client_config(&self.identity, fp) {
            Ok(c) => c,
            Err(e) => return self.log(format!("TLS setup failed: {e:#}")),
        };
        let (tx, hello, address) = (self.internal.clone(), self.hello(), self.config.peer_address.trim().to_owned());
        if self.reconnect_delay == RECONNECT_MIN {
            self.log(format!("Connecting to {address}…"));
        }
        self.connecting = true;
        tokio::spawn(async move {
            let _ = tx.send(Internal::Established(connect(endpoint, client, address, hello).await));
        });
    }

    /// Tries again later, backing off up to 30 s, with jitter so two devices
    /// that both dial don't keep colliding.
    fn schedule_reconnect(&mut self) {
        if self.user_disconnected || !self.config.connection.auto_reconnect || !self.peer_configured() {
            return;
        }
        let jitter = Duration::from_millis(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_millis() as u64).unwrap_or(0) % 1000,
        );
        let delay = self.reconnect_delay + jitter;
        self.reconnect_at = Some(Instant::now() + delay);
        self.reconnect_delay = (self.reconnect_delay * 2).min(RECONNECT_MAX);
        let _ = self.events.send(Event::Reconnecting(delay.as_secs().max(1)));
    }

    fn start_session(&mut self, est: Established) {
        let Established { conn, mut send, mut recv, peer } = est;
        self.connecting = false;
        if self.session.is_some() {
            conn.close(1u32.into(), b"busy");
            self.log(format!("Refused a second connection from {}.", peer.name));
            return;
        }
        let id = self.next_session;
        self.next_session += 1;
        self.reconnect_at = None;
        self.reconnect_delay = RECONNECT_MIN;
        self.user_disconnected = false;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        // Writer: keeps the engine loop from ever awaiting the network.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write_message(&mut send, &msg).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });
        // Reliable stream reader.
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let reason = loop {
                match read_message(&mut recv).await {
                    Ok(message) => {
                        let _ = tx.send(Internal::Inbound { session: id, message });
                    }
                    Err(e) => break format!("{e:#}"),
                }
            };
            let _ = tx.send(Internal::SessionClosed { session: id, reason });
        });
        // Datagram reader (pointer motion).
        let (tx, c) = (self.internal.clone(), conn.clone());
        tokio::spawn(async move {
            while let Ok(bytes) = c.read_datagram().await {
                if let Ok(message @ Message::Input(Input::Motion { .. })) = Message::from_bytes(&bytes) {
                    let _ = tx.send(Internal::Inbound { session: id, message });
                }
            }
        });

        let _ = self.events.send(Event::Connected {
            name: peer.name.clone(),
            os: peer.os,
            address: conn.remote_address(),
            screens: peer.displays.clone(),
        });
        self.remember_peer(&peer.name, &peer.displays);
        if peer.displays.is_empty() {
            self.log(format!("{} reported no screens, so the pointer can't move there yet.", peer.name));
        }
        self.session = Some(Session { id, conn, out: out_tx, peer });
        self.sync_capture();
    }

    fn remember_peer(&mut self, name: &str, screens: &[Rect]) {
        if self.config.last_peer_name.as_deref() != Some(name) || self.config.last_peer_screens != screens {
            self.config.last_peer_name = Some(name.to_owned());
            self.config.last_peer_screens = screens.to_vec();
            if let Err(e) = self.config.save() {
                self.log(format!("Cannot save settings: {e:#}"));
            }
        }
    }

    fn end_session(&mut self, reason: String) {
        let Some(session) = self.session.take() else { return };
        match self.mode {
            Mode::Controlling => {
                if let Some(c) = &self.capture {
                    let ours = self.our_screens();
                    let p = edge::return_point(self.config.link(), self.pointer.0, self.pointer.1, &ours, &session.peer.displays)
                        .unwrap_or((0.0, 0.0));
                    c.release(p.0, p.1);
                }
            }
            Mode::Controlled => {
                if let Some(i) = &self.injector {
                    i.release_all();
                }
            }
            Mode::Local => {}
        }
        self.set_mode(Mode::Local);
        self.sync_capture();
        session.conn.close(0u32.into(), b"bye");
        let _ = self.events.send(Event::Disconnected { reason });
        self.schedule_reconnect();
    }

    // ── Sending side ────────────────────────────────────────────────────

    fn captured(&mut self, event: CaptureEvent) {
        if self.pending_switch.is_some() {
            return self.while_pending(event);
        }
        match event {
            CaptureEvent::Activated { x, y } => self.activated(x, y),
            CaptureEvent::Input(input) if self.mode == Mode::Controlling => self.forward(input),
            CaptureEvent::Input(_) => {}
            CaptureEvent::Deactivated => {
                if self.mode == Mode::Controlling {
                    self.send(Message::Leave);
                    self.set_mode(Mode::Local);
                }
            }
            CaptureEvent::Stopped { reason } => {
                self.capture = None;
                if self.mode == Mode::Controlling {
                    self.send(Message::Leave);
                    self.set_mode(Mode::Local);
                }
                let _ = self.events.send(Event::CaptureReady(Err(reason)));
            }
        }
    }

    fn activated(&mut self, x: f32, y: f32) {
        let side = self.config.peer_side;
        let ours = self.our_screens();
        let in_corner = edge::in_dead_corner(side, x, y, &ours, self.config.switching.dead_corners as f32);
        let entry = if in_corner || self.paused { None } else { edge::entry_point(self.config.link(), x, y, &ours, self.peer_screens()) };
        match (entry, &self.session) {
            (Some(p), Some(_)) if self.config.switching.delay_ms > 0 => {
                // The compositor holds the pointer at the edge meanwhile.
                self.pending_switch = Some(PendingSwitch {
                    deadline: Instant::now() + Duration::from_millis(self.config.switching.delay_ms as u64),
                    at: (x, y),
                    entry: p,
                    retreat: 0.0,
                });
            }
            (Some(p), Some(_)) => self.enter_peer(p),
            _ => {
                // Nowhere to go: hand the pointer straight back.
                let back = edge::inside_edge(side, x, y, &ours).unwrap_or((x, y));
                if let Some(c) = &self.capture {
                    c.release(back.0, back.1);
                }
            }
        }
    }

    fn enter_peer(&mut self, p: (f32, f32)) {
        self.pointer = p;
        self.ctrl_down = false;
        self.alt_down = false;
        self.send(Message::Enter { x: p.0, y: p.1 });
        self.set_mode(Mode::Controlling);
    }

    /// The pointer kept pushing for the whole switch delay: go.
    fn commit_switch(&mut self) {
        if let Some(p) = self.pending_switch.take() {
            if self.session.is_some() {
                self.enter_peer(p.entry);
            } else {
                self.cancel_switch(p);
            }
        }
    }

    fn cancel_switch(&mut self, p: PendingSwitch) {
        let ours = self.our_screens();
        let back = edge::inside_edge(self.config.peer_side, p.at.0, p.at.1, &ours).unwrap_or(p.at);
        if let Some(c) = &self.capture {
            c.release(back.0, back.1);
        }
    }

    /// Input during the switch delay: moving back away from the edge, or
    /// pressing anything, cancels the switch.
    fn while_pending(&mut self, event: CaptureEvent) {
        let Some(mut p) = self.pending_switch.take() else { return };
        match event {
            CaptureEvent::Input(Captured::Motion { dx, dy }) => {
                let away = match self.config.peer_side {
                    Side::Right => -dx,
                    Side::Left => dx,
                    Side::Bottom => -dy,
                    Side::Top => dy,
                };
                p.retreat = (p.retreat + away).max(0.0);
                if p.retreat > 6.0 {
                    self.cancel_switch(p);
                } else {
                    self.pending_switch = Some(p);
                }
            }
            CaptureEvent::Input(_) => self.cancel_switch(p),
            CaptureEvent::Activated { .. } => self.pending_switch = Some(p),
            CaptureEvent::Deactivated => {}
            CaptureEvent::Stopped { reason } => {
                self.capture = None;
                let _ = self.events.send(Event::CaptureReady(Err(reason)));
            }
        }
    }

    fn forward(&mut self, input: Captured) {
        let (px, py) = self.pointer;
        match input {
            Captured::Motion { dx, dy } => {
                let speed = self.config.mouse.speed;
                let (x, y) = (px + dx * speed, py + dy * speed);
                // Only look up our screens once the pointer leaves the peer's.
                let inside = Rect::bounds(self.peer_screens()).is_some_and(|b| b.contains(x, y));
                if !inside && edge::crossed_back(self.config.link(), x, y, &self.our_screens(), self.peer_screens()) {
                    self.leave_to_local(x, y);
                } else {
                    self.pointer = Rect::clamp_to_any(self.peer_screens(), x, y);
                    self.send_motion(self.pointer.0, self.pointer.1);
                }
            }
            Captured::Button { button, pressed } => {
                let button = match (self.config.mouse.swap_buttons, button) {
                    (true, Button::Left) => Button::Right,
                    (true, Button::Right) => Button::Left,
                    (_, b) => b,
                };
                self.send(Message::Input(Input::Button { button, pressed, x: px, y: py }));
            }
            Captured::Scroll { dx, dy } => {
                let k = self.scroll_factor();
                self.send(Message::Input(Input::Scroll { dx: dx * k, dy: dy * k }));
            }
            Captured::ScrollDiscrete { dx, dy } => {
                let k = self.scroll_factor();
                let scale = |v: i32| (v as f32 * k).round() as i32;
                self.send(Message::Input(Input::ScrollDiscrete { dx: scale(dx), dy: scale(dy) }));
            }
            Captured::ScrollStop => self.send(Message::Input(Input::ScrollStop)),
            Captured::Key { key, pressed } => {
                if key.is_ctrl() {
                    self.ctrl_down = pressed;
                }
                if key.is_alt() {
                    self.alt_down = pressed;
                }
                // The return shortcut always brings the pointer home.
                let shortcut = self.config.keyboard.return_shortcut;
                let triggered = match shortcut.single_key() {
                    Some(k) => key == k,
                    None => key == Key::ESCAPE && self.ctrl_down && self.alt_down,
                };
                if triggered {
                    if pressed {
                        self.leave_to_local(px, py);
                    }
                    return;
                }
                let peer_os = self.session.as_ref().map(|s| s.peer.os).unwrap_or(Os::Other);
                let key = self.config.keyboard.remap(key, Os::current(), peer_os);
                self.send(Message::Input(Input::Key { key, pressed }));
            }
        }
    }

    fn scroll_factor(&self) -> f32 {
        let m = &self.config.mouse;
        if m.natural_scroll { -m.scroll_speed } else { m.scroll_speed }
    }

    fn leave_to_local(&mut self, x: f32, y: f32) {
        self.send(Message::Leave);
        let ours = self.our_screens();
        let back = edge::return_point(self.config.link(), x, y, &ours, self.peer_screens()).unwrap_or((0.0, 0.0));
        if let Some(c) = &self.capture {
            c.release(back.0, back.1);
        }
        self.set_mode(Mode::Local);
    }

    // ── Receiving side ──────────────────────────────────────────────────

    fn inbound(&mut self, message: Message) {
        match message {
            Message::Enter { x, y } => {
                if !self.config.connection.allow_control {
                    // Hand control straight back.
                    self.send(Message::Leave);
                    self.log("The other device tried to take control, but being controlled is turned off.".into());
                    return;
                }
                if self.mode == Mode::Controlling {
                    // Both sides crossed at once: yield to the peer.
                    let (px, py) = self.pointer;
                    self.leave_to_local(px, py);
                }
                self.set_mode(Mode::Controlled);
                self.sync_capture();
                if let Some(i) = &self.injector {
                    i.inject(Input::Motion { x, y });
                } else {
                    self.log("The peer is trying to control this device, but remote input wasn't allowed here.".into());
                }
            }
            Message::Input(input) => {
                if self.mode == Mode::Controlled
                    && let Some(i) = &self.injector
                {
                    i.inject(input);
                }
            }
            Message::Leave => {
                if self.mode == Mode::Controlling {
                    // The peer handed control back (it refused it, or its
                    // user pressed its own return shortcut).
                    let ours = self.our_screens();
                    let (x, y) = self.pointer;
                    let back = edge::return_point(self.config.link(), x, y, &ours, self.peer_screens()).unwrap_or((0.0, 0.0));
                    if let Some(c) = &self.capture {
                        c.release(back.0, back.1);
                    }
                    self.set_mode(Mode::Local);
                    return;
                }
                if let Some(i) = &self.injector {
                    i.release_all();
                }
                if self.mode == Mode::Controlled {
                    self.set_mode(Mode::Local);
                    self.sync_capture();
                }
            }
            Message::ReleaseAll => {
                if let Some(i) = &self.injector {
                    i.release_all();
                }
            }
            Message::Displays(displays) => {
                let name = match &mut self.session {
                    Some(s) => {
                        s.peer.displays = displays.clone();
                        s.peer.name.clone()
                    }
                    None => return,
                };
                let _ = self.events.send(Event::PeerScreens(displays.clone()));
                self.remember_peer(&name, &displays);
            }
            Message::Hello(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_identity(tag: &str) -> Identity {
        let dir = std::env::temp_dir().join(format!("dextr-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = Identity::load_or_create(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        id
    }

    fn hello(name: &str) -> Hello {
        Hello { protocol: PROTOCOL_VERSION, name: name.into(), os: Os::current(), displays: vec![Rect::new(0, 0, 1920, 1080)] }
    }

    /// Connects `b` to `a`, where `a` trusts `a_trusts`.
    async fn try_pair(a: &Identity, a_trusts: identity::Fingerprint, b: &Identity) -> anyhow::Result<(Established, Established)> {
        let server = bind(a, Arc::new(RwLock::new(Some(a_trusts))), 0)?;
        let port = server.local_addr()?.port();
        let client_ep = quinn::Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let accept_task = {
            let server = server.clone();
            tokio::spawn(async move { accept(server.accept().await.unwrap(), hello("A")).await })
        };
        let dial = connect(client_ep, tls::client_config(b, a.fingerprint)?, format!("127.0.0.1:{port}"), hello("B")).await;
        let accepted = accept_task.await?;
        Ok((accepted?, dial?))
    }

    #[tokio::test]
    async fn paired_devices_connect_and_exchange_messages() {
        let (a, b) = (temp_identity("a"), temp_identity("b"));
        let (mut at_a, mut at_b) = try_pair(&a, b.fingerprint, &b).await.unwrap();
        assert_eq!(at_a.peer.name, "B");
        assert_eq!(at_b.peer.name, "A");

        write_message(&mut at_b.send, &Message::Enter { x: 1.0, y: 2.0 }).await.unwrap();
        assert_eq!(read_message(&mut at_a.recv).await.unwrap(), Message::Enter { x: 1.0, y: 2.0 });

        let motion = Message::Input(Input::Motion { x: 3.0, y: 4.0 });
        at_a.conn.send_datagram(motion.to_bytes().unwrap().into()).unwrap();
        let got = at_b.conn.read_datagram().await.unwrap();
        assert_eq!(Message::from_bytes(&got).unwrap(), motion);
    }

    #[tokio::test]
    async fn unpaired_device_is_rejected() {
        let (a, b, stranger) = (temp_identity("a2"), temp_identity("b2"), temp_identity("s2"));
        // A only trusts B; a stranger dials in claiming nothing special.
        assert!(try_pair(&a, b.fingerprint, &stranger).await.is_err());
    }
}
