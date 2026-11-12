use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, bail};
use dextr_platform::{
    CaptureEvent, CaptureHandle, Captured, InjectorHandle,
    clipboard::{self, ClipContent, ClipboardHandle},
};
use dextr_proto::{
    Beacon, Button, Clipboard, DEFAULT_PORT, Hello, Input, Key, MAX_CLIPBOARD, MAX_FRAME, Message, Os, PROTOCOL_VERSION, Rect, Side,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

use crate::{
    config::{self, Config, PeerEntry},
    discovery::{self, LocalAddress},
    edge,
    identity::{self, Fingerprint, Identity},
    settings::{Channel, DisplayLayout},
    tls,
    world::{self, Arrangement, DeviceId, World},
};

/// Test only: pretend screens, as `DEXTR_TEST_SCREENS=1920x1080,1280x1024`
/// (side by side), when no input backend reports real ones.
fn test_screens() -> Vec<Rect> {
    let Ok(spec) = std::env::var("DEXTR_TEST_SCREENS") else { return Vec::new() };
    let mut x = 0;
    spec.split(',')
        .filter_map(|s| {
            let (w, h) = s.trim().split_once('x')?;
            let r = Rect::new(x, 0, w.parse().ok()?, h.parse().ok()?);
            x += r.w as i32;
            Some(r)
        })
        .collect()
}

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
    Apply(Box<Config>),
    /// Connect to one paired device, or all of them (`None`).
    Connect(Option<DeviceId>),
    /// Disconnect from one device, or all of them (`None`).
    Disconnect(Option<DeviceId>),
    /// Ask the OS again for any permission that was refused.
    RetryPermissions,
    /// Pause or resume sending this mouse and keyboard to other devices.
    SetPaused(bool),
    /// Disconnect and remove a paired device.
    ForgetPeer(DeviceId),
    /// Restore every setting to its default (keeps this device's identity
    /// and pairings).
    ResetSettings,
    /// Start pairing with the device at this address (`host` or `host:port`).
    Pair(String),
    /// The user confirmed (true) or declined/cancelled (false) the pairing.
    PairDecision(bool),
}

/// Another dextr device on the local network.
#[derive(Debug, Clone, PartialEq)]
pub struct Nearby {
    pub name: String,
    pub os: Os,
    pub address: SocketAddr,
    pub fingerprint: String,
    /// This device is paired with it.
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

/// Who owns this device's pointer right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Pointer is on this device, used locally.
    Local,
    /// Our mouse and keyboard drive that device.
    Controlling(DeviceId),
    /// That device's mouse and keyboard drive this one.
    Controlled(DeviceId),
}

/// Notifications for the UI.
#[derive(Debug, Clone)]
pub enum Event {
    Ready { config: Config, fingerprint: String, listening: Option<SocketAddr> },
    /// Local input capture (sending to other devices) is available, or why not.
    CaptureReady(Result<(), String>),
    /// Local input injection (being controlled) is available, or why not.
    InjectReady(Result<(), String>),
    Connected { id: DeviceId, name: String, os: Os, address: SocketAddr, screens: Vec<Rect> },
    Disconnected { id: DeviceId, reason: String },
    /// This device's screens, in logical pixels.
    LocalScreens(Vec<Rect>),
    /// A connected device's screens changed.
    PeerScreens { id: DeviceId, screens: Vec<Rect> },
    Mode(Mode),
    /// The saved settings changed in the engine (pairing, forget, reset,
    /// another device's arrangement).
    Config(Config),
    /// Waiting to reconnect to a device, in seconds (0: not waiting).
    Reconnecting { id: DeviceId, secs: u64 },
    /// Dialing a device.
    Connecting { id: DeviceId },
    /// Pairing started, progressed or ended (`None`).
    Pairing(Option<PairingState>),
    /// Why the last pairing didn't complete.
    PairingFailed(String),
    /// Other dextr devices on the local network.
    Nearby(Vec<Nearby>),
    /// This device's network addresses.
    Addresses(Vec<LocalAddress>),
    Log(String),
}

