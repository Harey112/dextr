//! Watching and setting the system clipboard.
//!
//! A thread watches the clipboard and reports new text or images. Images
//! travel as PNG. Contents set from the other device aren't reported back,
//! so copies never bounce between the two devices.
//!
//! * macOS: the pasteboard's change counter is checked a few times a second
//!   (nearly free); the contents are read only when it changes.
//! * Linux: text is compared a few times a second. Images are compared as
//!   the raw PNG bytes the source offers, about once a second, and only
//!   decoded when they actually change. On Wayland this uses the data-control
//!   protocol (KDE Plasma, wlroots), so it works in the background.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::mpsc as std_mpsc,
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::time::Instant;

use tokio::sync::mpsc;

/// What's on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipContent {
    Text(String),
    /// A PNG image.
    Png(Vec<u8>),
}

impl ClipContent {
    pub fn len(&self) -> usize {
        match self {
            ClipContent::Text(t) => t.len(),
            ClipContent::Png(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

}

fn hash(value: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// Identifies clipboard contents. Images are compared by their pixels: the
/// same picture is encoded differently by each app and OS, and comparing
/// bytes would bounce it between the devices forever.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sig {
    Text(u64),
    Image(u64),
}

fn pixels_sig(image: &image::RgbaImage) -> Sig {
    Sig::Image(hash((image.dimensions(), image.as_raw())))
}

/// Sets the clipboard. Dropping every clone stops the watcher.
#[derive(Clone)]
pub struct ClipboardHandle {
    set: std_mpsc::Sender<ClipContent>,
}

impl ClipboardHandle {
    pub fn set(&self, content: ClipContent) {
        let _ = self.set.send(content);
    }
}

const TEXT_EVERY: Duration = Duration::from_millis(300);
#[cfg(target_os = "linux")]
const IMAGE_EVERY: Duration = Duration::from_secs(1);

/// Starts watching the clipboard; new local contents arrive on `changes`.
pub fn start_clipboard(changes: mpsc::UnboundedSender<ClipContent>) -> anyhow::Result<ClipboardHandle> {
    let (set_tx, set_rx) = std_mpsc::channel::<ClipContent>();
    std::thread::Builder::new().name("dextr-clipboard".into()).spawn(move || {
        let mut board = match arboard::Clipboard::new() {
            Ok(b) => b,
            Err(e) => return tracing::warn!("clipboard unavailable: {e}"),
        };
        let mut watch = Watch::new(&mut board);
        loop {
            match set_rx.recv_timeout(TEXT_EVERY) {
                Ok(content) => watch.apply(&mut board, content),
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(content) = watch.poll(&mut board)
                        && changes.send(content).is_err()
                    {
                        break;
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })?;
    Ok(ClipboardHandle { set: set_tx })
}

/// Tracks what the clipboard held last, to report only real changes.
struct Watch {
    /// The last contents seen or set.
    last: Option<Sig>,
    /// The PNG bytes last looked at, to skip decoding unchanged images.
    last_png: Option<u64>,
    #[cfg(target_os = "macos")]
    change_count: isize,
    #[cfg(target_os = "linux")]
    next_image: Instant,
}

impl Watch {
    fn new(board: &mut arboard::Clipboard) -> Self {
        let mut w = Watch {
            last: None,
            last_png: None,
            #[cfg(target_os = "macos")]
            change_count: macos::change_count(),
            #[cfg(target_os = "linux")]
            next_image: Instant::now(),
        };
        // Whatever is on the clipboard at start isn't a new copy.
        if let Some(content) = read(board, true) {
            w.is_new(&content);
        }
        w
    }

    /// Puts the other device's contents on our clipboard.
    fn apply(&mut self, board: &mut arboard::Clipboard, content: ClipContent) {
        let result = match content {
            ClipContent::Text(text) => {
                self.last = Some(Sig::Text(hash(&text)));
                board.set_text(text)
            }
            ClipContent::Png(png) => match decode_png(&png) {
                Some(image) => {
                    self.last = Some(pixels_sig(&image));
                    self.last_png = None;
                    let (width, height) = image.dimensions();
                    board.set_image(arboard::ImageData { width: width as usize, height: height as usize, bytes: image.into_raw().into() })
                }
                None => return tracing::warn!("received an image that isn't a valid PNG"),
            },
        };
        if let Err(e) = result {
            tracing::warn!("cannot set the clipboard: {e}");
        }
        #[cfg(target_os = "macos")]
        {
            self.change_count = macos::change_count();
        }
    }

    /// New contents copied here since the last check, if any.
    fn poll(&mut self, board: &mut arboard::Clipboard) -> Option<ClipContent> {
        #[cfg(target_os = "macos")]
        {
            let count = macos::change_count();
            if count == self.change_count {
                return None;
            }
            self.change_count = count;
        }
        #[cfg(target_os = "linux")]
        let images = {
            let due = Instant::now() >= self.next_image;
            if due {
                self.next_image = Instant::now() + IMAGE_EVERY;
            }
            due
        };
        #[cfg(not(target_os = "linux"))]
        let images = true;

        let content = read(board, images)?;
        self.is_new(&content).then_some(content)
    }

    /// Whether `content` differs from the last contents; remembers it.
    fn is_new(&mut self, content: &ClipContent) -> bool {
        let sig = match content {
            ClipContent::Text(text) => Sig::Text(hash(text)),
            ClipContent::Png(png) => {
                // Same bytes as last time: same picture, no need to decode.
                let bytes = hash(png);
                if self.last_png == Some(bytes) {
                    return false;
                }
                self.last_png = Some(bytes);
                match decode_png(png) {
                    Some(image) => pixels_sig(&image),
                    None => return false,
                }
            }
        };
        if self.last == Some(sig) {
            return false;
        }
        self.last = Some(sig);
        true
    }
}

/// The clipboard's text, or (if `images`) its image as PNG.
fn read(board: &mut arboard::Clipboard, images: bool) -> Option<ClipContent> {
    if let Ok(text) = board.get_text()
        && !text.is_empty()
    {
        return Some(ClipContent::Text(text));
    }
    if !images {
        return None;
    }
    // On Wayland, take the PNG the source offers as is: no decoding just to
    // see whether it changed.
    #[cfg(target_os = "linux")]
    if let Some(png) = linux::png() {
        return Some(ClipContent::Png(png));
    }
    let image = board.get_image().ok()?;
    encode_png(&image).map(ClipContent::Png)
}

fn encode_png(image: &arboard::ImageData<'_>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    );
    image::ImageEncoder::write_image(encoder, &image.bytes, image.width as u32, image.height as u32, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

fn decode_png(png: &[u8]) -> Option<image::RgbaImage> {
    Some(image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read;

    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types};

    /// The clipboard's PNG, if it offers one (Wayland only).
    pub fn png() -> Option<Vec<u8>> {
        let types = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
        if !types.contains("image/png") {
            return None;
        }
        let (mut pipe, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific("image/png")).ok()?;
        let mut png = Vec::new();
        pipe.read_to_end(&mut png).ok()?;
        (!png.is_empty()).then_some(png)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    /// `NSPasteboard.generalPasteboard.changeCount`: bumped on every copy.
    pub fn change_count() -> isize {
        unsafe {
            let send_obj: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_int: unsafe extern "C" fn(*mut c_void, *const c_void) -> isize =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let class = objc_getClass(c"NSPasteboard".as_ptr());
            if class.is_null() {
                return 0;
            }
            let board = send_obj(class, sel_registerName(c"generalPasteboard".as_ptr()));
            if board.is_null() {
                return 0;
            }
            send_int(board, sel_registerName(c"changeCount".as_ptr()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch() -> Watch {
        Watch {
            last: None,
            last_png: None,
            #[cfg(target_os = "macos")]
            change_count: 0,
            #[cfg(target_os = "linux")]
            next_image: Instant::now(),
        }
    }

    fn png(image: &image::RgbaImage, compression: image::codecs::png::CompressionType) -> Vec<u8> {
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new_with_quality(&mut out, compression, image::codecs::png::FilterType::Adaptive);
        image::ImageEncoder::write_image(encoder, image.as_raw(), image.width(), image.height(), image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    #[test]
    fn the_same_picture_encoded_differently_is_not_a_new_copy() {
        let picture = image::RgbaImage::from_fn(40, 30, |x, y| image::Rgba([x as u8 * 6, y as u8 * 8, 99, 255]));
        let (fast, best) = (png(&picture, image::codecs::png::CompressionType::Fast), png(&picture, image::codecs::png::CompressionType::Best));
        assert_ne!(fast, best);

        let mut w = watch();
        assert!(w.is_new(&ClipContent::Png(fast.clone())));
        // Re-encoded by the other side: same pixels, so it doesn't go back.
        assert!(!w.is_new(&ClipContent::Png(best)));
        assert!(!w.is_new(&ClipContent::Png(fast)));
        assert!(w.is_new(&ClipContent::Text("hi".into())));
        assert!(!w.is_new(&ClipContent::Text("hi".into())));
        let other = image::RgbaImage::from_pixel(40, 30, image::Rgba([1, 2, 3, 255]));
        assert!(w.is_new(&ClipContent::Png(png(&other, image::codecs::png::CompressionType::Fast))));
    }
}
