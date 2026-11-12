use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context as _, bail};
use dextr_platform::{
    CaptureEvent, CaptureHandle, Captured, InjectorHandle,
    window::{self as capture_window, ShareableWindow, WindowCapture, WindowGeometry},
};
use dextr_proto::{
    Beacon, Button, DEFAULT_PORT, Hello, Input, Key, MAX_FRAME, MAX_WINDOW_FRAME, Message, Os, PROTOCOL_VERSION, Rect, Side, WindowInfo,
    WindowStream,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

use crate::{
    config::{self, Config},
    discovery::{self, LocalAddress},
    edge::{self, Link},
    identity::{self, Fingerprint, Identity},
    settings::FrameRate,
    tls,
    window::{Encoder, Frame, Surface},
};

/// The paired device's fingerprint, if any.
type Trusted = Arc<RwLock<Option<Fingerprint>>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// How long a pairing request waits for both users.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
/// A nearby device that stops broadcasting is dropped from the list after this.
const NEARBY_TIMEOUT: Duration = Duration::from_secs(7);

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
    /// Start pairing with the device at this address (`host` or `host:port`).
    Pair(String),
    /// The user confirmed (true) or declined/cancelled (false) the pairing.
    PairDecision(bool),
    /// List the windows that can be shared without the OS picker.
    ListWindows,
    /// Share a window from [`Event::ShareableWindows`], or pick one with the
    /// OS's dialog (`None`).
    ShareWindow(Option<u64>),
    /// Stop sharing one of our windows.
    StopSharing(u32),
    /// The user closed a window shared by the other device.
    CloseWindow(u32),
    /// Input on a window shared by the other device, in its pixels.
    WindowInput { id: u32, input: Input },
}

/// One of our windows being shared.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedWindow {
    pub id: u32,
    pub title: String,
    pub app: String,
    /// Input from the other device can reach it (its position is known).
    pub interactive: bool,
}

/// Another dextr device on the local network.
#[derive(Debug, Clone, PartialEq)]
pub struct Nearby {
    pub name: String,
    pub os: Os,
    pub address: SocketAddr,
    pub fingerprint: String,
    /// It's the device this one is paired with.
    pub paired: bool,
}

/// Progress of a pairing.
#[derive(Debug, Clone, PartialEq)]
pub enum PairingState {
    /// Reaching the device at this address.
    Contacting { address: String },
    /// Both devices show `code`; waiting for both users to confirm it.
    Confirm {
        name: String,
        os: Os,
        code: String,
        /// The other device started it.
        incoming: bool,
        we_accepted: bool,
        they_accepted: bool,
    },
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
    /// Pairing started, progressed or ended (`None`).
    Pairing(Option<PairingState>),
    /// Why the last pairing didn't complete.
    PairingFailed(String),
    /// Other dextr devices on the local network.
    Nearby(Vec<Nearby>),
    /// This device's network addresses.
    Addresses(Vec<LocalAddress>),
    /// Windows that can be shared, or `None` when the OS shows its own picker.
    ShareableWindows(Option<Vec<ShareableWindow>>),
    /// Our windows being shared now.
    Sharing(Vec<SharedWindow>),
    /// Why a window couldn't be shared.
    ShareFailed(String),
    /// The other device shares a window: show it.
    /// `pointer` is where our pointer is (for a window dragged across).
    WindowOpened { info: WindowInfo, from: String, surface: Arc<Surface>, pointer: Option<(f32, f32)> },
    /// New pixels for a shared window.
    WindowFrame { id: u32 },
    /// A shared window changed size.
    WindowResized { id: u32, width: u32, height: u32 },
    /// A shared window is gone.
    WindowClosed { id: u32 },
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
    AcceptFailed(String),
    PairDialed { id: u64, result: anyhow::Result<PairLink> },
    PairIncoming(PairLink),
    PairPeerAccepted { id: u64 },
    PairEnded { id: u64, reason: String },
    Paired { id: u64, est: Established },
    /// A window capture started; `grab` if it was dragged across.
    ShareReady { result: anyhow::Result<WindowCapture>, grab: Option<(f32, f32)> },
    ShareOpened { session: u64, info: WindowInfo },
    ShareEnded { id: u32, reason: String },
    WindowFrame { session: u64, id: u32, resized: bool },
    Inbound { session: u64, message: Message },
    SessionClosed { session: u64, reason: String },
}

struct Established {
    /// This device opened the connection.
    dialed: bool,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    peer: Hello,
}

/// A connection opened to pair, before both users have confirmed.
struct PairLink {
    dialed: bool,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    fingerprint: Fingerprint,
    name: String,
    os: Os,
}

enum Accepted {
    Session(Established),
    Pair(PairLink),
}

struct Pairing {
    id: u64,
    state: PairingState,
    /// Where to reach the device later.
    address: String,
    fingerprint: Option<Fingerprint>,
    /// The user's answers: true confirms, false cancels (at any time).
    decide: Option<mpsc::UnboundedSender<bool>>,
}

/// One of our windows being shared.
struct Share {
    title: String,
    app: String,
    geometry: watch::Receiver<Option<WindowGeometry>>,
    /// Brings a parked window back before input is injected.
    input: dextr_platform::window::InputHandle,
    /// The latest frame size, in pixels.
    size: Arc<std::sync::Mutex<(u32, u32)>>,
    task: tokio::task::JoinHandle<()>,
}

/// Windows shared by the other device, by id; written by the stream readers.
type Surfaces = Arc<std::sync::Mutex<HashMap<u32, Arc<Surface>>>>;

