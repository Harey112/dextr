//! Window capture on macOS with Quartz window services.
//!
//! dextr lists the on-screen app windows itself, so the user picks one in
//! dextr. The chosen window is captured on its own (no other windows, no
//! shadow) at its full Retina resolution, and its position is read from
//! the window list so input from the other device lands on it.
//!
//! Needs Screen Recording permission (System Settings → Privacy & Security).

use std::{
    ffi::c_void,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{oneshot, watch};

use super::ffi::*;
use crate::window::{CapturedFrame, InputHandle, ShareableWindow, WindowCapture, WindowGeometry};

const PERMISSION: &str = "macOS didn't allow dextr to see other windows. Allow it under System Settings → Privacy & Security → \
Screen Recording (for dextr, or for the terminal app that starts it), then restart dextr.";

/// How often the window is captured. Unchanged pixels cost nothing on the
/// network; the encoder sends only what changed.
const INTERVAL: Duration = Duration::from_millis(33);

/// Info about one window from the window list.
pub(super) struct Info {
    pub(super) id: u32,
    pid: i64,
    layer: i64,
    title: String,
    owner: String,
    bounds: CGRect,
}

unsafe fn number(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    let v = unsafe { CFDictionaryGetValue(dict, key) };
    let mut out = 0i64;
    (!v.is_null() && unsafe { CFNumberGetValue(v, CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void) }).then_some(out)
}

unsafe fn string(dict: CFDictionaryRef, key: CFStringRef) -> String {
    let v = unsafe { CFDictionaryGetValue(dict, key) };
    if v.is_null() {
        return String::new();
    }
    let cap = unsafe { CFStringGetLength(v) } * 4 + 1;
    let mut buf = vec![0u8; cap as usize];
    if !unsafe { CFStringGetCString(v, buf.as_mut_ptr() as *mut _, cap, STRING_ENCODING_UTF8) } {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Windows from the window list: all on-screen ones, or just `only`.
fn window_list(only: Option<u32>) -> Vec<Info> {
    let (option, relative) = match only {
        Some(id) => (WINDOW_LIST_INCLUDING_WINDOW, id),
        None => (WINDOW_LIST_ON_SCREEN_ONLY | WINDOW_LIST_EXCLUDE_DESKTOP, 0),
    };
    let mut out = Vec::new();
    unsafe {
        let list = CGWindowListCopyWindowInfo(option, relative);
        if list.is_null() {
            return out;
        }
        for i in 0..CFArrayGetCount(list) {
            let dict = CFArrayGetValueAtIndex(list, i);
            let mut bounds = CGRect::default();
            let b = CFDictionaryGetValue(dict, kCGWindowBounds);
            if b.is_null() || !CGRectMakeWithDictionaryRepresentation(b, &mut bounds) {
                continue;
            }
            out.push(Info {
                id: number(dict, kCGWindowNumber).unwrap_or(0) as u32,
                pid: number(dict, kCGWindowOwnerPID).unwrap_or(0),
                layer: number(dict, kCGWindowLayer).unwrap_or(0),
                title: string(dict, kCGWindowName),
                owner: string(dict, kCGWindowOwnerName),
                bounds,
            });
        }
        CFRelease(list);
    }
    out
}

pub(crate) async fn shareable_windows() -> anyhow::Result<Option<Vec<ShareableWindow>>> {
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        unsafe { CGRequestScreenCaptureAccess() };
        anyhow::bail!(PERMISSION);
    }
    let me = std::process::id() as i64;
    let list = window_list(None)
        .into_iter()
        // Ordinary app windows: normal layer, not tiny, not dextr itself.
        .filter(|w| w.layer == 0 && w.pid != me && w.bounds.size.width >= 64.0 && w.bounds.size.height >= 48.0)
        .map(|w| ShareableWindow {
            id: w.id as u64,
            title: if w.title.is_empty() { w.owner.clone() } else { w.title },
            app: w.owner,
        })
        .collect();
    Ok(Some(list))
}

/// Captures window `id` into a BGRA frame.
fn grab(id: u32) -> Option<CapturedFrame> {
    unsafe {
        let image = CGWindowListCreateImage(
            CGRectNull,
            WINDOW_LIST_INCLUDING_WINDOW,
            id,
            WINDOW_IMAGE_IGNORE_FRAMING | WINDOW_IMAGE_BEST_RESOLUTION,
        );
        if image.is_null() {
            return None;
        }
        let frame = (|| {
            let (w, h) = (CGImageGetWidth(image), CGImageGetHeight(image));
            if w == 0 || h == 0 || CGImageGetBitsPerPixel(image) != 32 {
                return None;
            }
            let stride = CGImageGetBytesPerRow(image);
            let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
            if data.is_null() {
                return None;
            }
            let len = CFDataGetLength(data) as usize;
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), len);
            let mut out = bytes[..len.min(stride * h)].to_vec();
            CFRelease(data);
            if out.len() < stride * (h - 1) + w * 4 {
                return None;
            }
            // Little-endian 32-bit (the usual case) is already BGRA in
            // memory; big-endian ARGB needs reordering.
            if CGImageGetBitmapInfo(image) & BITMAP_BYTE_ORDER_MASK != BITMAP_BYTE_ORDER_32_LITTLE {
                for px in out.as_chunks_mut::<4>().0 {
                    *px = [px[3], px[2], px[1], px[0]];
                }
            }
            Some(CapturedFrame { width: w as u32, height: h as u32, stride, data: out })
        })();
        CFRelease(image);
        frame
    }
}

