//! A minimal PipeWire video consumer, loaded at runtime.
//!
//! `libpipewire-0.3.so.0` ships with every PipeWire desktop, but its headers
//! (and the clang toolchain the usual bindings need) often don't. So the few
//! functions used here are looked up with `dlopen`, and the one SPA POD we
//! send (the formats we accept) is serialized by hand.
//!
//! Frames arrive in shared memory (we don't offer DMA-BUF modifiers), are
//! copied once into BGRA, and are published through a `watch` channel, so a
//! slow consumer simply sees the latest frame.

use std::{
    ffi::{CString, c_char, c_int, c_void},
    os::fd::{IntoRawFd, OwnedFd},
    ptr,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context as _, bail};
use libloading::Library;
use tokio::sync::{oneshot, watch};

use crate::window::CapturedFrame;

// ── SPA constants ────────────────────────────────────────────────────────

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;

const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_PARAM_BUFFERS: u32 = 5;
const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;

const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;

const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;
const SPA_CHOICE_FLAGS: u32 = 4;

const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;

const SPA_DATA_MEM_PTR: u32 = 1;
const SPA_DATA_MEM_FD: u32 = 2;

const PW_DIRECTION_INPUT: u32 = 0;
const PW_ID_ANY: u32 = 0xffff_ffff;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;

const PW_STREAM_STATE_ERROR: c_int = -1;
const PW_STREAM_STATE_UNCONNECTED: c_int = 0;
const PW_STREAM_STATE_STREAMING: c_int = 3;

// ── POD writing ──────────────────────────────────────────────────────────

/// Writes SPA PODs: 8-byte headers (size, type), bodies padded to 8 bytes.
#[derive(Default)]
struct Pod(Vec<u8>);

impl Pod {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_ne_bytes());
    }

    fn pad(&mut self) {
        while !self.0.len().is_multiple_of(8) {
            self.0.push(0);
        }
    }

    /// A POD whose body is written by `body`.
    fn pod(&mut self, kind: u32, body: impl FnOnce(&mut Pod)) {
        let start = self.0.len();
        self.u32(0);
        self.u32(kind);
        body(self);
        let size = (self.0.len() - start - 8) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_ne_bytes());
        self.pad();
    }

    fn id(&mut self, v: u32) {
        self.pod(SPA_TYPE_ID, |p| p.u32(v));
    }

    /// A choice of `values`, each `child_size` bytes of `child_type`.
    fn choice(&mut self, choice: u32, child_type: u32, child_size: u32, values: &[&[u32]]) {
        self.pod(SPA_TYPE_CHOICE, |p| {
            p.u32(choice);
            p.u32(0);
            p.u32(child_size);
            p.u32(child_type);
            for v in values {
                for &x in *v {
                    p.u32(x);
                }
            }
        });
    }

    fn object(&mut self, kind: u32, id: u32, props: impl FnOnce(&mut Pod)) {
        self.pod(SPA_TYPE_OBJECT, |p| {
            p.u32(kind);
            p.u32(id);
            props(p);
        });
    }

    fn prop(&mut self, key: u32, value: impl FnOnce(&mut Pod)) {
        self.u32(key);
        self.u32(0);
        value(self);
    }
}

/// The raw video formats we accept: 32-bit RGB in any order, any size.
fn enum_format() -> Pod {
    let mut p = Pod::default();
    p.object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT, |p| {
        p.prop(SPA_FORMAT_MEDIA_TYPE, |p| p.id(SPA_MEDIA_TYPE_VIDEO));
        p.prop(SPA_FORMAT_MEDIA_SUBTYPE, |p| p.id(SPA_MEDIA_SUBTYPE_RAW));
        p.prop(SPA_FORMAT_VIDEO_FORMAT, |p| {
            p.choice(
                SPA_CHOICE_ENUM,
                SPA_TYPE_ID,
                4,
                &[&[VIDEO_BGRX], &[VIDEO_BGRX], &[VIDEO_BGRA], &[VIDEO_RGBX], &[VIDEO_RGBA]],
            )
        });
        p.prop(SPA_FORMAT_VIDEO_SIZE, |p| {
            p.choice(SPA_CHOICE_RANGE, SPA_TYPE_RECTANGLE, 8, &[&[1920, 1080], &[1, 1], &[16384, 16384]])
        });
        p.prop(SPA_FORMAT_VIDEO_FRAMERATE, |p| {
            p.choice(SPA_CHOICE_RANGE, SPA_TYPE_FRACTION, 8, &[&[60, 1], &[0, 1], &[1000, 1]])
        });
    });
    p
}

