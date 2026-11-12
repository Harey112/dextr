//! Launch at login.
//!
//! Linux uses the freedesktop autostart directory. macOS (a LaunchAgent) and
//! Windows (the Run registry key) come with their backends.

#[cfg(target_os = "linux")]
fn entry() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("dextr.desktop"))
}

pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

pub fn enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        entry().is_some_and(|p| p.exists())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub fn set(on: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let path = entry().ok_or("cannot find the autostart folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=dextr\nComment=Share one mouse and keyboard between computers\nExec=\"{}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, desktop).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on;
        Err("launch at login isn't available on this OS yet".into())
    }
}
