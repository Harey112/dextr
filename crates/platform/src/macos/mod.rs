//! macOS backend.
//!
//! * capture: a Quartz event tap. At the edge facing the peer it detaches
//!   the cursor from the mouse (`CGAssociateMouseAndMouseCursorPosition`),
//!   hides it, and swallows all input while reporting it, until released.
//! * inject: `CGEventPost` at the HID level, so posted input behaves like a
//!   real device (key repeat, double clicks, modifier state).
//!
//! Both need dextr (or the terminal running it) to be allowed under System
//! Settings → Privacy & Security → Accessibility; capture also under Input
//! Monitoring.
//!
//! Coordinates are Quartz global points: origin at the top-left of the main
//! display, y down, like the core's [`Rect`].

mod capture;
mod ffi;
mod inject;

pub(crate) use capture::spawn_capture;
pub(crate) use inject::spawn_injector;

use dextr_proto::{Button, Rect};

/// Marks events dextr posts itself, so the capture tap lets them through.
const DEXTR_EVENT: i64 = 0x6465_7874; // "dext"

/// How often the display layout is checked for changes.
const SCREEN_POLL: std::time::Duration = std::time::Duration::from_secs(2);

const PERMISSION_HINT: &str = "Allow it under System Settings → Privacy & Security → Accessibility \
(and Input Monitoring), for dextr or for the terminal app that starts it. Then press \"Ask again\" \
or restart dextr.";

/// The active displays, in global points.
fn screens() -> Vec<Rect> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    if unsafe { ffi::CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids[..count as usize]
        .iter()
        .map(|&id| {
            let b = unsafe { ffi::CGDisplayBounds(id) };
            Rect::new(
                b.origin.x.round() as i32,
                b.origin.y.round() as i32,
                b.size.width.round() as u32,
                b.size.height.round() as u32,
            )
        })
        .collect()
}

/// Quartz mouse button number.
fn button_number(button: Button) -> u32 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Back => 3,
        Button::Forward => 4,
        Button::Other(n) => n as u32,
    }
}

fn button_from_number(n: i64) -> Button {
    match n {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::Back,
        4 => Button::Forward,
        n => Button::Other(n as u16),
    }
}
