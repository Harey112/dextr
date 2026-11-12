//! The few CoreGraphics and CoreFoundation calls the backend needs.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void};

pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFStringRef = *const c_void;
pub type CFTypeRef = *const c_void;
pub type CGEventTapProxy = *mut c_void;
pub type CGDirectDisplayID = u32;
pub type CGEventType = u32;
pub type CGEventFlags = u64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

pub type CGEventTapCallBack =
    unsafe extern "C" fn(CGEventTapProxy, CGEventType, CGEventRef, *mut c_void) -> CGEventRef;

// Event types.
pub const LEFT_DOWN: CGEventType = 1;
pub const LEFT_UP: CGEventType = 2;
pub const RIGHT_DOWN: CGEventType = 3;
pub const RIGHT_UP: CGEventType = 4;
pub const MOUSE_MOVED: CGEventType = 5;
pub const LEFT_DRAGGED: CGEventType = 6;
pub const RIGHT_DRAGGED: CGEventType = 7;
pub const KEY_DOWN: CGEventType = 10;
pub const KEY_UP: CGEventType = 11;
pub const FLAGS_CHANGED: CGEventType = 12;
pub const SCROLL_WHEEL: CGEventType = 22;
pub const OTHER_DOWN: CGEventType = 25;
pub const OTHER_UP: CGEventType = 26;
pub const OTHER_DRAGGED: CGEventType = 27;
pub const TAP_DISABLED_BY_TIMEOUT: CGEventType = 0xFFFF_FFFE;
pub const TAP_DISABLED_BY_USER_INPUT: CGEventType = 0xFFFF_FFFF;

// Event fields.
pub const FIELD_CLICK_STATE: u32 = 1;
pub const FIELD_BUTTON_NUMBER: u32 = 3;
pub const FIELD_DELTA_X: u32 = 4;
pub const FIELD_DELTA_Y: u32 = 5;
pub const FIELD_AUTOREPEAT: u32 = 8;
pub const FIELD_KEYCODE: u32 = 9;
pub const FIELD_SCROLL_DELTA_1: u32 = 11;
pub const FIELD_SCROLL_DELTA_2: u32 = 12;
pub const FIELD_SOURCE_PID: u32 = 41;
/// The window under the pointer that will handle a mouse event.
pub const FIELD_WINDOW_UNDER_POINTER: u32 = 92;
pub const FIELD_SOURCE_USER_DATA: u32 = 42;
pub const FIELD_SCROLL_CONTINUOUS: u32 = 88;
pub const FIELD_SCROLL_POINT_DELTA_1: u32 = 96;
pub const FIELD_SCROLL_POINT_DELTA_2: u32 = 97;
pub const FIELD_SCROLL_MOMENTUM_PHASE: u32 = 123;
pub const MOMENTUM_PHASE_END: i64 = 3;

// Modifier flags: device-independent masks, then the left/right bits.
pub const FLAG_CAPS: u64 = 0x0001_0000;
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALT: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
pub const DEV_LCTRL: u64 = 0x0001;
pub const DEV_LSHIFT: u64 = 0x0002;
pub const DEV_RSHIFT: u64 = 0x0004;
pub const DEV_LCMD: u64 = 0x0008;
pub const DEV_RCMD: u64 = 0x0010;
pub const DEV_LALT: u64 = 0x0020;
pub const DEV_RALT: u64 = 0x0040;
pub const DEV_RCTRL: u64 = 0x2000;

pub const SESSION_EVENT_TAP: u32 = 1;
pub const HID_EVENT_TAP: u32 = 0;
pub const HEAD_INSERT: u32 = 0;
pub const TAP_OPTION_DEFAULT: u32 = 0;
pub const SOURCE_STATE_HID_SYSTEM: i32 = 1;
pub const SCROLL_UNIT_PIXEL: u32 = 0;
pub const SCROLL_UNIT_LINE: u32 = 1;
pub const STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
    pub fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    pub fn CGEventSetType(event: CGEventRef, kind: CGEventType);

    pub fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    pub fn CGEventSourceSetLocalEventsSuppressionInterval(source: CGEventSourceRef, seconds: f64);
    pub fn CGEventCreateMouseEvent(source: CGEventSourceRef, kind: CGEventType, at: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, keycode: u16, down: bool) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventPost(tap: u32, event: CGEventRef);

    pub fn CGWarpMouseCursorPosition(at: CGPoint) -> i32;
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    pub fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
    pub fn CGDisplayHideCursor(display: CGDirectDisplayID) -> i32;
    pub fn CGDisplayShowCursor(display: CGDirectDisplayID) -> i32;
    pub fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;

    pub fn CGPreflightPostEventAccess() -> bool;
    pub fn CGRequestPostEventAccess() -> bool;
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestListenEventAccess() -> bool;

    // Private, but stable for over a decade and used by every KVM tool:
    // lets a background app hide the cursor.
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(cid: i32, target: i32, key: CFStringRef, value: CFTypeRef) -> i32;
}