/// Asks for buffers we can read directly: plain or shared memory.
fn buffers_param() -> Pod {
    let mut p = Pod::default();
    p.object(SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS, |p| {
        p.prop(SPA_PARAM_BUFFERS_DATA_TYPE, |p| {
            p.choice(SPA_CHOICE_FLAGS, SPA_TYPE_INT, 4, &[&[(1 << SPA_DATA_MEM_PTR) | (1 << SPA_DATA_MEM_FD)]])
        });
    });
    p
}

/// Reads the negotiated video format and size from a Format object.
fn parse_format(pod: *const u8) -> Option<(u32, u32, u32)> {
    // SAFETY: PipeWire passes a valid POD; we read only within its size.
    unsafe {
        let size = ptr::read_unaligned(pod as *const u32) as usize;
        let kind = ptr::read_unaligned(pod.add(4) as *const u32);
        if kind != SPA_TYPE_OBJECT || size < 8 {
            return None;
        }
        let body = std::slice::from_raw_parts(pod.add(8), size);
        let word = |at: usize| body.get(at..at + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()));
        let (mut format, mut dims) = (None, None);
        let mut at = 8; // object type and id
        while at + 16 <= size {
            let key = word(at)?;
            let vsize = word(at + 8)? as usize;
            let (mut vtype, mut value) = (word(at + 12)?, at + 16);
            // A negotiated value may still be wrapped in a one-value choice.
            if vtype == SPA_TYPE_CHOICE {
                vtype = word(value + 12)?;
                value += 16;
            }
            match (key, vtype) {
                (SPA_FORMAT_VIDEO_FORMAT, SPA_TYPE_ID) => format = word(value),
                (SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE) => dims = Some((word(value)?, word(value + 4)?)),
                _ => {}
            }
            at += 16 + vsize.next_multiple_of(8);
        }
        let (w, h) = dims?;
        Some((format?, w, h))
    }
}

// ── Library ──────────────────────────────────────────────────────────────

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
}

type Callback = Option<unsafe extern "C" fn()>;

#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Callback,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Callback,
    io_changed: Callback,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const u8)>,
    add_buffer: Callback,
    remove_buffer: Callback,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Callback,
    command: Callback,
    trigger_done: Callback,
}

struct Api {
    _lib: Library,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    context_destroy: unsafe extern "C" fn(*mut c_void),
    core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener: unsafe extern "C" fn(*mut c_void, *mut c_void, *const StreamEvents, *mut c_void),
    stream_connect: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const u8, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const u8, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
}

fn api() -> anyhow::Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("PipeWire is unavailable: {e}"))
}

fn load() -> anyhow::Result<Api> {
    // SAFETY: loading the system PipeWire library; the signatures below match
    // its stable public API.
    unsafe {
        let lib = Library::new("libpipewire-0.3.so.0").context("cannot load libpipewire-0.3.so.0")?;
        macro_rules! sym {
            ($name:literal) => {
                *lib.get(concat!($name, "\0").as_bytes()).context($name)?
            };
        }
        let api = Api {
            init: sym!("pw_init"),
            thread_loop_new: sym!("pw_thread_loop_new"),
            thread_loop_get_loop: sym!("pw_thread_loop_get_loop"),
            thread_loop_start: sym!("pw_thread_loop_start"),
            thread_loop_stop: sym!("pw_thread_loop_stop"),
            thread_loop_destroy: sym!("pw_thread_loop_destroy"),
            thread_loop_lock: sym!("pw_thread_loop_lock"),
            thread_loop_unlock: sym!("pw_thread_loop_unlock"),
            context_new: sym!("pw_context_new"),
            context_connect_fd: sym!("pw_context_connect_fd"),
            context_destroy: sym!("pw_context_destroy"),
            core_disconnect: sym!("pw_core_disconnect"),
            properties_new_string: sym!("pw_properties_new_string"),
            stream_new: sym!("pw_stream_new"),
            stream_add_listener: sym!("pw_stream_add_listener"),
            stream_connect: sym!("pw_stream_connect"),
            stream_update_params: sym!("pw_stream_update_params"),
            stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: sym!("pw_stream_queue_buffer"),
            stream_destroy: sym!("pw_stream_destroy"),
            _lib: lib,
        };
        (api.init)(ptr::null_mut(), ptr::null_mut());
        Ok(api)
    }
}

// ── Stream ───────────────────────────────────────────────────────────────

/// Shared with the PipeWire thread through the listener's data pointer.
struct State {
    stream: *mut c_void,
    format: Option<(u32, u32, u32)>,
    frames: watch::Sender<Option<Arc<CapturedFrame>>>,
    ended: Mutex<Option<oneshot::Sender<String>>>,
    warned_unreadable: bool,
}

impl State {
    fn end(&self, reason: String) {
        if let Some(tx) = self.ended.lock().unwrap().take() {
            let _ = tx.send(reason);
        }
    }
}

