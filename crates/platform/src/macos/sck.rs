//! Single-window capture with ScreenCaptureKit (macOS 12.3+).
//!
//! `SCContentFilter(desktopIndependentWindow:)` captures one window on its
//! own: covered, moved to another display or parked off-screen, it keeps
//! coming, at its full Retina resolution, and frames arrive only when the
//! window changes.

use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::Context as _;
use block2::RcBlock;
use objc2::{AnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject, sel};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use tokio::sync::watch;

use crate::window::CapturedFrame;

type Frames = watch::Sender<Option<Arc<CapturedFrame>>>;

pub struct Ivars {
    frames: Frames,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Idle frames (nothing changed) carry no image.
            if let Some(frame) = unsafe { copy_frame(sample) } {
                self.ivars().frames.send_replace(Some(Arc::new(frame)));
            }
        }
    }
);

unsafe fn copy_frame(sample: &CMSampleBuffer) -> Option<CapturedFrame> {
    let buffer = unsafe { sample.image_buffer() }?;
    if CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA {
        return None;
    }
    if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let (w, h, stride) = (CVPixelBufferGetWidth(&buffer), CVPixelBufferGetHeight(&buffer), CVPixelBufferGetBytesPerRow(&buffer));
    let base = CVPixelBufferGetBaseAddress(&buffer) as *const u8;
    let frame = (!base.is_null() && w > 0 && h > 0 && stride >= w * 4).then(|| {
        let src = unsafe { std::slice::from_raw_parts(base, stride * h) };
        CapturedFrame { width: w as u32, height: h as u32, stride, data: src.to_vec() }
    });
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) };
    frame
}

/// Moves a value across threads. Used only to hand results out of
/// ScreenCaptureKit completion handlers.
struct Sendable<T>(T);
// SAFETY: the wrapped Objective-C objects are only used on one thread at a
// time; ScreenCaptureKit objects are thread-safe for these uses.
unsafe impl<T> Send for Sendable<T> {}

/// The shareable window with this ID.
fn find_window(id: u32) -> anyhow::Result<Retained<SCWindow>> {
    let (tx, rx) = mpsc::channel::<Sendable<Result<Option<Retained<SCWindow>>, String>>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(content) => {
                let windows = unsafe { content.windows() };
                Ok(windows.iter().find(|w| unsafe { w.windowID() } == id))
            }
            None => Err(unsafe { Retained::retain(error) }
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "no shareable content".into())),
        };
        let _ = tx.send(Sendable(result));
    });
    unsafe { SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, false, &handler) };
    let Sendable(result) = rx.recv_timeout(Duration::from_secs(5)).context("ScreenCaptureKit didn't answer")?;
    result.map_err(anyhow::Error::msg)?.context("ScreenCaptureKit doesn't list that window")
}

/// Waits for a completion handler that reports only an error.
fn wait(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(unsafe { Retained::retain(error) }.map(|e| e.localizedDescription().to_string()));
    });
    call(&handler);
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => anyhow::bail!(e),
        Err(_) => anyhow::bail!("ScreenCaptureKit didn't answer"),
    }
}

/// A running window capture. Dropping it stops the stream.
pub struct Capture {
    stream: Retained<SCStream>,
    config: Retained<SCStreamConfiguration>,
    _sink: Retained<FrameSink>,
    /// Pixels per point of the window's display.
    pub scale: f64,
}

// SAFETY: SCStream and its configuration are used from one thread at a time.
unsafe impl Send for Capture {}

impl Capture {
    pub fn start(id: u32, frames: Frames) -> anyhow::Result<Capture> {
        let window = find_window(id)?;
        unsafe {
            let filter = SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window);
            // pointPixelScale is macOS 14+; assume Retina before that.
            let scale = if filter.respondsToSelector(sel!(pointPixelScale)) { filter.pointPixelScale() as f64 } else { 2.0 };
            let frame = window.frame();

            let config = SCStreamConfiguration::new();
            config.setWidth((frame.size.width * scale).round().max(1.0) as usize);
            config.setHeight((frame.size.height * scale).round().max(1.0) as usize);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(false);
            config.setQueueDepth(4);
            config.setMinimumFrameInterval(CMTime::new(1, 120));
            if config.respondsToSelector(sel!(setIgnoreShadowsSingleWindow:)) {
                config.setIgnoreShadowsSingleWindow(true);
            }

            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None);
            let sink = FrameSink::alloc().set_ivars(Ivars { frames });
            let sink: Retained<FrameSink> = msg_send![super(sink), init];
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*sink), SCStreamOutputType::Screen, None)
                .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))?;
            wait(|h| stream.startCaptureWithCompletionHandler(Some(h))).context("starting the capture (Screen Recording allowed?)")?;
            Ok(Capture { stream, config, _sink: sink, scale })
        }
    }

    /// Follows a new window size, in points.
    pub fn resize(&self, width: f64, height: f64) {
        unsafe {
            self.config.setWidth((width * self.scale).round().max(1.0) as usize);
            self.config.setHeight((height * self.scale).round().max(1.0) as usize);
            self.stream.updateConfiguration_completionHandler(&self.config, None);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = wait(|h| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(h)) });
    }
}
