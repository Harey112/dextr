//! Capture with a Quartz event tap.
//!
//! macOS has no pointer barriers, so the tap watches the cursor itself: when
//! it is on the outer edge facing the peer and still pushing outward, the
//! cursor is detached from the mouse and hidden, and every event is
//! swallowed and reported until the core releases it.

use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicPtr, Ordering},
    },
};

use dextr_proto::{Key, Rect, Side, keys};
use tokio::sync::{mpsc, oneshot, watch};

use super::{DEXTR_EVENT, PERMISSION_HINT, SCREEN_POLL, button_from_number, ffi::*, screens};
use crate::{CaptureCommand, CaptureEvent, Captured, spawn_backend_thread};

struct Shared {
    events: mpsc::UnboundedSender<CaptureEvent>,
    state: Mutex<State>,
    port: AtomicPtr<c_void>,
    run_loop: AtomicPtr<c_void>,
}

struct State {
    /// Watching the edge at all.
    enabled: bool,
    /// Input currently goes to the peer.
    captured: bool,
    /// Cleared on release until the cursor leaves the edge, so pushing on
    /// after being handed back doesn't re-trigger at every event.
    armed: bool,
    side: Side,
    /// Our screens whose `side` edge is on the outside of the layout.
    edges: Vec<Rect>,
    cursor_hidden: bool,
    /// Keys held on this Mac (macOS keycodes). If one is still held when
    /// the pointer crosses, its release must reach this Mac, not the peer,
    /// or macOS keeps it pressed.
    held: Vec<u16>,
}

impl State {
    fn set_layout(&mut self, side: Side, screens: &[Rect]) {
        self.side = side;
        self.edges = Rect::on_outer_edge(screens, side);
    }

    /// Whether the cursor at `p`, moving by (dx, dy), is pushing against the
    /// edge facing the peer.
    fn pushing_edge(&self, p: CGPoint, dx: f64, dy: f64) -> bool {
        let (x, y) = (p.x as f32, p.y as f32);
        let outward = match self.side {
            Side::Right => dx > 0.0,
            Side::Left => dx < 0.0,
            Side::Bottom => dy > 0.0,
            Side::Top => dy < 0.0,
        };
        outward
            && self.edges.iter().any(|r| {
                let along_y = y >= r.y as f32 && y < r.bottom();
                let along_x = x >= r.x as f32 && x < r.right();
                match self.side {
                    Side::Right => along_y && x >= r.right() - 1.0,
                    Side::Left => along_y && x < r.x as f32 + 1.0,
                    Side::Bottom => along_x && y >= r.bottom() - 1.0,
                    Side::Top => along_x && y < r.y as f32 + 1.0,
                }
            })
    }

    fn grab(&mut self) {
        self.captured = true;
        self.armed = false;
        unsafe {
            CGAssociateMouseAndMouseCursorPosition(0);
            if !self.cursor_hidden {
                CGDisplayHideCursor(0);
                self.cursor_hidden = true;
            }
        }
    }

    /// Gives the cursor back, at (x, y) if given.
    fn release(&mut self, at: Option<(f32, f32)>) {
        if !self.captured {
            return;
        }
        self.captured = false;
        unsafe {
            if let Some((x, y)) = at {
                CGWarpMouseCursorPosition(CGPoint { x: x as f64, y: y as f64 });
            }
            CGAssociateMouseAndMouseCursorPosition(1);
            if self.cursor_hidden {
                CGDisplayShowCursor(0);
                self.cursor_hidden = false;
            }
        }
    }
}

const fn bit(kind: CGEventType) -> u64 {
    1 << kind
}

const EVENTS: u64 = bit(LEFT_DOWN)
    | bit(LEFT_UP)
    | bit(RIGHT_DOWN)
    | bit(RIGHT_UP)
    | bit(MOUSE_MOVED)
    | bit(LEFT_DRAGGED)
    | bit(RIGHT_DRAGGED)
    | bit(KEY_DOWN)
    | bit(KEY_UP)
    | bit(FLAGS_CHANGED)
    | bit(SCROLL_WHEEL)
    | bit(OTHER_DOWN)
    | bit(OTHER_UP)
    | bit(OTHER_DRAGGED);