const EVENTS: StreamEvents = StreamEvents {
    version: 2,
    destroy: None,
    state_changed: Some(on_state_changed),
    control_info: None,
    io_changed: None,
    param_changed: Some(on_param_changed),
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

unsafe extern "C" fn on_state_changed(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    // SAFETY: `data` is the `State` owned by the `Capture` that registered us.
    let st = unsafe { &*(data as *const State) };
    match state {
        PW_STREAM_STATE_ERROR => {
            let msg = if error.is_null() {
                "stream error".into()
            } else {
                unsafe { std::ffi::CStr::from_ptr(error) }.to_string_lossy().into_owned()
            };
            st.end(format!("PipeWire: {msg}"));
        }
        PW_STREAM_STATE_UNCONNECTED => st.end("the window was closed".into()),
        PW_STREAM_STATE_STREAMING => tracing::debug!("window stream started"),
        _ => {}
    }
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const u8) {
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    // SAFETY: as above; this runs on the PipeWire thread, the only writer.
    let st = unsafe { &mut *(data as *mut State) };
    st.format = parse_format(param);
    tracing::debug!("window stream format {:?}", st.format);
    let buffers = buffers_param();
    let mut params = [buffers.0.as_ptr()];
    if let Ok(api) = api() {
        unsafe { (api.stream_update_params)(st.stream, params.as_mut_ptr(), 1) };
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    // SAFETY: as above.
    let st = unsafe { &mut *(data as *mut State) };
    let Ok(api) = api() else { return };
    unsafe {
        // Only the newest buffer matters.
        let mut newest: *mut PwBuffer = ptr::null_mut();
        loop {
            let b = (api.stream_dequeue_buffer)(st.stream);
            if b.is_null() {
                break;
            }
            if !newest.is_null() {
                (api.stream_queue_buffer)(st.stream, newest);
            }
            newest = b;
        }
        if newest.is_null() {
            return;
        }
        if let Some(frame) = st.copy(&*newest) {
            st.frames.send_replace(Some(Arc::new(frame)));
        }
        (api.stream_queue_buffer)(st.stream, newest);
    }
}

impl State {
    /// Copies a buffer into a BGRA frame.
    unsafe fn copy(&mut self, b: &PwBuffer) -> Option<CapturedFrame> {
        let (format, w, h) = self.format?;
        let buffer = unsafe { b.buffer.as_ref()? };
        if buffer.n_datas == 0 {
            return None;
        }
        let d = unsafe { &*buffer.datas };
        let chunk = unsafe { d.chunk.as_ref()? };
        if d.data.is_null() {
            if !self.warned_unreadable {
                self.warned_unreadable = true;
                tracing::warn!("window stream sent buffers of type {} that can't be read", d.kind);
            }
            return None;
        }
        if chunk.size == 0 {
            return None; // no new content (for example, only the cursor moved)
        }
        let row = w as usize * 4;
        let stride = if chunk.stride > 0 { chunk.stride as usize } else { row };
        let offset = chunk.offset as usize;
        let needed = offset + stride * (h as usize - 1) + row;
        if stride < row || needed > d.maxsize as usize {
            return None;
        }
        let src = unsafe { std::slice::from_raw_parts((d.data as *const u8).add(offset), needed - offset) };
        let mut out = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            out.extend_from_slice(&src[y * stride..y * stride + row]);
        }
        if matches!(format, VIDEO_RGBX | VIDEO_RGBA) {
            for px in out.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
        }
        Some(CapturedFrame { width: w, height: h, stride: row, data: out })
    }
}

/// A running capture of one PipeWire video node. Dropping it stops it.
pub struct Capture {
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    state: *mut State,
    _hook: Box<[usize; 8]>,
}

// SAFETY: the PipeWire objects are only touched with the thread loop locked.
unsafe impl Send for Capture {}

impl Capture {
    /// Connects to the PipeWire remote `fd` (from the ScreenCast portal) and
    /// starts receiving `node`.
    pub fn start(
        fd: OwnedFd,
        node: u32,
        frames: watch::Sender<Option<Arc<CapturedFrame>>>,
        ended: oneshot::Sender<String>,
    ) -> anyhow::Result<Capture> {
        Self::start_with(fd, node, "", frames, ended)
    }

    /// Like [`start`](Self::start), with extra stream properties.
    fn start_with(
        fd: OwnedFd,
        node: u32,
        extra_props: &str,
        frames: watch::Sender<Option<Arc<CapturedFrame>>>,
        ended: oneshot::Sender<String>,
    ) -> anyhow::Result<Capture> {
        let api = api()?;
        unsafe {
            let name = CString::new("dextr-window").unwrap();
            let thread_loop = (api.thread_loop_new)(name.as_ptr(), ptr::null());
            if thread_loop.is_null() {
                bail!("cannot create a PipeWire loop");
            }
            let context = (api.context_new)((api.thread_loop_get_loop)(thread_loop), ptr::null_mut(), 0);
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("cannot create a PipeWire context");
            }
            if (api.thread_loop_start)(thread_loop) < 0 {
                (api.context_destroy)(context);
                (api.thread_loop_destroy)(thread_loop);
                bail!("cannot start the PipeWire loop");
            }
            (api.thread_loop_lock)(thread_loop);

            let mut capture = Capture {
                thread_loop,
                context,
                core: ptr::null_mut(),
                stream: ptr::null_mut(),
                state: ptr::null_mut(),
                _hook: Box::new([0; 8]),
            };
            let result = (|| {
                capture.core = (api.context_connect_fd)(context, fd.into_raw_fd(), ptr::null_mut(), 0);
                if capture.core.is_null() {
                    bail!("cannot connect to PipeWire");
                }
                let props = CString::new(format!("media.type=Video media.category=Capture media.role=Screen {extra_props}"))?;
                capture.stream = (api.stream_new)(capture.core, name.as_ptr(), (api.properties_new_string)(props.as_ptr()));
                if capture.stream.is_null() {
                    bail!("cannot create a PipeWire stream");
                }
                capture.state = Box::into_raw(Box::new(State {
                    stream: capture.stream,
                    format: None,
                    frames,
                    ended: Mutex::new(Some(ended)),
                    warned_unreadable: false,
                }));
                (api.stream_add_listener)(
                    capture.stream,
                    capture._hook.as_mut_ptr() as *mut c_void,
                    &EVENTS,
                    capture.state as *mut c_void,
                );
                let format = enum_format();
                let mut params = [format.0.as_ptr()];
                let target = if node == 0 { PW_ID_ANY } else { node };
                let rc = (api.stream_connect)(
                    capture.stream,
                    PW_DIRECTION_INPUT,
                    target,
                    PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                    params.as_mut_ptr(),
                    1,
                );
                if rc < 0 {
                    bail!("cannot connect the PipeWire stream ({rc})");
                }
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result.map(|()| capture)
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let Ok(api) = api() else { return };
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pod_round_trips_through_the_parser() {
        let mut p = Pod::default();
        p.object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT, |p| {
            p.prop(SPA_FORMAT_MEDIA_TYPE, |p| p.id(SPA_MEDIA_TYPE_VIDEO));
            p.prop(SPA_FORMAT_VIDEO_FORMAT, |p| p.id(VIDEO_BGRX));
            p.prop(SPA_FORMAT_VIDEO_SIZE, |p| p.pod(SPA_TYPE_RECTANGLE, |p| { p.u32(1280); p.u32(720); }));
        });
        assert_eq!(parse_format(p.0.as_ptr()), Some((VIDEO_BGRX, 1280, 720)));
        assert_eq!(p.0.len() % 8, 0);
        assert_eq!(enum_format().0.len() % 8, 0);
    }

    /// Captures a GStreamer test pattern through the user's PipeWire daemon.
    /// Run with `cargo test -p dextr-platform -- --ignored pipewire`.
    #[test]
    #[ignore = "needs a running PipeWire and gst-launch-1.0"]
    fn captures_a_real_pipewire_stream() {
        let name = format!("dextr-test-{}", std::process::id());
        let mut gst = std::process::Command::new("gst-launch-1.0")
            .args(["-q", "videotestsrc", "pattern=smpte", "!", "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1", "!", "pipewiresink", "mode=provide"])
            .arg(format!("stream-properties=props,node.name={name},media.class=Video/Source"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("gst-launch-1.0");
        std::thread::sleep(std::time::Duration::from_millis(1500));

        let socket = std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap()).join("pipewire-0");
        let fd: OwnedFd = std::os::unix::net::UnixStream::connect(socket).unwrap().into();
        let (tx, mut rx) = watch::channel(None);
        let (ended, _ended_rx) = oneshot::channel();
        let capture = Capture::start_with(fd, 0, &format!("target.object={name}"), tx, ended);
        let result = (|| {
            let _capture = capture?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(f) = rx.borrow_and_update().clone() {
                    return anyhow::Ok((f.width, f.height, f.data[..4].to_vec()));
                }
                anyhow::ensure!(std::time::Instant::now() < deadline, "no frame within 5 s");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })();
        let _ = gst.kill();
        let _ = gst.wait();
        let (w, h, first) = result.unwrap();
        assert_eq!((w, h), (320, 240));
        // SMPTE bars start with light grey (BGRx ≈ c0 c0 c0).
        assert!(first[0] > 150 && first[1] > 150 && first[2] > 150, "{first:?}");
    }
}
