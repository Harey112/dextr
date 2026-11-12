//! Launch at login.
//!
//! Linux uses the freedesktop autostart directory, macOS a LaunchAgent.
//! Windows (the Run registry key) comes with its backend.

use std::path::PathBuf;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn entry() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))?;
    Some(base.join("autostart").join("dextr.desktop"))
}

#[cfg(target_os = "macos")]
fn entry() -> Option<PathBuf> {
    Some(home()?.join("Library/LaunchAgents/dev.dextr.dextr.plist"))
}

#[cfg(target_os = "linux")]
fn contents(exe: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=dextr\nComment=Share one mouse and keyboard between computers\nExec=\"{}\"\nIcon=dev.dextr.dextr\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

#[cfg(target_os = "macos")]
fn contents(exe: &std::path::Path) -> String {
    let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.dextr.dextr</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

pub fn enabled() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        entry().is_some_and(|p| p.exists())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

pub fn set(on: bool) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let path = entry().ok_or("cannot find the home folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, contents(&exe)).map_err(|e| e.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = on;
        Err("launch at login isn't available on this OS yet".into())
    }
}
