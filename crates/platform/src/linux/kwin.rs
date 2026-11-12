//! Window titles and positions on KDE Plasma.
//!
//! Wayland doesn't tell applications where other windows are, but KWin
//! scripts can see them. dextr loads a small script that reports every
//! window's caption and geometry over D-Bus, and again whenever one moves,
//! so input on a shared window can be placed on the real window.

use std::path::PathBuf;

use anyhow::Context as _;
use tokio::sync::mpsc;

/// A window as KWin reports it, in compositor (logical) coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct KWinWindow {
    pub id: String,
    pub caption: String,
    pub app: String,
    pub frame: [f32; 4],
    pub buffer: [f32; 4],
    pub active: bool,
}

struct Reporter {
    tx: mpsc::UnboundedSender<KWinWindow>,
}

#[zbus::interface(name = "dev.dextr.KWin")]
impl Reporter {
    fn window(&self, line: String) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 12 {
            return;
        }
        let n = |i: usize| f[i].parse::<f32>().unwrap_or(0.0);
        let _ = self.tx.send(KWinWindow {
            id: f[0].into(),
            caption: f[1].into(),
            app: f[2].into(),
            frame: [n(3), n(4), n(5), n(6)],
            buffer: [n(7), n(8), n(9), n(10)],
            active: f[11] == "true",
        });
    }
}

const SCRIPT: &str = r#"
const SERVICE = "__SERVICE__";
function send(w) {
    const f = w.frameGeometry, b = w.bufferGeometry;
    callDBus(SERVICE, "/", "dev.dextr.KWin", "window", [
        String(w.internalId), String(w.caption).replace(/\t/g, " "), String(w.resourceClass),
        f.x, f.y, f.width, f.height, b.x, b.y, b.width, b.height, w.active
    ].join("\t"));
}
for (const w of workspace.windowList()) {
    if (!(w.normalWindow || w.dialog) || w.minimized) continue;
    send(w);
    w.frameGeometryChanged.connect(() => send(w));
    w.captionChanged.connect(() => send(w));
}
"#;

/// A loaded KWin script and the windows it reports. Call [`stop`] when done.
pub struct KWinWatch {
    conn: zbus::Connection,
    plugin: String,
    path: PathBuf,
    pub windows: mpsc::UnboundedReceiver<KWinWindow>,
}

const KWIN: &str = "org.kde.KWin";

impl KWinWatch {
    /// Starts reporting windows, or fails if this isn't KDE Plasma.
    pub async fn start() -> anyhow::Result<KWinWatch> {
        let pid = std::process::id();
        let service = format!("dev.dextr.Windows.p{pid}");
        let (tx, windows) = mpsc::unbounded_channel();
        let conn = zbus::connection::Builder::session()
            .name(service.clone())
            .serve_at("/", Reporter { tx })
            .build()
            .await
            .context("D-Bus session bus")?;

        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("dextr-windows-{pid}.js"));
        std::fs::write(&path, SCRIPT.replace("__SERVICE__", &service))?;
        let plugin = format!("dextr-windows-{pid}");
        let watch = KWinWatch { conn, plugin, path, windows };

        // A leftover from an earlier run would make loading fail.
        let _ = watch.unload().await;
        let id: i32 = watch
            .conn
            .call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(watch.path.to_string_lossy().as_ref(), watch.plugin.as_str()))
            .await
            .context("KWin scripting is unavailable (not KDE Plasma?)")?
            .body()
            .deserialize()?;
        anyhow::ensure!(id >= 0, "KWin refused the script");
        watch
            .conn
            .call_method(Some(KWIN), format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &())
            .await
            .context("running the KWin script")?;
        Ok(watch)
    }

    async fn unload(&self) -> anyhow::Result<()> {
        self.conn
            .call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(self.plugin.as_str(),))
            .await?;
        Ok(())
    }

    pub async fn stop(self) {
        let _ = self.unload().await;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Moves the window titled `caption` so its content's top-left (inside the
/// title bar) is at (x, y), with a one-shot KWin script.
pub async fn move_window(caption: &str, x: f32, y: f32) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await.context("D-Bus session bus")?;
    let pid = std::process::id();
    let n = SCRIPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let plugin = format!("dextr-place-{pid}-{n}");
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("{plugin}.js"));
    // Rust's escaped form (\", \\, \n, \u{..}) is a valid JavaScript string.
    let quoted = format!("{caption:?}");
    let script = format!(
        r#"for (const w of workspace.windowList()) {{
    if (w.caption === {quoted}) {{
        // Place the content, not the frame: offset by the title bar.
        const g = w.frameGeometry, c = w.clientGeometry;
        w.frameGeometry = {{ x: {x} - (c.x - g.x), y: {y} - (c.y - g.y), width: g.width, height: g.height }};
    }}
}}"#
    );
    std::fs::write(&path, script)?;
    let result = async {
        let id: i32 = conn
            .call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(path.to_string_lossy().as_ref(), plugin.as_str()))
            .await
            .context("KWin scripting is unavailable (not KDE Plasma?)")?
            .body()
            .deserialize()?;
        anyhow::ensure!(id >= 0, "KWin refused the script");
        conn.call_method(Some(KWIN), format!("/Scripting/Script{id}").as_str(), Some("org.kde.kwin.Script"), "run", &())
            .await?;
        anyhow::Ok(())
    }
    .await;
    // The script runs synchronously; unload it right after.
    let _ = conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(plugin.as_str(),)).await;
    let _ = std::fs::remove_file(&path);
    result
}

static SCRIPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
