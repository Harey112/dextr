//! Linux backend.
//!
//! Wayland (KDE Plasma 6.1+, GNOME 45+) and any desktop whose portal offers
//! these interfaces:
//!
//! * capture: `org.freedesktop.portal.InputCapture` + libei (receiver).
//!   The compositor owns the pointer barrier, so no access to `/dev/input`
//!   and no `input` group are needed.
//! * inject: `org.freedesktop.portal.RemoteDesktop` + libei (sender).
//!
//! libei is spoken through the pure-Rust `reis` crate, so there are no
//! system library dependencies.

mod capture;
mod inject;

pub(crate) use capture::spawn_capture;
pub(crate) use inject::spawn_injector;

/// CLOCK_MONOTONIC in microseconds, the timestamp libei expects.
fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

fn button_from_evdev(code: u32) -> dextr_proto::Button {
    use dextr_proto::Button;
    match code {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE => Button::Back,
        BTN_EXTRA => Button::Forward,
        other => Button::Other(other as u16),
    }
}

fn button_to_evdev(button: dextr_proto::Button) -> u32 {
    use dextr_proto::Button;
    match button {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
        Button::Other(code) => code as u32,
    }
}
