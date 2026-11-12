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
use crate::window::{CapturedFrame, ShareableWindow, WindowCapture, WindowGeometry};

const PERMISSION: &str = "macOS didn't allow dextr to see other windows. Allow it under System Settings → Privacy & Security → \
Screen Recording (for dextr, or for the terminal app that starts it), then restart dextr.";

/// How often the window is captured. Unchanged pixels cost nothing on the
/// network; the encoder sends only what changed.
const INTERVAL: Duration = Duration::from_millis(33);

/// Info about one window from the window list.
struct Info {
    id: u32,
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

pub(crate) async fn capture_window(id: Option<u64>) -> anyhow::Result<WindowCapture> {
    let id = id.ok_or_else(|| anyhow::anyhow!("choose a window to share"))? as u32;
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        unsafe { CGRequestScreenCaptureAccess() };
        anyhow::bail!(PERMISSION);
    }
    let info = window_list(Some(id)).into_iter().find(|w| w.id == id).ok_or_else(|| anyhow::anyhow!("that window is gone"))?;
    let first = grab(id).ok_or_else(|| anyhow::anyhow!("couldn't capture that window"))?;

    let (frames_tx, frames) = watch::channel(Some(Arc::new(first)));
    let (geometry_tx, geometry_rx) = watch::channel(Some(geometry(&info)));
    let (stop_tx, mut stop) = oneshot::channel::<()>();
    let (ended_tx, ended) = oneshot::channel();

    std::thread::Builder::new().name("dextr-window".into()).spawn(move || {
        let mut misses = 0;
        let mut next_info = Instant::now();
        let reason = loop {
            match stop.try_recv() {
                Ok(()) | Err(oneshot::error::TryRecvError::Closed) => break "stopped",
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            let started = Instant::now();
            // Follow the window when it moves (a few times a second).
            if started >= next_info {
                next_info = started + Duration::from_millis(250);
                match window_list(Some(id)).into_iter().find(|w| w.id == id) {
                    Some(w) => {
                        geometry_tx.send_if_modified(|g| {
                            let new = Some(geometry(&w));
                            let changed = *g != new;
                            *g = new;
                            changed
                        });
                    }
                    None => break "the window closed",
                }
            }
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
        };
        let _ = ended_tx.send(reason.to_string());
    })?;

    let title = if info.title.is_empty() { info.owner.clone() } else { info.title };
    Ok(WindowCapture::with_stop(title, info.owner, frames, geometry_rx, ended, stop_tx))
}
