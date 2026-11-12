use std::path::PathBuf;

use anyhow::Context as _;
use dextr_proto::{DEFAULT_PORT, Rect, Side};
use serde::{Deserialize, Serialize};

use crate::settings::{
    Appearance, ClipboardSettings, ConnectionSettings, DeviceRules, DisplayLayout, KeyboardSettings, MouseSettings, SwitchingSettings,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Shown to the peer.
    pub device_name: String,
    /// UDP port this device listens on.
    pub port: u16,
    /// Peer address as `host` or `host:port`.
    pub peer_address: String,
    /// The peer's certificate fingerprint (SHA-256). Only this peer may
    /// connect, and we only connect to it.
    pub peer_fingerprint: String,
    /// Where the peer sits relative to this device.
    pub peer_side: Side,
    /// Position of the peer's screens along that side, in pixels from the
    /// start of our edge. `None` centers them.
    pub peer_offset: Option<i32>,
    /// Exact or proportional edges.
    pub layout: DisplayLayout,
    /// When the arrangement last changed (Unix ms), so the two devices can
    /// keep the newest one.
    pub arranged_at: u64,
    /// Saved RemoteDesktop portal grant, so the permission dialog is shown once.
    pub inject_restore_token: Option<String>,
    /// The peer's name and screens from the last connection, so the display
    /// arrangement can be shown while it's offline.
    pub last_peer_name: Option<String>,
    pub last_peer_screens: Vec<Rect>,
    pub connection: ConnectionSettings,
    pub switching: SwitchingSettings,
    pub mouse: MouseSettings,
    pub keyboard: KeyboardSettings,
    pub clipboard: ClipboardSettings,
    /// What may go to and come from each paired device, by fingerprint.
    pub devices: std::collections::BTreeMap<String, DeviceRules>,
    pub appearance: Appearance,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: hostname(),
            port: DEFAULT_PORT,
            peer_address: String::new(),
            peer_fingerprint: String::new(),
            peer_side: Side::Right,
            peer_offset: None,
            layout: DisplayLayout::Exact,
            arranged_at: 0,
            inject_restore_token: None,
            last_peer_name: None,
            last_peer_screens: Vec::new(),
            connection: ConnectionSettings::default(),
            switching: SwitchingSettings::default(),
            mouse: MouseSettings::default(),
            keyboard: KeyboardSettings::default(),
            clipboard: ClipboardSettings::default(),
            devices: Default::default(),
            appearance: Appearance::default(),
        }
    }
}

fn hostname() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        return name;
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "dextr device".into())
}

pub fn dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "dextr", "dextr")
        .context("cannot determine the config directory")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

impl Config {
    /// The rules for the paired device (all allowed until changed).
    pub fn peer_rules(&self) -> DeviceRules {
        crate::identity::parse(&self.peer_fingerprint)
            .and_then(|fp| self.devices.get(&crate::identity::format(&fp)).cloned())
            .unwrap_or_default()
    }

    /// How the peer's screens sit against ours.
    pub fn link(&self) -> crate::edge::Link {
        crate::edge::Link {
            side: self.peer_side,
            offset: self.peer_offset,
            proportional: self.layout == DisplayLayout::Proportional,
        }
    }

    fn path() -> anyhow::Result<PathBuf> {
        Ok(dir()?.join("config.toml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()?;
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}
