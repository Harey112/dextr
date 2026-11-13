//! The app icon: the window icon, and where the OS shows it outside the
//! window (the Linux launcher and taskbar, the macOS Dock).

/// The desktop app ID: the Wayland app ID, the .desktop file and icon name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const APP_ID: &str = "dev.dextr.dextr";

const PNG_256: &[u8] = include_bytes!("../../../assets/icons/dextr-256.png");

/// The icon for dextr's window.
pub fn window_icon() -> Option<iced::window::Icon> {
    let image = image::load_from_memory_with_format(PNG_256, image::ImageFormat::Png).ok()?.into_rgba8();
    let (w, h) = image.dimensions();
    iced::window::icon::from_rgba(image.into_raw(), w, h).ok()
}

/// Makes the desktop show dextr's icon and name: on Wayland a window's icon
/// comes from the .desktop file matching its app ID, so install one (and the
/// icon) for the current user, and keep them pointing at this binary.
#[cfg(target_os = "linux")]
pub fn install_desktop_entry() {
    use std::path::PathBuf;

    const ICONS: &[(u32, &[u8])] = &[
        (16, include_bytes!("../../../assets/icons/dextr-16.png")),
        (24, include_bytes!("../../../assets/icons/dextr-24.png")),
        (32, include_bytes!("../../../assets/icons/dextr-32.png")),
        (48, include_bytes!("../../../assets/icons/dextr-48.png")),
        (64, include_bytes!("../../../assets/icons/dextr-64.png")),
        (128, include_bytes!("../../../assets/icons/dextr-128.png")),
        (256, PNG_256),
        (512, include_bytes!("../../../assets/icons/dextr-512.png")),
    ];

    let Some(data) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else { return };

    // Write only what differs, so this costs nothing on later starts.
    let write = |path: PathBuf, bytes: &[u8]| -> std::io::Result<bool> {
        if std::fs::read(&path).is_ok_and(|old| old == bytes) {
            return Ok(false);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        Ok(true)
    };

    let mut changed = false;
    for (size, png) in ICONS {
        let path = data.join(format!("icons/hicolor/{size}x{size}/apps/{APP_ID}.png"));
        changed |= write(path, png).unwrap_or(false);
    }
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=dextr\nGenericName=Mouse and keyboard sharing\n\
         Comment=Share one mouse, keyboard and clipboard between computers\nExec=\"{}\"\nIcon={APP_ID}\n\
         Terminal=false\nCategories=Utility;Network;\nStartupWMClass={APP_ID}\n",
        exe.display()
    );
    changed |= write(data.join(format!("applications/{APP_ID}.desktop")), entry.as_bytes()).unwrap_or(false);

    if changed {
        // Let the desktop pick up the new icon and entry.
        for tool in ["kbuildsycoca6", "update-desktop-database"] {
            let _ = std::process::Command::new(tool)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

/// Shows dextr's icon in the Dock (an unbundled binary otherwise gets a
/// generic one). Call on the main thread once the app is running.
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use std::ffi::{c_char, c_void};

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    const ICNS: &[u8] = include_bytes!("../../../assets/dextr.icns");
    unsafe {
        let msg = objc_msgSend as unsafe extern "C" fn();
        let send0: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(msg);
        let send1: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> *mut c_void = std::mem::transmute(msg);
        let send_bytes: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, usize) -> *mut c_void =
            std::mem::transmute(msg);
        let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());

        let app = send0(objc_getClass(c"NSApplication".as_ptr()), sel(c"sharedApplication"));
        let data = send_bytes(objc_getClass(c"NSData".as_ptr()), sel(c"dataWithBytes:length:"), ICNS.as_ptr().cast(), ICNS.len());
        let image = send0(objc_getClass(c"NSImage".as_ptr()), sel(c"alloc"));
        let image = send1(image, sel(c"initWithData:"), data);
        if !app.is_null() && !image.is_null() {
            send1(app, sel(c"setApplicationIconImage:"), image);
        }
    }
}