pub(crate) fn spawn_capture(
    side: Side,
    mut commands: mpsc::UnboundedReceiver<CaptureCommand>,
    events: mpsc::UnboundedSender<CaptureEvent>,
    screens_tx: watch::Sender<Vec<Rect>>,
    ready: oneshot::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let mut known = screens();
    screens_tx.send_replace(known.clone());
    let mut state = State { enabled: false, captured: false, armed: true, side, edges: Vec::new(), cursor_hidden: false, held: Vec::new() };
    state.set_layout(side, &known);
    let shared = Arc::new(Shared {
        events,
        state: Mutex::new(state),
        port: AtomicPtr::new(ptr::null_mut()),
        run_loop: AtomicPtr::new(ptr::null_mut()),
    });

    let (tap_tx, tap_rx) = std::sync::mpsc::channel();
    let tap = shared.clone();
    std::thread::Builder::new().name("dextr-capture-tap".into()).spawn(move || run_tap(tap, tap_tx))?;

    spawn_backend_thread("dextr-capture", move || async move {
        match tap_rx.recv() {
            Ok(Ok(())) => {
                let _ = ready.send(Ok(()));
            }
            Ok(Err(e)) => {
                let _ = ready.send(Err(anyhow::anyhow!(e)));
                return;
            }
            Err(_) => {
                let _ = ready.send(Err(anyhow::anyhow!("the capture thread exited")));
                return;
            }
        }

        let mut poll = tokio::time::interval(SCREEN_POLL);
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    let mut st = shared.state.lock().unwrap();
                    match command {
                        CaptureCommand::Enable => st.enabled = true,
                        CaptureCommand::Disable => {
                            st.release(None);
                            st.enabled = false;
                        }
                        CaptureCommand::Release { x, y } => st.release(Some((x, y))),
                        CaptureCommand::SetSide(side) => st.set_layout(side, &known),
                    }
                }
                _ = poll.tick() => {
                    let now = screens();
                    if now != known {
                        known = now;
                        let mut st = shared.state.lock().unwrap();
                        let side = st.side;
                        st.set_layout(side, &known);
                        screens_tx.send_replace(known.clone());
                    }
                }
            }
        }

        // The handle was dropped: give the cursor back and stop the tap.
        shared.state.lock().unwrap().release(None);
        unsafe {
            let port = shared.port.swap(ptr::null_mut(), Ordering::AcqRel);
            if !port.is_null() {
                CFMachPortInvalidate(port);
            }
            let rl = shared.run_loop.swap(ptr::null_mut(), Ordering::AcqRel);
            if !rl.is_null() {
                CFRunLoopStop(rl);
            }
        }
    })
}

fn run_tap(shared: Arc<Shared>, ready: std::sync::mpsc::Sender<Result<(), String>>) {
    unsafe {
        // Show the permission prompts on first run.
        if !CGPreflightListenEventAccess() {
            CGRequestListenEventAccess();
        }
        if !CGPreflightPostEventAccess() {
            CGRequestPostEventAccess();
        }
        allow_background_cursor_hiding();
        // Don't freeze local input briefly after warping the cursor back.
        CGSetLocalEventsSuppressionInterval(0.0);

        let user = Arc::as_ptr(&shared) as *mut c_void;
        let port = CGEventTapCreate(SESSION_EVENT_TAP, HEAD_INSERT, TAP_OPTION_DEFAULT, EVENTS, callback, user);
        if port.is_null() {
            let _ = ready.send(Err(format!("macOS didn't allow dextr to read the mouse and keyboard. {PERMISSION_HINT}")));
            return;
        }
        let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
        let rl = CFRunLoopGetCurrent();
        CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
        shared.port.store(port, Ordering::Release);
        shared.run_loop.store(rl, Ordering::Release);
        CGEventTapEnable(port, true);
        let _ = ready.send(Ok(()));

        CFRunLoopRun();

        CFRelease(source);
        CFRelease(port);
    }
}

unsafe extern "C" fn callback(_proxy: CGEventTapProxy, kind: CGEventType, event: CGEventRef, user: *mut c_void) -> CGEventRef {
    // SAFETY: `user` is the `Shared` kept alive by the tap thread for as long
    // as its run loop (the only caller of this callback) runs.
    let shared = unsafe { &*(user as *const Shared) };
    unsafe {
        if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
            let port = shared.port.load(Ordering::Acquire);
            if !port.is_null() {
                CGEventTapEnable(port, true);
            }
            return event;
        }
        // Our own injected input (when the other device controls this Mac):
        // never treat it as the user's hand. The tag can be lost on the way
        // through the HID layer, the posting process ID can't.
        if CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) == DEXTR_EVENT
            || CGEventGetIntegerValueField(event, FIELD_SOURCE_PID) == std::process::id() as i64
        {
            return event;
        }
    }
    let Ok(mut st) = shared.state.lock() else { return event };
    if !st.enabled {
        return event;
    }

    // Track keys held here (see `State::held`).
    let key = key_change(kind, event);
    if !st.captured {
        match key {
            Some((code, true)) if !st.held.contains(&code) => st.held.push(code),
            Some((code, false)) => st.held.retain(|&k| k != code),
            _ => {}
        }
        // Only plain moves switch: crossing while dragging would leave the
        // button held here.
        if kind == MOUSE_MOVED {
            let (p, dx, dy) = unsafe {
                (
                    CGEventGetLocation(event),
                    CGEventGetDoubleValueField(event, FIELD_DELTA_X),
                    CGEventGetDoubleValueField(event, FIELD_DELTA_Y),
                )
            };
            if st.pushing_edge(p, dx, dy) {
                if st.armed {
                    st.grab();
                    let _ = shared.events.send(CaptureEvent::Activated { x: p.x as f32, y: p.y as f32 });
                    return ptr::null_mut();
                }
            } else {
                st.armed = true;
            }
        }
        return event;
    }

    // The release of a key pressed before crossing belongs to this Mac.
    if let Some((code, false)) = key
        && st.held.contains(&code)
    {
        st.held.retain(|&k| k != code);
        return event;
    }

    // Captured: report everything, deliver nothing locally.
    let send = |input: Captured| {
        let _ = shared.events.send(CaptureEvent::Input(input));
    };
    unsafe { translate(kind, event, send) };
    ptr::null_mut()
}