/// Starts the engine on its own thread and runtime.
pub fn spawn() -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("dextr-core".into())
        .spawn(move || {
            // One thread for the whole engine: input passes from capture to
            // the network without hopping between threads. OS backends and
            // the clipboard run on threads of their own.
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
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
    /// A connection we accepted.
    Established(anyhow::Result<Established>),
    /// A connection we dialed to `id`.
    Dialed { id: DeviceId, result: anyhow::Result<Established> },
    AcceptFailed(String),
    PairDialed { id: u64, result: anyhow::Result<PairLink> },
    PairIncoming(PairLink),
    PairPeerAccepted { id: u64 },
    PairEnded { id: u64, reason: String },
    Paired { id: u64, est: Established },
    Inbound { session: u64, message: Message },
    /// A device copied something.
    Clipboard { session: u64, content: Clipboard },
    SessionClosed { session: u64, reason: String },
}

struct Established {
    /// This device opened the connection.
    dialed: bool,
    /// The other device's certificate fingerprint.
    fingerprint: Fingerprint,
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

/// How long a controlled device may stay silent before the pointer comes home.
const TARGET_SILENT: Duration = Duration::from_millis(2500);

struct Session {
    id: u64,
    dialed: bool,
    conn: quinn::Connection,
    out: mpsc::UnboundedSender<Message>,
    peer: Hello,
    since: Instant,
}

/// Connections this close together are a simultaneous dial by both sides.
const SIMULTANEOUS: Duration = Duration::from_secs(3);

/// Connection state of one paired device.
#[derive(Default)]
struct Link {
    connecting: bool,
    /// The user disconnected it on purpose: don't reconnect on our own.
    user_disconnected: bool,
    reconnect_at: Option<Instant>,
    delay: Duration,
}

struct Engine {
    config: Config,
    identity: Arc<Identity>,
    /// This device, as it appears in the shared arrangement.
    me: DeviceId,
    /// Paired devices' fingerprints.
    trusted: HashSet<Fingerprint>,
    endpoint: Option<quinn::Endpoint>,
    events: mpsc::UnboundedSender<Event>,
    internal: mpsc::UnboundedSender<Internal>,
    capture: Option<CaptureHandle>,
    injector: Option<InjectorHandle>,
    sessions: HashMap<DeviceId, Session>,
    next_session: u64,
    links: HashMap<DeviceId, Link>,
    mode: Mode,
    /// This device and the connected devices, placed in the shared space.
    world: World,
    /// While controlling: the pointer, in the shared space.
    pointer: (f32, f32),
    ctrl_down: bool,
    alt_down: bool,
    paused: bool,
    starting_capture: bool,
    starting_injector: bool,
    /// Pointer is pushing against an edge, waiting for the switch delay.
    pending_switch: Option<PendingSwitch>,
    /// While controlling: packets received from the target, and when that last changed.
    heard: Option<(u64, Instant)>,
    /// The edges to watch changed while we controlled another device; apply
    /// them once the pointer is back.
    sides_stale: bool,
    /// The edges currently watched.
    sides: Vec<Side>,
    pairing: Option<Pairing>,
    next_pairing: u64,
    nearby: HashMap<Fingerprint, (Nearby, Instant)>,
    beacon: watch::Sender<Beacon>,
    addresses: Vec<LocalAddress>,
    clipboard: Option<ClipboardHandle>,
}

struct PendingSwitch {
    deadline: Instant,
    /// Where the pointer hit our edge, and which edge.
    at: (f32, f32),
    side: Side,
    /// The device it will enter, and where.
    target: DeviceId,
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
    let trusted: HashSet<Fingerprint> = config.peers.keys().filter_map(|id| identity::parse(id)).collect();
    let endpoint = match bind(&identity, config.port) {
        Ok(ep) => Some(ep),
        Err(e) => {
            log(format!("Cannot listen on UDP port {}: {e:#}", config.port));
            None
        }
    };
    let me = identity::format(&identity.fingerprint);
    let _ = events.send(Event::Ready {
        config: config.clone(),
        fingerprint: me.clone(),
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
    let mut liveness = tokio::time::interval(Duration::from_millis(500));
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let (clip_tx, mut clip_rx) = mpsc::unbounded_channel();
    let clipboard = match clipboard::start_clipboard(clip_tx) {
        Ok(handle) => Some(handle),
        Err(e) => {
            log(format!("Clipboard sharing is unavailable: {e:#}"));
            None
        }
    };

    let mut engine = Engine {
        config,
        identity,
        me,
        trusted,
        endpoint: endpoint.clone(),
        events: events.clone(),
        internal: internal_tx,
        capture: None,
        injector: None,
        sessions: HashMap::new(),
        next_session: 0,
        links: HashMap::new(),
        mode: Mode::Local,
        world: World { devices: Vec::new(), proportional: false },
        pointer: (0.0, 0.0),
        ctrl_down: false,
        alt_down: false,
        paused: false,
        starting_capture: false,
        starting_injector: false,
        pending_switch: None,
        heard: None,
        sides_stale: false,
        sides: Vec::new(),
        pairing: None,
        next_pairing: 0,
        nearby: HashMap::new(),
        beacon,
        addresses,
        clipboard,
    };
    engine.start_backends();
    engine.report_screens();
    engine.rebuild_world();
    if engine.config.connection.auto_connect {
        engine.connect_all();
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
                    engine.log("Remote input is ready: other devices can control this one.".into());
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
                    capture_rx = Some(rx);
                    let _ = events.send(Event::CaptureReady(Ok(())));
                    engine.log("Input capture is ready: this mouse and keyboard can move to other devices.".into());
                    engine.report_screens();
                    engine.resend_screens();
                    engine.sides.clear();
                    engine.rebuild_world();
                    engine.sync_capture();
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
            () = sleep_opt(engine.next_reconnect()) => engine.reconnect_due(),
            () = sleep_opt(engine.pending_switch.as_ref().map(|p| p.deadline)) => engine.commit_switch(),
            incoming = accept_opt(&endpoint) => {
                if let Some(incoming) = incoming {
                    if !engine.config.connection.allow_incoming {
                        engine.log(format!("Refused a connection from {} (incoming connections are off).", incoming.remote_address().ip()));
                        incoming.refuse();
                        continue;
                    }
                    let (tx, hello) = (engine.internal.clone(), engine.hello());
                    let trusted = engine.trusted.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(match accept(incoming, hello, &trusted).await {
                            Ok(Accepted::Session(est)) => Internal::Established(Ok(est)),
                            Ok(Accepted::Pair(link)) => Internal::PairIncoming(link),
                            Err(e) => Internal::AcceptFailed(format!("{e:#}")),
                        });
                    });
                }
            }
            Some((beacon, from)) = found_rx.recv() => engine.on_beacon(beacon, from),
            Some(content) = clip_rx.recv() => engine.local_clipboard(content),
            _ = housekeeping.tick() => engine.housekeeping(),
            _ = liveness.tick(), if matches!(engine.mode, Mode::Controlling(_)) => engine.check_target(),
            note = injector_note(&mut injector_watch) => match note {
                InjectorNote::Screens(screens) => {
                    engine.broadcast(Message::Displays(screens));
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
    for id in engine.sessions.keys().cloned().collect::<Vec<_>>() {
        engine.end_session(&id, "app closed".into());
    }
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
async fn accept(incoming: quinn::Incoming, hello: Hello, trusted: &HashSet<Fingerprint>) -> anyhow::Result<Accepted> {
    let conn = incoming.await.context("incoming connection failed")?;
    let from = conn.remote_address().ip().to_canonical();
    let fingerprint = tls::peer_fingerprint(&conn).context("the other device sent no certificate")?;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let first = tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv))
        .await
        .context("the other device said nothing")??;
    match first {
        Message::Hello(peer) if trusted.contains(&fingerprint) => {
            write_message(&mut send, &Message::Hello(hello)).await?;
            check_protocol(&peer)?;
            Ok(Accepted::Session(Established { dialed: false, fingerprint, conn, send, recv, peer }))
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
    fingerprint: Fingerprint,
) -> anyhow::Result<Established> {
    let conn = dial(&endpoint, config, &address).await?;
    let (send, recv) = conn.open_bi().await?;
    exchange_hello(conn, send, recv, hello, true, fingerprint).await
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
    let PairLink { conn, mut send, mut recv, name, dialed, fingerprint, .. } = link;
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
    match exchange_hello(conn, send, recv, hello, dialed, fingerprint).await {
        Ok(est) => {
            let _ = tx.send(Internal::Paired { id, est });
        }
        Err(e) => ended(format!("Paired, but connecting failed: {e:#}")),
    }
}

/// Receives the other device's clipboard: each copy on a stream of its own.
async fn receive_clipboard(conn: quinn::Connection, session: u64, tx: mpsc::UnboundedSender<Internal>) {
    while let Ok(mut stream) = conn.accept_uni().await {
        let tx = tx.clone();
        tokio::spawn(async move {
            match read_clipboard(&mut stream).await {
                Ok(content) => {
                    let _ = tx.send(Internal::Clipboard { session, content });
                }
                Err(e) => tracing::debug!("clipboard not received: {e:#}"),
            }
        });
    }
}

async fn read_clipboard(recv: &mut quinn::RecvStream) -> anyhow::Result<Clipboard> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_CLIPBOARD {
        bail!("the other device sent an oversized clipboard ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Clipboard::from_bytes(&buf)?)
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
    fingerprint: Fingerprint,
) -> anyhow::Result<Established> {
    write_message(&mut send, &Message::Hello(hello)).await?;
    let peer = match tokio::time::timeout(Duration::from_secs(5), read_message(&mut recv)).await?? {
        Message::Hello(h) => h,
        other => bail!("expected Hello, got {other:?}"),
    };
    check_protocol(&peer)?;
    Ok(Established { dialed, fingerprint, conn, send, recv, peer })
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
        let restore = self.config.inject_restore_token.clone();
        tokio::spawn(async move {
            if need_injector {
                let _ = tx.send(Internal::InjectorStarted(dextr_platform::start_injector(restore).await));
            }
            if need_capture {
                // Edges are set once the arrangement is known.
                let _ = tx.send(Internal::CaptureStarted(dextr_platform::start_capture(Vec::new()).await));
            }
        });
    }

    /// Edge watching is on while some device is connected next to us, sharing
    /// isn't paused, and no device is controlling this one.
    fn sync_capture(&self) {
        if let Some(c) = &self.capture {
            let controlled = matches!(self.mode, Mode::Controlled(_));
            if !self.sessions.is_empty() && !self.paused && self.config.mouse.enabled && !controlled && !self.sides.is_empty() {
                c.enable();
            } else {
                c.disable();
            }
        }
    }

    /// Tells connected devices our screens again, e.g. once a backend that
    /// knows them has started after we connected.
    fn resend_screens(&self) {
        let displays = self.hello().displays;
        if !displays.is_empty() {
            self.broadcast(Message::Displays(displays));
        }
    }

    fn report_screens(&self) {
        let _ = self.events.send(Event::LocalScreens(self.hello().displays));
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode.clone();
            let _ = self.events.send(Event::Mode(mode));
        }
        if !matches!(self.mode, Mode::Controlling(_)) && self.sides_stale {
            self.update_sides();
        }
    }

    /// Our screens in the coordinate space others should use for Motion.
    fn hello(&self) -> Hello {
        let from_injector = self.injector.as_ref().map(|i| i.screens()).unwrap_or_default();
        let displays = if from_injector.is_empty() {
            self.capture.as_ref().map(|c| c.screens()).unwrap_or_default()
        } else {
            from_injector
        };
        let displays = if displays.is_empty() { test_screens() } else { displays };
        Hello { protocol: PROTOCOL_VERSION, name: self.config.device_name.clone(), os: Os::current(), displays }
    }

    /// Our screens as the capture backend sees them (the coordinates it
    /// reports and accepts).
    fn our_screens(&self) -> Vec<Rect> {
        let s = self.capture.as_ref().map(|c| c.screens()).unwrap_or_default();
        if s.is_empty() { self.hello().displays } else { s }
    }

    fn send_to(&self, id: &str, msg: Message) {
        if let Some(s) = self.sessions.get(id) {
            let _ = s.out.send(msg);
        }
    }

    fn broadcast(&self, msg: Message) {
        for s in self.sessions.values() {
            let _ = s.out.send(msg.clone());
        }
    }

    /// Unreliable and unordered: only for absolute pointer motion.
    fn send_motion(&self, id: &str, (x, y): (f32, f32)) {
        if let Some(s) = self.sessions.get(id)
            && let Ok(bytes) = Message::Input(Input::Motion { x, y }).to_bytes()
            && s.conn.send_datagram(bytes.into()).is_err()
        {
            // Datagrams unsupported or too large: fall back to the stream.
            let _ = s.out.send(Message::Input(Input::Motion { x, y }));
        }
    }

    fn peer_name(&self, id: &str) -> String {
        self.sessions
            .get(id)
            .map(|s| s.peer.name.clone())
            .or_else(|| self.config.peers.get(id).map(|p| p.name.clone()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "the other device".into())
    }

    // ── Shared arrangement ──────────────────────────────────────────────

    /// Places this device and the connected devices, and watches the edges
    /// that lead somewhere.
    fn rebuild_world(&mut self) {
        let mut screens: BTreeMap<DeviceId, Vec<Rect>> = self.sessions.iter().map(|(id, s)| (id.clone(), s.peer.displays.clone())).collect();
        screens.insert(self.me.clone(), self.our_screens());
        self.world = World::build(
            &self.config.arrangement,
            self.config.layout == DisplayLayout::Proportional,
            &self.me,
            &screens,
            self.config.new_device_side,
        );
        self.adopt_placements();
        self.update_sides();
    }

    /// Devices placed automatically (not arranged yet) are written into the
    /// shared arrangement and sent to everyone, so every device ends up with
    /// the same layout. Automatic placements keep the arrangement's time (1 if
    /// never arranged), so an arrangement someone made always wins, and among
    /// equal times the lowest fingerprint's does.
    fn adopt_placements(&mut self) {
        let a = &mut self.config.arrangement;
        let mut added = false;
        if self.world.proportional {
            for d in &self.world.devices {
                if !a.row.contains(&d.id) {
                    added = true;
                }
            }
            if added {
                a.row = self.world.devices.iter().map(|d| d.id.clone()).collect();
            }
        } else {
            for d in &self.world.devices {
                if !a.origins.contains_key(&d.id) {
                    a.origins.insert(d.id.clone(), (d.origin.0.round() as i32, d.origin.1.round() as i32));
                    added = true;
                }
            }
        }
        if !added {
            return;
        }
        a.changed_at = a.changed_at.max(1);
        if let Err(e) = self.config.save() {
            tracing::warn!("saving the arrangement: {e:#}");
        }
        let _ = self.events.send(Event::Config(self.config.clone()));
        self.broadcast(self.layout_message());
    }

    /// Watches the edges with a device past them. Not while controlling
    /// another device: re-arming the edges can end the capture and drop the
    /// pointer back here, so it waits until the pointer comes home.
    fn update_sides(&mut self) {
        if matches!(self.mode, Mode::Controlling(_)) {
            self.sides_stale = true;
            return;
        }
        self.sides_stale = false;
        let sides = self.world.sides_with_neighbors(&self.me);
        if sides != self.sides {
            self.sides = sides.clone();
            if let Some(c) = &self.capture {
                c.set_sides(sides);
            }
            self.sync_capture();
        }
    }

    fn layout_message(&self) -> Message {
        let fp = |id: &DeviceId| identity::parse(id);
        let a = &self.config.arrangement;
        Message::Layout {
            proportional: self.config.layout == DisplayLayout::Proportional,
            origins: a.origins.iter().filter_map(|(id, &(x, y))| Some((fp(id)?, x, y))).collect(),
            row: a.row.iter().filter_map(fp).collect(),
            changed_at: a.changed_at,
        }
    }

    /// Another device's arrangement: keep it if it's newer than ours.
    fn peer_layout(&mut self, from: &str, proportional: bool, origins: Vec<([u8; 32], i32, i32)>, row: Vec<[u8; 32]>, changed_at: u64) {
        let ours = self.config.arrangement.changed_at;
        // Same instant (or never arranged): the lower fingerprint wins.
        let sender_lower = identity::parse(from).is_some_and(|fp| fp < self.identity.fingerprint);
        if changed_at < ours || (changed_at == ours && (changed_at == 0 || !sender_lower)) {
            return;
        }
        let arrangement = Arrangement {
            origins: origins.into_iter().map(|(fp, x, y)| (identity::format(&fp), (x, y))).collect(),
            row: row.iter().map(identity::format).collect(),
            changed_at,
        };
        let layout = if proportional { DisplayLayout::Proportional } else { DisplayLayout::Exact };
        let changed = arrangement.origins != self.config.arrangement.origins
            || arrangement.row != self.config.arrangement.row
            || layout != self.config.layout;
        self.config.arrangement = arrangement;
        self.config.layout = layout;
        if changed {
            let name = self.peer_name(from);
            self.save_and_publish(&format!("Display arrangement updated to match {name}."));
            self.rebuild_world();
        } else {
            let _ = self.config.save();
        }
    }

    // ── Commands ────────────────────────────────────────────────────────

    fn command(&mut self, command: Command) {
        match command {
            Command::Apply(new) => {
                let mut new = *new;
                let rearranged = new.layout != self.config.layout
                    || new.arrangement.origins != self.config.arrangement.origins
                    || new.arrangement.row != self.config.arrangement.row;
                new.arrangement.changed_at = if rearranged { now_ms() } else { self.config.arrangement.changed_at };
                if new.device_name != self.config.device_name {
                    let name = new.device_name.clone();
                    self.beacon.send_modify(|b| b.name = name);
                }
                if new.port != self.config.port {
                    self.log("The new port takes effect after restarting dextr.".into());
                }
                // The engine owns pairings and what it learns about devices;
                // the UI owns their sharing rules.
                let mut peers = self.config.peers.clone();
                for (id, entry) in &mut peers {
                    if let Some(edited) = new.peers.get(id) {
                        entry.send = edited.send;
                        entry.receive = edited.receive;
                    }
                }
                let token = self.config.inject_restore_token.take();
                self.config = Config { peers, inject_restore_token: token, ..new };
                match self.config.save() {
                    Ok(()) => self.log("Settings saved.".into()),
                    Err(e) => self.log(format!("Cannot save settings: {e:#}")),
                }
                if rearranged {
                    self.broadcast(self.layout_message());
                    self.rebuild_world();
                }
                // Master switches can turn edge watching on or off.
                self.sync_capture();
            }
            Command::Connect(None) => self.connect_all(),
            Command::Connect(Some(id)) => {
                let link = self.links.entry(id.clone()).or_default();
                link.user_disconnected = false;
                link.delay = RECONNECT_MIN;
                link.reconnect_at = None;
                self.connect(&id);
            }
            Command::Disconnect(target) => {
                let ids: Vec<DeviceId> = match target {
                    Some(id) => vec![id],
                    None => self.config.peers.keys().cloned().collect(),
                };
                for id in ids {
                    let link = self.links.entry(id.clone()).or_default();
                    link.user_disconnected = true;
                    link.reconnect_at = None;
                    let _ = self.events.send(Event::Reconnecting { id: id.clone(), secs: 0 });
                    self.end_session(&id, "disconnected".into());
                }
            }
            Command::ForgetPeer(id) => {
                self.links.entry(id.clone()).or_default().user_disconnected = true;
                self.end_session(&id, "device forgotten".into());
                self.links.remove(&id);
                if let Some(fp) = identity::parse(&id) {
                    self.trusted.remove(&fp);
                }
                let name = self.config.peers.remove(&id).map(|p| p.name).unwrap_or_default();
                self.config.arrangement.origins.remove(&id);
                self.config.arrangement.row.retain(|d| d != &id);
                self.save_and_publish(&format!("Forgot {}.", if name.is_empty() { "the device" } else { &name }));
                self.refresh_nearby();
                self.rebuild_world();
            }
            Command::ResetSettings => {
                self.config = Config {
                    inject_restore_token: self.config.inject_restore_token.take(),
                    peers: std::mem::take(&mut self.config.peers)
                        .into_iter()
                        .map(|(id, p)| (id, PeerEntry { name: p.name, address: p.address, os: p.os, screens: p.screens, ..PeerEntry::default() }))
                        .collect(),
                    port: self.config.port,
                    device_name: std::mem::take(&mut self.config.device_name),
                    ..Config::default()
                };
                self.config.arrangement.changed_at = now_ms();
                self.paused = false;
                self.save_and_publish("Settings reset to defaults. Pairings, name and port were kept.");
                self.broadcast(self.layout_message());
                self.rebuild_world();
                self.sync_capture();
            }
            Command::RetryPermissions => self.start_backends(),
            Command::Pair(address) => self.start_pairing(address),
            Command::PairDecision(ok) => self.decide_pairing(ok),
            Command::SetPaused(paused) => {
                self.paused = paused;
                if paused && matches!(self.mode, Mode::Controlling(_)) {
                    self.leave_to_local(self.pointer);
                }
                self.sync_capture();
                self.log(if paused { "Sharing paused." } else { "Sharing resumed." }.into());
            }
        }
    }

    fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Established(Ok(est)) => self.start_session(est),
            Internal::Established(Err(e)) => self.log(format!("Connection failed: {e:#}")),
            Internal::Dialed { id, result } => {
                let link = self.links.entry(id.clone()).or_default();
                link.connecting = false;
                match result {
                    Ok(est) => self.start_session(est),
                    Err(e) => {
                        let quiet = link.delay > RECONNECT_MIN;
                        if !quiet {
                            let name = self.peer_name(&id);
                            self.log(format!("Connection to {name} failed: {e:#}"));
                        }
                        self.schedule_reconnect(&id);
                    }
                }
            }
            Internal::Inbound { session, message } => {
                if let Some(id) = self.session_id(session) {
                    self.inbound(&id, message);
                }
            }
            Internal::SessionClosed { session, reason } => {
                if let Some(id) = self.session_id(session) {
                    self.end_session(&id, reason);
                    self.schedule_reconnect(&id);
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
                if self.pairing.is_some() {
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
                let fp = est.fingerprint;
                let device = identity::format(&fp);
                self.trusted.insert(fp);
                let entry = self.config.peers.entry(device.clone()).or_default();
                entry.name = est.peer.name.clone();
                entry.address = p.address;
                entry.os = Some(est.peer.os);
                self.links.entry(device).or_default().user_disconnected = false;
                self.save_and_publish(&format!("Paired with {}.", est.peer.name));
                self.refresh_nearby();
                self.start_session(est);
            }
            Internal::Clipboard { session, content } => {
                if let Some(id) = self.session_id(session) {
                    self.peer_clipboard(&id, content);
                }
            }
            Internal::InjectorStarted(_) | Internal::CaptureStarted(_) => unreachable!("handled in run()"),
        }
    }

    fn session_id(&self, session: u64) -> Option<DeviceId> {
        self.sessions.iter().find(|(_, s)| s.id == session).map(|(id, _)| id.clone())
    }

    // ── Pairing ─────────────────────────────────────────────────────────

    fn start_pairing(&mut self, address: String) {
        let address = address.trim().to_owned();
        if address.is_empty() || self.pairing.is_some() {
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

    // ── Discovery ───────────────────────────────────────────────────────

    fn on_beacon(&mut self, beacon: Beacon, from: SocketAddr) {
        if beacon.fingerprint == self.identity.fingerprint || beacon.protocol != PROTOCOL_VERSION {
            return;
        }
        let address = SocketAddr::new(from.ip().to_canonical(), beacon.port);
        let paired = self.trusted.contains(&beacon.fingerprint);
        let id = identity::format(&beacon.fingerprint);
        let entry = Nearby { name: beacon.name, os: beacon.os, address, fingerprint: id.clone(), paired };
        let changed = self.nearby.get(&beacon.fingerprint).is_none_or(|(n, _)| *n != entry);
        let name = entry.name.clone();
        self.nearby.insert(beacon.fingerprint, (entry, Instant::now()));
        if changed {
            self.publish_nearby();
        }

        if paired && !self.sessions.contains_key(&id) {
            // Follow the device to its new address.
            if let Some(p) = self.config.peers.get_mut(&id)
                && p.address.trim() != address.to_string()
            {
                p.address = address.to_string();
                self.save_and_publish(&format!("{name} is now at {address}."));
            }
            // It's online: retry now instead of waiting out the backoff. Only
            // one side does, so the two don't keep dialing each other at once.
            let link = self.links.entry(id.clone()).or_default();
            if link.reconnect_at.is_some() && !link.connecting && self.identity.fingerprint < beacon.fingerprint {
                link.reconnect_at = None;
                self.connect(&id);
            }
        }
    }

    /// Drops devices that went quiet, and notices address and screen changes.
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
        // Our screens changed (a monitor plugged in): place them again.
        if self.world.device(&self.me).is_none_or(|d| d.screens != self.our_screens()) {
            self.rebuild_world();
        }
    }

    /// Re-marks which nearby devices are paired.
    fn refresh_nearby(&mut self) {
        for (fp, (n, _)) in self.nearby.iter_mut() {
            n.paired = self.trusted.contains(fp);
        }
        self.publish_nearby();
    }

    fn publish_nearby(&self) {
        let mut list: Vec<Nearby> = self.nearby.values().map(|(n, _)| n.clone()).collect();
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.address.cmp(&b.address)));
        let _ = self.events.send(Event::Nearby(list));
    }

    // ── Clipboard ───────────────────────────────────────────────────────

    /// Something was copied here: send it to every device allowed to get it.
    fn local_clipboard(&mut self, content: ClipContent) {
        if !self.config.clipboard.enabled || self.sessions.is_empty() {
            return;
        }
        let image = matches!(content, ClipContent::Png(_));
        let size = content.len();
        let (message, what) = match content {
            ClipContent::Text(text) => {
                let what = format!("text ({} characters)", text.chars().count());
                (Clipboard::Text(text), what)
            }
            ClipContent::Png(png) => {
                let what = format!("an image ({} KB)", png.len().div_ceil(1024));
                (Clipboard::Image { png }, what)
            }
        };
        let frame = match message.to_frame() {
            Ok(f) => Arc::new(f),
            Err(e) => return tracing::debug!("clipboard not encoded: {e}"),
        };
        let mut too_big = Vec::new();
        for (id, session) in &self.sessions {
            let peer = self.config.peer(id);
            if !peer.allows(Channel::Clipboard, true) || (image && !peer.clipboard.images) {
                continue;
            }
            if size > peer.clipboard.max_mb as usize * 1024 * 1024 {
                too_big.push(format!("{} ({} MB limit)", session.peer.name, peer.clipboard.max_mb));
                continue;
            }
            tracing::debug!("sharing the clipboard with {}: {what}", session.peer.name);
            let (conn, frame) = (session.conn.clone(), frame.clone());
            tokio::spawn(async move {
                let result = async {
                    let mut out = conn.open_uni().await?;
                    out.write_all(&frame).await?;
                    out.finish()?;
                    anyhow::Ok(())
                };
                if let Err(e) = result.await {
                    tracing::debug!("clipboard not sent: {e:#}");
                }
            });
        }
        if !too_big.is_empty() {
            self.log(format!("Didn't share {what} with {}: it's over the limit.", too_big.join(", ")));
        }
    }

    /// A device copied something: put it on our clipboard.
    fn peer_clipboard(&mut self, from: &str, content: Clipboard) {
        let c = self.config.peer(from).clipboard;
        if !self.config.clipboard.enabled || !self.config.peer(from).allows(Channel::Clipboard, false) {
            return;
        }
        let content = match content {
            Clipboard::Text(text) => ClipContent::Text(text),
            Clipboard::Image { png } if c.images => ClipContent::Png(png),
            Clipboard::Image { .. } => return,
        };
        if content.len() > c.max_mb as usize * 1024 * 1024 {
            return;
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.set(content);
        }
    }

    // ── Connections ─────────────────────────────────────────────────────

    fn save_and_publish(&mut self, message: &str) {
        match self.config.save() {
            Ok(()) => self.log(message.into()),
            Err(e) => self.log(format!("Cannot save settings: {e:#}")),
        }
        let _ = self.events.send(Event::Config(self.config.clone()));
    }

    fn connect_all(&mut self) {
        for id in self.config.peers.keys().cloned().collect::<Vec<_>>() {
            let link = self.links.entry(id.clone()).or_default();
            link.user_disconnected = false;
            link.delay = RECONNECT_MIN;
            link.reconnect_at = None;
            self.connect(&id);
        }
    }

    fn connect(&mut self, id: &str) {
        if self.sessions.contains_key(id) || self.links.get(id).is_some_and(|l| l.connecting) {
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            self.log("Networking is unavailable (see the error above).".into());
            return;
        };
        let (Some(fp), Some(peer)) = (identity::parse(id), self.config.peers.get(id)) else { return };
        let address = peer.address.trim().to_owned();
        if address.is_empty() {
            return;
        }
        let client = match tls::client_config(&self.identity, Some(fp)) {
            Ok(c) => c,
            Err(e) => return self.log(format!("TLS setup failed: {e:#}")),
        };
        let link = self.links.entry(id.to_owned()).or_default();
        if link.delay <= RECONNECT_MIN {
            let name = if peer.name.is_empty() { address.clone() } else { peer.name.clone() };
            self.log(format!("Connecting to {name}…"));
        }
        let link = self.links.entry(id.to_owned()).or_default();
        link.connecting = true;
        let _ = self.events.send(Event::Connecting { id: id.to_owned() });
        let (tx, hello, id) = (self.internal.clone(), self.hello(), id.to_owned());
        tokio::spawn(async move {
            let result = connect(endpoint, client, address, hello, fp).await;
            let _ = tx.send(Internal::Dialed { id, result });
        });
    }

    /// Tries again later, backing off up to 30 s, with jitter so two devices
    /// that both dial don't keep colliding.
    fn schedule_reconnect(&mut self, id: &str) {
        if !self.config.connection.auto_reconnect || !self.config.peers.contains_key(id) || self.sessions.contains_key(id) {
            return;
        }
        let link = self.links.entry(id.to_owned()).or_default();
        if link.user_disconnected {
            return;
        }
        let jitter = Duration::from_millis(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_millis() as u64).unwrap_or(0) % 1000,
        );
        link.delay = link.delay.max(RECONNECT_MIN);
        let delay = link.delay + jitter;
        link.reconnect_at = Some(Instant::now() + delay);
        link.delay = (link.delay * 2).min(RECONNECT_MAX);
        let _ = self.events.send(Event::Reconnecting { id: id.to_owned(), secs: delay.as_secs().max(1) });
    }

    fn next_reconnect(&self) -> Option<Instant> {
        self.links.values().filter_map(|l| l.reconnect_at).min()
    }

    fn reconnect_due(&mut self) {
        let now = Instant::now();
        let due: Vec<DeviceId> = self
            .links
            .iter()
            .filter(|(_, l)| l.reconnect_at.is_some_and(|at| at <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some(l) = self.links.get_mut(&id) {
                l.reconnect_at = None;
            }
            self.connect(&id);
        }
    }

    fn start_session(&mut self, est: Established) {
        let Established { dialed, fingerprint, conn, mut send, mut recv, peer } = est;
        let device = identity::format(&fingerprint);
        if !self.trusted.contains(&fingerprint) {
            conn.close(2u32.into(), b"not paired");
            return;
        }
        if let Some(existing) = self.sessions.get(&device) {
            // Both devices dialed at once. Each keeps the connection dialed by
            // the device with the lower fingerprint, so both keep the same one.
            let we_are_lower = self.identity.fingerprint < fingerprint;
            // An older connection means the device restarted (or its network
            // changed) and that one is dead: take the new one.
            let preferred = |by_us: bool| by_us == we_are_lower;
            let simultaneous = existing.since.elapsed() < SIMULTANEOUS;
            if simultaneous && (preferred(existing.dialed) || !preferred(dialed)) {
                conn.close(1u32.into(), b"busy");
                tracing::debug!("refused a second connection from {}", peer.name);
                return;
            }
            tracing::debug!("replacing the connection to {} with a newer one", peer.name);
            self.end_session(&device, "replaced by a newer connection".into());
        }
        let id = self.next_session;
        self.next_session += 1;
        let link = self.links.entry(device.clone()).or_default();
        link.reconnect_at = None;
        link.delay = RECONNECT_MIN;
        link.connecting = false;
        link.user_disconnected = false;

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
        // Clipboard contents from the device (one stream per copy).
        tokio::spawn(receive_clipboard(conn.clone(), id, self.internal.clone()));
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
            id: device.clone(),
            name: peer.name.clone(),
            os: peer.os,
            address: conn.remote_address(),
            screens: peer.displays.clone(),
        });
        self.log(format!("Connected to {}.", peer.name));
        self.remember_peer(&device, &peer);
        if peer.displays.is_empty() {
            self.log(format!("{} reported no screens, so the pointer can't move there yet.", peer.name));
        }
        let out = out_tx.clone();
        self.sessions.insert(device, Session { id, dialed, conn, out: out_tx, peer, since: Instant::now() });
        // Whichever device was arranged last wins.
        let _ = out.send(self.layout_message());
        self.rebuild_world();
        self.sync_capture();
    }

    /// Notes a device's name, OS and screens (to show it while it's offline).
    fn remember_peer(&mut self, id: &str, hello: &Hello) {
        let Some(entry) = self.config.peers.get_mut(id) else { return };
        if entry.name != hello.name || entry.os != Some(hello.os) || entry.screens != hello.displays {
            entry.name = hello.name.clone();
            entry.os = Some(hello.os);
            entry.screens = hello.displays.clone();
            if let Err(e) = self.config.save() {
                self.log(format!("Cannot save settings: {e:#}"));
            }
            let _ = self.events.send(Event::Config(self.config.clone()));
        }
    }

    fn end_session(&mut self, id: &str, reason: String) {
        let Some(session) = self.sessions.remove(id) else { return };
        match &self.mode {
            Mode::Controlling(t) if t == id => {
                // Its screens are gone: bring the pointer home.
                let p = self.home_point(self.pointer);
                if let Some(c) = &self.capture {
                    c.release(p.0, p.1);
                }
                self.set_mode(Mode::Local);
            }
            Mode::Controlled(by) if by == id => {
                if let Some(i) = &self.injector {
                    i.release_all();
                }
                self.set_mode(Mode::Local);
            }
            _ => {}
        }
        session.conn.close(0u32.into(), b"bye");
        let _ = self.events.send(Event::Disconnected { id: id.to_owned(), reason });
        self.rebuild_world();
        self.sync_capture();
    }

    // ── Sending side ────────────────────────────────────────────────────

    fn captured(&mut self, event: CaptureEvent) {
        // Another device drives our pointer, and it touched our edge before
        // capture was switched off: never take the pointer (it would stay
        // hidden). Hand it straight back.
        if matches!(self.mode, Mode::Controlled(_))
            && let CaptureEvent::Activated { x, y } = event
        {
            if let Some(c) = &self.capture {
                c.release(x, y);
            }
            return;
        }
        if self.pending_switch.is_some() {
            return self.while_pending(event);
        }
        match event {
            CaptureEvent::Activated { x, y } => self.activated(x, y),
            CaptureEvent::Input(input) if matches!(self.mode, Mode::Controlling(_)) => self.forward(input),
            CaptureEvent::Input(_) => {}
            CaptureEvent::Deactivated => {
                if let Mode::Controlling(t) = self.mode.clone() {
                    self.send_to(&t, Message::Leave);
                    self.set_mode(Mode::Local);
                }
            }
            CaptureEvent::Stopped { reason } => {
                self.capture = None;
                if let Mode::Controlling(t) = self.mode.clone() {
                    self.send_to(&t, Message::Leave);
                    self.set_mode(Mode::Local);
                }
                let _ = self.events.send(Event::CaptureReady(Err(reason)));
            }
        }
    }

    /// Gives the pointer straight back, just inside the edge it hit.
    fn bounce(&self, side: Option<Side>, (x, y): (f32, f32)) {
        let ours = self.our_screens();
        let back = side.and_then(|s| edge::inside_edge(s, x, y, &ours)).unwrap_or((x, y));
        if let Some(c) = &self.capture {
            c.release(back.0, back.1);
        }
    }

    /// The pointer hit one of our edges at (x, y).
    fn activated(&mut self, x: f32, y: f32) {
        let ours = self.our_screens();
        let side = world::side_at(&ours, (x, y), &self.sides);
        if self.paused || !self.config.mouse.enabled {
            return self.bounce(side, (x, y));
        }
        let Some(side) = side else { return self.bounce(None, (x, y)) };
        if edge::in_dead_corner(side, x, y, &ours, self.config.switching.dead_corners as f32) {
            return self.bounce(Some(side), (x, y));
        }
        let Some((target, entry)) = self.world.beyond(&self.me, (x, y), side).map(|(d, p)| (d.id.clone(), p)) else {
            return self.bounce(Some(side), (x, y));
        };
        if !self.config.peer(&target).allows(Channel::Mouse, true) {
            return self.bounce(Some(side), (x, y));
        }
        if self.config.switching.delay_ms > 0 {
            // The compositor holds the pointer at the edge meanwhile.
            self.pending_switch = Some(PendingSwitch {
                deadline: Instant::now() + Duration::from_millis(self.config.switching.delay_ms as u64),
                at: (x, y),
                side,
                target,
                entry,
                retreat: 0.0,
            });
        } else {
            self.enter(&target, entry);
        }
    }

    /// The pointer moves onto `target` at its local point `at`.
    fn enter(&mut self, target: &str, at: (f32, f32)) {
        let Some(device) = self.world.device(target).filter(|_| self.sessions.contains_key(target)) else {
            // Gone meanwhile: never keep the pointer captured with nowhere to go.
            return self.leave_to_local(self.pointer);
        };
        self.heard = None;
        self.pointer = device.to_world(at);
        self.ctrl_down = false;
        self.alt_down = false;
        self.send_to(target, Message::Enter { x: at.0, y: at.1 });
        self.set_mode(Mode::Controlling(target.to_owned()));
    }

    /// The pointer kept pushing for the whole switch delay: go.
    fn commit_switch(&mut self) {
        if let Some(p) = self.pending_switch.take() {
            if self.sessions.contains_key(&p.target) {
                self.enter(&p.target.clone(), p.entry);
            } else {
                self.bounce(Some(p.side), p.at);
            }
        }
    }

    /// Input during the switch delay: moving back away from the edge, or
    /// pressing anything, cancels the switch.
    fn while_pending(&mut self, event: CaptureEvent) {
        let Some(mut p) = self.pending_switch.take() else { return };
        match event {
            CaptureEvent::Input(Captured::Motion { dx, dy }) => {
                let away = match p.side {
                    Side::Right => -dx,
                    Side::Left => dx,
                    Side::Bottom => -dy,
                    Side::Top => dy,
                };
                p.retreat = (p.retreat + away).max(0.0);
                if p.retreat > 6.0 {
                    self.bounce(Some(p.side), p.at);
                } else {
                    self.pending_switch = Some(p);
                }
            }
            CaptureEvent::Input(_) => self.bounce(Some(p.side), p.at),
            CaptureEvent::Activated { .. } => self.pending_switch = Some(p),
            CaptureEvent::Deactivated => {}
            CaptureEvent::Stopped { reason } => {
                self.capture = None;
                let _ = self.events.send(Event::CaptureReady(Err(reason)));
            }
        }
    }

    /// Where the pointer is on the controlled device, in its coordinates.
    fn pointer_on(&self, target: &str) -> (f32, f32) {
        self.world.device(target).map_or(self.pointer, |d| d.to_local(self.pointer))
    }

    fn forward(&mut self, input: Captured) {
        let Mode::Controlling(target) = self.mode.clone() else { return };
        match input {
            Captured::Motion { dx, dy } => self.move_pointer(&target, dx, dy),
            Captured::Button { button, pressed } => {
                let button = match (self.config.peer(&target).mouse.swap_buttons, button) {
                    (true, Button::Left) => Button::Right,
                    (true, Button::Right) => Button::Left,
                    (_, b) => b,
                };
                let (x, y) = self.pointer_on(&target);
                self.send_to(&target, Message::Input(Input::Button { button, pressed, x, y }));
            }
            Captured::Scroll { dx, dy } => {
                let k = self.config.peer(&target).mouse.scroll_factor();
                self.send_to(&target, Message::Input(Input::Scroll { dx: dx * k, dy: dy * k }));
            }
            Captured::ScrollDiscrete { dx, dy } => {
                let k = self.config.peer(&target).mouse.scroll_factor();
                let scale = |v: i32| (v as f32 * k).round() as i32;
                self.send_to(&target, Message::Input(Input::ScrollDiscrete { dx: scale(dx), dy: scale(dy) }));
            }
            Captured::ScrollStop => self.send_to(&target, Message::Input(Input::ScrollStop)),
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
                        self.leave_to_local(self.pointer);
                    }
                    return;
                }
                // Keyboard off for this device: keys go nowhere while the
                // pointer is over there (the shortcut above still works).
                let peer = self.config.peer(&target);
                if !(self.config.keyboard.enabled && peer.allows(Channel::Keyboard, true)) {
                    return;
                }
                let peer_os = self.sessions.get(&target).map(|s| s.peer.os).unwrap_or(Os::Other);
                let key = peer.keyboard.remap(key, Os::current(), peer_os);
                self.send_to(&target, Message::Input(Input::Key { key, pressed }));
            }
        }
    }

    /// Moves the pointer through the shared space: on the device it's over,
    /// onto the next device past an edge, or home.
    fn move_pointer(&mut self, target: &str, dx: f32, dy: f32) {
        let Some(current) = self.world.device(target) else { return self.leave_to_local(self.pointer) };
        // Motion is in our pixels; move the same number of pixels over there.
        let k = self.config.peer(target).mouse.speed * current.scale;
        let to = (self.pointer.0 + dx * k, self.pointer.1 + dy * k);
        let next = self.world.device_at(to).map(|d| d.id.clone());
        match next {
            Some(id) if id == target => {
                self.pointer = to;
                self.send_motion(target, self.pointer_on(target));
            }
            Some(id) if id == self.me => self.leave_to_local(to),
            Some(id) if self.config.peer(&id).allows(Channel::Mouse, true) && self.sessions.contains_key(&id) => {
                // On to the next device.
                self.send_to(target, Message::Leave);
                let at = self.world.device(&id).map_or(to, |d| d.to_local(to));
                self.enter(&id, at);
                self.pointer = to;
            }
            _ => {
                // Nothing there: the edge is a wall.
                self.pointer = current.clamp(to);
                self.send_motion(target, self.pointer_on(target));
            }
        }
    }

    /// Our nearest point to `world_point`, a little inside our edges so the
    /// pointer doesn't land on one and switch straight back.
    fn home_point(&self, world_point: (f32, f32)) -> (f32, f32) {
        let Some(me) = self.world.device(&self.me) else { return (0.0, 0.0) };
        let (x, y) = me.to_local(me.clamp(world_point));
        let b = me.bounds;
        (x.clamp(b.x as f32 + 2.0, b.right() - 3.0), y.clamp(b.y as f32 + 2.0, b.bottom() - 3.0))
    }

    /// While controlling: if the device stops answering (asleep, out of
    /// range), bring the pointer home now instead of when the connection
    /// times out. A live device acknowledges our keep-alives every second.
    fn check_target(&mut self) {
        let Mode::Controlling(target) = self.mode.clone() else { return };
        let Some(session) = self.sessions.get(&target) else { return self.leave_to_local(self.pointer) };
        let received = session.conn.stats().udp_rx.datagrams;
        match self.heard {
            Some((count, at)) if count == received => {
                if at.elapsed() > TARGET_SILENT {
                    let name = session.peer.name.clone();
                    self.log(format!("{name} stopped responding: the pointer is back here."));
                    self.leave_to_local(self.pointer);
                }
            }
            _ => self.heard = Some((received, Instant::now())),
        }
    }

    fn leave_to_local(&mut self, world_point: (f32, f32)) {
        if let Mode::Controlling(t) = self.mode.clone() {
            self.send_to(&t, Message::Leave);
        }
        let back = self.home_point(world_point);
        if let Some(c) = &self.capture {
            c.release(back.0, back.1);
        }
        self.set_mode(Mode::Local);
    }

    // ── Receiving side ──────────────────────────────────────────────────

    fn inbound(&mut self, from: &str, message: Message) {
        match message {
            Message::Enter { x, y } => {
                let name = self.peer_name(from);
                if !self.config.connection.allow_control || !self.config.peer(from).allows(Channel::Mouse, false) {
                    // Hand control straight back.
                    self.send_to(from, Message::Leave);
                    self.log(format!("{name} tried to take control, but its mouse isn't allowed here."));
                    return;
                }
                match self.mode.clone() {
                    Mode::Controlled(by) if by != from => {
                        // Someone else is already in control.
                        self.send_to(from, Message::Leave);
                        return;
                    }
                    // Both crossed at once: yield.
                    Mode::Controlling(_) => self.leave_to_local(self.pointer),
                    _ => {}
                }
                self.set_mode(Mode::Controlled(from.to_owned()));
                self.sync_capture();
                if let Some(i) = &self.injector {
                    i.inject(Input::Motion { x, y });
                } else {
                    self.log(format!("{name} is trying to control this device, but remote input wasn't allowed here."));
                }
            }
            Message::Input(input) => {
                if self.mode != Mode::Controlled(from.to_owned()) {
                    return;
                }
                if matches!(input, Input::Key { .. }) && !self.config.peer(from).allows(Channel::Keyboard, false) {
                    return; // its keyboard isn't allowed here
                }
                if let Some(i) = &self.injector {
                    i.inject(input);
                }
            }
            Message::Leave => match self.mode.clone() {
                Mode::Controlling(t) if t == from => {
                    // The device handed control back (it refused it, or its
                    // user pressed its own return shortcut).
                    let back = self.home_point(self.pointer);
                    if let Some(c) = &self.capture {
                        c.release(back.0, back.1);
                    }
                    self.set_mode(Mode::Local);
                }
                Mode::Controlled(by) if by == from => {
                    if let Some(i) = &self.injector {
                        i.release_all();
                    }
                    self.set_mode(Mode::Local);
                    self.sync_capture();
                }
                _ => {}
            },
            Message::ReleaseAll => {
                if self.mode == Mode::Controlled(from.to_owned())
                    && let Some(i) = &self.injector
                {
                    i.release_all();
                }
            }
            Message::Displays(displays) => {
                let Some(s) = self.sessions.get_mut(from) else { return };
                s.peer.displays = displays.clone();
                let hello = s.peer.clone();
                let _ = self.events.send(Event::PeerScreens { id: from.to_owned(), screens: displays });
                self.remember_peer(from, &hello);
                self.rebuild_world();
            }
            Message::Layout { proportional, origins, row, changed_at } => self.peer_layout(from, proportional, origins, row, changed_at),
            // Only valid before a session starts.
            Message::Hello(_) | Message::PairRequest { .. } | Message::PairAccept | Message::PairReject => {}
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
            tokio::spawn(async move { accept(server.accept().await.unwrap(), hello("A"), &HashSet::from([a_trusts])).await })
        };
        let dial = connect(client_ep, tls::client_config(b, Some(a.fingerprint))?, format!("127.0.0.1:{port}"), hello("B"), a.fingerprint).await;
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
        let accept_task = tokio::spawn(async move { accept(server.accept().await.unwrap(), hello("A"), &HashSet::new()).await });
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
    async fn clipboard_crosses_on_its_own_stream() {
        let (a, b) = (temp_identity("ca"), temp_identity("cb"));
        let (at_a, at_b) = try_pair(&a, b.fingerprint, &b).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(receive_clipboard(at_a.conn.clone(), 9, tx));

        for sent in [Clipboard::Text("héllo 👋".into()), Clipboard::Image { png: vec![0x89, b'P', b'N', b'G', 1, 2, 3] }] {
            let mut out = at_b.conn.open_uni().await.unwrap();
            out.write_all(&sent.to_frame().unwrap()).await.unwrap();
            out.finish().unwrap();
            let Some(Internal::Clipboard { session: 9, content }) = rx.recv().await else { panic!("nothing arrived") };
            assert_eq!(content, sent);
        }
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