struct Session {
    id: u64,
    dialed: bool,
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
    /// A window was dragged across: the user's eventual button release
    /// belongs to the drag that already ended here, so don't send it.
    swallow_release: bool,
    /// Where the other device last put our pointer while controlling us.
    pointer_here: (f32, f32),
    pairing: Option<Pairing>,
    next_pairing: u64,
    nearby: HashMap<Fingerprint, (Nearby, Instant)>,
    beacon: watch::Sender<Beacon>,
    addresses: Vec<LocalAddress>,
    shares: HashMap<u32, Share>,
    next_share: u32,
    surfaces: Surfaces,
    /// Windows shared by the other device that the UI shows.
    incoming: HashMap<u32, WindowInfo>,
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
    let endpoint = match bind(&identity, config.port) {
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

    let port = endpoint.as_ref().and_then(|e| e.local_addr().ok()).map_or(config.port, |a| a.port());
    let (beacon, beacon_rx) = watch::channel(Beacon {
        magic: Beacon::MAGIC,
        protocol: PROTOCOL_VERSION,
        name: config.device_name.clone(),
        os: Os::current(),
        port,
        fingerprint: identity.fingerprint,
    });
    let (found_tx, mut found_rx) = mpsc::unbounded_channel();
    if endpoint.is_some() {
        discovery::spawn(beacon_rx, found_tx);
    }
    let addresses = discovery::local_addresses();
    let _ = events.send(Event::Addresses(addresses.clone()));
    let mut housekeeping = tokio::time::interval(Duration::from_secs(2));

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
        swallow_release: false,
        pointer_here: (0.0, 0.0),
        pairing: None,
        next_pairing: 0,
        nearby: HashMap::new(),
        beacon,
        addresses,
        shares: HashMap::new(),
        next_share: 0,
        surfaces: Arc::default(),
        incoming: HashMap::new(),
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
                    engine.log("Remote input is ready: the other device can control this one.".into());
                    engine.report_screens();
                    engine.resend_screens();
                    if token.is_some() && token != engine.config.inject_restore_token {
                        engine.config.inject_restore_token = token;
                        if let Err(e) = engine.config.save() {
                            engine.log(format!("Cannot save settings: {e:#}"));
                        }
                    }
                }
                Internal::InjectorStarted(Err(e)) => {
                    engine.starting_injector = false;
                    engine.log(format!("Remote input unavailable: {e:#}"));
                    let _ = events.send(Event::InjectReady(Err(format!("{e:#}"))));
                }
                Internal::CaptureStarted(Ok((handle, rx))) => {
                    engine.starting_capture = false;
                    engine.capture = Some(handle);
                    engine.sync_capture();
                    capture_rx = Some(rx);
                    let _ = events.send(Event::CaptureReady(Ok(())));
                    engine.log("Input capture is ready: this mouse and keyboard can move to the other device.".into());
                    engine.report_screens();
                    engine.resend_screens();
                }
                Internal::CaptureStarted(Err(e)) => {
                    engine.starting_capture = false;
                    engine.log(format!("Input capture unavailable: {e:#}"));
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
                    let trusted = *engine.trusted.read().unwrap();
                    tokio::spawn(async move {
                        let _ = tx.send(match accept(incoming, hello, trusted).await {
                            Ok(Accepted::Session(est)) => Internal::Established(Ok(est)),
                            Ok(Accepted::Pair(link)) => Internal::PairIncoming(link),
                            Err(e) => Internal::AcceptFailed(format!("{e:#}")),
                        });
                    });
                }
            }
            Some((beacon, from)) = found_rx.recv() => engine.on_beacon(beacon, from),
            _ = housekeeping.tick() => engine.housekeeping(),
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

fn bind(id: &Identity, port: u16) -> anyhow::Result<quinn::Endpoint> {
    let server = tls::server_config(id)?;
    // Dual-stack first, then IPv4 only.
    quinn::Endpoint::server(server.clone(), SocketAddr::from(([0u16; 8], port)))
        .or_else(|_| quinn::Endpoint::server(server, SocketAddr::from(([0, 0, 0, 0], port))))
        .context("binding the UDP socket")
}

/// Answers an incoming connection: a session if it comes from the paired
/// device, a pairing request, or a refusal.
async fn accept(incoming: quinn::Incoming, hello: Hello, trusted: Option<Fingerprint>) -> anyhow::Result<Accepted> {
    let conn = incoming.await.context("incoming connection failed")?;
    let from = conn.remote_address().ip().to_canonical();
    let fingerprint = tls::peer_fingerprint(&conn).context("the other device sent no certificate")?;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let first = tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv))
        .await
        .context("the other device said nothing")??;
    match first {
        Message::Hello(peer) if trusted == Some(fingerprint) => {
            write_message(&mut send, &Message::Hello(hello)).await?;
            check_protocol(&peer)?;
            Ok(Accepted::Session(Established { dialed: false, conn, send, recv, peer }))
        }
        Message::Hello(peer) => {
            conn.close(2u32.into(), b"not paired");
            bail!("refused {} at {from}: it isn't paired with this device", peer.name)
        }
        Message::PairRequest { name, os } => {
            write_message(&mut send, &Message::PairRequest { name: hello.name, os: hello.os }).await?;
            Ok(Accepted::Pair(PairLink { dialed: false, conn, send, recv, fingerprint, name, os }))
        }
        other => bail!("{from} sent {other:?} instead of a greeting"),
    }
}

async fn resolve(address: &str) -> anyhow::Result<SocketAddr> {
    let address = if address.parse::<SocketAddr>().is_ok() || address.rsplit_once(':').is_some_and(|(h, _)| !h.contains(':')) {
        address.to_owned()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    };
    tokio::net::lookup_host(&address)
        .await
        .with_context(|| format!("cannot resolve {address}"))?
        .next()
        .with_context(|| format!("no address for {address}"))
}

async fn dial(endpoint: &quinn::Endpoint, config: quinn::ClientConfig, address: &str) -> anyhow::Result<quinn::Connection> {
    let target = resolve(address).await?;
    tokio::time::timeout(Duration::from_secs(8), endpoint.connect_with(config, target, "dextr")?)
        .await
        .context("timed out (is dextr running there, and is UDP allowed through its firewall?)")?
        .context("connection failed")
}