/// A key press or release (macOS keycode, pressed), including modifiers.
fn key_change(kind: CGEventType, event: CGEventRef) -> Option<(u16, bool)> {
    let code = || unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) } as u16;
    match kind {
        KEY_DOWN => Some((code(), true)),
        KEY_UP => Some((code(), false)),
        FLAGS_CHANGED => {
            let code = code();
            modifier_bit(code).map(|bit| (code, unsafe { CGEventGetFlags(event) } & bit != 0))
        }
        _ => None,
    }
}

/// The left/right-specific flag bit of a modifier key.
fn modifier_bit(code: u16) -> Option<u64> {
    Some(match code {
        0x3B => DEV_LCTRL,
        0x3E => DEV_RCTRL,
        0x38 => DEV_LSHIFT,
        0x3C => DEV_RSHIFT,
        0x3A => DEV_LALT,
        0x3D => DEV_RALT,
        0x37 => DEV_LCMD,
        0x36 => DEV_RCMD,
        _ => return None,
    })
}

unsafe fn translate(kind: CGEventType, event: CGEventRef, send: impl Fn(Captured)) {
    let int = |field| unsafe { CGEventGetIntegerValueField(event, field) };
    let double = |field| unsafe { CGEventGetDoubleValueField(event, field) };
    let button = |pressed| {
        let number = match kind {
            LEFT_DOWN | LEFT_UP => 0,
            RIGHT_DOWN | RIGHT_UP => 1,
            _ => int(FIELD_BUTTON_NUMBER),
        };
        send(Captured::Button { button: button_from_number(number), pressed });
    };

    match kind {
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => {
            send(Captured::Motion { dx: double(FIELD_DELTA_X) as f32, dy: double(FIELD_DELTA_Y) as f32 });
        }
        LEFT_DOWN | RIGHT_DOWN | OTHER_DOWN => button(true),
        LEFT_UP | RIGHT_UP | OTHER_UP => button(false),
        SCROLL_WHEEL => {
            // Quartz: positive is up/left. The protocol: positive is down/right.
            if int(FIELD_SCROLL_CONTINUOUS) != 0 {
                let (dx, dy) = (-double(FIELD_SCROLL_POINT_DELTA_2), -double(FIELD_SCROLL_POINT_DELTA_1));
                if dx != 0.0 || dy != 0.0 {
                    send(Captured::Scroll { dx: dx as f32, dy: dy as f32 });
                }
            } else {
                let (dx, dy) = (-int(FIELD_SCROLL_DELTA_2), -int(FIELD_SCROLL_DELTA_1));
                if dx != 0 || dy != 0 {
                    send(Captured::ScrollDiscrete { dx: dx as i32 * 120, dy: dy as i32 * 120 });
                }
            }
            if int(FIELD_SCROLL_MOMENTUM_PHASE) == MOMENTUM_PHASE_END {
                send(Captured::ScrollStop);
            }
        }
        KEY_DOWN | KEY_UP => {
            // The receiving OS repeats held keys itself.
            if int(FIELD_AUTOREPEAT) != 0 {
                return;
            }
            if let Some(key) = keys::from_mac(int(FIELD_KEYCODE) as u16) {
                send(Captured::Key { key, pressed: kind == KEY_DOWN });
            }
        }
        FLAGS_CHANGED => {
            let code = int(FIELD_KEYCODE) as u16;
            let flags = unsafe { CGEventGetFlags(event) };
            let Some(key) = keys::from_mac(code) else { return };
            if key == Key::keyboard(0x39) {
                // Caps Lock reports toggles, not presses: send a full press.
                send(Captured::Key { key, pressed: true });
                send(Captured::Key { key, pressed: false });
                return;
            }
            let Some(bit) = modifier_bit(code) else { return };
            send(Captured::Key { key, pressed: flags & bit != 0 });
        }
        _ => {}
    }
}
