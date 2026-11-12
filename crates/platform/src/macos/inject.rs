//! Injection with `CGEventPost` at the HID level.
//!
//! Quartz doesn't derive click counts, drag events or modifier flags from
//! posted events the way it does for real devices, so they're tracked here
//! and set on every event.

use std::time::{Duration, Instant};

use dextr_proto::{Button, Input, Key, Rect, keys};
use tokio::sync::{mpsc, oneshot, watch};

use super::{DEXTR_EVENT, PERMISSION_HINT, SCREEN_POLL, button_number, ffi::*, screens};
use crate::{InjectCommand, RestoreToken, spawn_backend_thread};

/// Clicks closer than this, in time and distance, count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 5.0;

pub(crate) fn spawn_injector(
    _restore: Option<RestoreToken>,
    mut commands: mpsc::UnboundedReceiver<InjectCommand>,
    screens_tx: watch::Sender<Vec<Rect>>,
    stopped: watch::Sender<Option<String>>,
    ready: oneshot::Sender<anyhow::Result<Option<RestoreToken>>>,
) -> anyhow::Result<()> {
    spawn_backend_thread("dextr-inject", move || async move {
        // Keep the sender alive for as long as the injector runs: dropping it
        // tells the engine the injector stopped.
        let stopped = stopped;
        let source = unsafe {
            if !CGPreflightPostEventAccess() {
                CGRequestPostEventAccess();
                let _ = ready.send(Err(anyhow::anyhow!("macOS didn't allow dextr to control this Mac. {PERMISSION_HINT}")));
                return;
            }
            CGEventSourceCreate(SOURCE_STATE_HID_SYSTEM)
        };
        if source.is_null() {
            let _ = ready.send(Err(anyhow::anyhow!("cannot create a Quartz event source")));
            return;
        }
        unsafe { CGEventSourceSetLocalEventsSuppressionInterval(source, 0.0) };

        let mut known = screens();
        screens_tx.send_replace(known.clone());
        let _ = ready.send(Ok(None));

        let mut injector = Injector::new(source);
        let mut poll = tokio::time::interval(SCREEN_POLL);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(InjectCommand::Input(input)) => injector.input(input),
                    Some(InjectCommand::ReleaseAll) => injector.release_all(),
                    None => break,
                },
                _ = poll.tick() => {
                    let now = screens();
                    if now != known {
                        known = now;
                        screens_tx.send_replace(known.clone());
                    }
                }
            }
        }
        injector.release_all();
        unsafe { CFRelease(source) };
        let _ = stopped.send(Some("input injection stopped".into()));
    })
}

struct Click {
    button: Button,
    at: Instant,
    pos: CGPoint,
    count: i64,
}

struct Injector {
    source: CGEventSourceRef,
    pos: CGPoint,
    /// Held buttons, by Quartz button number.
    buttons: u32,
    /// Held keys, as macOS keycodes.
    keys: Vec<u16>,
    caps_lock: bool,
    click: Option<Click>,
    /// Sub-pixel and sub-line scroll left over from earlier events.
    scroll: (f32, f32),
    lines: (i32, i32),
}

/// Modifier flags for a held modifier key.
fn modifier_flags(code: u16) -> Option<u64> {
    Some(match code {
        0x3B => FLAG_CONTROL | DEV_LCTRL,
        0x3E => FLAG_CONTROL | DEV_RCTRL,
        0x38 => FLAG_SHIFT | DEV_LSHIFT,
        0x3C => FLAG_SHIFT | DEV_RSHIFT,
        0x3A => FLAG_ALT | DEV_LALT,
        0x3D => FLAG_ALT | DEV_RALT,
        0x37 => FLAG_COMMAND | DEV_LCMD,
        0x36 => FLAG_COMMAND | DEV_RCMD,
        _ => return None,
    })
}

const CAPS_LOCK: u16 = 0x39;

impl Injector {
    fn new(source: CGEventSourceRef) -> Self {
        Self {
            source,
            pos: CGPoint::default(),
            buttons: 0,
            keys: Vec::new(),
            caps_lock: false,
            click: None,
            scroll: (0.0, 0.0),
            lines: (0, 0),
        }
    }

    fn flags(&self) -> u64 {
        let held = self.keys.iter().filter_map(|&k| modifier_flags(k)).fold(0, |a, b| a | b);
        held | if self.caps_lock { FLAG_CAPS } else { 0 }
    }