async fn connect(
    endpoint: quinn::Endpoint,
    config: quinn::ClientConfig,
    address: String,
    hello: Hello,
) -> anyhow::Result<Established> {
    let conn = dial(&endpoint, config, &address).await?;
    let (send, recv) = conn.open_bi().await?;
    exchange_hello(conn, send, recv, hello, true).await
}

/// Opens a pairing connection: any certificate is accepted here because the
/// users compare a code derived from both before anything is trusted.
async fn dial_pair(endpoint: quinn::Endpoint, config: quinn::ClientConfig, address: String, name: String) -> anyhow::Result<PairLink> {
    let conn = dial(&endpoint, config, &address).await?;
    let fingerprint = tls::peer_fingerprint(&conn).context("the other device sent no certificate")?;
    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &Message::PairRequest { name, os: Os::current() }).await?;
    let answer = tokio::time::timeout(Duration::from_secs(10), read_message(&mut recv))
        .await
        .context("no answer")?
        .context("the other device closed the connection (it may be busy, or not accept incoming connections)")?;
    match answer {
        Message::PairRequest { name, os } => Ok(PairLink { dialed: true, conn, send, recv, fingerprint, name, os }),
        Message::PairReject => bail!("the other device is busy"),
        other => bail!("unexpected answer {other:?}"),
    }
}

/// Waits for both users to confirm, then greets as a normal session.
async fn pair_conversation(link: PairLink, mut decisions: mpsc::UnboundedReceiver<bool>, hello: Hello, tx: mpsc::UnboundedSender<Internal>, id: u64) {
    let PairLink { conn, mut send, mut recv, name, dialed, .. } = link;
    let ended = |reason: String| {
        let _ = tx.send(Internal::PairEnded { id, reason });
    };
    {
        let mut theirs = Box::pin(read_message(&mut recv));
        let (mut mine_ok, mut theirs_ok) = (false, false);
        let deadline = tokio::time::sleep(PAIRING_TIMEOUT);
        tokio::pin!(deadline);
        while !(mine_ok && theirs_ok) {
            tokio::select! {
                d = decisions.recv() => {
                    if d == Some(true) {
                        if mine_ok {
                            continue;
                        }
                        mine_ok = true;
                        if write_message(&mut send, &Message::PairAccept).await.is_err() {
                            return ended(format!("Lost the connection to {name}."));
                        }
                    } else {
                        let _ = write_message(&mut send, &Message::PairReject).await;
                        let _ = send.finish();
                        // Give the reject a moment to arrive before closing.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        conn.close(0u32.into(), b"declined");
                        return ended("Pairing cancelled.".into());
                    }
                }
                m = &mut theirs, if !theirs_ok => match m {
                    Ok(Message::PairAccept) => {
                        theirs_ok = true;
                        let _ = tx.send(Internal::PairPeerAccepted { id });
                    }
                    Ok(Message::PairReject) => return ended(format!("{name} declined.")),
                    Ok(other) => return ended(format!("{name} sent {other:?} while pairing.")),
                    Err(_) => return ended(format!("{name} cancelled or lost the connection.")),
                },
                () = &mut deadline => {
                    conn.close(0u32.into(), b"timeout");
                    return ended("Nobody confirmed the code in time.".into());
                }
            }
        }
    }
    match exchange_hello(conn, send, recv, hello, dialed).await {
        Ok(est) => {
            let _ = tx.send(Internal::Paired { id, est });
        }
        Err(e) => ended(format!("Paired, but connecting failed: {e:#}")),
    }
}

/// Sends one window: waits for its first frame, announces it, then streams
/// changed tiles, always encoding the newest frame so a slow network skips
/// frames instead of falling behind.
#[allow(clippy::too_many_arguments)]
async fn stream_window(
    conn: quinn::Connection,
    session: u64,
    id: u32,
    mut capture: WindowCapture,
    grab: Option<(f32, f32)>,
    size: Arc<std::sync::Mutex<(u32, u32)>>,
    min_interval: Duration,
    tx: mpsc::UnboundedSender<Internal>,
) {
    let reason = async {
        let first = loop {
            if let Some(f) = capture.frames.borrow_and_update().clone() {
                break f;
            }
            tokio::select! {
                r = capture.frames.changed() => if r.is_err() { anyhow::bail!("the window closed") },
                r = &mut capture.ended => anyhow::bail!(r.unwrap_or_else(|_| "capture ended".into())),
            }
        };
        let scale = capture.geometry.borrow().map_or(1.0, |g| first.width as f32 / g.width.max(1.0));
        let grab = grab.map(|(x, y)| (x * scale, y * scale));
        let info = WindowInfo { id, title: capture.title.clone(), app: capture.app.clone(), width: first.width, height: first.height, scale, grab };
        let _ = tx.send(Internal::ShareOpened { session, info });

        let mut out = conn.open_uni().await?;
        out.write_all(&WindowStream::Start { id }.to_frame()?).await?;
        let mut encoder = Encoder::default();
        let mut frame = first;
        loop {
            let sent_at = Instant::now();
            *size.lock().unwrap() = (frame.width, frame.height);
            let update = encoder.encode(&Frame { width: frame.width, height: frame.height, stride: frame.stride, data: &frame.data });
            if let Some(update) = update {
                out.write_all(&WindowStream::Frame(update).to_frame()?).await?;
            }
            tokio::time::sleep_until(sent_at + min_interval).await;
            tokio::select! {
                r = capture.frames.changed() => if r.is_err() { break },
                r = &mut capture.ended => anyhow::bail!(r.unwrap_or_else(|_| "capture ended".into())),
            }
            let Some(next) = capture.frames.borrow_and_update().clone() else { continue };
            frame = next;
        }
        anyhow::Ok(String::from("the window closed"))
    }
    .await
    .unwrap_or_else(|e| format!("{e:#}"));
    let _ = tx.send(Internal::ShareEnded { id, reason });
}