fn geometry(info: &Info) -> WindowGeometry {
    let b = info.bounds;
    WindowGeometry { x: b.origin.x as f32, y: b.origin.y as f32, width: b.size.width as f32, height: b.size.height as f32 }
}

/// One window, by ID, if it still exists.
pub(super) fn window_info(id: u32) -> Option<Info> {
    window_list(Some(id)).into_iter().find(|w| w.id == id)
}

/// The topmost ordinary app window at `p` (not dextr's).
pub(super) fn window_at(p: CGPoint) -> Option<Info> {
    let me = std::process::id() as i64;
    window_list(None).into_iter().find(|w| {
        let b = w.bounds;
        w.layer == 0
            && w.pid != me
            && p.x >= b.origin.x
            && p.x < b.origin.x + b.size.width
            && p.y >= b.origin.y
            && p.y < b.origin.y + b.size.height
    })
}

impl Info {
    pub(super) fn origin(&self) -> CGPoint {
        self.bounds.origin
    }

    pub(super) fn is_app_window(&self) -> bool {
        self.layer == 0 && self.pid != std::process::id() as i64
    }
}

pub(crate) async fn place_own_window(_title: &str, _x: f32, _y: f32) -> bool {
    // macOS lets the app place its windows itself.
    false
}

// ── Parking ─────────────────────────────────────────────────────────────

/// Another app's window, through the Accessibility API.
struct AxWindow(AXUIElementRef);

// SAFETY: AX elements can be used from any thread.
unsafe impl Send for AxWindow {}

impl Drop for AxWindow {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

impl AxWindow {
    fn find(pid: i64, id: u32) -> Option<AxWindow> {
        unsafe {
            let app = AXUIElementCreateApplication(pid as i32);
            if app.is_null() {
                return None;
            }
            let attr = cfstr(c"AXWindows");
            let mut windows: CFTypeRef = std::ptr::null();
            let found = if AXUIElementCopyAttributeValue(app, attr, &mut windows) == 0 && !windows.is_null() {
                let found = (0..CFArrayGetCount(windows)).find_map(|i| {
                    let w = CFArrayGetValueAtIndex(windows, i);
                    let mut wid = 0u32;
                    (_AXUIElementGetWindow(w, &mut wid) == 0 && wid == id).then(|| AxWindow(CFRetain(w)))
                });
                CFRelease(windows);
                found
            } else {
                None
            };
            CFRelease(attr);
            CFRelease(app);
            found
        }
    }

    fn position(&self) -> Option<CGPoint> {
        unsafe {
            let attr = cfstr(c"AXPosition");
            let mut value: CFTypeRef = std::ptr::null();
            let mut p = CGPoint::default();
            let ok = AXUIElementCopyAttributeValue(self.0, attr, &mut value) == 0
                && !value.is_null()
                && AXValueGetValue(value, AX_VALUE_CGPOINT, &mut p as *mut CGPoint as *mut c_void);
            if !value.is_null() {
                CFRelease(value);
            }
            CFRelease(attr);
            ok.then_some(p)
        }
    }

    fn set_position(&self, p: CGPoint) -> bool {
        unsafe {
            let attr = cfstr(c"AXPosition");
            let value = AXValueCreate(AX_VALUE_CGPOINT, &p as *const CGPoint as *const c_void);
            let ok = !value.is_null() && AXUIElementSetAttributeValue(self.0, attr, value) == 0;
            if !value.is_null() {
                CFRelease(value);
            }
            CFRelease(attr);
            ok
        }
    }
}

/// How long a parked window stays visible after input from the other device.
const UNPARK_FOR: Duration = Duration::from_millis(2500);

/// A window moved out of sight while it's shown on the other device. It is
/// brought back briefly when input for it arrives (injected clicks need it
/// on screen), and for good when sharing stops.
struct Park {
    ax: AxWindow,
    /// Where it belongs on this Mac.
    home: CGPoint,
    size: CGSize,
    parked: bool,
    last_input: Instant,
}

impl Park {
    /// The bottom-right corner of all displays, where only a sliver of the
    /// window stays on screen.
    fn spot() -> CGPoint {
        let screens = super::screens();
        let right = screens.iter().map(|r| r.right()).fold(0.0f32, f32::max);
        let bottom = screens.iter().map(|r| r.bottom()).fold(0.0f32, f32::max);
        CGPoint { x: right as f64 - 1.0, y: bottom as f64 - 1.0 }
    }

