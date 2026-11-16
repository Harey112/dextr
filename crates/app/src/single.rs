//! One dextr at a time (per settings folder): starting it again brings the
//! running one to the front instead.

use dextr_platform::tray::TrayAction;

/// Returns false when another dextr is running; it has been asked to show
/// its window (unless `hidden`) and this one should quit.
#[cfg(unix)]
pub fn claim(hidden: bool) -> bool {
    use std::{
        io::{Read as _, Write as _},
        os::unix::net::{UnixListener, UnixStream},
        time::Duration,
    };

    let path = socket_path();
    if let Ok(mut other) = UnixStream::connect(&path) {
        let _ = other.write_all(if hidden { b"hidden\n" } else { b"show\n" });
        return false;
    }
    // Nobody is listening: whatever is there is left over from a crash.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("cannot check for another running dextr ({e}); starting anyway");
            return true;
        }
    };
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
                let mut buf = [0u8; 16];
                let n = conn.read(&mut buf).unwrap_or(0);
                if buf[..n].starts_with(b"show") {
                    let _ = crate::tray_channel().0.send(TrayAction::Open);
                }
            }
        })
        .ok();
    true
}

#[cfg(not(unix))]
pub fn claim(_hidden: bool) -> bool {
    true
}

/// A socket named after the settings folder, so separate setups (and test
/// instances) don't collide.
#[cfg(unix)]
fn socket_path() -> std::path::PathBuf {
    let config = dextr_core::config::dir().map(|d| d.display().to_string()).unwrap_or_default();
    // FNV-1a: stable across versions, unlike the standard hasher.
    let hash = config.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("dextr-{hash:016x}.sock"))
}