/// A moving test pattern instead of a real window (`DEXTR_TEST_WINDOW`), to
/// test sharing without the OS's window picker.
fn test_pattern() -> WindowCapture {
    use dextr_platform::window::CapturedFrame;
    let (w, h) = (640u32, 400u32);
    let (frames_tx, frames) = watch::channel(None);
    let (_geometry_tx, geometry) = watch::channel(None);
    let (ended_tx, ended) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let _keep = (ended_tx, _geometry_tx);
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        for n in 0u32.. {
            tick.tick().await;
            let mut data = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    // Stripes for sharpness, a sweeping bar for motion.
                    let stripe = if (x / 2) % 2 == 0 { 230 } else { 30 };
                    let bar = (x + n * 4) % w < 40;
                    data[i..i + 4].copy_from_slice(&if bar { [40, 160, 240, 255] } else { [stripe, stripe, stripe, 255] });
                }
            }
            if frames_tx.send(Some(Arc::new(CapturedFrame { width: w, height: h, stride: (w * 4) as usize, data }))).is_err() {
                break;
            }
        }
    });
    WindowCapture::from_parts("Test pattern".into(), "dextr".into(), frames, geometry, ended)
}

/// Receives the other device's shared windows: one stream per window.
async fn receive_windows(conn: quinn::Connection, session: u64, surfaces: Surfaces, tx: mpsc::UnboundedSender<Internal>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let (surfaces, tx) = (surfaces.clone(), tx.clone());
        tokio::spawn(async move {
            let result = async {
                let WindowStream::Start { id } = read_window_message(&mut stream).await? else {
                    anyhow::bail!("window stream didn't start with its id");
                };
                loop {
                    let WindowStream::Frame(update) = read_window_message(&mut stream).await? else { continue };
                    let surface = surfaces.lock().unwrap().entry(id).or_default().clone();
                    let resized = surface.apply(&update)?;
                    let _ = tx.send(Internal::WindowFrame { session, id, resized });
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            };
            if let Err(e) = result.await {
                tracing::debug!("window stream ended: {e:#}");
            }
        });
    }
}