    fn publish(&self, at: CGPoint, geometry: &watch::Sender<Option<WindowGeometry>>) {
        geometry.send_replace(Some(WindowGeometry {
            x: at.x as f32,
            y: at.y as f32,
            width: self.size.width as f32,
            height: self.size.height as f32,
        }));
    }

    fn park(&mut self, geometry: &watch::Sender<Option<WindowGeometry>>) {
        if !self.parked
            && let Some(p) = self.ax.position()
        {
            self.home = p; // it may have been moved while visible
        }
        let spot = Park::spot();
        if self.ax.set_position(spot) {
            self.parked = true;
            self.publish(spot, geometry);
        }
    }

    fn unpark(&mut self, geometry: &watch::Sender<Option<WindowGeometry>>) {
        if self.parked && self.ax.set_position(self.home) {
            self.parked = false;
            self.publish(self.home, geometry);
        }
    }
}

pub(crate) async fn capture_window(id: Option<u64>, park: bool) -> anyhow::Result<WindowCapture> {
    let id = id.ok_or_else(|| anyhow::anyhow!("choose a window to share"))? as u32;
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        unsafe { CGRequestScreenCaptureAccess() };
        anyhow::bail!(PERMISSION);
    }
    let info = window_info(id).ok_or_else(|| anyhow::anyhow!("that window is gone"))?;

    let (frames_tx, frames) = watch::channel(None);
    let (geometry_tx, geometry_rx) = watch::channel(Some(geometry(&info)));
    let geometry_tx = std::sync::Arc::new(geometry_tx);

    // ScreenCaptureKit, or the older window image API before macOS 12.3.
    let sck = match super::sck::Capture::start(id, frames_tx.clone()) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("ScreenCaptureKit capture failed ({e:#}); using the older window capture");
            let first = grab(id).ok_or_else(|| anyhow::anyhow!("couldn't capture that window ({e:#})"))?;
            frames_tx.send_replace(Some(Arc::new(first)));
            None
        }
    };

    let parking = std::sync::Arc::new(std::sync::Mutex::new(None::<Park>));
    if park {
        match AxWindow::find(info.pid, id) {
            Some(ax) => {
                let mut p = Park { ax, home: info.origin(), size: info.bounds.size, parked: false, last_input: Instant::now() };
                p.park(&geometry_tx);
                *parking.lock().unwrap() = Some(p);
            }
            None => tracing::warn!("can't move that window out of sight (Accessibility allowed?)"),
        }
    }
    let input = {
        let (parking, geometry_tx) = (parking.clone(), geometry_tx.clone());
        InputHandle::new(move || {
            if let Some(p) = parking.lock().unwrap().as_mut() {
                p.last_input = Instant::now();
                p.unpark(&geometry_tx);
            }
        })
    };

    let (stop_tx, mut stop) = oneshot::channel::<()>();
    let (ended_tx, ended) = oneshot::channel();
    std::thread::Builder::new().name("dextr-window".into()).spawn(move || {
        let mut misses = 0;
        let mut next_info = Instant::now();
        let mut size = info.bounds.size;
        let reason = loop {
            match stop.try_recv() {
                Ok(()) | Err(oneshot::error::TryRecvError::Closed) => break "stopped",
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            let started = Instant::now();
            if started >= next_info {
                next_info = started + Duration::from_millis(250);
                let Some(w) = window_info(id) else { break "the window closed" };
                if (w.bounds.size.width, w.bounds.size.height) != (size.width, size.height) {
                    size = w.bounds.size;
                    if let Some(c) = &sck {
                        c.resize(size.width, size.height);
                    }
                }
                let mut parking = parking.lock().unwrap();
                match parking.as_mut() {
                    Some(p) => {
                        p.size = size;
                        if !p.parked && p.last_input.elapsed() >= UNPARK_FOR {
                            p.park(&geometry_tx);
                        }
                    }
                    None => {
                        geometry_tx.send_if_modified(|g| {
                            let new = Some(geometry(&w));
                            let changed = *g != new;
                            *g = new;
                            changed
                        });
                    }
                }
            }
            if sck.is_none() {
                match grab(id) {
                    Some(frame) => {
                        misses = 0;
                        frames_tx.send_replace(Some(Arc::new(frame)));
                    }
                    // Minimized or on another Space: keep the last frame.
                    None => {
                        misses += 1;
                        if misses > 300 {
                            break "the window can't be captured anymore";
                        }
                    }
                }
                std::thread::sleep(INTERVAL.saturating_sub(started.elapsed()));
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        drop(sck);
        // Put the window back where it belongs.
        if let Some(mut p) = parking.lock().unwrap().take() {
            p.unpark(&geometry_tx);
        }
        drop(frames_tx);
        let _ = ended_tx.send(reason.to_string());
    })?;

    let title = if info.title.is_empty() { info.owner.clone() } else { info.title };
    Ok(WindowCapture::with_stop(title, info.owner, frames, geometry_rx, ended, stop_tx, input))
}