// ── Window capture ──────────────────────────────────────────────────────

pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CGImageRef = *const c_void;

pub const WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1 << 0;
pub const WINDOW_LIST_INCLUDING_WINDOW: u32 = 1 << 3;
pub const WINDOW_LIST_EXCLUDE_DESKTOP: u32 = 1 << 4;
pub const WINDOW_IMAGE_IGNORE_FRAMING: u32 = 1 << 0;
pub const WINDOW_IMAGE_BEST_RESOLUTION: u32 = 1 << 3;
pub const BITMAP_BYTE_ORDER_MASK: u32 = 0x7000;
pub const BITMAP_BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
pub const CF_NUMBER_SINT64: isize = 4;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub static CGRectNull: CGRect;
    pub static kCGWindowNumber: CFStringRef;
    pub static kCGWindowOwnerName: CFStringRef;
    pub static kCGWindowOwnerPID: CFStringRef;
    pub static kCGWindowName: CFStringRef;
    pub static kCGWindowBounds: CFStringRef;
    pub static kCGWindowLayer: CFStringRef;

    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    pub fn CGWindowListCreateImage(bounds: CGRect, option: u32, window: u32, image_option: u32) -> CGImageRef;
    pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
    pub fn CGImageGetWidth(image: CGImageRef) -> usize;
    pub fn CGImageGetHeight(image: CGImageRef) -> usize;
    pub fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
    pub fn CGImageGetBitsPerPixel(image: CGImageRef) -> usize;
    pub fn CGImageGetBitmapInfo(image: CGImageRef) -> u32;
    pub fn CGImageGetDataProvider(image: CGImageRef) -> *const c_void;
    pub fn CGDataProviderCopyData(provider: *const c_void) -> CFDataRef;
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFArrayGetCount(array: CFArrayRef) -> isize;
    pub fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFNumberGetValue(number: *const c_void, kind: isize, out: *mut c_void) -> bool;
    pub fn CFStringGetLength(s: CFStringRef) -> isize;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> bool;
    pub fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    pub fn CFDataGetLength(data: CFDataRef) -> isize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFRunLoopCommonModes: CFStringRef;
    pub static kCFBooleanTrue: CFTypeRef;
    pub fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: CFMachPortRef, order: isize) -> CFRunLoopSourceRef;
    pub fn CFMachPortInvalidate(port: CFMachPortRef);
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
}

// ── Accessibility (moving other apps' windows) ──────────────────────────

pub type AXUIElementRef = *const c_void;
pub const AX_VALUE_CGPOINT: u32 = 1;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(element: AXUIElementRef, attribute: CFStringRef, value: *mut CFTypeRef) -> i32;
    pub fn AXUIElementSetAttributeValue(element: AXUIElementRef, attribute: CFStringRef, value: CFTypeRef) -> i32;
    /// Private but used by every window manager: the CGWindowID of a window.
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, id: *mut u32) -> i32;
    pub fn AXValueCreate(kind: u32, value: *const c_void) -> CFTypeRef;
    pub fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
}

/// A CFString for `s`; release it with `CFRelease`.
pub fn cfstr(s: &std::ffi::CStr) -> CFStringRef {
    unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), STRING_ENCODING_UTF8) }
}

/// Allows hiding the cursor while dextr isn't the frontmost app.
pub fn allow_background_cursor_hiding() {
    unsafe {
        let key = CFStringCreateWithCString(std::ptr::null(), c"SetsCursorInBackground".as_ptr(), STRING_ENCODING_UTF8);
        if key.is_null() {
            return;
        }
        let cid = _CGSDefaultConnection();
        CGSSetConnectionProperty(cid, cid, key, kCFBooleanTrue);
        CFRelease(key);
    }
}