async fn read_window_message(recv: &mut quinn::RecvStream) -> anyhow::Result<WindowStream> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_WINDOW_FRAME {
        bail!("peer sent an oversized window frame ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(WindowStream::from_bytes(&buf)?)
}

fn check_protocol(peer: &Hello) -> anyhow::Result<()> {
    if peer.protocol != PROTOCOL_VERSION {
        bail!("peer speaks protocol {} but this build speaks {PROTOCOL_VERSION}; update both", peer.protocol);
    }
    Ok(())
}

async fn exchange_hello(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    hello: Hello,
    dialed: bool,
) -> anyhow::Result<Established> {
    write_message(&mut send, &Message::Hello(hello)).await?;
    let peer = match tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv)).await?? {
        Message::Hello(h) => h,
        other => bail!("expected Hello, got {other:?}"),
    };
    check_protocol(&peer)?;
    Ok(Established { dialed, conn, send, recv, peer })
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
        // For automated tests of the network and UI: no permission prompts.
        if std::env::var_os("DEXTR_TEST_NO_INPUT").is_some() {
            return;
        }
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

    /// Tells the peer our screens again, e.g. once a backend that knows
    /// them has started after we connected.
    fn resend_screens(&self) {
        let displays = self.hello().displays;
        if !displays.is_empty() {
            self.send(Message::Displays(displays));
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
            Command::Apply(mut new) => {
                if new.peer_side != self.config.peer_side
                    && let Some(c) = &self.capture
                {
                    c.set_side(new.peer_side);
                }
                let rearranged = new.link() != self.config.link();
                new.arranged_at = if rearranged { now_ms() } else { self.config.arranged_at };
                *self.trusted.write().unwrap() = identity::parse(&new.peer_fingerprint);
                if new.device_name != self.config.device_name {
                    let name = new.device_name.clone();
                    self.beacon.send_modify(|b| b.name = name);
                }
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
                if rearranged {
                    self.send_arrangement();
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
                self.refresh_nearby();
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
                self.config.arranged_at = now_ms();
                self.save_and_publish("Settings reset to defaults. Pairing, name and port were kept.");
                self.send_arrangement();
            }
            Command::RetryPermissions => self.start_backends(),
            Command::Pair(address) => self.start_pairing(address),
            Command::ListWindows => {
                let events = self.events.clone();
                tokio::spawn(async move {
                    let _ = events.send(match capture_window::shareable_windows().await {
                        Ok(list) => Event::ShareableWindows(list),
                        Err(e) => Event::ShareFailed(format!("{e:#}")),
                    });
                });
            }
            Command::ShareWindow(window) => {
                if self.session.is_none() {
                    let _ = self.events.send(Event::ShareFailed("Connect to the other device first.".into()));
                    return;
                }
                if !self.config.windows.enabled {
                    let _ = self.events.send(Event::ShareFailed("Window sharing is turned off (Windows page).".into()));
                    return;
                }
                let tx = self.internal.clone();
                if let Some(test) = std::env::var_os("DEXTR_TEST_WINDOW") {
                    // "drag" pretends the window was dragged across, held at (100, 20).
                    let grab = (test == "drag").then_some((100.0, 20.0));
                    let _ = tx.send(Internal::ShareReady { result: Ok(test_pattern()), grab });
                    return;
                }
                tokio::spawn(async move {
                    let result = capture_window::capture_window(window, false).await;
                    let _ = tx.send(Internal::ShareReady { result, grab: None });
                });
            }
            Command::StopSharing(id) => self.stop_sharing(id, true),
            Command::CloseWindow(id) => {
                if self.incoming.remove(&id).is_some() {
                    self.surfaces.lock().unwrap().remove(&id);
                    self.send(Message::WindowDismiss { id });
                }
            }
            Command::WindowInput { id, input } => {
                if self.incoming.contains_key(&id) {
                    self.send(Message::WindowInput { id, input });
                }
            }
            Command::PairDecision(ok) => self.decide_pairing(ok),
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
            Internal::AcceptFailed(reason) => self.log(reason),
            Internal::PairDialed { id, result } => {
                if self.pairing.as_ref().is_none_or(|p| p.id != id) {
                    return; // cancelled meanwhile; dropping the link closes it
                }
                match result {
                    Ok(link) if link.fingerprint == self.identity.fingerprint => self.pairing_failed("That's this device.".into()),
                    Ok(link) => self.begin_confirm(link, false),
                    Err(e) => self.pairing_failed(format!("Couldn't reach the device: {e:#}")),
                }
            }
            Internal::PairIncoming(mut link) => {
                if self.session.is_some() || self.pairing.is_some() {
                    self.log(format!("Declined a pairing request from {} (busy).", link.name));
                    tokio::spawn(async move {
                        let _ = write_message(&mut link.send, &Message::PairReject).await;
                        let _ = link.send.finish();
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    });
                    return;
                }
                self.next_pairing += 1;
                let address = SocketAddr::new(link.conn.remote_address().ip().to_canonical(), link.conn.remote_address().port());
                self.pairing = Some(Pairing {
                    id: self.next_pairing,
                    state: PairingState::Contacting { address: address.to_string() },
                    address: address.to_string(),
                    fingerprint: None,
                    decide: None,
                });
                self.log(format!("{} wants to pair.", link.name));
                self.begin_confirm(link, true);
            }
            Internal::PairPeerAccepted { id } => {
                if let Some(p) = self.pairing.as_mut().filter(|p| p.id == id)
                    && let PairingState::Confirm { they_accepted, .. } = &mut p.state
                {
                    *they_accepted = true;
                    self.publish_pairing();
                }
            }
            Internal::PairEnded { id, reason } => {
                if self.pairing.as_ref().is_some_and(|p| p.id == id) {
                    self.pairing_failed(reason);
                }
            }
            Internal::Paired { id, est } => {
                let Some(p) = self.pairing.take().filter(|p| p.id == id) else { return };
                let _ = self.events.send(Event::Pairing(None));
                let Some(fp) = p.fingerprint else { return };
                *self.trusted.write().unwrap() = Some(fp);
                self.config.peer_fingerprint = identity::format(&fp);
                self.config.peer_address = p.address;
                self.config.last_peer_name = Some(est.peer.name.clone());
                self.user_disconnected = false;
                self.save_and_publish(&format!("Paired with {}.", est.peer.name));
                self.refresh_nearby();
                self.start_session(est);
            }
            Internal::ShareReady { result: Ok(capture), grab } => self.start_share(capture, grab),
            Internal::ShareReady { result: Err(e), .. } => {
                let reason = format!("{e:#}");
                self.log(format!("Window sharing: {reason}"));
                let _ = self.events.send(Event::ShareFailed(reason));
            }
            Internal::ShareOpened { session, info } => {
                if self.session.as_ref().is_some_and(|s| s.id == session) {
                    self.log(format!("Sharing “{}”.", info.title));
                    self.send(Message::WindowOpen(info));
                }
            }
            Internal::ShareEnded { id, reason } => {
                if self.shares.contains_key(&id) {
                    self.log(format!("Stopped sharing a window: {reason}."));
                    self.stop_sharing(id, true);
                }
            }
            Internal::WindowFrame { session, id, resized } => {
                if self.session.as_ref().is_none_or(|s| s.id != session) || !self.incoming.contains_key(&id) {
                    return;
                }
                let surface = self.surfaces.lock().unwrap().get(&id).cloned();
                let event = match surface {
                    Some(s) if resized => {
                        let (width, height) = s.size();
                        Event::WindowResized { id, width, height }
                    }
                    Some(_) => Event::WindowFrame { id },
                    None => return,
                };
                let _ = self.events.send(event);
            }
            Internal::InjectorStarted(_) | Internal::CaptureStarted(_) => unreachable!("handled in run()"),
        }
    }

    fn start_pairing(&mut self, address: String) {
        let address = address.trim().to_owned();
        if address.is_empty() {
            return;
        }
        if self.session.is_some() {
            return self.pairing_failed("Disconnect from the current device first.".into());
        }
        if self.pairing.is_some() {
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return self.pairing_failed("Networking is unavailable (see Activity).".into());
        };
        let client = match tls::client_config(&self.identity, None) {
            Ok(c) => c,
            Err(e) => return self.pairing_failed(format!("TLS setup failed: {e:#}")),
        };
        self.next_pairing += 1;
        let id = self.next_pairing;
        self.pairing = Some(Pairing {
            id,
            state: PairingState::Contacting { address: address.clone() },
            address: address.clone(),
            fingerprint: None,
            decide: None,
        });
        self.publish_pairing();
        let (tx, name) = (self.internal.clone(), self.config.device_name.clone());
        tokio::spawn(async move {
            let result = dial_pair(endpoint, client, address, name).await;
            let _ = tx.send(Internal::PairDialed { id, result });
        });
    }

    /// Both devices are talking: show the code and wait for the users.
    fn begin_confirm(&mut self, link: PairLink, incoming: bool) {
        let Some(p) = self.pairing.as_mut() else { return };
        let (decide, decisions) = mpsc::unbounded_channel();
        p.fingerprint = Some(link.fingerprint);
        p.decide = Some(decide);
        p.state = PairingState::Confirm {
            name: link.name.clone(),
            os: link.os,
            code: tls::pairing_code(&self.identity.fingerprint, &link.fingerprint),
            incoming,
            we_accepted: false,
            they_accepted: false,
        };
        let (id, hello, tx) = (p.id, self.hello(), self.internal.clone());
        self.publish_pairing();
        tokio::spawn(pair_conversation(link, decisions, hello, tx, id));
    }

    fn decide_pairing(&mut self, ok: bool) {
        let Some(p) = self.pairing.as_mut() else { return };
        match (&p.decide, &mut p.state) {
            (Some(decide), PairingState::Confirm { we_accepted, .. }) => {
                let _ = decide.send(ok);
                if ok && !*we_accepted {
                    *we_accepted = true;
                    self.publish_pairing();
                }
                // A cancel ends through PairEnded once it's sent.
            }
            _ if !ok => {
                // Still contacting: forget it; the dial's result is ignored.
                self.pairing = None;
                let _ = self.events.send(Event::Pairing(None));
            }
            _ => {}
        }
    }

    fn pairing_failed(&mut self, reason: String) {
        self.pairing = None;
        let _ = self.events.send(Event::Pairing(None));
        self.log(format!("Pairing: {reason}"));
        let _ = self.events.send(Event::PairingFailed(reason));
    }

    fn publish_pairing(&self) {
        let _ = self.events.send(Event::Pairing(self.pairing.as_ref().map(|p| p.state.clone())));
    }

    fn on_beacon(&mut self, beacon: Beacon, from: SocketAddr) {
        if beacon.fingerprint == self.identity.fingerprint || beacon.protocol != PROTOCOL_VERSION {
            return;
        }
        let address = SocketAddr::new(from.ip().to_canonical(), beacon.port);
        let paired = *self.trusted.read().unwrap() == Some(beacon.fingerprint);
        let entry = Nearby {
            name: beacon.name,
            os: beacon.os,
            address,
            fingerprint: identity::format(&beacon.fingerprint),
            paired,
        };
        let changed = self.nearby.get(&beacon.fingerprint).is_none_or(|(n, _)| *n != entry);
        let name = entry.name.clone();
        self.nearby.insert(beacon.fingerprint, (entry, Instant::now()));
        if changed {
            self.publish_nearby();
        }

        if paired && self.session.is_none() {
            // Follow the paired device to its new address.
            if self.config.peer_address.trim() != address.to_string() {
                self.config.peer_address = address.to_string();
                self.save_and_publish(&format!("{name} is now at {address}."));
            }
            // It's online: retry now instead of waiting out the backoff. Only
            // one side does, so the two don't keep dialing each other at once.
            if self.reconnect_at.is_some() && !self.connecting && self.identity.fingerprint < beacon.fingerprint {
                self.reconnect_at = None;
                self.connect();
            }
        }
    }

    /// Drops devices that went quiet, and notices address changes.
    fn housekeeping(&mut self) {
        let before = self.nearby.len();
        self.nearby.retain(|_, (_, seen)| seen.elapsed() < NEARBY_TIMEOUT);
        if self.nearby.len() != before {
            self.publish_nearby();
        }
        let addresses = discovery::local_addresses();
        if addresses != self.addresses {
            self.addresses = addresses.clone();
            let _ = self.events.send(Event::Addresses(addresses));
        }
    }

    /// Re-marks which nearby device is the paired one.
    fn refresh_nearby(&mut self) {
        let trusted = *self.trusted.read().unwrap();
        for (fp, (n, _)) in self.nearby.iter_mut() {
            n.paired = trusted == Some(*fp);
        }
        self.publish_nearby();
    }

    fn publish_nearby(&self) {
        let mut list: Vec<Nearby> = self.nearby.values().map(|(n, _)| n.clone()).collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.address.cmp(&b.address)));
        let _ = self.events.send(Event::Nearby(list));
    }

    // ── Display arrangement ─────────────────────────────────────────────

    fn send_arrangement(&self) {
        let link = self.config.link();
        self.send(Message::Arrangement { side: link.side, offset: link.offset, changed_at: self.config.arranged_at });
    }

    /// The other device's arrangement: mirror it if it's newer than ours.
    fn peer_arranged(&mut self, theirs: Link, changed_at: u64) {
        let ours = self.config.arranged_at;
        // Same instant (or neither ever arranged): the lower fingerprint wins.
        let peer_lower = self.trusted.read().unwrap().is_some_and(|peer| peer < self.identity.fingerprint);
        if changed_at < ours || (changed_at == ours && (changed_at == 0 || !peer_lower)) {
            return;
        }
        let link = theirs.mirrored();
        self.config.arranged_at = changed_at;
        if link == self.config.link() {
            let _ = self.config.save();
            return;
        }
        self.config.peer_side = link.side;
        self.config.peer_offset = link.offset;
        if let Some(c) = &self.capture {
            c.set_side(link.side);
        }
        let name = self.session.as_ref().map(|s| s.peer.name.clone()).unwrap_or_default();
        self.save_and_publish(&format!("Display arrangement updated to match {name}."));
    }

    // ── Window sharing ──────────────────────────────────────────────────

    fn start_share(&mut self, capture: WindowCapture, grab: Option<(f32, f32)>) {
        let Some(session) = &self.session else { return };
        self.next_share += 1;
        let id = self.next_share;
        let size = Arc::new(std::sync::Mutex::new((0, 0)));
        let geometry = capture.geometry.clone();
        let input = capture.input_handle();
        let (title, app) = (capture.title.clone(), capture.app.clone());
        let min_interval = match self.config.windows.frame_rate {
            FrameRate::Match => Duration::ZERO,
            FrameRate::Fps30 => Duration::from_micros(33_333),
            FrameRate::Fps60 => Duration::from_micros(16_667),
            FrameRate::Fps120 => Duration::from_micros(8_333),
        };
        let task = tokio::spawn(stream_window(
            session.conn.clone(),
            session.id,
            id,
            capture,
            grab,
            size.clone(),
            min_interval,
            self.internal.clone(),
        ));
        if geometry.borrow().is_none() {
            self.log(format!("“{title}” is shared view-only: dextr can't tell where it is on screen (this needs KDE Plasma)."));
        }
        self.shares.insert(id, Share { title, app, geometry, input, size, task });
        self.publish_shares();
    }

    fn stop_sharing(&mut self, id: u32, tell_peer: bool) {
        if let Some(share) = self.shares.remove(&id) {
            share.task.abort();
            if tell_peer {
                self.send(Message::WindowClose { id });
            }
            self.publish_shares();
        }
    }

    fn publish_shares(&self) {
        let mut list: Vec<SharedWindow> = self
            .shares
            .iter()
            .map(|(&id, s)| SharedWindow { id, title: s.title.clone(), app: s.app.clone(), interactive: s.geometry.borrow().is_some() })
            .collect();
        list.sort_by_key(|w| w.id);
        let _ = self.events.send(Event::Sharing(list));
    }

    /// The other device shares a window.
    fn window_opened(&mut self, info: WindowInfo) {
        let w = &self.config.windows;
        if !w.enabled || !w.accept_incoming {
            self.send(Message::WindowDismiss { id: info.id });
            self.log(format!("Declined a shared window (“{}”): receiving windows is turned off.", info.title));
            return;
        }
        let surface = self.surfaces.lock().unwrap().entry(info.id).or_default().clone();
        let from = self.session.as_ref().map(|s| s.peer.name.clone()).unwrap_or_default();
        self.incoming.insert(info.id, info.clone());
        // Dragged across while the sender controls us: it goes under the pointer.
        let pointer = (info.grab.is_some() && self.mode == Mode::Controlled).then_some(self.pointer_here);
        let pointer = pointer.or_else(|| {
            // Test mode: no input, so use a fixed spot.
            (info.grab.is_some() && std::env::var_os("DEXTR_TEST_NO_INPUT").is_some()).then_some((500.0, 300.0))
        });
        let _ = self.events.send(Event::WindowOpened { info, from, surface, pointer });
    }

    /// The other device stopped sharing a window.
    fn window_closed(&mut self, id: u32) {
        self.surfaces.lock().unwrap().remove(&id);
        if self.incoming.remove(&id).is_some() {
            let _ = self.events.send(Event::WindowClosed { id });
        }
    }

    /// Input from the other device on one of our shared windows: place it on
    /// the real window.
    fn window_input(&mut self, id: u32, input: Input) {
        let Some(share) = self.shares.get(&id) else { return };
        share.input.prepare();
        let Some(g) = *share.geometry.borrow() else { return };
        let (fw, fh) = *share.size.lock().unwrap();
        if fw == 0 || fh == 0 {
            return;
        }
        let at = |x: f32, y: f32| (g.x + x * g.width / fw as f32, g.y + y * g.height / fh as f32);
        let input = match input {
            Input::Motion { x, y } => {
                let (x, y) = at(x, y);
                Input::Motion { x, y }
            }
            Input::Button { button, pressed, x, y } => {
                let (x, y) = at(x, y);
                Input::Button { button, pressed, x, y }
            }
            other => other,
        };
        if let Some(i) = &self.injector {
            i.inject(input);
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
            self.log("Pair with the other device first (Pairing page).".into());
            return;
        };
        if self.config.peer_address.trim().is_empty() {
            self.log("Pair with the other device first (Pairing page).".into());
            return;
        }
        let client = match tls::client_config(&self.identity, Some(fp)) {
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
        let Established { dialed, conn, mut send, mut recv, peer } = est;
        self.connecting = false;
        if let Some(existing) = &self.session {
            // Both devices dialed at once. Each keeps the connection dialed by
            // the device with the lower fingerprint, so both keep the same one.
            let we_are_lower = self.trusted.read().unwrap().is_none_or(|peer| self.identity.fingerprint < peer);
            let preferred = |by_us: bool| by_us == we_are_lower;
            if preferred(existing.dialed) || !preferred(dialed) {
                conn.close(1u32.into(), b"busy");
                tracing::debug!("refused a second connection from {}", peer.name);
                return;
            }
            tracing::debug!("replacing the connection to {} with the one both sides keep", peer.name);
            self.end_session("replaced by a simultaneous connection".into());
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
        // Windows the other device shares.
        tokio::spawn(receive_windows(conn.clone(), id, self.surfaces.clone(), self.internal.clone()));
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
        self.session = Some(Session { id, dialed, conn, out: out_tx, peer });
        // Whichever device was arranged last wins.
        self.send_arrangement();
        if std::env::var_os("DEXTR_TEST_WINDOW").is_some_and(|v| v == "share" || v == "drag") {
            self.command(Command::ShareWindow(None));
        }
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
        self.swallow_release = false;
        // Shared windows end with the connection, in both directions.
        for id in self.shares.keys().copied().collect::<Vec<_>>() {
            self.stop_sharing(id, false);
        }
        for id in std::mem::take(&mut self.incoming).into_keys() {
            let _ = self.events.send(Event::WindowClosed { id });
        }
        self.surfaces.lock().unwrap().clear();
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
            CaptureEvent::DragOut { x, y, window, grab } => self.drag_out(x, y, window, grab),
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

    /// A window was dragged across the edge: the pointer goes to the peer and
    /// the window comes along, parked out of sight here.
    fn drag_out(&mut self, x: f32, y: f32, window: u64, grab: (f32, f32)) {
        let ours = self.our_screens();
        let entry = if self.paused { None } else { edge::entry_point(self.config.link(), x, y, &ours, self.peer_screens()) };
        let Some(p) = entry.filter(|_| self.session.is_some()) else {
            // Nowhere to go: give the pointer back.
            let back = edge::inside_edge(self.config.peer_side, x, y, &ours).unwrap_or((x, y));
            if let Some(c) = &self.capture {
                c.release(back.0, back.1);
            }
            return;
        };
        self.enter_peer(p);
        self.swallow_release = true;
        if !self.config.windows.enabled {
            return;
        }
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let result = capture_window::capture_window(Some(window), true).await;
            let _ = tx.send(Internal::ShareReady { result, grab: Some(grab) });
        });
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
            CaptureEvent::Activated { .. } | CaptureEvent::DragOut { .. } => self.pending_switch = Some(p),
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
            Captured::Button { button: Button::Left, pressed: false } if std::mem::take(&mut self.swallow_release) => {}
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
                self.pointer_here = (x, y);
                if let Some(i) = &self.injector {
                    i.inject(Input::Motion { x, y });
                } else {
                    self.log("The peer is trying to control this device, but remote input wasn't allowed here.".into());
                }
            }
            Message::Input(input) => {
                if let Input::Motion { x, y } | Input::Button { x, y, .. } = input {
                    self.pointer_here = (x, y);
                }
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
            // Only valid before a session starts.
            Message::Hello(_) | Message::PairRequest { .. } | Message::PairAccept | Message::PairReject => {}
            Message::Arrangement { side, offset, changed_at } => self.peer_arranged(Link { side, offset }, changed_at),
            Message::WindowOpen(info) => self.window_opened(info),
            Message::WindowClose { id } => self.window_closed(id),
            Message::WindowDismiss { id } => self.stop_sharing(id, false),
            Message::WindowInput { id, input } => self.window_input(id, input),
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
        let server = bind(a, 0)?;
        let port = server.local_addr()?.port();
        let client_ep = quinn::Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let accept_task = {
            let server = server.clone();
            tokio::spawn(async move { accept(server.accept().await.unwrap(), hello("A"), Some(a_trusts)).await })
        };
        let dial = connect(client_ep, tls::client_config(b, Some(a.fingerprint))?, format!("127.0.0.1:{port}"), hello("B")).await;
        let accepted = match accept_task.await?? {
            Accepted::Session(est) => est,
            Accepted::Pair(_) => anyhow::bail!("expected a session"),
        };
        Ok((accepted, dial?))
    }

    /// Runs a pairing between fresh devices A (listening) and B (dialing),
    /// with each user's answer.
    async fn run_pairing(a_ok: bool, b_ok: bool) -> (Vec<Internal>, Vec<Internal>, String, String) {
        let (a, b) = (temp_identity(&format!("pa{a_ok}{b_ok}")), temp_identity(&format!("pb{a_ok}{b_ok}")));
        let server = bind(&a, 0).unwrap();
        let port = server.local_addr().unwrap().port();
        let client_ep = quinn::Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let accept_task = tokio::spawn(async move { accept(server.accept().await.unwrap(), hello("A"), None).await });
        let at_b = dial_pair(client_ep, tls::client_config(&b, None).unwrap(), format!("127.0.0.1:{port}"), "B".into()).await.unwrap();
        let Accepted::Pair(at_a) = accept_task.await.unwrap().unwrap() else { panic!("expected a pairing request") };
        assert_eq!((at_a.name.as_str(), at_b.name.as_str()), ("B", "A"));
        let code_a = tls::pairing_code(&a.fingerprint, &at_a.fingerprint);
        let code_b = tls::pairing_code(&b.fingerprint, &at_b.fingerprint);

        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let (da, decision_a) = mpsc::unbounded_channel();
        let (db, decision_b) = mpsc::unbounded_channel();
        let task_a = tokio::spawn(pair_conversation(at_a, decision_a, hello("A"), tx_a, 1));
        let task_b = tokio::spawn(pair_conversation(at_b, decision_b, hello("B"), tx_b, 1));
        da.send(a_ok).unwrap();
        db.send(b_ok).unwrap();
        task_a.await.unwrap();
        task_b.await.unwrap();
        let drain = |rx: &mut mpsc::UnboundedReceiver<Internal>| std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        (drain(&mut rx_a), drain(&mut rx_b), code_a, code_b)
    }

    #[tokio::test]
    async fn pairing_by_code_connects_both() {
        let (at_a, at_b, code_a, code_b) = run_pairing(true, true).await;
        assert_eq!(code_a, code_b);
        assert!(matches!(at_a.last(), Some(Internal::Paired { est, .. }) if est.peer.name == "B"));
        assert!(matches!(at_b.last(), Some(Internal::Paired { est, .. }) if est.peer.name == "A"));
    }

    #[tokio::test]
    async fn shared_window_arrives_pixel_exact() {
        use dextr_platform::window::CapturedFrame;
        let (a, b) = (temp_identity("wa"), temp_identity("wb"));
        let (at_a, at_b) = try_pair(&a, b.fingerprint, &b).await.unwrap();

        // B shares a 64×40 window that sits at (100, 50) on its screen at 2×.
        let (frames_tx, frames) = watch::channel(None);
        let (_geometry_tx, geometry) = watch::channel(Some(WindowGeometry { x: 100.0, y: 50.0, width: 32.0, height: 20.0 }));
        let (_ended_tx, ended) = tokio::sync::oneshot::channel();
        let capture = WindowCapture::from_parts("Notes".into(), "notes".into(), frames, geometry, ended);
        let pixels: Vec<u8> = (0..64 * 40 * 4).map(|i| (i % 251) as u8).collect();
        frames_tx.send_replace(Some(Arc::new(CapturedFrame { width: 64, height: 40, stride: 256, data: pixels.clone() })));

        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let size = Arc::new(std::sync::Mutex::new((0, 0)));
        tokio::spawn(stream_window(at_b.conn.clone(), 1, 7, capture, Some((10.0, 5.0)), size.clone(), Duration::ZERO, tx_b));
        let surfaces: Surfaces = Arc::default();
        tokio::spawn(receive_windows(at_a.conn.clone(), 1, surfaces.clone(), tx_a));

        let Some(Internal::ShareOpened { info, .. }) = rx_b.recv().await else { panic!("not announced") };
        assert_eq!((info.id, info.width, info.height, info.scale, info.title.as_str()), (7, 64, 40, 2.0, "Notes"));
        // Held 10×5 points from its corner on a 2× display: 20×10 pixels.
        assert_eq!(info.grab, Some((20.0, 10.0)));
        let Some(Internal::WindowFrame { id: 7, resized: true, .. }) = rx_a.recv().await else { panic!("no frame") };
        let surface = surfaces.lock().unwrap().get(&7).cloned().unwrap();
        assert_eq!(surface.lock().pixels, pixels);
        assert_eq!(*size.lock().unwrap(), (64, 40));
    }

    #[tokio::test]
    async fn declined_pairing_ends_on_both_sides() {
        let (at_a, at_b, _, _) = run_pairing(true, false).await;
        assert!(matches!(at_a.last(), Some(Internal::PairEnded { .. })));
        assert!(matches!(at_b.last(), Some(Internal::PairEnded { .. })));
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