    /// Tags, sets modifiers on, posts and frees `event`.
    fn post(&self, event: CGEventRef) {
        if event.is_null() {
            return;
        }
        unsafe {
            CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, DEXTR_EVENT);
            CGEventSetFlags(event, self.flags());
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::Motion { x, y } => self.motion(x, y),
            Input::Button { button, pressed, x, y } => self.button(button, pressed, x, y),
            Input::Scroll { dx, dy } => self.scroll(dx, dy),
            Input::ScrollDiscrete { dx, dy } => self.scroll_lines(dx, dy),
            Input::ScrollStop => {}
            Input::Key { key, pressed } => self.key(key, pressed),
        }
    }

    fn motion(&mut self, x: f32, y: f32) {
        let to = CGPoint { x: x as f64, y: y as f64 };
        let (dx, dy) = (to.x - self.pos.x, to.y - self.pos.y);
        self.pos = to;
        // With a button held, macOS apps expect drag events, not moves.
        let (kind, number) = if self.buttons & 1 != 0 {
            (LEFT_DRAGGED, 0)
        } else if self.buttons & 2 != 0 {
            (RIGHT_DRAGGED, 1)
        } else if self.buttons != 0 {
            (OTHER_DRAGGED, self.buttons.trailing_zeros())
        } else {
            (MOUSE_MOVED, 0)
        };
        unsafe {
            let event = CGEventCreateMouseEvent(self.source, kind, to, number);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, FIELD_DELTA_X, dx.round() as i64);
                CGEventSetIntegerValueField(event, FIELD_DELTA_Y, dy.round() as i64);
            }
            self.post(event);
        }
    }

    fn button(&mut self, button: Button, pressed: bool, x: f32, y: f32) {
        self.pos = CGPoint { x: x as f64, y: y as f64 };
        let number = button_number(button);
        let count = if pressed {
            let now = Instant::now();
            let count = match &self.click {
                Some(c)
                    if c.button == button
                        && now.duration_since(c.at) < DOUBLE_CLICK
                        && (c.pos.x - self.pos.x).hypot(c.pos.y - self.pos.y) < DOUBLE_CLICK_DISTANCE =>
                {
                    c.count + 1
                }
                _ => 1,
            };
            self.click = Some(Click { button, at: now, pos: self.pos, count });
            if number < 32 {
                self.buttons |= 1 << number;
            }
            count
        } else {
            if number < 32 {
                self.buttons &= !(1 << number);
            }
            self.click.as_ref().filter(|c| c.button == button).map_or(1, |c| c.count)
        };
        let kind = match (number, pressed) {
            (0, true) => LEFT_DOWN,
            (0, false) => LEFT_UP,
            (1, true) => RIGHT_DOWN,
            (1, false) => RIGHT_UP,
            (_, true) => OTHER_DOWN,
            (_, false) => OTHER_UP,
        };
        unsafe {
            let event = CGEventCreateMouseEvent(self.source, kind, self.pos, number);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, FIELD_CLICK_STATE, count);
                CGEventSetIntegerValueField(event, FIELD_BUTTON_NUMBER, number as i64);
            }
            self.post(event);
        }
    }

    /// Smooth scrolling in pixels. The protocol's positive is down/right,
    /// Quartz's is up/left.
    fn scroll(&mut self, dx: f32, dy: f32) {
        self.scroll.0 -= dx;
        self.scroll.1 -= dy;
        let (wx, wy) = (self.scroll.0.trunc(), self.scroll.1.trunc());
        if wx == 0.0 && wy == 0.0 {
            return;
        }
        self.scroll.0 -= wx;
        self.scroll.1 -= wy;
        unsafe {
            let event = CGEventCreateScrollWheelEvent2(self.source, SCROLL_UNIT_PIXEL, 2, wy as i32, wx as i32, 0);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, FIELD_SCROLL_CONTINUOUS, 1);
            }
            self.post(event);
        }
    }

    /// Wheel clicks, in 120ths of a click.
    fn scroll_lines(&mut self, dx: i32, dy: i32) {
        self.lines.0 -= dx;
        self.lines.1 -= dy;
        let (lx, ly) = (self.lines.0 / 120, self.lines.1 / 120);
        if lx == 0 && ly == 0 {
            return;
        }
        self.lines.0 -= lx * 120;
        self.lines.1 -= ly * 120;
        unsafe { self.post(CGEventCreateScrollWheelEvent2(self.source, SCROLL_UNIT_LINE, 2, ly, lx, 0)) };
    }

    fn key(&mut self, key: Key, pressed: bool) {
        // Media keys (the consumer page) need AppKit system events; not yet.
        let Some(code) = keys::to_mac(key) else { return };
        if code == CAPS_LOCK {
            if pressed {
                self.caps_lock = !self.caps_lock;
                self.post_key(code, true, true);
            }
            return;
        }
        if pressed {
            if !self.keys.contains(&code) {
                self.keys.push(code);
            }
        } else {
            self.keys.retain(|&k| k != code);
        }
        self.post_key(code, pressed, modifier_flags(code).is_some());
    }

    fn post_key(&self, code: u16, pressed: bool, modifier: bool) {
        unsafe {
            let event = CGEventCreateKeyboardEvent(self.source, code, pressed);
            if modifier && !event.is_null() {
                CGEventSetType(event, FLAGS_CHANGED);
            }
            self.post(event);
        }
    }

    fn release_all(&mut self) {
        // Ordinary keys first, so nothing types with a stuck modifier.
        let mut held = std::mem::take(&mut self.keys);
        held.sort_by_key(|&k| modifier_flags(k).is_some());
        for (i, &code) in held.iter().enumerate() {
            self.keys = held[i + 1..].to_vec();
            self.post_key(code, false, modifier_flags(code).is_some());
        }
        self.keys.clear();
        for number in 0..32 {
            if self.buttons & (1 << number) != 0 {
                let button = super::button_from_number(number as i64);
                self.button(button, false, self.pos.x as f32, self.pos.y as f32);
            }
        }
    }
}
